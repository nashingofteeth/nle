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
# Generated-media caches (black filler, proxies) live in the sandbox: a run
# must not read a warm cache from a previous one, or write the user's.
export XDG_CACHE_HOME="$PWD/.cache"

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

# base: red 0–3, gap 3–4, blue 4–7, pool of three. Always rebuilt: a stale
# base (from before a format-version bump) would fail to load and poison
# every case with "project version … is not supported".
$N --project base.json init --force >/dev/null
$N --project base.json import red.mp4 blue.mp4 >/dev/null
$N --project base.json add red.mp4  --in 0 --out 3 >/dev/null
$N --project base.json add blue.mp4 --at 4 --in 0 --out 3 >/dev/null
$N --project base.json seek 1.5 >/dev/null

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
case_inv "speed 1e18"                  $N --project f.json speed 1 1000000000000000000
case_inv "speed i64::MAX/1"            $N --project f.json speed 1 9223372036854775807/1
case_inv "speed 1/i64::MAX"            $N --project f.json speed 1 1/9223372036854775807

echo "== transitions =="
# base is red 0–3, gap 3–4, blue 4–7: 3.0 is a clip|gap seam.
case_inv "transition frames 0"             $N --project f.json transition --at 3 --frames 0
case_inv "transition frames -5"            $N --project f.json transition --at 3 --frames -5
case_inv "transition frames i64::MAX"      $N --project f.json transition --at 3 --frames 9223372036854775807
case_inv "transition dur 0"                $N --project f.json transition --at 3 --dur 0
case_inv "transition dur -1"               $N --project f.json transition --at 3 --dur -1
case_inv "transition --frames and --dur"   $N --project f.json transition --at 3 --frames 15 --dur 1
case_inv "transition kind sideways"        $N --project f.json transition sideways --at 3 --frames 15
case_inv "transition at 0"                 $N --project f.json transition --at 0 --frames 15
case_inv "transition at the end"           $N --project f.json transition --at 7 --frames 15
case_inv "transition at 1e18"              $N --project f.json transition --at 1000000000000000000 --frames 15
case_inv "transition at 999"               $N --project f.json transition --at 999 --frames 15
case_inv "transition mid-clip"             $N --project f.json transition --at 1.5 --frames 15
case_inv "transition longer than the clip" $N --project f.json transition --at 3 --dur 99
case_inv "transition at a clip|clip seam"  bash -c ''"$N"' --project f.json split --at 1 >/dev/null; '"$N"' --project f.json transition --at 1 --frames 15'
case_inv "transition twice at one seam"    bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json transition --at 3 --frames 15'
case_inv "transition then purge-stumps"    bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json purge-stumps'
case_inv "transition then export"          bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json export -o tx.mp4'
case_inv "transition then delete --seg"    bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json delete --seg 1'
case_inv "transition then delete twice"    bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json delete --at 3.1 >/dev/null; '"$N"' --project f.json delete --at 3.1'
case_inv "transition between two gaps"     bash -c ''"$N"' --project f.json delete --seg 0 >/dev/null; '"$N"' --project f.json delete --seg 1 >/dev/null; '"$N"' --project f.json transition --at 0 --frames 15'
case_inv "transition on an empty timeline" $N --project empty.json transition --at 1 --frames 15

echo "== effects =="
case_inv "effect crop w 0"                $N --project f.json effect crop --clip 1 --w 0 --h 10
case_inv "effect crop h 0"                $N --project f.json effect crop --clip 1 --w 10 --h 0
case_inv "effect scale w -1"              $N --project f.json effect scale --clip 1 --w -1 --h 10
case_inv "effect scale w 2^64"            $N --project f.json effect scale --clip 1 --w 18446744073709551616 --h 10
case_inv "effect scale 1x1"               $N --project f.json effect scale --clip 1 --w 1 --h 1
case_inv "effect eq brightness abc"       $N --project f.json effect eq --clip 1 --brightness abc
case_inv "effect eq contrast 1e18"        $N --project f.json effect eq --clip 1 --contrast 1000000000000000000
case_inv "effect lut a missing file"      $N --project f.json effect lut --clip 1 --path /nope.cube
case_inv "effect lut an empty path"       $N --project f.json effect lut --clip 1 --path ""
case_inv "effect lut a path with commas"  $N --project f.json effect lut --clip 1 --path "/tmp/a,b:c.cube"
case_inv "effect unknown kind"            $N --project f.json effect bogus --clip 1
case_inv "effect without a kind"          $N --project f.json effect --clip 1
case_inv "effect clip 999"                $N --project f.json effect eq --clip 999
case_inv "effect without --clip"          $N --project f.json effect eq
case_inv "effect on an audio track"       bash -c ''"$N"' --project f.json track add audio >/dev/null; '"$N"' --project f.json effect eq --clip 1 --track 2'
case_inv "effect then purge-stumps"       bash -c ''"$N"' --project f.json effect eq --clip 1 --saturation 0 >/dev/null; '"$N"' --project f.json purge-stumps'
case_inv "effect then export a bad lut"   bash -c ''"$N"' --project f.json effect lut --clip 1 --path /nope.cube >/dev/null; '"$N"' --project f.json export -o lx.mp4'
case_inv "effect then speed then delete"  bash -c ''"$N"' --project f.json effect eq --clip 1 >/dev/null; '"$N"' --project f.json speed 1 2 >/dev/null; '"$N"' --project f.json delete --clip 1'
case_inv "effect on an empty timeline"    $N --project empty.json effect eq --clip 1

