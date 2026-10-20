# detect-edits pitfalls

Read before touching `crates/nle-core/src/detect.rs` or `detect-edits`.

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
  can't cut there, so the export resamples each clip and drifts and `f`/`F`
  steps back off a cut instead of returning to it. `--append` loads the target
  up front so `snap_fps_for` sees its fps; a fresh project is created at the
  source's, so they coincide.
