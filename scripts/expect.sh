#!/usr/bin/env bash
# Expected behaviours, asserted: normal workflows checked against the
# result (export pixels, durations, structure). Every ✗ is a bug — the
# command ran, but the timeline or the render is not what the model says
# it should be. Run from anywhere; it sandboxes itself.
#
# This is the behavioural contract the unit tests can't see: real ffmpeg,
# real pixels, real durations.
set -uo pipefail
cd "$(dirname "$0")/.."

N=$(readlink -f ./target/release/nle)
DIR=expect-sandbox
mkdir -p "$DIR"
cd "$DIR"

FAIL=0
check_px() { # check_px FILE TIME HEX LABEL
  ffmpeg -v error -y -ss "$2" -i "$1" -frames:v 1 -f rawvideo -pix_fmt rgb24 px.raw
  local got; got=$(xxd -p -l 3 px.raw); rm -f px.raw
  if [ "$got" = "$3" ]; then echo "  ✓ $4"; else echo "  ✗ $4 — got $got want $3"; FAIL=$((FAIL+1)); fi
}
check_dur() { # check_dur FILE SECONDS LABEL
  local got; got=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$1")
  local ok; ok=$(python3 -c "print(abs(float('$got')-float('$2'))<0.06)")
  if [ "$ok" = "True" ]; then echo "  ✓ $3 ($got s)"; else echo "  ✗ $3 — got ${got}s want ${2}s"; FAIL=$((FAIL+1)); fi
}
check_has() { if printf '%s' "$2" | grep -qF -- "$1"; then echo "  ✓ $3"; else echo "  ✗ $3 — no '$1' in: $(printf '%s' "$2" | head -3 | tr '\n' '|')"; FAIL=$((FAIL+1)); fi; }
check_no() { if printf '%s' "$2" | grep -qF -- "$1"; then echo "  ✗ $3 — found '$1'"; FAIL=$((FAIL+1)); else echo "  ✓ $3"; fi; }

gen() { # gen NAME COLOR SIZE RATE DUR FREQ
  local name=$1 color=$2 size=$3 rate=$4 dur=$5 freq=$6
  [ -f "$name" ] && return 0
  echo "== generating $name ($color, $size @ $rate, ${dur}s) =="
  ffmpeg -v error -y \
    -f lavfi -i "color=c=$color:s=$size:d=$dur:r=$rate" \
    -f lavfi -i "sine=frequency=$freq:duration=$dur" \
    -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p \
    -c:a aac -b:a 128k "$name"
}
gen red.mp4   red    320x240   30           5 440
gen green.mp4 green  320x240   30           5 660
gen blue.mp4  blue   320x240   60000/1001   5 880

RED=fd0000; GREEN=007f00; BLUE=0000fe; BLACK=000000

# base: red 0–2, green 2–4, blue 4–6 (one clip each, in order)
base() {
  rm -f b.json b.history.json b.clipboard.json
  $N --project b.json init --force >/dev/null
  $N --project b.json import red.mp4 green.mp4 blue.mp4 >/dev/null
  $N --project b.json add red.mp4   --in 0 --out 2 >/dev/null
  $N --project b.json add green.mp4 --at 2 --in 0 --out 2 >/dev/null
  $N --project b.json add blue.mp4  --at 4 --in 0 --out 2 >/dev/null
}
export_() { $N --project "$1" export -o "$2" >/dev/null 2>&1; }

echo "== baseline export =="
base
export_ b.json b.mp4
check_px b.mp4 1 "$RED"   "red 0–2"
check_px b.mp4 3 "$GREEN" "green 2–4"
check_px b.mp4 5 "$BLUE"  "blue 4–6"
check_dur b.mp4 6 "6 s"

echo "== split + ripple delete =="
base
$N --project b.json split --at 1 >/dev/null
$N --project b.json delete --clip 1 --ripple >/dev/null
export_ b.json b.mp4
check_px b.mp4 0.5 "$RED"   "the red half survives"
check_px b.mp4 2 "$GREEN"   "green slid left"
check_dur b.mp4 5 "5 s"

