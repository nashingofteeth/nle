# nle — plan of record

Status: **Everything through M5 is shipped** — multi-track (M4), EDL/OTIO
import, clip speed, edit-detect, CLI/TUI action parity, and M5 (transitions,
effects, proxy cache). **M6 next.** This file is the design/roadmap; the
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
(`space`, `h/l/H/L`, `f/F`, `j/k`, `w/b`, `n/N`, `gg/G/0/$`, `< >`, counts),
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
| **M5** | Transitions (`xfade`/`acrossfade`) + per-clip effects + proxy cache | ✅ done |
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
- **Verified:** 183 tests, clippy clean, `./demo.sh` multi-track pixel +
  audio checks.

**Deferred:** composite preview (M5); linked a/v detach; a muted sole video
track is a no-op in the fast path.

## EDL (CMX3600) import — shipped

- **Timecode** (`time.rs`): `Timecode` + `parse_timecode` (colon or
  drop-frame `;`), `timecode_to_frames` (drop-frame skips 2/4 frame numbers
  per minute except every tenth), `frames_to_seconds`.
- **Parser** (`nle-core/src/edl.rs`): `TITLE:`/`FCM:`, event lines (reel,
  channel, transition, source/record timecodes), and `* FROM CLIP NAME:` /
  `* SOURCE FILE:` comments. Cuts only — dissolves/wipes error by event.
- **Build** (`edl.rs::build`): video events → video track 0 (record gaps →
  `Gap`; origin = the earliest event); one audio track per distinct channel;
  audio events whose reel + source match a video event are skipped as
  **linked** (the clip already carries that audio); source TC → file time
  via `--src-offset`.
- **CLI:** `nle import-edl FILE --media DIR [--fps] [--src-offset]
  [--map REEL=PATH]` — probes the directory, resolves reels/clip names
  (exact file/stem, then a unique substring), infers fps/size from the first
  event's asset, and refuses to clobber an existing project without
  `--force`.
- **Verified:** unit fixtures (cuts, drop-frame, gaps, linked audio) +
  `./demo.sh` (import → export → red/gap/blue pixels).

**Deferred:** audio-only EDLs; additive import into an
existing project. (TUI `:edl-import` landed with the action-parity work
below.)

## OTIO import + clip speed — shipped

- **OTIO import** (`nle-core/src/otio.rs`): reads a `Timeline`/`Stack`/
  `Track`/`Clip`/`Gap` document from `serde_json::Value`, dispatching on the
  schema *name* (the part before the `.`) — tags are versioned (`Clip.1`,
  `Clip.2`), so a tagged enum would break on a version bump. `nle import-otio
  FILE [--media DIR] [--fps] [--remap OLD=NEW]` resolves each clip's
  `ExternalReference.target_url`, probes it into the pool, and builds a
  multi-track timeline. Transitions/nesting are **warned**, never silently
  dropped.
- **Rate conforms:** Kdenlive emits **no `LinearTimeWarp`** — it writes the
  clip's `source_range` in *project* frames and the media's `available_range`
  in the *media* rate, so `speed = source_rate / media_rate` and source
  seconds = `frame / media_rate`. Validated on a real 26-min project (27
  clips: 26× 0.8008 + 1× 0.4004, total exactly 37538/24 s).
- **Clip speed** (`timeline.rs`): `Clip.speed` (rational, default 1); timeline
  duration = `(asset_out - asset_in) / speed`. `PROJECT_VERSION` → 4,
  `HISTORY_VERSION` → 7 (the `Atom` carries **exact** source times + speed, so
  undo/redo is bit-for-bit; an earlier frame-count form quantized source
  seconds and slowly drifted speed clips).
- **Editing:** timeline↔source deltas scale by speed (`source_span`, or
  `Clip::source_at`/`timeline_offset_of`); a speed change **always ripples**
  (`set_speed` / `:speed` / `nle speed`, and the `R` key) and lands the new
  duration on the frame grid, storing the rate that fits (`source / duration`)
  with the source span untouched.
