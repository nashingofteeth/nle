# Transitions

Read before touching `add_transition`, `trim_blend_on`,
`trim_beside_blend_on`, the transition parts of `set_speed_on` / extend, or
the export's `xfade` chain.

- **A blend is part of both neighbours.** `add_transition` gives each side
  `dur × that neighbour's rate` and trims the same span off the clip, so the
  material is used once; clip↔clip shortens the timeline by `dur`, clip↔black
  leaves it. `a`/`b` stay contiguous with the neighbouring clips and
  `span / dur` is the rate at the seam.
- **The ends of a neighbour clip are the ends of the transition.** `o`/`O`
  inside moves the blend's right edge (the preceding clip's out *and* the
  following clip's in), `i`/`I` its left; the clip on the moved side covers
  what the blend gives up, so the sides stay contiguous at the same rate
  (`trim_blend_on`).
- **A trim on a clip's visible span beside a blend moves the blend's near
  edge**, not a gap: `o` on the preceding clip moves its left edge, `i` on the
  following clip its right. `i`/`o` grow the blend (timeline length unchanged);
  `I`/`O` keep its `dur` and ripple. Growing needs handle material — pre-roll
  before the following in-point, post-roll after the preceding out-point — and
  refuses with `BlendNoHandle` when the asset has none
  (`trim_beside_blend_on`).
- **Extend moves the side with the edge** (`move_blend_edge`); a *ripple*
  extend shifts the side (`shift_blend_side`), or the material plays twice.
- **A side carries its neighbour's effects.** A transition renders that
  neighbour's material, so `TransitionSource::Clip` owns a copy of its effect
  list: `add_transition` copies it, the three effect edits
  (`add`/`remove`/`clear`) keep it in sync through the same `EditOp` batch
  (`sync_effect_sides`, so undo restores both), and the export's `xfade`
  chain applies it. Without this the monitor and export jumped back to the
  ungraded picture inside the blend. The proxy hash includes the side's LUTs
  too.
  Extend mode hops to the blend's far end (`blend_far_end`).
- **Retime resizes the blend, not the sides' rates.** `retime_transition_on`
  (the `x`/`X` frames form, `R`'s `--percent`, and `transition --frames/--dur`)
  replaces `dur` and scales each clip side's span by `new/old`, so the
  neighbour keeps its rate and gives up or takes back the extra material. Two
  clip sides change the timeline by `-delta` (a longer blend shortens it); a
  black side leaves it. The original cut is the blend's far edge, so `--at`
  accepts it too; insufficient handles refuse (`BlendNoHandle`); one undo
  batch.
- **Speed.** A speed change retimes the sides the retimed clip owns
  (`set_speed_on`): the blend's `dur` and the cut (`a.src_out` / `b.src_in`)
  stay put and the inner edge follows (it is also the neighbour's in/out), so
  one rate covers the clip and its sides.
- **Removing a transition gives its trimmed frames back.** `dd`/`D` re-extend
  the neighbours, but only where they still line up exactly (same asset,
  contiguous source); one edited since is left alone and that material is
  really gone. Never merge them (before `x` they were two segments). The
  playhead anchor is the transition's **end**.
- **A blend is opaque and video-only.** An insert/paste at/over one is refused
  (`EditError::Transition`, as `_` does); an overwrite `p` consumes it whole;
  `add_transition` refuses a non-video track because the export graph emits
  video `xfade` chains an audio track never consumes. A composite span is
  widened to contain a transition whole — a partial crossfade isn't
  expressible.
- **A fade's black side has no clip to cover it**, so a blend trim there is
  refused (`NoClipSide`).
- **The row draws the boundary as a bare colour change**; a dotted box glyph
  sits mid-cell and reads misaligned against the left-aligned `▏`.
