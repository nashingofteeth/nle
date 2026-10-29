# AGENTS.md

`nle`'s operating manual: the rules an agent needs before acting. Usage is
`README.md`, the roadmap is `PLAN.md`, and per-subsystem lore is in `docs/`.
The user is **learning Rust** through this project (from JS/PHP): teach as you
build — see "Learning Rust along the way".

## Layout and where to read first

| Crate | Job |
|---|---|
| `crates/nle-core` | Pure timeline logic — time math, media metadata, EDL model, edit ops + undo history. No processes. |
| `crates/nle-ffmpeg` | ffprobe/ffmpeg wrappers, filter-graph compiler. |
| `crates/nle-cli` | The `nle` binary + TUI + mpv IPC client. |

| Before touching | Read |
|---|---|
| `edl.rs`, `import-edl` | `docs/pitfalls/edl.md` |
| `otio.rs`, `import-otio` | `docs/pitfalls/otio.md` |
| `detect.rs`, `detect-edits` | `docs/pitfalls/detect-edits.md` |
| `graph.rs`, `preview.rs`, `proxy.rs`, mpv | `docs/pitfalls/mpv-ffmpeg.md` |
| actions, trims, playhead | `docs/pitfalls/playhead.md` |
| transitions, blends | `docs/pitfalls/transitions.md` |
| the borrow checker or clippy | `docs/rust-notes.md` |

Low-dependency stance: **the current set is frozen.** No new runtime
dependencies without explicit justification (a hand-rolled ~50 lines beats a
transitive tree for most needs).

- Direct deps (10): clap, anyhow, thiserror, serde, serde_json, num-rational,
  tempfile, ratatui, crossterm (+3 internal crates). Zero network code.
- **`tui` is a default Cargo feature.** `cargo build/test
  --no-default-features` is the headless variant — keep it compiling (gate
  `mod`s, commands, and TUI-only helpers).
- `Cargo.lock` is committed. After a dependency change run `cargo update`
  deliberately, then `cargo audit` (RustSec) before committing.
- Release profile is deliberately light (`lto = false`, `codegen-units = 16`);
  the hot work is in ffmpeg/mpv subprocesses. `release-lto` is opt-in. Builds
  require `clang` + `lld` (see `.cargo/config.toml`) for fast links.
- ffmpeg/ffprobe/mpv are the real attack surface: no shell, `--` before
  positional paths, isolated mpv config and IPC socket.

## Keeping the docs current

`README.md` = how to **use** it; `PLAN.md` = where it is **going**;
`AGENTS.md` = the always-on rules; `docs/` = per-area notes read on demand.

**Ask before editing `PLAN.md` or `AGENTS.md`**, and propose the exact
wording. `README.md` and `docs/` may be updated as part of a change.

**Default is don't add.** A note earns a place only if it changes an agent's
default action and is not obvious from the code, a test, `README.md`, or a
commit body. Otherwise it stays a commit message.

1. **Scope.** Always-relevant → `AGENTS.md`; one subsystem →
   `docs/pitfalls/<area>.md` plus an index line; a war story or measurement →
   the commit body (keep only the clause that prevents a wrong fix).
2. **Shape.** Imperative rule + the failure it prevents, ≤3 lines, naming a
   file/symbol/test/command. No dates, version numbers, or "this session".
3. **Dedupe.** Fold a new finding into the existing rule — one entry per class
   of failure — and point at the test that guards it.
4. **Evidence.** Add only if it bit us twice, caused data loss/corruption, or
   is a deliberate user preference.
5. **Prune at every milestone.** Delete what the code now enforces; when
   `AGENTS.md` passes ~250 lines, move detail into `docs/` rather than
   squeezing wording.

A milestone landing also means a `PLAN.md` pass: move its row and status
detail forward, note deferrals, keep the "next" pointer accurate — but don't
rewrite the roadmap unilaterally.

## Learning Rust along the way

**Every implementation step should teach, not just ship.** In plans,
explainers, and commit-adjacent notes:

- Name the Rust concept the change exercises and why (ownership/borrow,
  `Result`/`?`, enums + exhaustive `match`, `let-else`, modes-as-enums,
  closures/captures, lifetimes, iterator combinators, `Option`/`then`).
- Prefer the idiomatic construction and say what it replaces — illegal states
  unrepresentable vs. runtime checks; a pure `match` over stringly dispatch;
  splitting decide/apply with types.
- Call out the **gotcha** when the compiler or clippy pushes back, and what
  the error means (struct-literal parens, closure inference, `chars[cur..o]`
  panics). The running list is `docs/rust-notes.md`.
- Keep it short and tied to the code at hand — a concrete explainer beats a
  tutorial dump. Tie lessons to a real file/line.

## Verify before every commit

Run `scripts/check.sh`; do not commit otherwise — it is the gate list (fmt,
shellcheck, clippy, `cargo doc -D warnings`, tests, release build, install,
`demo.sh`). CI runs the same gates minus the install.

Commit one logical change at a time (`feat:`/`fix:`/`refactor:`/`docs:`),
subject then a short **why** body. Report findings (bugs, tradeoffs) plainly.

## Model invariants

- **Compatibility: none, by design.** Private, single user, not distributed.
  `nle.json`, `nle.history.json`, and cached artifacts may
  change shape any time; old files are **rejected with a clear error** (never
  silently misread) and the fix is "rebuild it" (`./demo.sh`). Bump the format
  version; don't write migration code. The project file is plain JSON with
  exact rational times (`"30000/1001"`); floats appear only at ffmpeg/IPC
  boundaries.
- **Frames are 0-based everywhere** (`f0` = first frame). A timeline of `dur`
  spans `[0, dur)`; the last representable position is `dur - 1/fps`.
- **Every write path clamps clips to their media.** A new import or write path
  must call `clamp_project_to_media`, or the model reads past EOF and the
  export silently truncates.
- **Track-scoped edits.** Every track-resolving edit is
  `*_on(project, track, …)`; the plain name wraps the first video track. New
  ops take the index — don't add fresh `video_track()` calls. A stale index
  returns `TrackNotFound` (`track_at`), never panics.
- **A track is a sequence.** A trim that shortens a clip without a gap op
  ripples the followers; gap trims always ripple; an inward gap move ripples
  and outward growth honours the flag (`e` overwrite, `E` insert). An append
  at/past the end is one gap (`append_at_end`) — never two adjacent gaps.
  `e`/`E`/`[`/`]` must agree on a leading gap's `GapHead`; retire redundant
  variants so paths can't drift.
- **Floors that shift a segment are slivers.** `insert_from_pool` floors an
  asset duration to whole frames (6.006 → 6.0) so later splits can't strand a
  sub-frame stump; the renderer skips zero-width segments.
- **Speed.** `Clip.speed` is a rational rate; timeline duration = source /
  speed. Cross the boundary only via `source_span` (edits) or `Clip::source_at`
  / `timeline_offset_of` (everything else) — the playhead anchor once missed
  the factor. A speed change always ripples, and the monitor proxies any
  non-1× clip because an EDL entry can't retime. The CLI/TUI take a
  **percentage** (100 = normal); convert at that boundary only — the model and
  `nle show` keep the rate (`@2×`).
- **Transitions.** Trimming beside or through a blend moves the blend, never a
  gap; a retime scales each side's span, not its rate. A blend is opaque,
  video-only and keeps its sides contiguous at the neighbour's rate. Full
  model and every edge case: `docs/pitfalls/transitions.md`.
- **A trim at an edge deletes the segment that edge would consume**
  (`trim_consume_target_on`): `i`/`I` the one **ending** at the playhead,
  `o`/`O` the one **starting** there — at a shared cut those are the two
  different neighbours. Deliberate and unit-tested.
- **`trim_in_on`/`trim_out_on` have a second job.** For a gap they *extend the
  neighbour into it* (extend mode's non-ripple grow); the trim keys and CLI
  ripple gaps first, via `trim_at_on`'s own gap check. Don't "simplify" that
  branch away — `extend_moves_an_edge_under_the_same_rules_as_the_tui` guards
  it. An opposite-end extend commit is a delete, by id for a clip and by index
  for a gap.
- **Undo/history.** Writes through `History` (edits, `add_asset` /
  `remove_asset`, `edit_tracks`) record; only whole-project rebuilds (`init`,
  `import-*`, `detect-edits`) re-baseline it via `save_and_reset_history`, or
  the next undo refuses ("history entry does not apply"). Both stacks persist.
  Track add/mute/rm are whole-list changes (`Change::Tracks`), verified by
  exact equality.
- **Shared state on disk.** The playhead is `Project.playhead`; the yank
  register is `nle.clipboard.json`. Both are convenience — a missing or
  version-skewed register reads as empty. The TUI adopts foreign changes
  (project, history, register, playhead) and refuses to clobber
  (`persist_playhead` → `Ok(false)`).
- **Gaps and the pool.** `Mode::Pool(PoolState)` keeps "pool open" and its
  selection together; tracks are 1-based (0 refused); a shorter track pads
  with a trailing gap in the row and the preview. The pool remembers the
  selected asset (by id), preview clock, and timeline resume across toggles
  and command detours (`remember_pool`); closing restores the cursor.
- **One quantizer per axis.** Draw from the same function the playhead/cut
  indicator uses (`marker_col`), or drawn edges drift from real cuts.

## Monitor and export

- The monitor plays the **composite**, not the active track:
  `composite_segments` walks every unmuted track's cuts and picks the topmost
  video with material; black only where all are gaps. A span where another
  track is audible is rendered as the export's mix through the proxy cache.
  Switching the active track does not reload it; `nle edl` prints what it
  plays.
- **The active track drives navigation and edits, not the monitor.** Digits
  are vim counts, so track selection is `t`/`T`/`<N>t` — bare `1..9` would
  break `3l`.
- **The monitor proxies whatever `edl://` can't express** — effects, non-1×
  retimes, transitions, foreign picture sizes, audible-mix spans — using the
  *export* graph (no second filter chain to drift), cached under
  `~/.cache/nle/proxy`. See `docs/pitfalls/mpv-ffmpeg.md`.
- **Export stays byte-identical for the simple case:** one video track + no
  audio track keeps the classic concat path; otherwise every track is padded
  to the timeline length, video layers `overlay` bottom→top (upper gaps
  `black@0.0`/`yuva420p`), audio `amix`es, and muted tracks drop out. A video
  track plus an audio one takes the multi-track path (fast = no audio).

## Editing and TUI conventions

- **One action catalog, three front-ends.** Every timeline edit is an `Action`
  (`nle-core/src/action.rs`) run by `run_action`; keys, the `:cmd` line and
  the CLI all use it, and the `:cmd` grammar *is* the CLI grammar (same flags;
  `--at`/`--track`/`--clip` default to the playhead/active track/clip-under in
  the TUI). A new action = an `Action` variant + a slot in `Action::slot`
  (exhaustive) + both parity tests. Session state (playhead, register) is not
  an action.
- **`--clip N` finds the clip's own track** (`resolve_track`); `--track`
  addresses position-based targets only. Tracks are 1-based — 0 is refused
  (`cli_index`), never silently "track 1".
- **The message line has one writer.** `run_action` sets the notice, then
  `persist_and_refresh` runs; a failure there **appends**
  (`{notice} — preview rebuild failed: …`) instead of replacing it.
- **Case encodes ripple** (`e`/`E`, `i`/`I`, `o`/`O`) everywhere.
  **`hints_for` is the single source of mode hints**; the message panel shows
  the latest notice only — don't put hint text in the message.
- A key-capturing panel forwards valid actions; `m` toggles the pool. Opening
  it notices "media", closing "timeline". Track changes work while it is open:
  they retarget `enter` but keep the asset preview (`select_active_track`).
- **The timeline panel sizes to show every track** (`panel_heights`): one row
  per track + marker row + borders; keys/message drop first on a short
  terminal. Keep hint lines ≲76 columns.

## Playhead

- Normal edits follow the **media frame** (clip id + source); extend lands on
  the edge; undo/redo restore the pre-edit position; deletes keep the offset
  (or the seam for ripple/gap).
- **A no-op edit must not move it**, an anchor survives while the segment
  does, and every write clamps to `last_frame` (`dur - 1/fps`), never `end`;
  a track or pool change leaves it put.
- Details, the float-vs-frames rule, and the audit tests:
  `docs/pitfalls/playhead.md`.

## Testing and debugging

- **Two integration batteries back the unit tests.**
  `crates/nle-cli/tests/behaviour.rs` asserts *results* (pixels, durations,
  frame counts, structure); `hostile.rs` holds garbage input to *invariants*
  (never panic, exit 0/1/2 only, leave a project that loads with a finite
  non-negative length). New end-to-end behaviour → `behaviour.rs`; new
  "garbage in" → `hostile.rs` (a case is a list of argv sequences). Both run
  under `cargo test` against the debug binary; `cargo test -p nle-core` is the
  sub-second loop. `tests/common` rebuilds templates every run — a stale
  template poisons every case.
- **Split decide from apply** so logic is testable without mpv/ffmpeg:
  `plan_for_armed`/`apply_extend_plan`, `commit_batch` vs
  `run_commit_action`, `pool_step_target` vs `pool_step`,
  `edit_anchor`/`reanchor_playhead`.
- **TUI render tests** use `ratatui::backend::TestBackend` and read the
  `Buffer` cell by cell. Count indices including the border, and when an
  assertion fails, print the grid first and write assertions from it.
- **Beware tests that pass by coincidence.** A one-item fixture where
  id == index hides an id-vs-index bug; build the distinguishing case (e.g.
  two assets so index 1 ≠ id 2).
- **The monitor is behind the `Mpv` trait**; TUI routing is tested with
  `FakeMpv` (add a `Call` variant, assert on it).

### TUI diagnostics

`nle edit --dump` renders once (no mpv), feeds `--dump-keys`, and prints the
model state + grid; it is read-only (a scratch copy) and backs the render
tests. Usage: README.
