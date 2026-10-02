# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status: M1

The EDL timeline model works end to end: project file, media pool,
timeline edits, and single-track export through the filter-graph compiler
(clips, gaps → black/silence, frame-exact cuts). Next: M2 — the TUI and
mpv monitor.

## Layout

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary. |

## Usage

```sh
cargo build --release
N=./target/release/nle

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
```

Times accept plain seconds (`12.5`) or timecodes (`0:01:23.500`).
The project file (`nle.json`) is plain JSON with exact rational times
(`"30000/1001"`) — human-readable and drift-free.

## Export presets (planned, M6)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival
