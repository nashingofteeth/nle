# nle — plan of record

Status: **M3 complete, M4 next.** This file is the design/roadmap; the
README is the user-facing how-to. Keep it in sync as milestones land.

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
- **No backward compatibility, by design:** format bumps reject old files;
  never migrate — rebuild via `./demo.sh`.
- **Media pool** shared by id: `add_asset` / `asset_usage` / guarded `remove_asset`.
- **Playhead anchoring:** normal edits follow the *media frame*; extend
  commits land on the *edge*; undo restores the *pre-edit* position; delete
  lands at the seam (ripple) or the same offset in the resulting gap (plain).
- **Frames are 0-based** everywhere (`f…`, timecode `:FF`).

## Keymap

Transport/motion: `space` · `h`/`l` ±1s · `H`/`L` ±5s · `f`/`F` frame ·
`j`/`k` cuts · `w`/`b` clips · `gg`/`G`/`0`/`$` ends · `<`/`>` (`,`/`.`)
clip start/end · counts (`3l`).

Editing: `i`/`o` trim (a gap always ripples) · `I`/`O` ripple-trim · at a
segment edge `i`/`o` delete it · `s` split · `_` insert gap (count =
frames) · `dd`/`D` delete/gap · `yy` yank · `p` paste-over · `P`
ripple-insert · `u`/Ctrl-r undo.

Extend: `e`/`E` enter (replace/ripple) · `[`/`]` side · `<`/`>` end ·
commit `e`.

Other: `m` media pool · `r` reload · `:` commands · `q` quit · `v` visual
*(deferred)*.

## Milestones

| # | Scope | Status |
|---|---|---|
| **M0** | Workspace, `probe`/`trim` (cpu/vaapi/qsv/copy), frame-accuracy (PSNR) | ✅ `2d854b3` |
| **M1** | EDL model, project file, media pool, timeline ops, graph compiler v0, export by content | ✅ `e30d1b4` |
| **—** | `demo.sh` playground + clippy cleanup | ✅ `100acdc`, `dac81c4` |
| **M2** | TUI + mpv monitor + vim nav + instant preview + `r` + `:cmd` + `nle edl` | ✅ `344291a` |
| **M3** | Editing for real | ✅ done |
| **M4** | Multi-track + separate audio tracks | ⏳ next |
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

## M4 — multi-track + audio (next)

See `M4` section below / discussion. Placeholder: multiple tracks (video +
audio) in the model, TUI rows, and graph (`overlay`/`amix`/`adelay`).

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
