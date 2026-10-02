# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status: M2

The mpv-like experience works: `nle edit` opens the TUI dashboard + mpv
monitor with vim keys and instant, frame-accurate preview (no rendering —
mpv's EDL protocol plays the timeline directly; gaps play black frames
from a cached file). Next: M3 — editing inside the TUI.

## Layout

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary + TUI + mpv IPC client. |

## Usage

```sh
cargo build --release
N=./target/release/nle        # or just `nle` if cargo-installed

# Interactive editor (the point)
$N edit                       # TUI + mpv monitor; vim keys:
#   space play/pause · h/l ±1s · H/L ±5s · f/F frame step
#   j/k next/prev cut · w/b next/prev clip · gg/G/0 ends
#   3l = 3s (counts) · r reload project from disk · :q quit
# Tip: edit nle.json in another window, press r, watch it update.

# Project workflow
$N init
$N import intro.mp4 b-roll.mov
$N add intro.mp4 --in 2.5 --out 7      # trimmed clip at end of track
$N add b-roll.mov --at 10              # creates a gap before it
$N show                                # or --json for the raw EDL
$N remove 2                            # by clip id from `show`
$N export -o final.mp4                 # libx264, frame-exact
$N export -o fast.mp4 --encoder vaapi  # GPU (also: qsv)

# One-shot tools (no project needed)
$N probe clip.mp4 [--json]
$N trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder vaapi] [--copy]
$N edl                                 # debug: print the preview EDL URL
```

Times accept plain seconds (`12.5`) or timecodes (`0:01:23.500`).
The project file (`nle.json`) is plain JSON with exact rational times
(`"30000/1001"`) — human-readable and drift-free.

## Export presets (planned, M6)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival
