# AGENTS.md

Procedures and conventions established in this working session (M3 polish:
extend mode, media pool, panels, playhead anchoring). See `PLAN.md` for the
roadmap and `README.md` for usage.

The user is **learning Rust through this project** (coming from JS/PHP):
teach as you build — see "Learning Rust along the way" below.

## Keeping the docs current

Three docs, three jobs: `README.md` = how to **use** it, `AGENTS.md` = how
to **work** on it, `PLAN.md` = where it's **going** (roadmap + status).

**When a milestone lands, or a task changes the plan, propose an update to
`PLAN.md` and ask before editing it.** Move the milestone's row and its
status detail forward, note anything deferred, and keep the "next"
pointer accurate — but don't rewrite the roadmap unilaterally.

When you (an agent) come across something during work that a future agent
would benefit from remembering, **ask the user before adding it to this
file** and propose the exact wording. Good candidates:

- a **sensibility** — how the user wants things to feel or be weighed;
- a **pitfall** — a trap that cost time or nearly slipped through;
- a **breakthrough** — a technique or insight that unblocked something;
- a **clarification of alignment** — a decision or reading of intent that
  wasn't obvious from the code (including "no, do it the other way").

Keep entries short, concrete, and reproducible. Don't add anything the
code already makes obvious, and don't document a one-off — wait until it's
clearly a durable rule or preference.

## Learning Rust along the way

The user knows JS/PHP, not Rust — this project is how they're learning it.
**Every implementation step should teach, not just ship.** In plans,
explainers, and commit-adjacent notes:

- Name the Rust concept the change exercises and why it applies here
  (ownership/borrow, `Result`/`?`, enums + exhaustive `match`, traits,
  lifetimes/NLL, `Arc<Mutex>`, closures/captures, `Drop`, `let-else`,
  modes-as-enums, iterator combinators, `Option`/`then`/`flatten`).
- Prefer the idiomatic construction and say what it replaces — e.g. making
  illegal states unrepresentable vs. runtime checks; a pure `match` over a
  stringly dispatch; splitting decide/apply with types.
- Call out the **gotcha** when the compiler or clippy pushes back, and what
  the error actually means (struct-literal parens, closure param
  inference, `chars[cur..o]` panics, moved-out values, `#[cfg(test)]`).
- Keep it concise and tied to the code at hand — a short, concrete
  explainer beats a tutorial dump. Tie lessons to a real file/line.

This is a durable expectation across all milestones, not a one-off.

## Verify before every commit

Run `scripts/check.sh`; do not commit otherwise. It is:

```sh
cargo fmt --all --check                      # formatting is enforced
cargo clippy --all-targets -- -D warnings    # warnings are errors
cargo test                                   # full workspace, must be green
cargo build --release
cargo install --path crates/nle-cli
./demo.sh                                    # end-to-end pixel checks
```

Commit one logical change at a time (`feat:`/`fix:`/`refactor:`/`docs:`),
subject then a short **why** body. Report findings (bugs, tradeoffs)
plainly.

## Testing procedures learned here

- **Split "decide" from "apply" so logic is testable without mpv/ffmpeg.**
  Functions that need a live `MpvPlayer` can't be unit-tested, so extract
  the pure decision and test that: `plan_for_armed`/`apply_extend_plan`,
  `commit_batch` (pure) vs `run_commit_action`, `pool_step_target` vs
  `pool_step`, `edit_anchor`/`reanchor_playhead`.
- **TUI render tests via `ratatui::backend::TestBackend`**, reading the
  `Buffer` by cell. **Count indices including the border** — dump the row
  and count, don't trust visual columns (borders offset by one).
- **Beware tests that pass by coincidence.** A one-item fixture where
  id == list index hides an id-vs-index bug; construct the case that
  distinguishes them (e.g. two assets so index 1 ≠ id 2).
- **Dump mid-fix.** When a render assertion fails, print the actual grid
  and read it, then write assertions from the dump.
