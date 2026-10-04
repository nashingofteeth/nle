# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status: M3

Editing for real: the TUI drives the model directly — trim in/out, split,
delete (gap or ripple), yank/paste, undo/redo — with auto-save and instant
preview rebuilds on every edit. Next: M4 — multi-track + audio.

## Compatibility: none, by design

**nle is a private application for one machine and one user. It is not
distributed, so there is no backward compatibility to preserve.**
Project files (`nle.json`), history sidecars (`nle.history.json`), and
cached artifacts may change shape at any time: old files are rejected
with a clear error (never silently misread), and the fix is always
"rebuild it" (`./demo.sh` regenerates the demo project). Don't write
migration code; bump the format version and move on.

## Layout

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops + undo history. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary + TUI + mpv IPC client. |

## Usage

```sh
cargo build --release
N=./target/release/nle        # or just `nle` if cargo-installed

# Interactive editor (the point)
$N edit                       # TUI + mpv monitor; vim keys:
#   transport: space play/pause · h/l ±1s · H/L ±5s · f/F frame step
#   motion:    j/k next/prev cut · w/b next/prev clip · gg/G/0 ends
#              < / > (, / .) clip start/end · 3l = 3s (counts)
#   editing:   i/o trim in/out to playhead · s split at playhead
#              dd delete (gap) · D delete + ripple · yy yank
#              p paste-over (replace what's underneath) · P ripple-insert
#              u / Ctrl-r undo/redo (edits save automatically)
#   extend:    e/E enter extend mode at a boundary (E = ripple) ·
#              [ / ] enter on the right / left clip of a cut
#              (bracket opens toward its side) ·
#              navigate to the target (mpv shows the armed material) ·
#              e commit (replaces what it crosses) · E flip ripple ·
#              [ ] switch side in-mode · < > (, .) switch end in-mode ·
#              gaps arm at their tail and extend without bound · Esc cancel
#   misc:      r reload project from disk · :q quit · q quit
# Tip: everything is undoable; the project file is written after every edit.

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

## Dependency policy

Low-dependency stance: **the current set is frozen.** No new runtime
dependencies without explicit justification (a hand-rolled ~50 lines beats
a transitive tree for most needs).

- Direct deps (10): clap, anyhow, thiserror, serde, serde_json,
  num-rational, tempfile, ratatui, crossterm (+3 internal crates).
  Zero network code in the tree.
- `Cargo.lock` is committed — builds are reproducible; upgrades are
  deliberate events, not side effects.
- After any dependency change: `cargo update` deliberately, then run
  `cargo audit` (RustSec advisories) before committing.
- External system binaries (`ffmpeg`, `ffprobe`, `mpv`) are the real
  attack surface, not crates: they're invoked without a shell, option
  injection is blocked with `--` before every positional path, and mpv
  runs with an isolated config and its own IPC socket.

## Export presets (planned, M6)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival
