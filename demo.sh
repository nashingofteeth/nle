#!/usr/bin/env bash
# nle demo — generates colorful sample media (mixed fps/resolution!) and
# builds a demo project end to end. Run from anywhere: it sandboxes itself.
set -euo pipefail
cd "$(dirname "$0")"

N=$(readlink -f ./target/release/nle)
DIR=demo-sandbox
mkdir -p "$DIR"
cd "$DIR"

gen() { # gen NAME COLOR SIZE RATE DUR FREQ
  local name=$1 color=$2 size=$3 rate=$4 dur=$5 freq=$6
  if [ ! -f "$name" ]; then
    echo "== generating $name ($color, $size @ $rate, ${dur}s) =="
    ffmpeg -v error -y \
      -f lavfi -i "color=c=$color:s=$size:d=$dur:r=$rate" \
      -f lavfi -i "sine=frequency=$freq:duration=$dur" \
      -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p \
      -c:a aac -b:a 128k "$name"
  fi
}

# A "real-looking" but synthetic clip: a linear colour gradient with a sine
# audio bed. Deterministic, and detailed enough that a crop/scale is visible
# (the flat colour clips above hide geometry effects).
grad() { # grad NAME SIZE RATE DUR R0 G0 B0 R1 G1 B1 AXIS FREQ
  local name=$1 size=$2 rate=$3 dur=$4
  local r0=$5 g0=$6 b0=$7 r1=$8 g1=$9 b1=${10} axis=${11} freq=${12}
  [ -f "$name" ] && return
  local w=${size%x*} h=${size#*x} expr
  if [ "$axis" = x ]; then
    expr="geq=r='$r0+$((r1 - r0))*X/$((w - 1))':g='$g0+$((g1 - g0))*X/$((w - 1))':b='$b0+$((b1 - b0))*X/$((w - 1))'"
  else
    expr="geq=r='$r0+$((r1 - r0))*Y/$((h - 1))':g='$g0+$((g1 - g0))*Y/$((h - 1))':b='$b0+$((b1 - b0))*Y/$((h - 1))'"
  fi
  echo "== generating $name (gradient, $size @ $rate, ${dur}s) =="
  ffmpeg -v error -y \
    -f lavfi -i "color=c=black:s=$size:d=$dur:r=$rate" \
    -f lavfi -i "sine=frequency=$freq:duration=$dur" \
    -vf "$expr" -c:v libx264 -preset fast -crf 20 -pix_fmt yuv420p \
    -c:a aac -b:a 96k "$name"
}

# Does a command's combined output contain PATTERN? Captured first, because
# `cmd | grep -q` under `set -o pipefail` can kill the producer with SIGPIPE
# (grep exits on the first match) and report failure on a match.
output_has() { # output_has PATTERN CMD...
  local pat=$1 out
  shift
  out=$("$@" 2>&1 || true)
  [[ "$out" == *"$pat"* ]]
}

gen red.mp4   red    320x240   30           5 440
gen blue.mp4  blue   640x360   60000/1001   6 880
gen green.mp4 green  1280x720  24           4 1320

echo "== building demo project =="
$N init --force
$N import red.mp4 blue.mp4 green.mp4
$N add red.mp4  --in 1 --out 4          # 30fps src  → 0–3
$N add blue.mp4 --in 0.5 --out 3.5      # 59.94fps!  → 3–6
$N add green.mp4 --at 7 --in 0 --out 2  # gap 6–7    → 7–9
$N show

echo "== exporting (libx264) =="
$N export -o demo.mp4

echo "== content check: first pixel at key moments (rrggbb hex) =="
# expect: red … blue … black(gap) … green
for t in 1 4.5 6.5 8; do
  ffmpeg -v error -y -ss "$t" -i demo.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "  t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw

echo "== multi-track: overlay + audio mix =="
rm -f multi.json multi.history.json
$N --project multi.json init --force >/dev/null
$N --project multi.json import red.mp4 blue.mp4 green.mp4 >/dev/null
$N --project multi.json add red.mp4   --in 0 --out 3              # base 0–3
$N --project multi.json add blue.mp4  --at 3 --in 0 --out 3       # base 3–6
$N --project multi.json add green.mp4 --at 6 --in 0 --out 3       # base 6–9
$N --project multi.json track add video >/dev/null               # upper video (index 1)
$N --project multi.json add red.mp4 --track 1 --at 4 --in 1 --out 2  # overlay red 4–5
$N --project multi.json track add audio >/dev/null               # audio track (index 2)
$N --project multi.json add green.mp4 --track 2 --in 0 --out 2   # audio bed 0–2
$N --project multi.json track ls
$N --project multi.json export -o demo-multi.mp4 >/dev/null
echo "  expect: red(base) blue(through top gap) red(overlaid) green(base)"
for t in 1.5 3.5 4.5 7; do
  ffmpeg -v error -y -ss "$t" -i demo-multi.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw
if output_has audio ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 demo-multi.mp4; then
  echo "  ✓ multi-track export has an audio stream"
else
  echo "  ✗ multi-track export has no audio"; exit 1
fi

echo "== showcase: transition · crop · scale overlay · eq · speed =="
grad sky.mp4 640x360 30 4 11 29 58 255 177 106 x 440
grad sea.mp4 1280x720 24 4 4 37 58 158 231 255 y 660
grad sun.mp4 320x240 30 2 254 227 166 254 136 61 y 880

rm -f showcase.json showcase.history.json
$N --project showcase.json init --force >/dev/null
$N --project showcase.json import sky.mp4 sea.mp4 sun.mp4 >/dev/null
$N --project showcase.json add sky.mp4 --in 0 --out 2 >/dev/null
$N --project showcase.json add sea.mp4 --at 2 --in 0 --out 2 >/dev/null
$N --project showcase.json add sun.mp4 --at 4 --in 0 --out 1 >/dev/null
# A dissolve at the cut, effects on both neighbours and the last clip, then a
# speed change that shortens the tail.
$N --project showcase.json transition --at 2 --frames 30 >/dev/null
$N --project showcase.json effect crop --clip 1 --left 40 >/dev/null
$N --project showcase.json effect eq --clip 3 --saturation 0 >/dev/null
$N --project showcase.json speed 3 200 >/dev/null
# An upper track: scale it down (the transparent pad reveals the sea), then
# crop the scaled picture itself. Plus an audio bed.
$N --project showcase.json track add video >/dev/null
$N --project showcase.json add sun.mp4 --track 2 --at 2.1 --in 0 --out 0.8 >/dev/null
$N --project showcase.json effect scale --clip 5 --w 50 --h 50 >/dev/null
$N --project showcase.json effect crop --clip 5 --left 30 >/dev/null
$N --project showcase.json effect offset --clip 5 --x 0 --y -20 >/dev/null
$N --project showcase.json track add audio >/dev/null
$N --project showcase.json add sea.mp4 --track 3 --in 0 --out 2 >/dev/null
$N --project showcase.json show
$N --project showcase.json export -o showcase.mp4 >/dev/null

# Sample one pixel. `crop=2:2` keeps the chroma valid on yuv420p.
px() { ffmpeg -v error -y -ss "$1" -i "$2" -frames:v 1 -vf "crop=2:2:$3:$4" -f rawvideo -pix_fmt rgb24 - | xxd -p -l 3; }
chan() { echo $((16#${1:$2:2})); }
near() { # near A B TOL — every RGB channel within TOL
  local a=$1 b=$2 tol=$3 i x y d
  for i in 0 2 4; do
    x=$(chan "$a" $i); y=$(chan "$b" $i)
    d=$((x > y ? x - y : y - x))
    [ "$d" -le "$tol" ] || return 1
  done
}
dark() { # every channel <= 24
  local i x
  for i in 0 2 4; do
    x=$(chan "$1" $i)
    [ "$x" -le 24 ] || return 1
  done
}
greyish() { # max channel - min channel <= 16
  local r g b max min v
  r=$(chan "$1" 0); g=$(chan "$1" 2); b=$(chan "$1" 4)
  max=$r; min=$r
  for v in $g $b; do
    [ "$v" -gt "$max" ] && max=$v
    [ "$v" -lt "$min" ] && min=$v
  done
  [ $((max - min)) -le 16 ]
}
warm() { # the sun is much redder than blue
  local r b
  r=$(chan "$1" 0); b=$(chan "$1" 4)
  [ "$r" -ge 180 ] && [ $((r - b)) -ge 60 ]
}
cool() { # the sea is blue/teal: blue clearly above red
  local r b
  r=$(chan "$1" 0); b=$(chan "$1" 4)
  [ "$b" -ge 40 ] && [ $((b - r)) -ge 20 ]
}

crop_edge=$(px 0.5 showcase.mp4 2 180)
crop_kept=$(px 0.5 showcase.mp4 300 180)
crop_raw=$(px 0.5 sky.mp4 300 180)
if dark "$crop_edge" && near "$crop_kept" "$crop_raw" 24; then
  echo "  ✓ crop left 40 masks the edge (${crop_edge}) and keeps the sky in place (${crop_kept})"
else
  echo "  ✗ crop wrong (masked ${crop_edge}, kept ${crop_kept}, source ${crop_raw})"; exit 1
fi
mid=$(px 2.5 showcase.mp4 318 178)
cut=$(px 2.5 showcase.mp4 200 180)
corner=$(px 2.5 showcase.mp4 2 2)
bottom=$(px 2.5 showcase.mp4 318 330)
if warm "$mid" && cool "$cut" && cool "$corner" && cool "$bottom"; then
  echo "  ✓ overlay scale 50 + crop 30 + offset -20: sun ${mid}; cut ${cut}, pad ${corner} and vacated ${bottom} show the sea"
else
  echo "  ✗ overlay/scale/crop/offset wrong (centre ${mid}, cut ${cut}, corner ${corner}, bottom ${bottom})"; exit 1
fi

# Offset 100% of the canvas is fully off screen, not a sliver at the edge.
$N --project showcase.json effect offset --clip 5 --x 100 --y 0 >/dev/null
$N --project showcase.json export -o showcase-off.mp4 >/dev/null
off=$(px 2.5 showcase-off.mp4 318 178)
if cool "$off"; then
  echo "  ✓ offset 100% is off screen (sea ${off})"
else
  echo "  ✗ offset 100% still shows the overlay (${off})"; exit 1
fi

# 80% is 80% of the way to the edge: a strip stays visible.
$N --project showcase.json effect offset --clip 5 --x 80 --y -20 >/dev/null
$N --project showcase.json export -o showcase-edge.mp4 >/dev/null
edge=$(px 2.5 showcase-edge.mp4 630 120)
amid=$(px 2.5 showcase-edge.mp4 318 178)
if warm "$edge" && cool "$amid"; then
  echo "  ✓ offset 80% leaves the sun at the edge (${edge}) and the sea in the middle (${amid})"
else
  echo "  ✗ offset 80% wrong (edge ${edge}, middle ${amid})"; exit 1
fi
$N --project showcase.json effect offset --clip 5 --x 0 --y -20 >/dev/null
grey_px=$(px 3.2 showcase.mp4 318 178)
if greyish "$grey_px"; then
  echo "  ✓ eq saturation 0 is grey (${grey_px})"
else
  echo "  ✗ eq not applied (${grey_px})"; exit 1
fi
if $N --project showcase.json show | head -1 | grep -q '3.5 s'; then
  echo "  ✓ transition shortening + 2× speed: 3.5 s timeline"
else
  echo "  ✗ unexpected showcase length:"; $N --project showcase.json show | head -1; exit 1
fi
if output_has audio ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 showcase.mp4; then
  echo "  ✓ showcase export has an audio stream"
else
  echo "  ✗ showcase export has no audio"; exit 1
fi

# Retime the existing blend in place: 15 frames gives the neighbours 0.5 s
# back, so the timeline grows from 3.5 s to 4 s.
$N --project showcase.json transition --at 2 --frames 15 >/dev/null
if $N --project showcase.json show | head -1 | grep -q '· 4 s ·' \
  && $N --project showcase.json show | grep -q '(15f)'; then
  echo "  ✓ transition retimed to 15f (timeline 4 s)"
else
  echo "  ✗ transition retime wrong:"; $N --project showcase.json show | head -4; exit 1
fi
$N --project showcase.json transition --at 2 --frames 30 >/dev/null

echo "== EDL (CMX3600) import =="
cat > demo.edl <<'EOF'
TITLE: demo
FCM: NON-DROP FRAME

001  AX  V  C  00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00
* FROM CLIP NAME: red.mp4
002  AX  V  C  00:00:00:00 00:00:02:00 01:00:03:00 01:00:05:00
* FROM CLIP NAME: blue.mp4
EOF
rm -f edl-demo.json edl-demo.history.json
$N --project edl-demo.json import-edl demo.edl --media . --force >/dev/null
$N --project edl-demo.json export -o demo-edl.mp4 >/dev/null
echo "  expect: red 0–2, gap 2–3, blue 3–5"
for t in 1 3.5; do
  ffmpeg -v error -y -ss "$t" -i demo-edl.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw
# A directory scan is a candidate list: one unreadable file must not sink it.
printf 'not media' > junk.mp4
rm -f edl-junk.json edl-junk.history.json
out=$($N --project edl-junk.json import-edl demo.edl --media . --force 2>&1)
rm -f junk.mp4
if printf '%s' "$out" | grep -q "skipping ./junk.mp4" && printf '%s' "$out" | grep -q "✓"; then
  echo "  ✓ an unreadable file in --media is skipped, not fatal"
else
  echo "  ✗ a bad file in --media sank the import"; exit 1
fi

echo "== detect-edits: reverse-engineer a rendered file =="
# A known 4-shot file: red 0–1, blue 1–2, black 2–3, green 3–4, with an
# audio bed. The black span exercises gap recovery (scene detection alone
# misses cuts to black); the audio gives the project 1 video + 1 audio
# track, exercising the multi-track export path.
ffmpeg -v error -y \
  -f lavfi -i "color=c=red:s=320x240:r=30:d=1" \
  -f lavfi -i "color=c=blue:s=320x240:r=30:d=1" \
  -f lavfi -i "color=c=black:s=320x240:r=30:d=1" \
  -f lavfi -i "color=c=green:s=320x240:r=30:d=1" \
  -f lavfi -i "sine=frequency=440:duration=4" \
  -filter_complex "[0:v][1:v][2:v][3:v]concat=n=4:v=1:a=0[v]" \
  -map "[v]" -map 4:a -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p \
  -c:a aac detect-src.mp4
rm -f detect.json detect.history.json
$N --project detect.json detect-edits detect-src.mp4 --force
$N --project detect.json export -o detect-out.mp4 >/dev/null
echo "  expect: red 0–1, blue 1–2, black(gap) 2–3, green 3–4"
for t in 0.5 1.5 2.5 3.5; do
  ffmpeg -v error -y -ss "$t" -i detect-out.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw
detect_gaps=$($N --project detect.json show | grep -c 'gap' || true)
if [ "$detect_gaps" -eq 2 ]; then
  echo "  ✓ black span recovered as one gap (video + mirrored audio)"
else
  echo "  ✗ expected 2 gap rows (v1+a1), got $detect_gaps"; exit 1
fi
if output_has audio ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 detect-out.mp4; then
  echo "  ✓ reconstructed audio track exported"
else
  echo "  ✗ reconstructed project has no audio"; exit 1
fi

echo "== detect-edits --append into an existing project =="
rm -f append.json append.history.json
$N --project append.json init --force >/dev/null
$N --project append.json add red.mp4 --in 0 --out 1 >/dev/null
$N --project append.json detect-edits detect-src.mp4 --append
$N --project append.json export -o append-out.mp4 >/dev/null
echo "  expect: red 0–1 (existing), red 1–2, blue 2–3, black 3–4, green 4–5"
for t in 0.5 1.5 2.5 3.5 4.5; do
  ffmpeg -v error -y -ss "$t" -i append-out.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw

echo "== agent chain: shell edits + undo + shared register =="
rm -f agent.json agent.history.json agent.clipboard.json
$N --project agent.json init --force >/dev/null
$N --project agent.json import red.mp4 blue.mp4 >/dev/null
$N --project agent.json add red.mp4  --in 0 --out 4 >/dev/null   # #1 0–4
$N --project agent.json add blue.mp4 --at 4 --in 0 --out 4 >/dev/null  # #2 4–8
$N --project agent.json split --at 2                             # #1 0–2, #3 2–4
$N --project agent.json delete --clip 1 --ripple
$N --project agent.json undo                                     # the delete undoes
$N --project agent.json redo                                     # and redo reapplies it
$N --project agent.json undo                                     # undo again: back to the split
$N --project agent.json gap --at 4 --frames 30                   # gap 4–5
$N --project agent.json yank --clip 3
$N --project agent.json paste --at 5 --ripple                    # red 5–7
$N --project agent.json seek 2.5
$N --project agent.json seek                                     # prints the cursor
$N --project agent.json show
$N --project agent.json export -o agent.mp4 >/dev/null
echo "  expect: red 0–4 · black 4–5 · red 5–7 · blue 7–11"
for t in 1 4.5 6 9; do
  ffmpeg -v error -y -ss "$t" -i agent.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw
if output_has "at 2.5 s" $N --project agent.json show; then
  echo "  ✓ shared playhead stored (nle seek → nle show)"
else
  echo "  ✗ playhead not stored"; exit 1
fi
if [ -f agent.clipboard.json ]; then
  echo "  ✓ shared register written (nle yank → nle paste)"
else
  echo "  ✗ no register sidecar"; exit 1
fi

echo "== shell editing: trim · extend · split · gap · speed =="
rm -f edit.json edit.history.json edit.clipboard.json
$N --project edit.json init --force >/dev/null
$N --project edit.json import red.mp4 blue.mp4 >/dev/null
$N --project edit.json add red.mp4  --in 0 --out 4               # #1 red 0–4
$N --project edit.json add blue.mp4 --at 4 --in 0 --out 4        # #2 blue 4–8
$N --project edit.json trim-out --at 3                           # #1 0–3, gap 3–4
$N --project edit.json extend --clip 1 --edge out --to 4         # #1 grows back over the gap
$N --project edit.json split --at 5                              # #2 4–5, #3 5–8
$N --project edit.json gap --dur 0.5 --at 5                      # 15-frame gap 5–5.5
$N --project edit.json speed 3 200                                 # #3 at 200%: 5.5–7
$N --project edit.json show
$N --project edit.json export -o edit.mp4 >/dev/null
echo "  expect: red 0–4 · blue 4–5 · black 5–5.5 · blue 5.5–7"
for t in 1 4.5 5.25 6.5; do
  ffmpeg -v error -y -ss "$t" -i edit.mp4 -frames:v 1 -f rawvideo -pix_fmt rgb24 pixel.raw
  printf "    t=%-4s → %s\n" "$t" "$(xxd -p -l 3 pixel.raw)"
done
rm -f pixel.raw

echo "== shell housekeeping: tracks · pool · purge =="
$N --project edit.json track add audio
$N --project edit.json add green.mp4 --track 2 --in 0 --out 2
$N --project edit.json track mute 2
$N --project edit.json track ls
$N --project edit.json purge-stumps
$N --project edit.json show --json | head -3

rm -f pool.json pool.history.json
$N --project pool.json init --force >/dev/null
$N --project pool.json import red.mp4 blue.mp4 >/dev/null
$N --project pool.json add red.mp4 --in 0 --out 1 >/dev/null
$N --project pool.json media rm 2                                # blue is unused
$N --project pool.json media ls
if output_has "used by" $N --project pool.json media rm 1; then
  echo "  ✓ media rm refuses an asset in use"
else
  echo "  ✗ media rm removed an in-use asset"; exit 1
fi
if output_has "asset(s)" $N --project pool.json media ls; then
  echo "  ✓ pool down to the one asset still in use"
else
  echo "  ✗ unexpected pool listing"; exit 1
fi

echo "== live hand-off playground =="
# A small project for the shell↔TUI experiment: open the TUI on it in one
# terminal, run nle commands in another. Each write is adopted within a tick
# ("adopted changes from the shell") — no keypress needed.
rm -f live.json live.history.json live.clipboard.json
$N --project live.json init --force >/dev/null
$N --project live.json import red.mp4 blue.mp4 green.mp4 >/dev/null
$N --project live.json add red.mp4  --in 0 --out 3 >/dev/null    # red 0–3
$N --project live.json add blue.mp4 --at 4 --in 0 --out 3 >/dev/null  # gap 3–4, blue 4–7
$N --project live.json seek 1.5 >/dev/null
$N --project live.json show
echo "  → in one terminal:  ./target/release/nle --project demo-sandbox/live.json edit"
echo "    in another:       ./target/release/nle --project demo-sandbox/live.json split --at 5"

echo "== robustness: a closed or unwritable stdout must not panic =="
# Rust ignores SIGPIPE, so a plain println! panics on `nle … | head` — and a
# full disk made every command crash. Both must exit cleanly.
if $N --project live.json media ls > /dev/full 2>/dev/null; then
  echo "  ✓ failed stdout write exits cleanly (no panic)"
else
  echo "  ✗ stdout write failure crashed the CLI"; exit 1
fi
if $N --project live.json media ls | head -1 >/dev/null; then
  echo "  ✓ early-closed pipe (| head) exits cleanly"
else
  echo "  ✗ broken pipe crashed the CLI"; exit 1
fi

echo "== log check (NLE_LOG end-to-end) =="
LOG="$(pwd)/nle-log-check.txt"
rm -f "$LOG" log-check.json
NLE_LOG="$LOG" $N --project log-check.json init --force >/dev/null
if [ -s "$LOG" ] && grep -q "undo history reset" "$LOG"; then
  printf "  ✓ NLE_LOG wrote: %s\n" "$(sed -n '1p' "$LOG")"
else
  echo "  ✗ NLE_LOG produced no log"; exit 1
fi
rm -f "$LOG" log-check.json

echo
echo "Play with it:  cd demo-sandbox && ../target/release/nle show"
