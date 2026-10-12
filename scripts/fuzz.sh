#!/usr/bin/env bash
# Adversarial battery: hostile input, held to invariants rather than exact
# messages. Whatever a command is handed, it must:
#   - not panic (a crash is always a bug),
#   - exit 0 (accepted), 1 (refused with a message) or 2 (usage error),
#   - leave a project that still loads with a finite, non-negative length.
# Run from anywhere; it sandboxes itself.
set -uo pipefail
cd "$(dirname "$0")/.."

N=$(readlink -f ./target/release/nle)
DIR=fuzz-sandbox
mkdir -p "$DIR"
cd "$DIR"

FAIL=0
gen() { # gen NAME COLOR RATE DUR
  [ -f "$1" ] && return 0
  ffmpeg -v error -y -f lavfi -i "color=c=$2:s=320x240:d=$4:r=$3" \
    -f lavfi -i "sine=frequency=440:duration=$4" \
    -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p -c:a aac -b:a 128k "$1"
}
gen red.mp4 red 30 5
gen blue.mp4 blue 60000/1001 5
# odd media: no audio, audio only, a sub-frame file, an empty file, text
[ -f video_only.mp4 ] || ffmpeg -v error -y -f lavfi -i "color=c=orange:s=320x240:d=2:r=30" -c:v libx264 -preset ultrafast -pix_fmt yuv420p -an video_only.mp4
[ -f audio_only.m4a ] || ffmpeg -v error -y -f lavfi -i "sine=frequency=300:duration=2" -c:a aac audio_only.m4a
[ -f tiny.mp4 ] || ffmpeg -v error -y -f lavfi -i "color=c=gray:s=320x240:d=0.1:r=30" -c:v libx264 -preset ultrafast -pix_fmt yuv420p -an tiny.mp4
: > empty.mp4
printf 'not media' > notmedia.txt

# base: red 0–3, gap 3–4, blue 4–7, pool of three
if [ ! -f base.json ]; then
  $N --project base.json init --force >/dev/null
  $N --project base.json import red.mp4 blue.mp4 >/dev/null
  $N --project base.json add red.mp4  --in 0 --out 3 >/dev/null
  $N --project base.json add blue.mp4 --at 4 --in 0 --out 3 >/dev/null
  $N --project base.json seek 1.5 >/dev/null
fi

# The invariant: run a command on a fresh copy of base.json and check the
# three rules above.
case_inv() { # case_inv LABEL CMD...
  local label=$1; shift
  rm -f f.json f.history.json f.clipboard.json
  cp base.json f.json
  local out code bad=""
  out=$("$@" 2>&1); code=$?
  case "$out" in
    *panicked*|*RUST_BACKTRACE*) bad="panicked" ;;
  esac
  if [ -z "$bad" ] && [ "$code" -gt 2 ]; then bad="exit $code"; fi
  if [ -z "$bad" ]; then
    local show len
    if ! show=$($N --project f.json show 2>&1); then
      bad="project no longer loads"
    else
      len=$(printf '%s' "$show" | head -1 | sed -n 's/.*· \([0-9.]*\) s.*/\1/p')
      if ! python3 -c "import sys; v=float('$len'); sys.exit(0 if v >= 0 and v < 1e9 else 1)" 2>/dev/null; then
        bad="corrupt length (${len:-?})"
      fi
    fi
  fi
  if [ -n "$bad" ]; then
    echo "  ✗ $label — $bad: $(printf '%s' "$out" | head -1)"
    FAIL=$((FAIL+1))
  else
    echo "  ✓ $label"
  fi
}

echo "== addressing and ranges =="
case_inv "split at 0"                  $N --project f.json split --at 0
case_inv "split at the end"            $N --project f.json split --at 7
case_inv "split on a cut"              $N --project f.json split --at 3
case_inv "split inside a gap"          $N --project f.json split --at 3.5
case_inv "split at -1"                 $N --project f.json split --at -1
case_inv "split at 999"                $N --project f.json split --at 999
case_inv "split at 1e18"               $N --project f.json split --at 1000000000000000000
case_inv "split at abc"                $N --project f.json split --at abc
case_inv "split at 1:2:3:4"            $N --project f.json split --at 1:2:3:4
case_inv "split at ''"                 $N --project f.json split --at ""
case_inv "trim-in at 0"                $N --project f.json trim-in --at 0
case_inv "trim-in at the end"          $N --project f.json trim-in --at 7
case_inv "trim-out at 0"               $N --project f.json trim-out --at 0
case_inv "delete at 7"                 $N --project f.json delete --at 7
case_inv "delete --clip 999"           $N --project f.json delete --clip 999
case_inv "delete --seg 99"             $N --project f.json delete --seg 99
case_inv "delete --seg 2^64-1"         $N --project f.json delete --seg 18446744073709551615
case_inv "yank --clip 999"             $N --project f.json yank --clip 999
case_inv "paste with an empty register" $N --project f.json paste --at 1

echo "== lengths and rates =="
case_inv "gap frames 0"                $N --project f.json gap --at 1 --frames 0
case_inv "gap frames -5"               $N --project f.json gap --at 1 --frames -5
case_inv "gap dur 0"                   $N --project f.json gap --at 1 --dur 0
case_inv "gap dur -2"                  $N --project f.json gap --at 1 --dur -2
case_inv "gap frames i64::MAX"         $N --project f.json gap --at 1 --frames 9223372036854775807
case_inv "gap at 1e18"                 $N --project f.json gap --at 1000000000000000000 --frames 30
case_inv "gap at 999 (bridges a void)" $N --project f.json gap --at 999 --frames 30
case_inv "speed 0"                     $N --project f.json speed 1 0
case_inv "speed -1"                    $N --project f.json speed 1 -1
case_inv "speed abc"                   $N --project f.json speed 1 abc
case_inv "speed 1/0"                   $N --project f.json speed 1 1/0
case_inv "speed 0.000001"              $N --project f.json speed 1 0.000001
case_inv "speed 100 (sub-frame)"       $N --project f.json speed 2 100

