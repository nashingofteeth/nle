# EDL import (CMX3600) pitfalls

Read before touching `crates/nle-core/src/edl.rs` or the `import-edl` path.

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
  the accumulated mismatch.
- **Fuzzy reel matching must be edge-aligned and unique.** A substring that
  matches anywhere latches `AX` onto anything containing it (`taxi.mp4`),
  and first-match-in-pool-order is a decision, not evidence: match a stem
  edge, require exactly one candidate, else report `Unresolved`.
