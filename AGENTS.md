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
cargo test                                   # unit + behaviour + hostile batteries
cargo build --release
cargo install --path crates/nle-cli
./demo.sh                                    # end-to-end pixel checks (release binary)
```

Commit one logical change at a time (`feat:`/`fix:`/`refactor:`/`docs:`),
subject then a short **why** body. Report findings (bugs, tradeoffs)
plainly.

## Testing procedures learned here

- **Two integration-test batteries back the unit tests.**
  `crates/nle-cli/tests/behaviour.rs` asserts *results* — export pixels,
  durations, frame counts, structure — for normal workflows
  (overwrite-vs-ripple lengths, the speed↔source math, audio retiming,
  multi-track overlay/mute, undo/redo byte-identity, EDL/OTIO round trips);
  `crates/nle-cli/tests/hostile.rs` holds hostile input to *invariants* —
  never panic, exit 0/1/2 only, and leave a project that loads with a finite
  non-negative length — rather than exact messages. A new end-to-end
  behaviour belongs in `behaviour.rs`; a new "garbage in" case belongs in
  `hostile.rs` (a case is a list of argv sequences). Both run under
  `cargo test` (~10 s) against the *debug* binary, so integer overflow panics
  there where release wraps; `cargo test -p nle-core` stays the sub-second
  loop. `tests/common` generates media once into `CARGO_TARGET_TMPDIR` and
  rebuilds the project templates every run — a stale template poisons every
  case.
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

## TUI diagnostics: dump the state and the grid

`nle edit --dump [--dump-size WxH] [--dump-keys KEYS]` renders the TUI **once**
(no mpv needed), feeds `KEYS` first (`\n` = Enter, `\e` = Esc), and prints the
project path, the model state (`nle show`'s summary + mode, active track,
playhead in seconds and frames, paused/eof, the message) and the grid with a
**column ruler**. A rendering, layout, or playhead-placement question can then
be answered from the dump instead of a screenshot — say *"dump `x` at 100x30 on
the demo project"* and the answer is one command. **The dump is read-only:** the
keys run through the real handlers (which save), so it works on a scratch copy
of the project + undo sidecar + register and deletes it afterwards — it never
touches the project it diagnoses. The same path backs the render tests
(`ratatui::backend::TestBackend`), so the dump and the tests see the identical
grid.

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
- **A no-op edit must not move the playhead.** `run_action` reanchors only
  when the outcome `changed`; only `Yank` reports `false`, and reanchoring a
  read used to slide the playhead (a transition anchor resolves to the
  transition's *end*, so `yy` on a blend moved it — and `dd` yanks first).
- **A gap/transition anchor keeps the playhead while the segment survives.**
  `edit_anchor` maps a transition to its end (the seam a removal leaves), but
  the seam is used only once the segment under the playhead is gone — an
  insert, a trim, or an effect elsewhere over a gap must not slide it.
- **Every playhead write clamps to `last_frame`** (`dur - 1/fps`), never
  `end`: an edit that shortens the timeline — or a redo restoring a pre-edit
  position into a shorter one — must not park the playhead past the final
  frame (`Project::last_frame`; the same rule as a seek).
- **A track or pool change leaves the playhead put** (`reanchors`): it doesn't
  move the active track's media, and `track rm` re-indexes — an anchor taken
  against the old active track would resolve against the wrong one.
- `playhead_placement_after_each_action` is the audit table (one row per rule);
  `the_playhead_clamps_to_the_last_frame_after_a_trim` and
  `a_track_removal_leaves_the_playhead_put` guard the two clamps above.
- Implemented by capturing an anchor **before** `history.apply` and
  resolving it after; `run.resume` for the pool.

## Domain rules fixed this session

- **Compatibility: none, by design.** Private, single machine, single user,
  not distributed — so no backward compatibility. `nle.json`,
  `nle.history.json`, and cached artifacts may change shape any time; old
  files are **rejected with a clear error** (never silently misread) and
  the fix is "rebuild it" (`./demo.sh`). Bump the format version; don't
  write migration code.
- **Undo sidecar rule (revised).** Writes that go through `History`
  (`run_action`, `history.remove_asset`, `history.add_asset`,
  `history.edit_tracks`) **record**; only whole-project rebuilds (`init`,
  `import-edl`, `import-otio`, `detect-edits`) re-baseline it via
  `save_and_reset_history`, or the next undo refuses with "history entry does
  not apply". Both stacks persist (`HISTORY_VERSION` 10) — a redo stack that
  died with the session made `nle redo` permanently empty.
- **Track edits are whole-list changes.** `Change::Tracks` snapshots the track
  list before/after for add/mute/rm/purge (segment snapshots can't express
  them), verified by exact equality like `Change::Assets` — a track set changed
  outside the stack refuses rather than mis-restoring. `History::edit_tracks`
  rolls back a refused edit.
- **The yank register is shared state.** `nle.clipboard.json` sits next to
  the project; `yy`/`nle yank` write it and `p`/`nle paste` read it, so a
  shell and the TUI hand segments back and forth. Convenience, never
  critical: a missing or version-skewed register reads as empty.
- **The playhead is stored in the project** (`Project.playhead`, v5) — the
  shared cursor `nle seek`/`:seek` move. Snapped to the frame grid and
  clamped into the timeline; the TUI opens there, adopts the file's value on
  reload, and writes it back through one writer (`persist_playhead`): after
  every edit, from `:seek`, and ~1 s after the transport stops moving (plus
  on quit). Session state: no history entry, and edits still take explicit
  `--at`.
- **Compare the playhead in frames, not floats.** The live position and the
  stored rational come from different computation paths and can differ in the
  last ulp — a float diff re-triggers the transport flush forever
  (`playhead_moved` compares `frame_of`).
- **Concurrent shell + TUI: adopt, never clobber.** The TUI watches the
  mtimes of `nle.json`, the undo sidecar, and the register (as of our last
  read/write); a foreign change reloads project + history + register +
  playhead within a tick, deferred to Normal while a mode holds indices
  (`watch_foreign`). Saves are guarded: `persist_playhead` refuses
  (`Ok(false)`) when the file moved under us, and `persist_and_refresh`
  adopts + reports `not saved: … (reloaded)`. The CLI's load→save window is
  milliseconds and stays unguarded.
- **Frames are 0-based everywhere** (`f0` = first frame; timecode `:FF`
  matches notices). A file/timeline of `dur` spans `[0, dur)`; the last
  representable position is `dur - 1/fps` — clamp seeks there.
- **One quantizer per axis:** stem drawn geometry from the same function
  the playhead/cut indicator uses (`marker_col`), or drawn edges drift from
  real cuts. A `.max(1)` on a rounded width desyncs everything after it.
- **Gap trims always ripple**; gaps have no material.
- **`trim_in_on`/`trim_out_on` have a dual role — don't "simplify" their gap
  branch away.** For a clip they trim; for a gap they *extend the neighbour
  into it*, which is what extend mode's non-ripple grow calls
  (`commit_batch` → `GrowHead/TailIntoGap`). The trim keys and the CLI ripple
  gaps instead, via `trim_at_on`'s own gap check. The unit test
  `extend_moves_an_edge_under_the_same_rules_as_the_tui` is the guard.
- **Opposite-end extend commit = delete** (ripple), by **id** for a clip
  (not by playhead position, which resolves to the follower) and by
  **index** for a gap.
- **A trim at an edge deletes the segment that edge would consume**
  (`trim_consume_target_on`): `i`/`I` the one **ending** at the playhead,
  `o`/`O` the one **starting** there — at a shared cut those are the two
  different neighbours, so `i` at a clip|clip cut deletes the *preceding*
  clip, and `i` on a clip's start deletes the preceding gap. Deliberate (and
  unit-tested): the trim collapses the segment whose edge is already there.
  **A transition's edge is not a neighbour's edge** (`blend_ends_at` /
  `blend_starts_at`): since the blend is part of both neighbours, `o` at its
  end and `i` at its start are no-ops, not a delete of the neighbour.
- **A transition's side plays at its neighbour's rate.** `add_transition` gives
  each side the span `dur × that neighbour's rate`, and the side renders that
  span into `dur`, so `span / dur` is the rate at the seam. `set_speed_on`
  keeps that true by retiming the sides the retimed clip owns: the transition's
  `dur` (the crossfade the user chose) and the cut (`a.src_out` / `b.src_in`)
  stay put and the inner edge follows — which is also the neighbour's in/out
  point, so the two stay contiguous. One rate covers the clip plus its sides,
  so a retimed clip gives up `dur × rate` of source and the seam has no speed
  step.
