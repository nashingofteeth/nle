# nle

Terminal-native video editor: TUI dashboard + mpv monitor window, vim keys,
FFmpeg underneath. Built for this machine only.

## Status

**M4 + EDL/OTIO import + clip speed + edit-detect + M5 (transitions,
per-clip effects, proxy preview cache, composited preview) complete — M6
next.** The
TUI drives the model directly: trim in/out, split, delete, yank/paste, insert
gap, **extend mode**, a **media pool** with file preview, **multi-track
editing** (track rows, active track, overlay video + mixed audio), and
undo/redo — auto-saved, instant preview. The CLI can also **import a CMX3600
EDL or an OpenTimelineIO (`.otio`) timeline**, set a **clip's playback
speed**, **detect the cuts in a rendered file** (`detect-edits`), and **purge
sub-frame stumps** (`purge-stumps`). **Every action is reachable from both
sides** — the TUI's `:cmd` line and the CLI share one grammar and one undo
history (see "One action catalog, three front-ends"). Full roadmap and
current status: `PLAN.md`.

## Usage

```sh
cargo build --release
N=./target/release/nle        # or just `nle` if cargo-installed

# Headless build (no TUI/mpv, CLI only — cargo feature `tui` off):
#   cargo build --release --no-default-features

# Interactive editor (the point)
$N edit                       # TUI + mpv monitor; vim keys:
#   diagnostics: `nle edit --dump [--dump-size 100x30] [--dump-keys "llx"]`
#              renders the TUI once (no mpv), feeds the keys, and prints the
#              state + grid with a column ruler — for reporting a rendering or
#              playhead question without a screenshot. Read-only: it runs on a
#              copy, so a dump never changes the project it diagnoses
#   transport: space play/pause · h/l ±1s · H/L ±5s · f/F frame step
#   motion:    j/k next/prev cut · w/b next/prev clip · n/N next/prev gap
#              gg/G/0 ends · < / > (, / .) clip start/end · 3l = 3s (counts)
#   editing:   i/o trim in/out (I/O ripple; a gap always ripples) ·
#              at a segment's edge i/o delete it, I/O or a gap ripples —
#              i/I deletes the segment ENDING at the playhead, o/O the one
#              STARTING there (at a shared cut: the preceding / following clip),
#              so i on a clip's start deletes the preceding gap; a transition's
#              edge is not a neighbour's edge, so i/o there are no-ops ·
#              inside a transition i/o trim the neighbouring clip's portion in
#              the blend — a transition is part of both neighbours, so `o`
#              works the preceding clip's out and `i` the following clip's in ·
#              a trim on a clip *beside* a transition moves the blend's near
#              edge instead of leaving a gap: the trimmed span goes into the
#              transition and the timeline keeps its length (I/O ripple the
#              side instead); refused if the neighbour has no material there ·
#              s split at playhead · _ insert gap (count = frames) ·
#              dd delete (gap) · D delete + ripple · yy yank clip/gap ·
#              p paste-over (replaces underneath) · P ripple-insert ·
#              u / Ctrl-r undo/redo (auto-saved)
#   tracks:    t/T cycle the active track · <N>t jump to track N ·
#              :track add video|audio · :track mute [N] · :track rm N ·
#              navigation, edits, and the monitor all follow the active
#              track (the timeline draws one row per track)
#   fx:        x effects panel for the clip under the playhead — j/k select ·
#              1-9 edit that row directly · enter edit the selected effect (a
#              blank field keeps the applied value, shown as "was N") · d drop ·
#              D clear · c/s/e/l/o add
#              crop/scale/eq/lut/offset; a kind already on the clip opens for
#              editing instead of stacking (one effect per kind) · the form's
#              fields: a letter jumps to its field, tab/↑↓ move, enter applies
#              and closes, ctrl-u clears, esc cancels; in scale, tab to the
#              "aspect lock" row (or press `a`) and space toggles it — on by
#              default and remembered for later scale forms · u undo ·
#              q/esc close; `*`
#              marks a clip with effects in the row and `nle show` lists them
#              in order ·
#              X cross-dissolve at the boundary (count = frames); `xx` also
#              works — the first x opens the panel, the second applies it and
#              closes ·
#              :transition [dissolve|fade] [--dur T|--frames N] ·
#              :effect [--clip N] crop|scale|offset|eq|lut … · :effect rm [--index N] ·
#              :effect clear (one effect per kind: re-adding a kind replaces
#              it in place) · rendered at export;
#              the monitor plays a rendered proxy for effects/speed/transitions,
#              and for any clip whose picture size isn't the sequence's ·
#              dd/D removes one, giving the frames it trimmed back ·
#              a blend is part of both neighbours: extending one moves its
#              portion in the transition, and extend mode hops over it
#   commands:  the :cmd line is the CLI grammar, flag for flag — every
#              action has both spellings:
#              :trim-in|out [--ripple] · :split · :gap [--frames N|--dur T] ·
#              :delete|--yank [--at T|--clip N|--seg N] · :paste [--ripple] ·
#              :extend --edge in|out --to T [--ripple] · :speed RATE (or R) ·
#              :transition · :effect [rm|clear] · :undo · :redo · :purge · :track … ·
#              :media ls|rm · :cache prune [--days N] [--dry-run] · :import ·
#              :add · :show · :seek T · :export -o FILE · :detect FILE ·
#              :edl-import · :otio-import · :help · :q quit
#              (--at defaults to the playhead, --track to the active track)
#   extend:    e/E enter at a boundary — a gap on the right is preferred
#              (E = ripple) · [ / ] enter on the right / left segment of
#              a cut (bracket opens toward its side) · at a transition either
#              bracket arms the neighbour's real edge (the blend's far end) ·
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
#              enter inserts it, i imports a file, d removes, u undoes ·
#              r reload project · : cmd (:help lists them) · :q quit · q quit
# Tip: everything is undoable; the project file is written after every edit.
# The bottom "keys" panel always shows the current mode's cheat-sheet and the
# latest notice.
# While the monitor rebuilds after an edit, the status row shows a spinner and
# "proxies 2/5 · 43%" — proxy renders run in the background, so the TUI stays
# responsive (the monitor keeps the previous frame until the new one is ready).
# The crocodile at the left of the status row opens its mouth when an edit
# removes material from the timeline, then closes again.

# Project workflow
$N init
$N import intro.mp4 b-roll.mov
$N add intro.mp4 --in 2.5 --out 7      # trimmed clip at end of track
$N add b-roll.mov --at 10              # creates a gap before it
$N track add video                     # a second (overlay) video track
$N add b-roll.mov --track 2 --at 3     # place on that track (tracks are 1-based)
$N track add audio                     # an audio track; mix on export
$N track ls                            # list tracks (index, kind, mute)
$N show                                # or --json for the raw EDL
$N remove 2                            # by clip id from `show`
$N media ls                            # pooled assets + usage
$N media rm 3                          # drop an unused asset
$N import-edl sequence.edl --media ~/footage   # CMX3600 → project
#   reels/clip names match files in --media; --fps, --src-offset, --map
#   REEL=PATH override; refuses to clobber without --force
#   cuts only: a dissolve/wipe/key imports as a cut (each is reported), a
#   black reel (BL) becomes a gap, a zero-length event is skipped
$N import-otio sequence.otio            # OpenTimelineIO (.otio) → project
#   e.g. a Kdenlive "OpenTimelineIO Export"; media paths come from the file
#   itself (--media/--remap for moved media; --fps forces the rate). Kdenlive
#   rate conforms (timewarp) import as per-clip speed.
$N speed 3 0.5                          # clip #3 at half speed (--track N, 1-based)
$N purge-stumps                        # drop sub-frame stumps; snap to the grid
#   --track N cleans just that track (1-based).

# Timeline edits from the shell — the same actions as the TUI's keys,
# addressed explicitly (see `nle show` for clip ids and [seg] indices):
$N split --at 3                        # cut at 3 s (TUI: s)
$N trim-in --at 2 --ripple             # move the in-edge to 2, close up (TUI: I)
$N trim-out --at 6                     # move the out-edge (TUI: o / O --ripple)
$N gap --at 4 --frames 30              # insert 30 frames of gap (TUI: _)
$N delete --at 5                       # leave a gap behind (TUI: dd)
$N delete --clip 3 --ripple            # by clip id, close up (TUI: D)
$N delete --seg 2                      # by segment index from `nle show`
$N yank --clip 3                       # into the shared register (TUI: yy)
$N paste --at 5 --ripple               # from the register (TUI: p / P --ripple)
$N extend --clip 3 --edge out --to 8   # move one edge (the extend-mode commit)
$N extend --seg 4 --edge in --to 2 --ripple
$N effect crop --clip 3 --left 10 --right 10 --top 5 --bottom 5
#   each edge is a percentage toward the opposite side: 0 = the frame edge,
#   100 = the other edge; the two sides of an axis must total under 100.
#   It insets the picture at that point (after any earlier scale) and leaves
#   the cut strips transparent: the track below shows through, else black.
$N effect scale --clip 3 --w 50 --h 50 # percent of the picture at this point
#   100 = default. Scaling down pads transparently, so the track below shows
#   through; scaling up is a centre zoom. Effects compose in the order added.
$N effect offset --clip 3 --x 10 --y -5 # percent of the way to the edge
#   0 = no move; 100 moves it fully off-screen (transparent — the track
#   below shows), 80 leaves the rest of the picture on screen. Negative
#   moves left/up.
$N effect eq --clip 3 --brightness -0.2 # contrast/saturation default to 1
$N effect rm --clip 3                  # drop the last effect (--index N picks)
$N effect clear --clip 3               # drop them all (one undo step)
$N undo                                # works on TUI edits too; redo likewise
$N show                                # [i] = --seg index, #n = --clip id
#   --track N (1-based) picks the track for position-based commands;
#   --clip N finds its own track. Times are seconds or timecodes; `--to`
#   may be negative (a gap head reaching back past time 0).
$N seek 12.5                           # park the shared playhead (or print it)
#   Stored in the project, snapped to the frame grid and clamped into the
#   timeline. The TUI opens here; `nle seek` alone prints it. `nle show`
#   ends its summary with `at N s`.
$N detect-edits edit.mov               # rebuild a project from a rendered file
$N detect-edits edit.mov --list        # print the detected cuts, write nothing
$N detect-edits edit.mov --append      # append the shots to an existing project
#   --append starts at the timeline end; --track N picks the video target
#   (1-based). One clip per detected shot; black spans come back as gaps.
#   Tune with --threshold (0..1) / --min-shot / --black-min; --no-gaps skips
#   gap recovery; --video-only drops audio; --fps forces the rate; --force
#   overwrites the project. Cuts snap to the project's frame grid (a 59.94
#   source in a 24 fps project lands on 24 fps frames). Recovers the edit
#   structure, not the sources.
$N export -o final.mp4                 # libx264, frame-exact
$N export -o fast.mp4 --encoder vaapi  # GPU (also: qsv)
#   VAAPI picks the first /dev/dri/renderD*; override with NLE_VAAPI_DEVICE.

# One-shot tools (no project needed)
$N probe clip.mp4 [--json]
$N trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder vaapi] [--copy]
$N edl                                 # the mpv EDL the monitor plays (composite)
$N edl --track N                       # …or one track (1-based; debugging)

# Debug log: with NLE_LOG set, every nle command appends a timestamped trail
# (TUI notices, mpv errors, project saves, ffmpeg runs). Unset = silent.
NLE_LOG=/tmp/nle.log $N edit            # or: NLE_LOG=/tmp/nle.log $N export -o out.mp4
```

