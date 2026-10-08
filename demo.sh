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
if ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 demo-multi.mp4 | grep -q audio; then
  echo "  ✓ multi-track export has an audio stream"
else
  echo "  ✗ multi-track export has no audio"; exit 1
fi

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

echo "== detect-edits: reverse-engineer a rendered file =="
# A known 4-shot file: red 0–1, blue 1–2, black 2–3, green 3–4. The black
# span exercises gap recovery (scene detection alone misses cuts to black).
if [ ! -f detect-src.mp4 ]; then
  ffmpeg -v error -y \
    -f lavfi -i "color=c=red:s=320x240:r=30:d=1" \
    -f lavfi -i "color=c=blue:s=320x240:r=30:d=1" \
    -f lavfi -i "color=c=black:s=320x240:r=30:d=1" \
    -f lavfi -i "color=c=green:s=320x240:r=30:d=1" \
    -filter_complex "[0:v][1:v][2:v][3:v]concat=n=4:v=1:a=0[v]" \
    -map "[v]" -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p detect-src.mp4
fi
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
if [ "$detect_gaps" -eq 1 ]; then
  echo "  ✓ recovered the black span as one gap segment"
else
  echo "  ✗ expected 1 gap, got $detect_gaps"; exit 1
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
echo "Play with it:  cd demo-sandbox && ../../target/release/nle show"