- **Removing a transition gives its trimmed frames back.** A transition owns
  the spans it took off its neighbours, so `dd`/`D` re-extend them — but
  *only* where the neighbour still lines up exactly (same asset, contiguous
  source); one edited since `x` is left alone and that material is really
  gone. Never merge the neighbours (before `x` they were two segments), and
  the playhead anchor is the transition's **end** — the cut the restore
  leaves — not its start.
- **A transition is opaque and video-only.** It can't be split or trimmed: an
  insert/paste at/over one is refused (`EditError::Transition`, as `_` does),
  an overwrite `p` consumes it whole. `add_transition` refuses a non-video
  track (the export graph emits video `xfade` chains an audio track never
  consumes). A composite *span* is widened to contain a transition whole and
  the slice keeps it whole — a partial crossfade isn't expressible.
- **The ends of a neighbour clip are the ends of the transition.** A blend is
  part of both neighbours: `o`/`O` moves its right edge (the preceding clip's
  out, and the following clip's in), `i`/`I` its left; the clip on the moved
  side covers what the blend gives up, so `a`/`b` stay contiguous and
  `span / dur` stays the neighbour's rate. Extending into a blend moves each
  side with its edge (`move_blend_edge`); a *ripple* extend shifts the side
  (`shift_blend_side`), or the material plays twice. A fade's black side has no
  clip to cover it, so a blend trim is refused (`NoClipSide`). Extend mode hops
  to the blend's far end (`blend_far_end`), and the row draws the boundary as a
  bare colour change — a dotted box glyph sits mid-cell and reads misaligned
  against the left-aligned `▏`.
- **A trim on a clip's visible span beside a blend moves the blend, not a
  gap.** `o` on the preceding clip moves the blend's left edge, `i` on the
  following clip its right: the visible span shrinks and the blend grows by
  the same amount, so the timeline length is unchanged and `a`/`b` stay
  contiguous at the neighbour's rate (`trim_beside_blend_on`). Growing needs
  handle material (pre-roll before the following clip's in-point, post-roll
  after the preceding clip's out-point); a shortage refuses with
  `BlendNoHandle` (asset + shortfall) rather than writing a source before 0.
  `I`/`O` keep the blend's `dur` and ripple: the side follows the new edge
  and the shorter clip slides the blend and followers left.
- **Every write path clamps clips to their media.** EDL import, OTIO import,
  `detect-edits` and `purge-stumps` all run `clamp_to_media`; a new import or
  write path must call `clamp_project_to_media`, or the model reads past EOF
  and the export silently truncates (a 10 s clip over a 5 s file exports as
  5 s and `nle show` disagrees).
- **Any inward gap move ripples**; outward gap growth honours the flag
  (`e` overwrite, `E` insert space).
- **An append at/past the end is one gap.** Inserting or pasting a gap beyond
  the track end bridges the void and the payload as a *single* gap segment
  (`append_at_end`) — the track never holds two adjacent gaps.
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
  walks the union of every unmuted track's cut points and picks the topmost
  video track with material per interval; black only where all tracks are a
  gap. mpv's EDL can pick only one source per span, so the video is
  fall-through (opaque clips make that the export's `overlay`) — but **sound
  is not**: a window where another track is audible needs the mix, so that
  span is rendered as the export's composite (`overlay` + `amix`) through the
  proxy cache and played as one entry (spans are widened to contain a
  transition whole, since a partial crossfade isn't expressible). Switching
  the active track does not reload the monitor; bare `nle edl` prints the
  composite the monitor plays, and `nle edl --track N` a single track for
  debugging.
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
  [0.5, 2]); the 1× graph stays byte-identical. The monitor can't retime an
  EDL entry, so a non-1× clip goes through the proxy cache like an effect.
  A rate change snaps the new *timeline* duration to the frame grid and stores
  the rate that fits (`source / duration`), leaving the source span exactly as
  selected — 0.7× of a 2 s clip is 86 frames at 30/43. Bending the span
  instead reads one extra source frame and the render grows a frame per
  retimed clip (measured 87, 174 against the model's 86, 172). A *rate
  conform* from an EDL/OTIO import is the other way round: `clamp_to_media`
  keeps its rate and nudges the span, because that rate is the source's
  timebase, not a request.
- **The monitor proxies whatever `edl://` can't express.** Two kinds, both
  rendered by the *export* graph (so there is no second filter chain to drift)
  and cached under `~/.cache/nle/proxy`:
  - a *segment* (`preview::needs_proxy`): an effect, a non-1× retime, a
    transition, or a picture size that isn't the sequence's (mpv re-frames a
    raw file to *its* aspect while the export scales it, so the framing would
    jump at the seam); hashed by the segment + geometry + fps + the assets it
    reads;
  - a *span* where another track is audible (`proxy::composite_proxy`): every
    unmuted track's slice of it, hashed the same way over the slices.
  Each asset's **path and mtime** are in the hash, so a replaced file
  re-renders — and so is each **LUT file** an effect reads (the segment JSON
  carries only its path, so editing a `.cube` in place must re-render too).
  The render is `export_quiet`: the monitor calls it, so ffmpeg must never
  write to the TUI's stderr.
- **Any timeline↔source mapping must scale by `Clip.speed`.** Edits use
  `source_span` (`edit.rs`); anything outside it uses `Clip::source_at` /
  `Clip::timeline_offset_of`. The playhead anchor missed the factor and
  silently followed the wrong frame — the pair on `Clip` is the only
  approved way to cross that boundary.
- **One video track + an audio track still takes the multi-track export
  path** (the fast path needs *zero* audio tracks), so a single video layer
  must reach `[vout]` with no dangling `null[vcomp]`.

## EDL import (CMX3600) pitfalls

- **Cuts only, but never fatal.** A non-cut event (`D`, `W001`, `K`, …)
  imports as a cut and is reported on `EdlDoc::warnings`, like the OTIO
  path's skipped transitions; a zero-length event is skipped; a black reel
  (`BL`/`BLK`/`BLACK`) becomes a gap. One dissolve must not sink a conform
  list. `build` never reads the transition field — the mapping is the
  parser's job.
- **The reel fallback skips the fuzzy match for a black reel**, or `BL`
  latches onto `blue.mp4` (the edge rule matches `"blue".starts_with("bl")`).
  Only an explicit `--map` or a `SOURCE FILE`/`FROM CLIP NAME` hint resolves
  one. `B` is not in the black list — it is a plausible tape name.
- **A channel code combines with `/`** (`AA/V` = both audio channels *plus*
  video): match any part, or `AA/V` reads as audio-only and the import dies
  with "no video events". A video clip carries its embedded audio, so a
  combined event's sound is linked, not doubled.
- **Record time starts at `01:00:00:00`** — the timeline origin is the
  *earliest* event, not 0. Treating 0 as the origin turns the first hour
  into one giant gap.
- **`FCM: NON-DROP FRAME` contains the substring "DROP"** — test
  `!contains("NON")`, or every non-drop EDL is misread as drop-frame.
- **Source timecodes are file-relative by default**; `--src-offset` maps
  hour-based media. Audio events whose reel + source match a video event are
  **linked** and skipped, so embedded audio isn't doubled.
- **A speed change hides in the src/rec span mismatch.** EDLs carry no rate
  field; `speed = src_span / rec_span`. Deriving it per event keeps the built
  timeline aligned with record time; assuming 1× shifts every later event by
  the accumulated mismatch (this shipped wrong once).
- **Fuzzy reel matching must be edge-aligned and unique.** A substring that
  matches anywhere latches `AX` onto anything containing it (`taxi.mp4`),
  and first-match-in-pool-order is a decision, not evidence: match a stem
  edge, require exactly one candidate, else report `Unresolved`.

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

- **One action catalog, three front-ends.** Every timeline-editing action is
  an `Action` (`nle-core/src/action.rs`) run by `run_action`; keybinds cover
  the high-frequency ones, the `:cmd` line covers **all** of them, and the
  CLI has a matching subcommand. The `:cmd` grammar *is* the CLI grammar
  (same flags; `--at`/`--track`/`--clip` default to the playhead/active
  track/clip-under in the TUI). A new action = a new `Action` variant + a
  slot in `Action::slot` (the match is exhaustive) + both parity tests — they
  fail loudly if a spelling misses it. **Session state is not an action**:
  the playhead (`nle seek`/`:seek`) and the yank register are shared slots,
  not catalog entries.
- **`--clip N` finds the clip's own track** (`resolve_track`), so delete,
  yank, speed and effect address a clip wherever it lives; `--track` only
  addresses position-based targets. Tracks are 1-based — 0 is refused
  (`cli_index`), never silently "track 1".
- **The message line has one writer.** `run_action` sets the edit's notice,
  then `persist_and_refresh` runs; a failure there **appends**
  (`{notice} — preview rebuild failed: …`) instead of replacing it — plumbing
  must not erase what happened to the timeline.
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
- **Snap to the timeline's grid, not the source's**: `reconstruct` must run at
  the *project's* fps. A 59.94 fps render appended to a 24 fps project, snapped
  to the source grid, lands ~0.4 of a frame off every cut — the 24 fps timeline
  can't cut there, so the export resamples each clip and drifts (measured
  ~1.7 frames / 70 ms over 18 cuts) and `f`/`F` steps back off a cut instead of
  returning to it. `--append` loads the target up front so `snap_fps_for` sees
  its fps; a fresh project is created at the source's, so they coincide.

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
- **Release profile is deliberately light** (`lto = "thin"`,
  `codegen-units = 16`). The hot work is in external ffmpeg/mpv
  subprocesses, not Rust, so fat LTO bought no runtime win while forcing a
  full relink (~28s) on every edit. The old `fat`/`cgu = 1` settings live in
  the opt-in `release-lto` profile — use that only when you actually want it.
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
