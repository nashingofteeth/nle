# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status

**M4 + EDL/OTIO import + clip speed complete — M5 next.** The TUI drives
the model directly: trim in/out, split, delete, yank/paste, insert gap,
**extend mode**, a **media pool** with file preview, **multi-track editing**
(track rows, active track, overlay video + mixed audio), and undo/redo —
auto-saved, instant preview. The CLI can also **import a CMX3600 EDL or an
OpenTimelineIO (`.otio`) timeline**, set a **clip's playback speed**, and
**detect the cuts in a rendered file** (`detect-edits`).
Full roadmap and current status: `PLAN.md`.

## Usage

```sh
cargo build --release
N=./target/release/nle        # or just `nle` if cargo-installed

# Headless build (no TUI/mpv, CLI only — cargo feature `tui` off):
#   cargo build --release --no-default-features

# Interactive editor (the point)
$N edit                       # TUI + mpv monitor; vim keys:
#   transport: space play/pause · h/l ±1s · H/L ±5s · f/F frame step
#   motion:    j/k next/prev cut · w/b next/prev clip · n/N next/prev gap
#              gg/G/0 ends · < / > (, / .) clip start/end · 3l = 3s (counts)
#   editing:   i/o trim in/out (I/O ripple; a gap always ripples) ·
#              at a segment's edge i/o delete it, I/O or a gap ripples ·
#              s split at playhead · _ insert gap (count = frames) ·
#              dd delete (gap) · D delete + ripple · yy yank clip/gap ·
#              p paste-over (replaces underneath) · P ripple-insert ·
#              u / Ctrl-r undo/redo (auto-saved)
#   tracks:    t/T cycle the active track · <N>t jump to track N ·
#              :track add video|audio · :track mute [N] · :track rm N ·
#              navigation, edits, and the monitor all follow the active
#              track (the timeline draws one row per track)
#   extend:    e/E enter at a boundary — a gap on the right is preferred
#              (E = ripple) · [ / ] enter on the right / left segment of
#              a cut (bracket opens toward its side) ·
#              navigate to the target (mpv shows the armed material) ·
#              e commit (replaces what it crosses) · E flip ripple ·
#              [ ] switch side · < > (, .) switch end in-mode ·
#              gaps arm at either edge: the tail extends right and the
#              head reaches back without bound (past time 0 too) — e
#              overwrites the neighbour, E inserts space · a clip at the
#              start can prepend its head material past time 0 · the
#              status shows the projected timeline length · Esc cancel
#   misc:      m media pool — takes over the info pane: asset list
#              (j/k, { }) + probe metadata; the monitor previews the file
#              (space play, h/l ±1s, f/F frame, gg/0 start, G/$ end);
#              enter inserts it, d removes, u undoes ·
#              r reload project · :q quit · q quit
# Tip: everything is undoable; the project file is written after every edit.
# The bottom "keys" panel always shows the current mode's cheat-sheet and the
# latest notice.

# Project workflow
$N init
$N import intro.mp4 b-roll.mov
$N add intro.mp4 --in 2.5 --out 7      # trimmed clip at end of track
$N add b-roll.mov --at 10              # creates a gap before it
$N track add video                     # a second (overlay) video track
$N add b-roll.mov --track 1 --at 3     # place on that track
$N track add audio                     # an audio track; mix on export
$N track ls                            # list tracks (index, kind, mute)
$N show                                # or --json for the raw EDL
$N remove 2                            # by clip id from `show`
$N media ls                            # pooled assets + usage
$N media rm 3                          # drop an unused asset
$N import-edl sequence.edl --media ~/footage   # CMX3600 → project
#   reels/clip names match files in --media; --fps, --src-offset, --map
#   REEL=PATH override; refuses to clobber without --force
$N import-otio sequence.otio            # OpenTimelineIO (.otio) → project
#   e.g. a Kdenlive "OpenTimelineIO Export"; media paths come from the file
#   itself (--media/--remap for moved media). Kdenlive rate conforms
#   (timewarp) import as per-clip speed.
$N speed 3 0.5                          # clip #3 plays at half speed (ripples)
$N detect-edits edit.mov               # rebuild a project from a rendered file
$N detect-edits edit.mov --list        # print the detected cuts, write nothing
$N detect-edits edit.mov --append      # append the shots to an existing project
#   --append starts at the timeline end; --track N picks the video target
#   one clip per detected shot; black spans come back as gaps. Tune with
#   --threshold (0..1) / --min-shot; --video-only drops audio; --force
#   overwrites the project. Cuts snap to the project's frame grid (a 59.94
#   source in a 24 fps project lands on 24 fps frames). Recovers the edit
#   structure, not the sources.
$N export -o final.mp4                 # libx264, frame-exact
$N export -o fast.mp4 --encoder vaapi  # GPU (also: qsv)

# One-shot tools (no project needed)
$N probe clip.mp4 [--json]
$N trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder vaapi] [--copy]
$N edl                                 # debug: print the preview EDL URL

# Debug log: with NLE_LOG set, every nle command appends a timestamped trail
# (TUI notices, mpv errors, project saves, ffmpeg runs). Unset = silent.
NLE_LOG=/tmp/nle.log $N edit            # or: NLE_LOG=/tmp/nle.log $N export -o out.mp4
```

Times accept plain seconds (`12.5`) or timecodes (`0:01:23.500`).
The project file (`nle.json`) is plain JSON — human-readable, vim-editable,
and drift-free.

## Export presets (planned, M6)

See `PLAN.md` — `preview` (VAAPI), `balanced` (x264), `max` (x265/SVT-AV1).