echo "== trim-in ripples, trim-out leaves a gap =="
base
$N --project b.json trim-in --at 1 --ripple >/dev/null
export_ b.json b.mp4
check_px b.mp4 0.5 "$RED" "red head pulled in"
check_dur b.mp4 5 "5 s"
base
$N --project b.json trim-out --at 1 >/dev/null
export_ b.json b.mp4
check_px b.mp4 0.5 "$RED"   "red 0–1"
check_px b.mp4 1.5 "$BLACK" "the trimmed material is a gap"
check_dur b.mp4 6 "length preserved"

echo "== gaps: insert, merge, delete =="
base
$N --project b.json gap --at 2 --frames 30 >/dev/null
export_ b.json b.mp4
check_px b.mp4 2.5 "$BLACK" "the inserted gap"
check_px b.mp4 4 "$GREEN"   "green pushed to 3–5"
check_dur b.mp4 7 "7 s"
base
$N --project b.json gap --at 2 --frames 30 >/dev/null
$N --project b.json gap --at 3 --frames 30 >/dev/null
out=$($N --project b.json show)
n=$(printf '%s' "$out" | grep -c "gap")
if [ "$n" -eq 1 ]; then echo "  ✓ adjacent gaps coalesced"; else echo "  ✗ $n gap rows after two inserts"; FAIL=$((FAIL+1)); fi
base
$N --project b.json gap --at 2 --frames 30 >/dev/null
$N --project b.json delete --at 2.5 >/dev/null
export_ b.json b.mp4
check_px b.mp4 2.5 "$GREEN" "deleting the gap pulled green back"
check_dur b.mp4 6 "6 s"

echo "== delete keeps the length; the tail is black =="
base
$N --project b.json delete --clip 2 >/dev/null
export_ b.json b.mp4
check_px b.mp4 3 "$BLACK" "green's span is a gap"
check_dur b.mp4 6 "length preserved"

echo "== paste over vs ripple =="
base
$N --project b.json yank --clip 1 >/dev/null
$N --project b.json paste --at 1 >/dev/null
out=$($N --project b.json show)
check_has "1 – 3" "$out" "the paste covers exactly 1–3"
check_has "(src 1–2)" "$out" "the covered green was trimmed to its tail"
export_ b.json b.mp4
check_px b.mp4 2 "$RED" "the pasted red"
check_dur b.mp4 6 "length unchanged"
base
$N --project b.json yank --clip 1 >/dev/null
$N --project b.json paste --at 0 --ripple >/dev/null
export_ b.json b.mp4
check_px b.mp4 1 "$RED"   "the ripple paste sits first"
check_px b.mp4 3 "$RED"   "the original red follows"
check_dur b.mp4 8 "8 s"

echo "== speed: source ↔ timeline math =="
rm -f s.json s.history.json
$N --project s.json init --force >/dev/null
$N --project s.json import red.mp4 >/dev/null
$N --project s.json add red.mp4 --in 0 --out 4 >/dev/null
$N --project s.json speed 1 2 >/dev/null
$N --project s.json trim-in --at 1 >/dev/null
out=$($N --project s.json show)
check_has "(src 2–4)" "$out" "1 timeline s at 2× consumes 2 source s"
$N --project s.json extend --clip 1 --edge out --to 2.5 >/dev/null
out=$($N --project s.json show)
check_has "(src 2–5)" "$out" "0.5 timeline s at 2× adds 1 source s"
export_ s.json s.mp4
check_dur s.mp4 2.5 "2.5 s"
check_has "75 frames" "$($N --project s.json export -o s2.mp4 2>&1 | tail -1)" "30 fps × 2.5 s"

