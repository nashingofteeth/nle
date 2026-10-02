# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status: M0

`probe` and `trim` work — the FFmpeg plumbing layer. The actual editor
(EDL timeline model, TUI, mpv monitor) arrives in M1–M3.

## Layout

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, later the EDL model. No I/O. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, later the filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary. |

## Usage

```sh
cargo build --release

# Inspect a file
./target/release/nle probe clip.mp4
./target/release/nle probe clip.mp4 --json

# Cut a segment (re-encodes by default → frame-accurate)
./target/release/nle trim clip.mp4 --in 2.5 --out 7 -o cut.mp4

# Hardware encoders (Intel UHD 630):
./target/release/nle trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 --encoder vaapi
./target/release/nle trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 --encoder qsv

# Stream copy: instant + lossless; exact start, end within a frame of --out
./target/release/nle trim clip.mp4 --in 2 --out 6 -o cut.mp4 --copy
```

Times accept plain seconds (`12.5`) or timecodes (`0:01:23.500`).

## Export presets (planned, M6)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival
