# AGENTS.md

Procedures and conventions established in this working session (M3 polish:
extend mode, media pool, panels, playhead anchoring). See `PLAN.md` for the
roadmap and `README.md` for usage.

## Keeping this file current

When you (an agent) come across something during work that a future agent
would benefit from remembering, **ask the user before adding it here** and
propose the exact wording. Good candidates:

- a **sensibility** — how the user wants things to feel or be weighed;
- a **pitfall** — a trap that cost time or nearly slipped through;
- a **breakthrough** — a technique or insight that unblocked something;
- a **clarification of alignment** — a decision or reading of intent that
  wasn't obvious from the code (including "no, do it the other way").

Keep entries short, concrete, and reproducible. Don't add anything the
code already makes obvious, and don't document a one-off — wait until it's
clearly a durable rule or preference.

## Verify before every commit

Run this exact sequence; do not commit otherwise:

```sh
cargo test                 # full workspace, must be green
cargo clippy --all-targets # must be 0 warnings
cargo build --release
cargo install --path crates/nle-cli
./demo.sh                  # end-to-end pixel checks (red/blue/black/green)
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

## Rust gotchas hit this session

- Struct literals need parens in comparisons: `if x == (Foo { .. })`.
- Closure params need type annotations when there's no call context.
- `chars[cur..o]` panics if `cur > o` — sort mark offsets first.
- `#[cfg(test)]` a helper used only by tests, or clippy warns dead-code.
- `(!cond).then(|| …).flatten()` for an optional-when-condition value.
- Two writers of one invariant (`sync` vs `seek`) — clamp at **every**
  write, not just the "obvious" one.

## Interaction conventions added this session

- **`e`/`E` = replace/ripple**; case encodes the ripple flag everywhere.
- **`keys` cheat-sheet (`hints_for`) is the single source of mode hints**;
  the **`message`** panel shows the latest notice only. Don't put the hint
  text in the message.
- Opening the pool notices **"media"**; closing notices **"timeline"**.
- A key-capturing panel must forward valid actions (undo works while the
  pool is open). `m` is a **toggle**.
- The pool previews pooled files in mpv: `j/k` assets, `{ }` list ends,
  `h/l/H/L` seek, `f/F` frame, `gg/0` start, `G/$` end, `space` (restart at
  EOF), `enter` insert, `d` remove, `u` undo.
- Panels hide below a height threshold so the timeline keeps its space;
  keep hint lines ≲76 cols so they don't wrap and push content off.

## mpv/ffmpeg states worth remembering

- mpv `--keep-open=always` holds at EOF a frame or two before the probe
  duration; the pool clock snaps to `dur − 1/fps` at EOF.
- Pool preview stops at the reachable source: a clip head previews
  `[0, aout]`, not the whole asset.
