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