- **Repair:** `nle purge-stumps [--track N]` / `:purge` drops sub-frame stumps
  and snaps a drifted timeline back onto the frame grid (cumulative snap, so
  cuts stay within half a frame).
- **Export:** `setpts=(PTS-STARTPTS)/speed`, `atempo` (chained outside its
  range, e.g. 0.4× → 0.5 × 0.8008), source-span trim; the 1× graph stays
  byte-identical. Preview plays a speed clip from a rendered proxy (mpv's
  EDL has no rate).

**Deferred:** OTIO transitions/nesting; audio-only OTIO.

## Edit detect (`detect-edits`) — shipped

- **Detect** (`nle-ffmpeg/src/detect.rs`): one ffmpeg pass —
  `blackdetect` + `select='gt(scene,T)',showinfo` — parsed from stderr.
  `detect` (process) is split from `build_args`/`parse_log` (pure) so the
  parsing is unit-tested without spawning ffmpeg.
- **Reconstruct** (`nle-core/src/detect.rs`): raw cuts/blacks → exact,
  frame-snapped `Span`s (media/gap); adjacent gaps coalesce; sub-`min_shot`
  media folds into a neighbour. Pure, no ids.
- **CLI:** `nle detect-edits FILE [--list --threshold --min-shot
  --black-min --no-gaps --fps --video-only --force]` — a video track plus a
  matching split audio track, written with `save_and_reset_history`.
  `--append [--track N]` adds the shots to an existing project at the
  timeline end instead (audio mirrored onto the first audio track).
- **Verified:** pure unit tests + `./demo.sh` (red/blue/black/green file →
  detect → export → pixel checks; the black span returns as one gap). Also
  run on a 1.4 GB 1080p60 dailies `.mov`: 19 cuts + 2 gaps, including 3 s
  of leading black and a 0.2 s black flash that scene detection missed.

**Deferred:** matching shots back to pooled source
assets (frame fingerprinting) — a rendered file can't recover its sources,
only the cut structure. (TUI `:detect` landed with the action-parity work
below.)

## CLI/TUI action parity — shipped

- **The action catalog** (`nle-core/src/action.rs`): one `Action` enum with
  explicit addressing (`--at` position, `--clip` id, `--seg` index — no
  hidden playhead) and one `run_action` executor over `Project` + `History`
  + the yank register. Three front-ends build the same values: TUI keybinds
  (high-frequency actions only), the TUI `:cmd` line (**every** action), and
  CLI subcommands (every action).
- **One grammar:** `Action::parse` is the `:cmd` syntax and mirrors the CLI
  flags (`:delete --at 3 --ripple` ≡ `nle delete --at 3 --ripple`), with
  `--at`/`--track`/`--clip` defaulting to the playhead / active track /
  clip-under in the TUI. `to_action` is the single CLI `Cmd`→`Action`
  mapping. Coverage tests walk `Action::slot`/`SLOTS` and fail if either
  spelling misses a variant — parity is enforced, not promised.
- **Shared session state:** the undo sidecar persists **both** stacks
  (`HISTORY_VERSION` 9), so `nle undo`/`nle redo` work across processes and
  on TUI-made edits; the yank register (`nle.clipboard.json`) is shared —
  `nle yank` ↔ `yy`, `nle paste` ↔ `p`.
- **New CLI surface:** `nle trim-in|trim-out|split|gap|delete|yank|paste|
  extend|undo|redo`, `remove --ripple`, `--at`/`--clip`/`--seg` addressing,
  and `nle show` segment indices.
- **IO commands in both:** `:export`/`:detect`/`:edl-import`/`:otio-import`/
  `:add` run in a worker thread (mpsc polled by the event loop) so the
  monitor keeps playing; the printing `cmd_*` functions became `*_work`
  functions returning a `CommandReport` (warnings + summary).