echo "== speeds next to transitions =="
# The retime has to retime the transition side it owns; the guards must hold
# while it splits `dur × rate` out of the clip.
case_inv "speed a transition's neighbour 1e6"  bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json speed 1 1000000'
case_inv "speed a transition's neighbour 1e-6" bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json speed 1 0.000001'
case_inv "speed the clip out of a transition"  bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json speed 2 2'
case_inv "speed a transition id"               bash -c ''"$N"' --project f.json transition --at 3 --frames 15 >/dev/null; '"$N"' --project f.json speed 4 2'

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

echo "== hostile projects (the transition model) =="
# The loader validates, so each of these must be refused with a message — not
# panic, and not divide by a zero `dur` somewhere downstream.
python3 - <<'PY'
import copy, json
base = json.load(open('base.json'))

def write(name, segs, media=None):
    d = copy.deepcopy(base)
    d['timeline']['tracks'][0]['segments'] = segs
    if media is not None:
        d['media'] = media
    json.dump(d, open(name, 'w'))

def tr(dur="1/1", kind="Dissolve", a=None, b="Black"):
    return {"Transition": {"id": 9, "dur": dur, "kind": kind,
                           "a": a or {"Clip": {"asset": 1, "src_in": "0/1", "src_out": "1/1"}},
                           "b": b}}

write('h_dur0.json', [tr(dur="0/1")])
write('h_span.json', [tr(a={"Clip": {"asset": 1, "src_in": "2/1", "src_out": "1/1"}})])
write('h_black.json', [tr(a="Black")])
write('h_asset.json', [tr(a={"Clip": {"asset": 99, "src_in": "0/1", "src_out": "1/1"}})])
write('h_media.json', [tr(a={"Clip": {"asset": 1, "src_in": "4/1", "src_out": "9/1"}})])
write('h_kind.json', [tr(kind="Wipe")])
write('h_speed0.json', [{"Clip": {"id": 1, "asset": 1, "asset_in": "0/1",
                                  "asset_out": "3/1", "speed": "0/1", "effects": []}}])
write('h_speed_tiny.json', [{"Clip": {"id": 1, "asset": 1, "asset_in": "0/1",
                                      "asset_out": "3/1", "speed": "1/9223372036854775807",
                                      "effects": []}}])
write('h_speed_neg.json', [{"Clip": {"id": 1, "asset": 1, "asset_in": "0/1",
                                     "asset_out": "3/1", "speed": "-1/1", "effects": []}}])
write('h_len_overflow.json', [{"Clip": {"id": 1, "asset": 1, "asset_in": "0/1",
                                        "asset_out": "9223372036854775807/1",
                                        "speed": "1/1", "effects": []}},
                               {"Clip": {"id": 2, "asset": 1, "asset_in": "0/1",
                                         "asset_out": "9223372036854775807/1",
                                         "speed": "1/1", "effects": []}}])
# Two tiny values whose exact sum overflows the rational's numerator.
write('h_len_frac.json', [{"Clip": {"id": 1, "asset": 1, "asset_in": "0/1",
                                    "asset_out": "1/9223372036854775807",
                                    "speed": "1/1", "effects": []}},
                          {"Clip": {"id": 2, "asset": 1, "asset_in": "0/1",
                                    "asset_out": "1/9223372036854775806",
                                    "speed": "1/1", "effects": []}}])
write('h_empty_pool.json', [tr()], media=[])
PY
case_inv "show a transition with dur 0"           $N --project h_dur0.json show
case_inv "show a transition with a reversed side" $N --project h_span.json show
case_inv "show a transition with no clip side"    $N --project h_black.json show
case_inv "show a transition on a missing asset"   $N --project h_asset.json show
case_inv "show a transition past its media"       $N --project h_media.json show
case_inv "show a transition with an unknown kind" $N --project h_kind.json show
case_inv "show a clip with speed 0"               $N --project h_speed0.json show
case_inv "show a clip with a tiny speed"          $N --project h_speed_tiny.json show
case_inv "show a clip with a negative speed"      $N --project h_speed_neg.json show
case_inv "show a timeline whose length overflows" $N --project h_len_overflow.json show
case_inv "show a timeline of huge-denominator slivers" $N --project h_len_frac.json show
case_inv "show a transition with an empty pool"   $N --project h_empty_pool.json show
case_inv "export a transition past its media"     $N --project h_media.json export -o hx.mp4
case_inv "speed a project with a broken transition" $N --project h_span.json speed 1 2

echo "== a truncated cached proxy or black file =="
rm -f px.json px.history.json
cp base.json px.json
# A cache entry the hash will hit again: it must be a real file, not a stub
# left behind by a full disk or an interrupted copy.
$N --project px.json effect eq --clip 1 --saturation 0 >/dev/null
$N --project px.json edl --track 1 >/dev/null
find .cache/nle -name '*.mp4' -exec sh -c ': > "$1"' _ {} \;
if ! out=$($N --project px.json edl --track 1 2>&1); then
  echo "  ✗ edl failed on a truncated cache entry: $out"
  FAIL=$((FAIL+1))
elif find .cache/nle -name '*.mp4' -empty | grep -q .; then
  echo "  ✗ a truncated cache entry is still served"
  FAIL=$((FAIL+1))
else
  echo "  ✓ truncated cache entries are re-rendered"
fi

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