echo "== audio retiming keeps the audio =="
for spec in "2:1" "0.5:4" "0.4:5" "3:0.666667"; do
  rate=${spec%%:*}; want=${spec##*:}
  rm -f r.json r.history.json
  $N --project r.json init --force >/dev/null
  $N --project r.json import red.mp4 >/dev/null
  $N --project r.json add red.mp4 --in 0 --out 2 >/dev/null
  $N --project r.json speed 1 "$rate" >/dev/null
  export_ r.json r.mp4
  check_dur r.mp4 "$want" "a 2 s clip at ${rate}×"
  if ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 r.mp4 | grep -q audio; then
    echo "  ✓ ${rate}× keeps its audio"
  else echo "  ✗ ${rate}× lost its audio"; FAIL=$((FAIL+1)); fi
done

echo "== 59.94 fps source follows the timeline rate =="
rm -f f.json f.history.json
$N --project f.json init --force >/dev/null
$N --project f.json import blue.mp4 >/dev/null
$N --project f.json add blue.mp4 --in 0 --out 2 >/dev/null
check_has "120 frames @ 60000/1001" "$($N --project f.json export -o f.mp4 2>&1 | tail -1)" "2 s of 59.94 = 120 frames"

echo "== multi-track: overlay, mute, base promotion =="
rm -f m.json m.history.json
$N --project m.json init --force >/dev/null
$N --project m.json import red.mp4 green.mp4 >/dev/null
$N --project m.json add red.mp4 --in 0 --out 4 >/dev/null
$N --project m.json track add video >/dev/null
$N --project m.json add green.mp4 --track 2 --at 1 --in 0 --out 2 >/dev/null
export_ m.json m.mp4
check_px m.mp4 2 "$GREEN" "the overlay is on top"
check_px m.mp4 0.5 "$RED" "the base shows through the overlay's gap"
$N --project m.json track mute 2 >/dev/null
export_ m.json m2.mp4
check_px m2.mp4 2 "$RED" "a muted overlay reveals the base"
$N --project m.json track mute 2 >/dev/null   # unmute the overlay again
$N --project m.json track mute 1 >/dev/null
export_ m.json m3.mp4
check_px m3.mp4 2 "$GREEN" "muting the base promotes the overlay"
check_px m3.mp4 0.5 "$BLACK" "…and it is black where the overlay is a gap"

echo "== a muted sole video track has nothing to export =="
rm -f mm.json mm.history.json
$N --project mm.json init --force >/dev/null
$N --project mm.json import red.mp4 >/dev/null
$N --project mm.json add red.mp4 --in 0 --out 2 >/dev/null
$N --project mm.json track mute 1 >/dev/null
out=$($N --project mm.json export -o mm.mp4 2>&1)
check_has "every video track is muted" "$out" "the export refuses instead of ignoring the mute"

echo "== an empty upper video track is transparent =="
rm -f e.json e.history.json
$N --project e.json init --force >/dev/null
$N --project e.json import red.mp4 >/dev/null
$N --project e.json add red.mp4 --in 0 --out 2 >/dev/null
$N --project e.json track add video >/dev/null
export_ e.json e.mp4
check_px e.mp4 1 "$RED" "the base still shows"

echo "== audio track mixes in =="
rm -f a.json a.history.json
$N --project a.json init --force >/dev/null
$N --project a.json import red.mp4 green.mp4 >/dev/null
$N --project a.json add red.mp4 --in 0 --out 2 >/dev/null
$N --project a.json track add audio >/dev/null
$N --project a.json add green.mp4 --track 2 --in 0 --out 1 >/dev/null
export_ a.json a.mp4
check_dur a.mp4 2 "2 s"
if ffprobe -v error -select_streams a -show_entries stream=codec_type -of csv=p=0 a.mp4 | grep -q audio; then
  echo "  ✓ audio stream present"
else echo "  ✗ no audio stream"; FAIL=$((FAIL+1)); fi

echo "== undo/redo round trip (byte-identical) =="
base
cp b.json b.before.json
$N --project b.json split --at 1 >/dev/null
$N --project b.json trim-out --at 5 >/dev/null
$N --project b.json gap --at 3 --frames 15 >/dev/null
$N --project b.json delete --clip 3 --ripple >/dev/null
$N --project b.json speed 1 2 >/dev/null
cp b.json b.after.json
for _ in 1 2 3 4 5; do $N --project b.json undo >/dev/null; done
if diff -q b.json b.before.json >/dev/null; then echo "  ✓ 5 undos restore the start exactly"; else echo "  ✗ undo did not restore"; FAIL=$((FAIL+1)); fi
for _ in 1 2 3 4 5; do $N --project b.json redo >/dev/null; done
if diff -q b.json b.after.json >/dev/null; then echo "  ✓ 5 redos restore the end exactly"; else echo "  ✗ redo did not restore"; FAIL=$((FAIL+1)); fi

echo "== undoable: track rm, media rm, imports =="
base
$N --project b.json track add video >/dev/null
$N --project b.json add blue.mp4 --track 2 --at 0 --in 0 --out 1 >/dev/null
$N --project b.json track rm 2 >/dev/null
$N --project b.json undo >/dev/null
export_ b.json b.mp4
check_px b.mp4 0.5 "$BLUE" "the removed track's clip came back"
# a pool with an unused asset: red+green placed, blue only pooled
rm -f b.json b.history.json b.clipboard.json
$N --project b.json init --force >/dev/null
$N --project b.json import red.mp4 green.mp4 blue.mp4 >/dev/null
$N --project b.json add red.mp4   --in 0 --out 2 >/dev/null
$N --project b.json add green.mp4 --at 2 --in 0 --out 2 >/dev/null
$N --project b.json media rm 3 >/dev/null
check_no "#3" "$($N --project b.json media ls)" "blue left the pool"
$N --project b.json undo >/dev/null
check_has "#3" "$($N --project b.json media ls)" "undo brought it back"
$N --project b.json import red.mp4 >/dev/null
check_has "3 asset(s)" "$($N --project b.json media ls)" "re-importing dedupes"

echo "== head material: a clip with --in > 0 reaches back past time 0 =="
rm -f h.json h.history.json
$N --project h.json init --force >/dev/null
$N --project h.json import red.mp4 >/dev/null
$N --project h.json add red.mp4 --in 1 --out 3 >/dev/null
$N --project h.json extend --clip 1 --edge in --to -1 >/dev/null
export_ h.json h.mp4
check_px h.mp4 0.5 "$RED" "the prepended source plays from 0"
check_dur h.mp4 3 "3 s"

echo "== EDL import round trip =="
cat > e.edl <<'EDL'
TITLE: e
FCM: NON-DROP FRAME

001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00
* FROM CLIP NAME: red.mp4
002  AX  V  C  00:00:00:00 00:00:01:00 01:00:02:00 01:00:03:00
* FROM CLIP NAME: blue.mp4
EDL
rm -f edl.json edl.history.json
$N --project edl.json import-edl e.edl --media . --force >/dev/null
export_ edl.json edl.mp4
check_px edl.mp4 0.5 "$RED"   "EDL event 1"
check_px edl.mp4 1.5 "$BLACK" "the record gap"
check_px edl.mp4 2.5 "$BLUE"  "EDL event 2"

echo "== OTIO import round trip =="
cat > e.otio <<'OTIO'
{
  "OTIO_SCHEMA": "Timeline.1",
  "tracks": {
    "OTIO_SCHEMA": "Stack.1",
    "children": [
      {
        "OTIO_SCHEMA": "Track.1", "kind": "Video",
        "children": [
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "red.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } },
          { "OTIO_SCHEMA": "Gap.1",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 30, "rate": 30 } } },
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "green.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } }
        ]
      }
    ]
  }
}
OTIO
rm -f otio.json otio.history.json
$N --project otio.json import-otio e.otio --force >/dev/null
export_ otio.json otio.mp4
check_px otio.mp4 1 "$RED"    "OTIO clip 1"
check_px otio.mp4 2.5 "$BLACK" "the OTIO gap"
check_px otio.mp4 4 "$GREEN"  "OTIO clip 2"
check_dur otio.mp4 5 "5 s"

echo "== CLI-only guards =="
out=$($N trim red.mp4 --in 0 --out 99 -o t.mp4 2>&1)
check_has "beyond the file's duration" "$out" "trim refuses an --out past the end"
out=$($N --project b.json export -o . 2>&1)
check_has "Error opening output" "$out" "a failed export names the reason"
out=$($N --project b.json media ls | head -1)
check_has "asset(s)" "$out" "piped output survives an early-closed consumer"

echo
if [ "$FAIL" -eq 0 ]; then echo "expect: ALL EXPECTATIONS HELD"; else echo "expect: $FAIL EXPECTATION(S) FAILED"; exit 1; fi
