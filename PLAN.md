# nle — plan of record

Status: **M4 complete, EDL (CMX3600) import next.** This file is the
design/roadmap; the README is the user-facing how-to. Keep it in sync as
milestones land.

## Vision

A terminal-native video editor: **TUI dashboard + mpv monitor window, vim
keybinds, FFmpeg underneath.** A general-purpose NLE, built solo, for
**this machine only** (MacBook Pro 16,1 — i9-9980HK 8C/16T, Intel UHD 630
+ AMD RX 5500M, 32 GB RAM, NVMe, CachyOS), tuned for hardware
utilization. Rust, learned along the way with JS/PHP→Rust explainers per
milestone.

**Rejected alternatives:** ffplay as monitor (no IPC), linking libav
(spawn the binary instead), building on MLT, async/tokio (threads
suffice).

## Architecture

```
nle (process)                       mpv window (child)
├── engine lib (EDL, undo, cache)   ├── hw decode (VAAPI)
├── TUI (ratatui) ◄─ JSON IPC ─►    ├── edl:// instant preview
└── spawns ffmpeg (export/preview)  └── observed time-pos → playhead
```

| Piece | Decision |
|---|---|
| Crates | `nle-core` (pure model) · `nle-ffmpeg` (wrappers + graph compiler) · `nle-cli` (binary + TUI + mpv IPC) |
| Project file | `nle.json` — media pool (embedded ffprobe reports) + **segment tracks**. Exact rational times, atomic saves, validated on load; vim is a legitimate UI for it |
| Preview | mpv `edl://` — instant cuts, EDL time = timeline time. Gaps → cached 10 s black file. Effects later → proxy renders (M5) |
| Export | filter-graph compiler: `trim`/`atrim` + normalization + `concat`; gaps → `color`/`anullsrc`; frame-grid snapping; hw tail; `-/filter_complex` script files |
| Encoders | `cpu` (libx264), `vaapi` (UHD 630), `qsv` (Quick Sync) |
| Time | `Rational64` everywhere; floats only at ffmpeg/IPC boundaries |
| Undo | frame-topology history (segment before/after), sidecar v4 |

**Lessons banked in comments:** `-filter_complex_script` removed in ffmpeg
9 → `-/filter_complex`; filter options use `:` not `,`;
`-avoid_negative_ts make_zero`; mpv EDL's 3rd field is **length** not end;
mpv blocks `lavfi://` inside EDL (hence black-file cache).

### Model decisions (settled)
- **Segment model:** a track is a sequence of `Segment = Clip{..} | Gap(dur)`;
  clip start is a prefix sum; overlaps unrepresentable; trailing gap ordinary.
- **No backward compatibility** (detail in AGENTS.md): format bumps reject
  old files; never migrate — rebuild via `./demo.sh`.
- **Media pool** shared by id: `add_asset` / `asset_usage` / guarded `remove_asset`.
- **Playhead anchoring:** normal edits follow the *media frame*; extend
  commits land on the *edge*; undo restores the *pre-edit* position; delete
  lands at the seam (ripple) or the same offset in the resulting gap (plain).
- **Frames are 0-based** everywhere (`f…`, timecode `:FF`).

## Keymap

Canonical keymap lives in **README.md** (the user manual); keep it there
and update it in place. Summary only, for orientation: transport/motion
(`space`, `h/l/H/L`, `f/F`, `j/k`, `w/b`, `gg/G/0/$`, `< >`, counts),
editing (`i/o`, `I/O`, `s`, `_`, `dd/D`, `yy/p/P`, `u`), and extend
(`e/E`, `[ ]`, `< >`).

## Milestones

| # | Scope | Status |
|---|---|---|
| **M0** | Workspace, `probe`/`trim` (cpu/vaapi/qsv/copy), frame-accuracy (PSNR) | ✅ `2d854b3` |
| **M1** | EDL model, project file, media pool, timeline ops, graph compiler v0, export by content | ✅ `e30d1b4` |
| **—** | `demo.sh` playground + clippy cleanup | ✅ `100acdc`, `dac81c4` |
| **M2** | TUI + mpv monitor + vim nav + instant preview + `r` + `:cmd` + `nle edl` | ✅ `344291a` |
| **M3** | Editing for real | ✅ done |
| **M4** | Multi-track + separate audio tracks | ✅ done |
| **M5** | Transitions (`xfade`/`acrossfade`) + effects + proxy cache + thumbnails/waveforms | pending |
| **M6** | Export scheduler (3 lanes), segment-parallel, `nle bench`, presets, resume+cache | pending |
| **M7** | GUI (Tauri+WebCodecs or libmpv) | someday |

### M3 — shipped (beyond the original scope)

- **Model:** segment model; project v2; media pool; ops — trim in/out
  (+ripple), split, delete (gap/ripple, by id/index),
  extend_edge/extend_gap_tail/head, absorb_outward, paste/paste_ripple
  (clip **or** gap), insert_gap, merge-on-delete; frame-topology undo
  sidecar **v4** storing the pre-edit playhead.
- **TUI:** full keymap; **extend mode** (clip & gap head/tail, material-wide
  bounds, `e` replace / `E` ripple, opposite-end delete, leading-gap
  support, strip **overlay**); **media pool** taking over the info pane
  (asset list + probe metadata + monitor preview with full seek/frame/end
  nav, insert/remove/undo); bottom **keys** cheat-sheet + **message**
  panels; playhead anchoring; 0-based frames.
- **Verified:** ~140 tests, clippy clean, `./demo.sh` pixel checks, release
  installed.

## M4 — multi-track + audio (shipped)

- **Model:** tracks are an ordered list of `Track { kind, muted, segments }`;
  helpers `track(ti)` / `video_tracks` / `audio_tracks` / `add_track`;
  validate requires ≥1 video track. Project **v3**.
- **Ops:** every track-resolving edit op is `*_on(project, track, …)`; the
  unsuffixed name wraps the first video track. A stale index returns
  `TrackNotFound` (never panics).
- **Export:** one video track with no audio track keeps the classic concat
  path **byte-for-byte**; otherwise each track is padded to the timeline
  length and video layers composite bottom→top with `overlay` (upper gaps
  transparent `black@0.0`/`yuva420p`), audio sums with
  `amix=inputs=N:normalize=0`. Muted tracks are dropped.
- **TUI:** one row per track with a `v#`/`a#` gutter, active row
  highlighted; `t`/`T` cycle and `<N>t` jumps (digits stay vim counts);
  navigation, edits, and the monitor (active-track EDL) all follow the
  active track; `:track add video|audio · mute [N] · rm N`.
- **CLI:** `nle track ls|add|mute|rm`, `nle add --track N`,
  `nle edl --track N`.
- **Verified:** 177 tests, clippy clean, `./demo.sh` multi-track pixel +
  audio checks.

**Deferred:** composite preview (M5); linked a/v detach; a muted sole video
track is a no-op in the fast path.

## EDL (CMX3600) import — next

Import an edit-decision-list exported from another NLE (Premiere/Resolve/
Avid) and rebuild the timeline. Cuts only; dissolves/wipes are rejected
(transitions are M5). A pure parser (`nle-core/src/edl.rs`) reads events and
`*` comment hints; a resolve step matches reels/clip names against the media
pool (`--media DIR`, `--map REEL=PATH`, error on any unresolved reel); a
build step emits clips and record gaps. Source timecode is file-relative by
default with `--src-offset HH:MM:SS:FF`. `A`/`A2` audio events become real
audio-track clips (M4 makes that possible); linked audio is skipped to avoid
doubling each clip's embedded audio. Command:
`nle import-edl <file.edl> --media DIR [--fps] [--src-offset] [--map]`.

## M5 — transitions, effects, proxies (planned)

**Goal:** richer edits beyond cuts — transitions between clips, per-clip
effect filters, and a **proxy render cache** so the instant preview can show
things mpv `edl://` can't (effects, composited multi-track, transitions);
plus optional **thumbnails / waveforms** in the TUI.

Scope:
- **Transitions:** `xfade` (video) + `acrossfade` (audio) between adjacent
  clips, with overlap semantics in the segment model (a transition consumes
  time from both sides). Small set first (cross-dissolve, fade, wipe).
- **Effects:** per-clip filter chains (color/scale/crop/speed, LUT, maybe
  `eq`/`curves`) stored on the clip; applied in export and, where possible,
  previewed. Speed change interacts with audio (`atempo`) and frame grid.
- **Proxy cache:** for segments the EDL can't express (effects, >1 video
  track, transitions), render a **proxy clip** once, cache by graph hash,
  and feed the preview an EDL of proxies. Reuses the M6 hashing idea.
- **TUI:** thumbnails on the timeline row and/or a waveform strip for audio
  tracks; a bin/`fx` UI for the clip's effect list.

**Depends on:** M4 (tracks/compositing must exist before composited
previews make sense).

**Decisions to settle when it starts:** transition model in the segment
sequence (does a transition replace a gap, or sit between clips?); proxy
format (all-intra, project geometry, `-crf` preset); effect storage shape
and whether it's undoable (it should be — extend `Change`).

**Risks:** transition timing/overlap math is exact-rational-hostile (frame
snapping); proxy invalidation correctness (hash must cover everything the
graph reads); speed + audio sync.

## M6 — export presets (planned)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival

## Open / deferred (post-M3)

- Pool **import** (path prompt) — not built.
- Pool **keyframe `j`/`k`** — discussed, not built.
- **Purge sub-frame stumps** cleanup — offered, not built.
- `v` visual/select mode — deferred.
- Runtime device detection (hardcoded `renderD128`) — folds into M6.

## Open decisions (non-blocking)

1. M6 encode-tier presets (`preview`/`balanced`/`max`).
2. `v` visual/select semantics.
3. M7 GUI technology.
4. Runtime device detection (→ M6 machine config).
