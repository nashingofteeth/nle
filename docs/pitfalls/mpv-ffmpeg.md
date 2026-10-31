# mpv / ffmpeg notes

Read before touching `graph.rs`, `preview.rs`, `proxy.rs`, or the mpv client.

## mpv states

- `--keep-open=always` holds at EOF a frame or two before the probe duration;
  the pool clock snaps to `dur − 1/fps` at EOF.
- Pool preview stops at the reachable source: a clip head previews
  `[0, aout]`, not the whole asset.

## Preview proxy cache

- The monitor proxies whatever `edl://` can't express, rendered by the
  *export* graph (so there is no second filter chain to drift) and cached
  under `.nile/<stem>/cache/proxy`:
  - a *segment* (`preview::needs_proxy`): an effect, a non-1× retime, a
    transition, or a picture size that isn't the sequence's (mpv re-frames a
    raw file to *its* aspect while the export scales it, so the framing would
    jump at the seam); hashed by the segment + geometry + fps + the assets it
    reads;
  - a *span* where another track is audible (`proxy::composite_proxy`): every
    unmuted track's slice of it, hashed the same way over the slices.
- Each asset's **path and mtime** are in the hash, so a replaced file
  re-renders — and so is each **LUT file** an effect reads (the segment JSON
  carries only its path, so editing a `.cube` in place must re-render too).
- The render is `export_quiet`: the monitor calls it, so ffmpeg must never
  write to the TUI's stderr.
- A composite span is widened to contain a transition whole and the slice
  keeps it whole — a partial crossfade isn't expressible in one `edl://`
  entry.
- The hash includes `RENDER_VERSION` (`proxy.rs`): bump it when the *meaning*
  of a segment's JSON changes (a filter rewrite), or stale renders are served.

## Filter-graph gotchas

- `crop`/`scale` are masks: they keep the canvas and pad `black@0` on
  `yuva420p`, so the export overlay (and the monitor's composite, gated by
  `Effect::masks`) reveals the track below; the single-track export drops to
  black. Opaque padding silently breaks "reveal below".

- Export retimes with `setpts=(PTS-STARTPTS)/speed` + `atempo` (chained
  outside [0.5, 2]); the 1× graph stays byte-identical.
- `overlay`'s `format` option accepts only `yuv420/yuv422/yuv444/rgb/gbrp/
  auto` — `yuva420p` is rejected as invalid, but the overlay's alpha plane
  still drives compositing (the top layer's transparency works anyway).
- A helper that emits a filter chain must push its output label too: the
  multi-track audio pad emitted its `anullsrc…` chain but forgot the label,
  and ffmpeg reported a filter with an unconnected pad.