echo "== extend =="
case_inv "extend to the same edge"     $N --project f.json extend --clip 1 --edge out --to 3
case_inv "extend to the opposite end"  $N --project f.json extend --clip 1 --edge out --to 0
case_inv "extend beyond the material"  $N --project f.json extend --clip 1 --edge out --to 99
case_inv "extend to -5"                $N --project f.json extend --clip 1 --edge out --to -5
case_inv "extend a gap's head back"    $N --project f.json extend --seg 1 --edge in --to -2
case_inv "extend --edge sideways"      $N --project f.json extend --clip 1 --edge sideways --to 2
case_inv "extend a missing clip"       $N --project f.json extend --clip 999 --edge in --to 1

echo "== tracks and pool =="
case_inv "track rm the only video"     $N --project f.json track rm 1
case_inv "track rm 99"                 $N --project f.json track rm 99
case_inv "track mute 99"               $N --project f.json track mute 99
case_inv "track add 0"                 $N --project f.json add red.mp4 --track 0
case_inv "track add 99"                $N --project f.json add red.mp4 --track 99
case_inv "add --track 0"               $N --project f.json add red.mp4 --track 0
case_inv "media rm an in-use asset"    $N --project f.json media rm 1
case_inv "media rm 99"                 $N --project f.json media rm 99
case_inv "undo on an empty stack"      $N --project f.json undo
case_inv "redo on an empty stack"      $N --project f.json redo
case_inv "seek -1"                     $N --project f.json seek -1
case_inv "seek 1e18"                   $N --project f.json seek 1000000000000000000

echo "== odd media =="
case_inv "add video-only onto an audio track" bash -c ''"$N"' --project f.json track add audio >/dev/null; '"$N"' --project f.json add video_only.mp4 --track 2 --in 0 --out 1'
case_inv "add audio-only onto a video track"  $N --project f.json add audio_only.m4a --in 0 --out 1
case_inv "add a 0.1 s file as 1 s"            $N --project f.json add tiny.mp4 --in 0 --out 1
case_inv "add an empty file"                  $N --project f.json add empty.mp4
case_inv "add a text file"                    $N --project f.json add notmedia.txt
case_inv "add a directory"                    $N --project f.json add .
case_inv "add --in 5 --out 2"                 $N --project f.json add red.mp4 --in 5 --out 2
case_inv "add --out 99"                       $N --project f.json add red.mp4 --in 0 --out 99
case_inv "add --at -1"                        $N --project f.json add red.mp4 --at -1
case_inv "import a missing file"              $N --project f.json import nope.mp4
case_inv "import an empty file"               $N --project f.json import empty.mp4
case_inv "import a text file"                 $N --project f.json import notmedia.txt
case_inv "detect-edits a missing file"        $N --project f.json detect-edits nope.mov --force
case_inv "detect-edits an audio-only file"    $N --project f.json detect-edits audio_only.m4a --force
case_inv "import-edl a missing file"          $N --project f.json import-edl nope.edl --media . --force
case_inv "import-otio a missing file"         $N --project f.json import-otio nope.otio --force
case_inv "trim --out past the file"           $N trim red.mp4 --in 0 --out 99 -o t.mp4
case_inv "trim inverted"                      $N trim red.mp4 --in 3 --out 1 -o t.mp4

echo "== odd projects =="
rm -f empty.json empty.history.json
$N --project empty.json init --force >/dev/null
case_inv "split on an empty timeline"   $N --project empty.json split --at 1
case_inv "gap on an empty timeline"     $N --project empty.json gap --at 1 --frames 30
case_inv "export an empty timeline"     $N --project empty.json export -o x.mp4
case_inv "seek an empty timeline"       $N --project empty.json seek 5
case_inv "track ls an empty timeline"   $N --project empty.json track ls
case_inv "media rm on an empty pool"    $N --project empty.json media rm 1
printf '{ not json' > bad.json
case_inv "show a corrupt project"       $N --project bad.json show
printf '{"version": 3}' > old.json
case_inv "show an old-version project"  $N --project old.json show
case_inv "show a missing project"       $N --project nope.json show

echo "== output plumbing =="
rm -f p.json p.history.json
cp base.json p.json
if $N --project p.json media ls > /dev/full 2>/dev/null; then
  echo "  ✓ a failed stdout write exits cleanly"
else echo "  ✗ a failed stdout write crashed"; FAIL=$((FAIL+1)); fi
$N --project p.json media ls 2>/dev/null | head -1 >/dev/null
if [ "${PIPESTATUS[0]}" -le 2 ]; then
  echo "  ✓ an early-closed pipe exits cleanly"
else echo "  ✗ a broken pipe crashed the CLI (${PIPESTATUS[0]})"; FAIL=$((FAIL+1)); fi

echo
if [ "$FAIL" -eq 0 ]; then echo "fuzz: ALL INVARIANTS HELD"; else echo "fuzz: $FAIL INVARIANT(S) BROKEN"; exit 1; fi
