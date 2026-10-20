# Playhead anchoring

Read before changing an action, a trim, or the TUI's playhead handling.

The playhead is stored in the project (`Project.playhead`), snapped to the
frame grid and clamped into the timeline. The TUI opens there, adopts the
file's value on reload, and writes it back through one writer
(`persist_playhead`): after every edit, from `:seek`, and ~1 s after the
transport stops moving (plus on quit). It is session state — no history
entry, and edits still take an explicit `--at`.

- Normal edits follow the **media frame** (clip id + source), not the
  timecode: a ripple head trim moves the playhead **back** by the trim; a
  gap-left head trim and tail trims leave it put.
- Extend commits land on the **edge** (the committed target) — otherwise a
  tail extend lands one frame short. Undo/redo restore the **pre-edit**
  position; an extend commit records the **entry edge** so undo returns where
  you entered the mode.
- Deletes: plain (`dd`) keeps the same offset in the resulting gap; ripple
  (`D`) and gap deletes land on the seam between the neighbours.
- **A no-op edit must not move the playhead.** `run_action` reanchors only
  when the outcome `changed`; only `Yank` reports `false`. Reanchoring a read
  slid the playhead because a transition anchor resolves to the transition's
  end (and `dd` yanks first).
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
- **Compare the playhead in frames, not floats.** The live position and the
  stored rational come from different computation paths and can differ in the
  last ulp — a float diff re-triggers the transport flush forever
  (`playhead_moved` compares `frame_of`).

Implemented by capturing an anchor before `history.apply` and resolving it
after (`run.resume` for the pool).
`playhead_placement_after_each_action` is the audit table (one row per rule);
`the_playhead_clamps_to_the_last_frame_after_a_trim` and
`a_track_removal_leaves_the_playhead_put` guard the two clamps above.