- **The monitor is behind the `Mpv` trait**; TUI input routing is tested with
  `FakeMpv`, which records the calls. Handlers that need mpv are no longer
  untestable — add a `Call` variant and assert on it.

## Playhead anchoring after an action (settled rules)

- Normal edits follow the **media frame** (clip id + source), not the
  timecode → a ripple head trim moves the playhead **back** by the trim;
  a gap-left head trim and tail trims leave it put.
- Extend commits land on the **edge** (the committed target) — without this
  a tail extend lands one frame short of the new end.
- Undo/redo restore the **pre-edit** position; an extend commit records the
  **entry edge** so undo returns where you entered the mode.
- Deletes: plain (`dd`) keeps the same offset in the resulting gap; ripple
  (`D`) and gap deletes land on the seam between the neighbours.
- Implemented by capturing an anchor **before** `history.apply` and
  resolving it after; `run.resume` for the pool.

## Domain rules fixed this session

- **Compatibility: none, by design.** Private, single machine, single user,
  not distributed — so no backward compatibility. `nle.json`,
  `nle.history.json`, and cached artifacts may change shape any time; old
  files are **rejected with a clear error** (never silently misread) and
  the fix is "rebuild it" (`./demo.sh`). Bump the format version; don't
  write migration code.
- **Out-of-band project writes must reset the undo sidecar.** Any path that
  saves `nle.json` without going through the TUI's `History` (CLI
  `init`/`import`/`add`/`remove`/`media rm`) re-baselines the project, so it
  deletes `nle.history.json` via `save_and_reset_history`. Otherwise the
  next TUI session refuses every undo with "history entry does not apply".
- **Frames are 0-based everywhere** (`f0` = first frame; timecode `:FF`
  matches notices). A file/timeline of `dur` spans `[0, dur)`; the last
  representable position is `dur - 1/fps` — clamp seeks there.
- **One quantizer per axis:** stem drawn geometry from the same function
  the playhead/cut indicator uses (`marker_col`), or drawn edges drift from
  real cuts. A `.max(1)` on a rounded width desyncs everything after it.
- **Gap trims always ripple**; gaps have no material.
- **Opposite-end extend commit = delete** (ripple), by **id** for a clip
  (not by playhead position, which resolves to the follower) and by
  **index** for a gap.
- **Any inward gap move ripples**; outward gap growth honours the flag
  (`e` overwrite, `E` insert space).
- **`e`/`E`/`[`/`]` must agree**: a leading gap's head is `GapHead` for all
  of them (retire redundant variants so paths can't drift — same for
  `resolve_armed` vs `armed_on_side`).
- **Floors that shift a segment are slivers:** `insert_from_pool` floors an
  asset duration to whole frames (6.006 s → 6.0 s) so later splits can't
  strand a sub-frame stump; the renderer also skips zero-width segments.
- **The media pool is a `Mode` variant** (`Mode::Pool(PoolState)`), not flat
  fields — so "pool open" and its selection can't drift apart. Pool handlers
  use small `Copy` accessors (`pool_sel`/`set_pool_sel`/…) so a borrow ends at
  the statement instead of borrowing all of `App` across a helper call.
- **Track-scoped edits (M4):** every track-resolving edit op is
  `*_on(project, track, …)`; the plain name is a wrapper for the first
  video track. New ops must take the index — don't add fresh
  `video_track()` calls. A stale index returns `TrackNotFound`, never
  panics (`track_at`).
- **The active track drives navigation and edits, not the monitor:**
  `App::active()` is the clamped index; navigation and every edit op follow
  it, but the monitor plays the **composite** (next entry). Digits are vim
  counts, so track selection is `t`/`T` and `<N>t` — **bare `1..9` would
  break `3l`**.
- **The monitor plays a composite, not a track:** `composite_segments`
  walks the union of every unmuted video track's cut points and picks the
  topmost track with a clip per interval; black only where all tracks are a
  gap. mpv's EDL can pick only one source per span (no blending), so this is
  fall-through, not `overlay` — real compositing is M5. Switching the active
  track does not reload the monitor; `nle edl --track N` still previews a
  single track for debugging.
- **A track shorter than the timeline pads with a trailing gap** in both the
  timeline row and the preview (`track_parts` / `segments`), so the row reads
  "gap" and playback spans the whole timeline instead of stopping at the
  track's own end.
- **Export stays byte-identical for the simple case:** one video track +
  no audio track keeps the classic concat path; otherwise each track is
  padded to the timeline length, video layers `overlay` bottom→top (upper
  gaps `black@0.0`/`yuva420p`), audio `amix`es, and muted tracks drop out.
- **Speed:** `Clip.speed` is a rational rate (default 1); timeline duration =
  source / speed. Every edit that turns a timeline delta into `asset_in/out`
  scales by speed (`source_span`). **A speed change always ripples** — the
  timeline is a sequence, so a new duration moves every follower. Export
  retimes with `setpts=(PTS-STARTPTS)/speed` + `atempo` (chained outside
  [0.5, 2]); the 1× graph stays byte-identical.
- **One video track + an audio track still takes the multi-track export
  path** (the fast path needs *zero* audio tracks), so a single video layer
  must reach `[vout]` with no dangling `null[vcomp]`.

## EDL import (CMX3600) pitfalls

- **Record time starts at `01:00:00:00`** — the timeline origin is the
  *earliest* event, not 0. Treating 0 as the origin turns the first hour
  into one giant gap.
- **`FCM: NON-DROP FRAME` contains the substring "DROP"** — test
  `!contains("NON")`, or every non-drop EDL is misread as drop-frame.
- **Source timecodes are file-relative by default**; `--src-offset` maps
  hour-based media. Audio events whose reel + source match a video event are
  **linked** and skipped, so embedded audio isn't doubled.

## OTIO import pitfalls

- **Kdenlive's OTIO export has no `LinearTimeWarp`.** It encodes a rate
  conform as `source_range` in *project* frames + `available_range` in the
  *media* rate, so derive `speed = source_rate / media_rate` and source
  seconds as `frame / media_rate`. (A real 24 fps project over 29.97 fps
  media comes out as 1001/1250, not a rounded 0.8.)
- **Dispatch on the OTIO schema *name*, not the tag:** `OTIO_SCHEMA` is
  versioned (`Clip.1`, `Clip.2`), so match the part before the `.` over
  `serde_json::Value`; a `#[serde(tag)]` enum breaks on a version bump.
- **`Clip.2` nests media under `media_references.DEFAULT_MEDIA`** (a map),
  while `Clip.1` uses a singular `media_reference` — handle both.

## Rust gotchas hit this session

- Struct literals need parens in comparisons: `if x == (Foo { .. })`.
- Closure params need type annotations when there's no call context.
- `chars[cur..o]` panics if `cur > o` — sort mark offsets first.
- `#[cfg(test)]` a helper used only by tests, or clippy warns dead-code.
- `(!cond).then(|| …).flatten()` for an optional-when-condition value.
- `thiserror` tuple variants: mixing `{0}` with a bare `{}` in `#[error]` is
  "ambiguous reference to positional arguments by number" — name the extra
  arg (`expected = CONST`) instead.
- Two writers of one invariant (`sync` vs `seek`) — clamp at **every**
  write, not just the "obvious" one.
- `clippy::too_many_arguments` (7+) → group the args in a parameter struct
  (`DetectArgs`) instead of `#[allow]`-ing it.

## Interaction conventions added this session

- **`e`/`E` = replace/ripple**; case encodes the ripple flag everywhere.
- **`keys` cheat-sheet (`hints_for`) is the single source of mode hints**;
  the **`message`** panel shows the latest notice only. Don't put the hint
  text in the message.
- Opening the pool notices **"media"**; closing notices **"timeline"**.
- A key-capturing panel must forward valid actions (undo works while the
  pool is open). `m` is a **toggle**.
- **Track change works while the pool is open** (`t`/`T`/`<N>t`): it
  retargets where `enter` inserts but leaves the monitor on the asset preview
  (`select_active_track` splits selection from the EDL reload; the pool title
  shows the target track).
- The pool previews pooled files in mpv: `j/k` assets, `{ }` list ends,
  `h/l/H/L` seek, `f/F` frame, `gg/0` start, `G/$` end, `space` (restart at
  EOF), `enter` insert, `d` remove, `u` undo.
- **The timeline panel sizes to show every track** (`panel_heights`): one row
  per track + marker row + borders; the keys/message panels drop first when
  the terminal is short, and tracks window only when they physically can't
  fit. Keep hint lines ≲76 cols so they don't wrap and push content off.

## mpv/ffmpeg states worth remembering

- mpv `--keep-open=always` holds at EOF a frame or two before the probe
  duration; the pool clock snaps to `dur − 1/fps` at EOF.
- Pool preview stops at the reachable source: a clip head previews
  `[0, aout]`, not the whole asset.
- `overlay`'s `format` option accepts only `yuv420/yuv422/yuv444/rgb/gbrp/
  auto` — `yuva420p` is rejected as invalid, but the overlay's alpha plane
  still drives compositing (the top layer's transparency works anyway).
- A helper that emits a filter chain must push its output label too: the
  multi-track audio pad emitted its `anullsrc…` chain but forgot the
  label, and ffmpeg reported a filter with an unconnected pad.

## detect-edits pitfalls

- **Cuts to/from black score below the scene threshold** — a hard cut to
  black is invisible to `select='gt(scene,T)'`. Pair it with
  `blackdetect` (the dailies test: 3 s leading black + a 0.2 s black flash,
  both recovered as gaps).
- **`select` fires on consecutive frames during flashes** — `min_shot`
  folds sub-threshold media spans into a neighbour; a short media span
  flanked by gaps is real and kept.
- **`-vf` argv needs ffmpeg-level quoting**: the `select` comma must be
  protected as `select='gt(scene,0.4)'` (we pass argv without a shell).
- **Detection decimals aren't rationals**: `showinfo` prints
  `pts_time:3.03333`; `Seconds::approximate_float` keeps the decimal and
  `reconstruct` re-snaps to the frame grid.
- **Extra streams are ignored, not silent**: the test `.mov` had two video
  streams; `detect-edits` warns and reconstructs the first.

## Layout & dependency policy (moved from README)

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops + undo history. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary + TUI + mpv IPC client. |

Low-dependency stance: **the current set is frozen.** No new runtime
dependencies without explicit justification (a hand-rolled ~50 lines beats
a transitive tree for most needs).

- Direct deps (10): clap, anyhow, thiserror, serde, serde_json,
  num-rational, tempfile, ratatui, crossterm (+3 internal crates).
  Zero network code in the tree.
- **`tui` is a default Cargo feature.** `cargo build/test
  --no-default-features` is the headless variant (no ratatui/crossterm) — keep
  it compiling when adding TUI code (gate `mod`s, commands, and TUI-only
  helpers).
- `Cargo.lock` is committed — builds are reproducible; upgrades are
  deliberate events, not side effects.
- After any dependency change: `cargo update` deliberately, then run
  `cargo audit` (RustSec advisories) before committing.
- External system binaries (`ffmpeg`, `ffprobe`, `mpv`) are the real
  attack surface, not crates: they're invoked without a shell, option
  injection is blocked with `--` before every positional path, and mpv
  runs with an isolated config and its own IPC socket.

## Project-file internals (implementation note)

`nle.json` is plain JSON with exact rational times (`"30000/1001"`) —
human-readable and drift-free. Times are `Rational64` throughout the
model; floats appear only at ffmpeg/IPC boundaries.