Times accept plain seconds (`12.5`) or timecodes (`0:01:23.500`).
Track numbers are 1-based (`v1`, `a1`), matching `nle track ls` and the TUI.
The project file (`nle.json`) is plain JSON — human-readable, vim-editable,
and drift-free.

## One action catalog, three front-ends

Everything the timeline can do is one `Action` (in `nle-core`), executed
by one runner — so the three spellings can't drift apart:

| front-end | for | example |
|---|---|---|
| TUI keys | high-frequency actions | `s`, `dd`, `p`, `e`/`E`, `u` |
| TUI `:` line | every action | `:delete --at 3 --ripple` |
| CLI | every action | `nle delete --at 3 --ripple` |

The `:cmd` line takes the CLI's flags verbatim (with `--at` defaulting to
the playhead, `--track` to the active track, `--clip` to the clip under
the playhead). Edits record undo history on both sides: `nle undo` undoes
a TUI edit and vice versa (`nle.history.json`, both stacks). The yank
register is shared too (`nle.clipboard.json`): `nle yank` then `p` in the
TUI, or `yy` in the TUI then `nle paste` from a script. The **playhead** is
the third shared slot (`nle.json`'s `playhead`): `nle seek T` / `:seek T`
move it; the TUI opens there, re-saves it after every edit, and writes it
about a second after the transport stops moving (so a pause or a seek is
visible to the next shell command).

**Concurrent shell + TUI.** The TUI watches `nle.json`, the undo sidecar,
and the register: a shell command (`nle split`, `nle seek`, `nle yank`) is
adopted within a tick — you see it in the TUI without pressing anything —
and the TUI never writes over a change it hasn't seen (it adopts that
instead of clobbering it). The reload waits while a mode is active
(extend, the pool, a `:` line) because those hold indices into the
current state; it applies as soon as you're back in Normal.

Commands that need processes (probe, export, detect, EDL/OTIO import) are
CLI-only in the model sense, but the TUI spells them too (`:export`,
`:detect`, `:edl-import`, `:otio-import`, `:add`) — long ones run in a
worker while the monitor keeps playing.

## Export presets (planned, M6)

See `PLAN.md` — `preview` (VAAPI), `balanced` (x264), `max` (x265/SVT-AV1).
