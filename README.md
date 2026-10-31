# nile

a terminal-native NLE

nile is a terminal-native video editor: a TUI dashboard, an mpv monitor,
vim-style keys, and FFmpeg underneath. The Rust process stays light while
ffmpeg and mpv do the decoding, rendering, and playback.

The project is **nile**; the installed command is **nle**.

## Features

- **A real timeline in the terminal** — trim, split, gap, delete, yank/paste,
  multi-track video and audio, transitions, and per-clip effects.
- **Vim-style editing** — counts, cuts/gaps navigation, extend mode, undo/redo,
  a command line, and a media pool with live file preview.
- **Fast previews** — mpv plays the composite; effects, speed changes,
  transitions, and mixed/overlaid spans are rendered once into a proxy cache.
- **One action catalog** — TUI keys, the `:` line, and the CLI share one model,
  one undo history, one playhead, and one yank register.
- **Interoperability** — CMX3600 EDL and OpenTimelineIO import, cut detection
  from a rendered file, frame-exact export.
- **Small dependency set** — the Rust side is a single native binary; the heavy
  media work is delegated to ffmpeg/ffprobe/mpv.

Roadmap and milestone history: [PLAN.md](PLAN.md).

## Requirements

- Rust with edition 2024 support
- `clang` and `lld` for linking (see `.cargo/config.toml`)
- **ffmpeg/ffprobe 9+** — the export passes its filter graph through
  `-/filter_complex`, which older ffmpeg (including Ubuntu 24.04's 6.x) lacks
- `mpv` for the interactive monitor

## Build and install

```sh
cargo build --release
./target/release/nle --help

# Install the `nle` command
cargo install --path crates/nle-cli

# Headless CLI only (no TUI/mpv; cargo feature `tui` off)
cargo build --release --no-default-features
```

## Quick start

```sh
nle init
nle import intro.mp4 b-roll.mov
nle add intro.mp4 --in 2.5 --out 7      # trimmed clip at the end of track 1
nle add b-roll.mov --at 10              # creates a gap before it
nle edit                                # TUI + mpv monitor
nle export -o final.mp4
```

The project lives in `nle.json`; edits are saved automatically. Shell commands
and an open TUI adopt each other's changes.

## TUI

Run `nle edit`. The timeline panel shows one row per track; navigation, edits,
and the monitor follow the **active track**.

### Transport and motion

| Key | Action |
|---|---|
| `space` | Play/pause |
| `h` / `l` | Seek −1 s / +1 s |
| `H` / `L` | Seek −5 s / +5 s |
| `f` / `F` | Step one frame |
| `j` / `k` | Next/previous cut |
| `w` / `b` | Next/previous clip start |
| `n` / `N` | Next/previous gap |
| `gg` / `G` / `0` | Start / end / start |
| `<` `>` (`,` `.`) | Clip start / clip end |
| `3l`, `5j`, … | Vim counts: seek 3 s, jump 5 cuts |

### Editing

| Key | Action |
|---|---|
| `i` / `o` | Trim in/out; uppercase (`I`/`O`) ripples |
| `s` | Split at the playhead |
| `_` | Insert a gap (count = frames) |
| `dd` / `D` | Delete (leave a gap) / delete and ripple |
| `yy` | Yank clip or gap into the shared register |
| `p` / `P` | Paste-over / ripple-insert |
| `u` / `Ctrl-r` | Undo / redo |
| `t` / `T`, `<N>t` | Cycle tracks / jump to track N |

### Effects, transitions, and the pool

| Key | Action |
|---|---|
| `x` | Effects panel for the clip under the playhead |
| `X` / `xx` | Cross-dissolve at the boundary (count = frames) |
| `x` / `X` on a blend | Open its length form |
| `r` on a blend | `:transition --percent` (50 = twice as long) |
| `m` | Open/close the media pool |
| `e` / `E` | Enter extend mode (replace / ripple) |
| `[` / `]` | Enter extend mode on the right/left side of a cut |
| `r` | Speed command prompt (100 = normal) |
| `R` | Reload the project from disk |

**Effects panel.** `j`/`k` select, `1`–`9` edit that row, `Enter` edits, `d`
drops, `D` clears. Add crop/scale/eq/offset with `c`/`s`/`e`/`o`; LUTs are
`:effect lut` only for now. A blank field keeps the applied value (shown as
“was N”); a letter jumps to its field, Tab/↑↓ move, Enter applies, Ctrl-U
clears, Esc cancels. In scale, `a` or the aspect-lock row toggles locking
(default on, remembered). One effect per kind: adding a kind again edits it.

**Transition form.** Inside a blend, `x` or `X` opens the frames field;
typing a count pre-fills it. `r` opens the rate command. A transition is part
of both neighbours, so trimming/extending nearby moves the blend’s edge.

**Media pool.** `j`/`k` (and `{`/`}`) move; `space` plays/restarts the asset;
`h`/`l`, `f`/`F`, `gg`/`0`, `G`/`$` seek; `enter` inserts at the timeline
cursor and returns; `d` removes the asset; `i` imports; `u` undoes. While the
pool is open, the timeline row and monitor show the selected asset; reopening
returns to the same asset and preview clock.

### Extend mode

`e`/`E` arms the boundary under the playhead — a gap on the right is preferred.
`[`/`]` arm the right/left side of a cut; either bracket at a transition arms
the neighbour’s real edge. Navigate to the target (the monitor shows the armed
material), then `e` commits (replaces what it crosses) or `E` inserts space.
`<`/`>` switch ends, `[`/`]` switch sides, Esc cancels.

### Command line

The `:` line is the CLI grammar, flag for flag:

```text
:trim-in|out [--ripple]       :split
:gap [--frames N|--dur T]     :delete|:yank [--at T|--clip N|--seg N]
:paste [--ripple]             :extend --edge in|out --to T [--ripple]
:speed PERCENT                :transition [dissolve|fade] [--frames N|--dur T|--percent P]
:effect [--clip N] [--index N] crop|scale|offset|eq|lut …
:effect rm [--index N]        :effect clear
:undo / :redo                 :purge
:track add video|audio        :track mute [N] / :track rm N
:media ls|rm                  :cache prune [--days N] [--dry-run]
:import / :add                :show / :seek T
:export -o FILE               :detect FILE
:edl-import / :otio-import    :help / :q
```

`--at` defaults to the playhead, `--track` to the active track, and `--clip`
to the clip under the playhead. `:help` lists the current mode’s keys.

### Status row

The crocodile reacts to edits: chomp delete, closed-mouth gulp insert, spit
undo and reverse-spit redo, snap split, nibble trim, dash speed, wink effects,
slither transitions, and hiss errors. It blinks when idle and swims while
playing; pausing returns it to idle, and muting closes its eyes.

While the monitor rebuilds, the status row shows a spinner and
`proxies 2/5 · 43%`; the monitor keeps the previous frame until the new proxy
is ready.

### Diagnostics

```sh
nle edit --dump [--dump-size 100x30] [--dump-keys "llx"]
```

Renders the TUI once without mpv, feeds the keys (`\n` = Enter, `\e` = Esc),
and prints the model state plus a grid with a column ruler. Read-only: it runs
on a copy and never changes the project.

## Editing rules

- Tracks are 1-based. `--track N` addresses position-based targets; `--clip N`
  finds the clip’s own track.
- Trimming at a segment’s edge deletes the segment that edge would consume:
  `i`/`I` removes the segment ending at the playhead, `o`/`O` the one starting
  there. This is deliberate.
- A trim beside a transition moves the blend’s near edge, keeping the timeline
  length. Inside a blend, `i`/`o` trim the neighbouring clip’s portion. A
  transition’s own edge is not a neighbour’s edge.
- A blend is opaque, video-only, and part of both neighbours. A clip↔clip
  transition shortens the timeline by its duration; a fade to black does not.
  Retiming (`--frames`, `--dur`, `--percent`) scales each side’s span, never
  its rate. Details: `docs/pitfalls/transitions.md`.
- Crop insets percentages toward the opposite edge (0 = edge, 100 = opposite;
  the two sides of an axis must total under 100). Scale is a percentage of the
  picture at that point (100 = unchanged); scaling down pads transparently.
  Offset moves a percentage of the way to the edge (100 = fully off-screen).
  Effects compose in the order added; crop resolves after scale.
- LUT effects are CLI-only in the TUI for now: `:effect lut --path FILE`.

## CLI

Every command has `--help`. Times accept seconds (`12.5`) or timecodes
(`0:01:23.500`).

### Project and media

```sh
nle init
nle import intro.mp4 b-roll.mov
nle add intro.mp4 --in 2.5 --out 7       # auto-imports a new file
nle add b-roll.mov --at 10 --track 2     # tracks are 1-based
nle track add video                      # overlay track
nle track add audio                      # mixed on export
nle track ls
nle show                                 # or --json
nle remove 2                             # by clip id
nle media ls
nle media rm 3                           # only unused assets
nle seek 12.5                            # shared playhead
```

### Timeline edits

```sh
nle split --at 3
nle trim-in --at 2 --ripple
nle trim-out --at 6
nle gap --at 4 --frames 30
nle delete --at 5
nle delete --clip 3 --ripple
nle delete --seg 2
nle yank --clip 3
nle paste --at 5 --ripple
nle extend --clip 3 --edge out --to 8
nle speed 3 50                           # clip #3 at 50%
nle transition --at 3 --frames 20
nle undo / nle redo
nle purge-stumps
```

`--at` is the playhead in the TUI; `--to` may be negative (a gap head reaching
back past time 0). `nle show` prints `[i]` segment indices and `#n` clip ids.

### Effects

```sh
nle effect crop --clip 3 --left 10 --right 10 --top 5 --bottom 5
nle effect scale --clip 3 --w 50 --h 50
nle effect offset --clip 3 --x 10 --y -5
nle effect eq --clip 3 --brightness -0.2  # contrast/saturation default to 1
nle effect eq --clip 3 --index 0          # replace row 0 (0-based)
nle effect rm --clip 3 [--index N]
nle effect clear --clip 3
```

Effects are rendered at export; the monitor plays a cached proxy for clips
with effects, speed changes, transitions, or a picture size that differs from
the sequence.

### Import, detect, and export

```sh
nle import-edl sequence.edl --media ~/footage
# Cuts only: dissolves/wipes/keys become cuts (each reported); black becomes
# a gap; zero-length events are skipped. --fps/--src-offset/--map/--force.
nle import-otio sequence.otio
# OpenTimelineIO e.g. a Kdenlive export; media paths come from the file;
# --media/--remap/--fps. Kdenlive timewarps import as clip speed.
nle detect-edits edit.mov [--list]
nle detect-edits edit.mov --append --track 2
# One clip per detected shot; black spans become gaps. Tune with
# --threshold/--min-shot/--black-min/--no-gaps/--video-only/--fps/--force.
nle export -o final.mp4
nle export -o fast.mp4 --encoder vaapi   # or qsv
# VAAPI picks the first /dev/dri/renderD*; override with NLE_VAAPI_DEVICE.
```

### One-shot tools and logging

```sh
nle probe clip.mp4 [--json]
nle trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder vaapi] [--copy]
nle edl                                  # composite URL the monitor plays
nle edl --track N                        # one track (debugging)

NLE_LOG=/tmp/nle.log nle edit            # timestamped diagnostics
```

## Files and concurrency

- `nle.json` — plain JSON project (rational times, no float drift)
- `nle.history.json` — undo/redo stacks
- `nle.clipboard.json` — yank register
- `~/.cache/nle/` — proxy renders and the black-gap filler

The TUI watches the project, history, and register; a shell edit or seek is
adopted within a tick and is never overwritten unseen. Reloads wait while a
mode holds indices (extend, pool, `:` line), then apply in Normal mode.

## One action catalog, three front-ends

Every timeline edit is one `Action` in `nle-core`, executed by one runner:

| Front-end | For | Example |
|---|---|---|
| TUI keys | frequent actions | `s`, `dd`, `p`, `e`/`E`, `u` |
| TUI `:` line | every action | `:delete --at 3 --ripple` |
| CLI | every action | `nle delete --at 3 --ripple` |

Edits made in either front-end share undo history, the playhead, and the yank
register. Commands that need processes (`:export`, `:detect`, `:edl-import`,
`:otio-import`, `:add`) run in a worker while the monitor keeps playing.

## Compatibility

Early software: project and history formats may change. Old files are rejected
with a clear error rather than silently misread; rebuild them with
`./demo.sh`. Project files are local state, not a distributed interchange
format.

## License

MIT — see [`LICENSE`](LICENSE).