- **Extend routing moved to `nle-core::extend`** (`resolve_armed`,
  `armed_edge`, `plan_commit`, `commit_batch`): the shell's `nle extend
  --edge in|out --to T` commits under the same rules as the TUI's `e`/`E`.
- **Verified:** 284 tests, clippy clean, `./demo.sh` agent chain (split →
  `delete --clip` → `undo` → `gap` → `yank` → `paste` → `export`) with pixel
  checks and a register check.

- **The stored playhead** (`PROJECT_VERSION` 5): `nle seek T` / `:seek T` move
  a shared cursor — snapped to the frame grid, clamped into the timeline; the
  TUI opens there, re-saves its position after every edit, and flushes it
  ~1 s after the transport stops (plus on quit); a reload adopts the file's
  value. `nle show` reports `at N s`. Session state, not a catalog action (no
  history entry; `--at` stays explicit).
- **Shared-session concurrency:** the TUI adopts shell writes within a tick
  (project, history, register, playhead) and never writes over an unseen
  change; reloads wait for Normal mode.
- **Undo covers track and pool edits:** `Change::Tracks` makes add/mute/rm and
  `purge-stumps` undoable, `History::add_asset` makes imports (and an `add`
  that auto-imports) undoable (`HISTORY_VERSION` 10); only whole-project
  rebuilds (`init`, EDL/OTIO/detect) re-baseline.
- **Hardening pass:** an edge-case sweep and then an expected-behaviour sweep
  (hostile input first, then normal workflows asserted on *results*) found 18
  bugs — the CLI's SIGPIPE panic on `| head`, a register that pasted the wrong
  media, a timeline that wrapped negative on absurd input, sub-frame clips
  from `speed`, kind-mismatched exports, a byte-unstable project file, and a
  handful of misleading errors. Both sweeps are in the repo
  (`scripts/expect.sh`, `scripts/fuzz.sh`) and run in `scripts/check.sh`.

**Deferred:** `nle paste --from-clip` (the register covers it); TUI `:probe`.

## M5 — transitions, effects, proxy cache (shipped)

- **Transitions:** a real `Segment::Transition { id, dur, kind, a, b }` that
  **owns** both source sides (`TransitionSource::Clip | Black`), so it never
  dangles when neighbours change and positions stay a prefix sum. Each clip
  side gives up `dur` — clip↔clip shortens the timeline by `dur` (matches
  `xfade`: 10+10−1 = 19), clip↔black leaves it. Export blends with
  `xfade`/`acrossfade`; creation via `x` / `:transition`. Cuts only:
  `Dissolve` + `FadeBlack`. `dd`/`D` remove one and hand the frames it
  trimmed back to its neighbours (only when they still line up); `i`/`o`/`s`/
  `_` refuse.
- **Effects:** a typed `Effect` enum (`Crop`/`Scale`/`Eq`/`Lut`) on
  `Clip.effects`, applied in the export chain after the retime and before
  the geometry normalize (so layers stay uniform for `overlay`); `:effect`
  creates them. Undoable (`Atom::Clip`). `merge_pair` only joins clips with
  matching effects; a paste carries them over.
- **Proxy cache:** segments `edl://` can't express (effects, transitions,
  retimes)
  are rendered once into `~/.cache/nle/proxy/<hash>.mp4` and the monitor
  plays the proxy; the hash covers the segment + geometry + frame rate +
  each asset's path **and mtime**. The render puts the segment on its own
  track and reuses the export graph compiler, so there's no second
  filter-graph to drift.
- **Verified:** unit tests + real ffmpeg runs — a dissolve reads a ≈50%
  blend, `eq` desaturates at `saturation: 0` and darkens with `brightness`,
  and `nle edl` renders and references the proxies.

**Deferred:** thumbnails / waveforms and a bin/`fx` panel (effects are
created by command); retiming a transition with `i`/`o`; OTIO transitions.

## M6 — export presets (planned)

- `preview` — VAAPI hardware, near-instant, lower quality
- `balanced` — libx264, good quality/speed trade
- `max` — libx265 / SVT-AV1, slow, archival

## Open / deferred

- Pool **keyframe `j`/`k`** — discussed, not built.
- `v` visual/select mode — deferred.

## Open decisions (non-blocking)

1. M6 encode-tier presets (`preview`/`balanced`/`max`).
2. `v` visual/select semantics.
3. M7 GUI technology.
