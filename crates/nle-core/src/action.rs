//! The action catalog: one enum per user action, one executor, three
//! front-ends.
//!
//! Keybinds (TUI), `:cmd` lines (TUI), and CLI subcommands all build the
//! same [`Action`] values and run them through [`run_action`]. Adding a
//! variant is a compile error in every front-end that forgets it — the
//! parity guarantee is the exhaustive `match`, not a checklist.
//!
//! Purity line: `Action` covers everything that mutates a [`Project`] or
//! its undo [`History`] from model state alone. Commands that need
//! processes (probe, export, detect, EDL/OTIO import) stay in `nle-cli`
//! and speak to this module only through the values they construct.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::edit::{self, EditError, History};
use crate::extend::{self, Armed};
use crate::project::Project;
use crate::time::{Seconds, frame_of, grid_time, parse_signed_time, secs_f64};
use crate::timeline::{Segment, TrackKind};

/// What an action addresses: the segment under a timeline position, a
/// clip by id (stable across edits), or a segment by index (as printed
/// by `nle show`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    At(f64),
    Clip(u32),
    Segment(usize),
}

/// Which edge of a segment an extend works on. `In` is the left edge
/// (a clip's head, a gap's head), `Out` the right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edge {
    In,
    Out,
}

/// How long an inserted gap is. `Seconds` resolves against the project's
/// frame rate at run time (the `:cmd`/CLI spelling `--dur` can't know it
/// at parse time).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GapLen {
    Frames(i64),
    Seconds(f64),
}

/// Every project-mutating action, addressed explicitly (no hidden
/// playhead): the shell gets the same handle on the timeline the TUI has.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Move the in-edge of the segment under `at` (`i`; `ripple` = `I`).
    TrimIn {
        track: usize,
        at: f64,
        ripple: bool,
    },
    /// Move the out-edge (`o` / `O`).
    TrimOut {
        track: usize,
        at: f64,
        ripple: bool,
    },
    /// Cut at `at` (`s`).
    Split {
        track: usize,
        at: f64,
    },
    /// Insert a gap at `at` (`_`).
    InsertGap {
        track: usize,
        at: f64,
        len: GapLen,
    },
    /// Remove the addressed segment (`dd` = no ripple, `D` = ripple).
    Delete {
        track: usize,
        target: Target,
        ripple: bool,
    },
    /// Copy the addressed segment into the register (`yy`).
    Yank {
        track: usize,
        target: Target,
    },
    /// Paste the register at `at` (`p` overwrites, `P` ripples). An
    /// explicit `segment` (media-pool insert, `nle add`) skips the
    /// register.
    Paste {
        track: usize,
        at: f64,
        ripple: bool,
        segment: Option<Segment>,
    },
    /// Retime a clip; the track ripples to fit (`R`, `:speed`).
    SetSpeed {
        track: usize,
        clip: u32,
        rate: Seconds,
    },
    /// Move one edge to `to` — the extend-mode commit (`e`/`E`).
    Extend {
        track: usize,
        target: Target,
        edge: Edge,
        to: f64,
        ripple: bool,
    },
    /// Drop sub-frame stumps and snap to the frame grid (`:purge`).
    /// Re-baselines the undo stack.
    PurgeStumps {
        /// `None` = every track.
        track: Option<usize>,
    },
    /// Append a track (`:track add video|audio`).
    TrackAdd {
        kind: TrackKind,
    },
    /// Toggle a track's mute flag (`:track mute [N]`).
    TrackMute {
        track: usize,
    },
    /// Remove a track and its clips (`:track rm N`); re-baselines the
    /// undo stack because track indices shift.
    TrackRm {
        track: usize,
    },
    /// Remove an unused asset from the media pool (`nle media rm`);
    /// undoable like any edit.
    AssetRm {
        id: u32,
    },
    Undo,
    Redo,
}

impl Action {
    /// The catalog's slot number. Every variant gets one, and a new
    /// variant fails to compile here until it has one — which is what lets
    /// the parity tests prove that *both* other spellings (the `:cmd`
    /// grammar and the CLI) cover the whole catalog.
    pub fn slot(&self) -> usize {
        match self {
            Action::TrimIn { .. } => 0,
            Action::TrimOut { .. } => 1,
            Action::Split { .. } => 2,
            Action::InsertGap { .. } => 3,
            Action::Delete { .. } => 4,
            Action::Yank { .. } => 5,
            Action::Paste { .. } => 6,
            Action::SetSpeed { .. } => 7,
            Action::Extend { .. } => 8,
            Action::PurgeStumps { .. } => 9,
            Action::TrackAdd { .. } => 10,
            Action::TrackMute { .. } => 11,
            Action::TrackRm { .. } => 12,
            Action::AssetRm { .. } => 13,
            Action::Undo => 14,
            Action::Redo => 15,
        }
    }
}

/// How many slots the catalog has — the coverage tests assert exactly
/// this many are reachable from each spelling.
pub const SLOTS: usize = 16;

/// What running an action produced. `changed` tells the caller whether
/// the project needs persisting (a yank only writes the register).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub desc: String,
    pub changed: bool,
    /// Frame the playhead belongs on (undo/redo restore it); `None` keeps
    /// whatever anchoring the caller does.
    pub playhead: Option<i64>,
}

/// Defaults for the `:cmd` parser — the TUI supplies the playhead, the
/// active track, and the clip under the playhead so commands can omit
/// `--at`/`--track`/`--clip`. The CLI keeps every flag explicit.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseDefaults {
    pub at: Option<f64>,
    pub track: usize,
    pub clip: Option<u32>,
}

/// Why a `:cmd` line didn't parse. [`Unknown`](ParseError::Unknown) verbs
/// are not edit actions at all (a front-end may know them as its own);
/// [`Invalid`](ParseError::Invalid) means the verb is right and the
/// arguments are not.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    Unknown(String),
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Unknown(v) => write!(f, "unknown command: {v}"),
            ParseError::Invalid(m) => f.write_str(m),
        }
    }
}

/// Run `action`, recording undo history when it edits the timeline.
/// `from` is the timeline position the action is initiated from (the TUI
/// passes the playhead — or the armed edge in extend mode — so undo
/// returns there).
pub fn run_action(
    project: &mut Project,
    history: &mut History,
    register: &mut Option<Register>,
    action: &Action,
    from: f64,
) -> Result<Outcome, EditError> {
    match action {
        Action::TrimIn { track, at, ripple } => {
            let batch = edit::trim_at_on(project, *track, *at, false, *ripple)?;
            apply(project, history, batch, from)
        }
        Action::TrimOut { track, at, ripple } => {
            let batch = edit::trim_at_on(project, *track, *at, true, *ripple)?;
            apply(project, history, batch, from)
        }
        Action::Split { track, at } => {
            let batch = edit::split_on(project, *track, *at)?;
            apply(project, history, batch, from)
        }
        Action::InsertGap { track, at, len } => {
            let frames = match len {
                GapLen::Frames(n) => *n,
                GapLen::Seconds(s) => {
                    let fps = project.fps.ok_or(EditError::NoFps)?;
                    (s * secs_f64(&fps)).round() as i64
                }
            };
            let batch = edit::insert_gap_on(project, *track, *at, frames)?;
            apply(project, history, batch, from)
        }
        Action::Delete {
            track,
            target,
            ripple,
        } => {
            let ti = resolve_track(project, *track, *target)?;
            let batch = delete_batch(project, ti, *target, *ripple)?;
            apply(project, history, batch, from)
        }
        Action::Yank { track, target } => {
            let ti = resolve_track(project, *track, *target)?;
            let seg = segment_at(project, ti, *target)?;
            let desc = yank_desc(project, &seg);
            // Keep the asset's identity with it: ids don't survive a
            // rebuilt project, paths do.
            *register = Some(Register::of(project, seg));
            Ok(Outcome {
                desc,
                changed: false,
                playhead: None,
            })
        }
        Action::Paste {
            track,
            at,
            ripple,
            segment,
        } => {
            let seg = match segment {
                Some(s) => s.clone(),
                None => register
                    .as_ref()
                    .ok_or(EditError::RegisterEmpty)?
                    .resolve(project)?,
            };
            let batch = if *ripple {
                edit::paste_ripple_on(project, *track, &seg, *at)?
            } else {
                edit::paste_on(project, *track, &seg, *at)?
            };
            apply(project, history, batch, from)
        }
        Action::SetSpeed { track, clip, rate } => {
            let batch = edit::set_speed_on(project, *track, *clip, *rate)?;
            apply(project, history, batch, from)
        }
        Action::Extend {
            track,
            target,
            edge,
            to,
            ripple,
        } => {
            let ti = resolve_track(project, *track, *target)?;
            let armed = arm(project, ti, *target, *edge)?;
            let edge_pos = extend::armed_edge(project, &armed, ti).map_err(EditError::Extend)?;
            let kind = extend::plan_commit(project, &armed, edge_pos, *to, ti)
                .map_err(EditError::Extend)?;
            let batch = extend::commit_batch(project, *to, &armed, kind, *ripple, ti)?;
            // Undo attribution: like an extend-mode entry, the edit is
            // made from the edge it moved — undo returns there.
            apply(project, history, batch, edge_pos)
        }
        Action::PurgeStumps { track } => purge_stumps(project, history, *track, from),
        Action::TrackAdd { kind } => {
            let playhead = frame_of(project.fps.as_ref(), from);
            let desc = history.edit_tracks(project, playhead, |p| {
                let idx = p.timeline.add_track(*kind);
                Ok(format!("added track {}", track_label(p, idx)))
            })?;
            Ok(Outcome {
                desc,
                changed: true,
                playhead: None,
            })
        }
        Action::TrackMute { track } => {
            let playhead = frame_of(project.fps.as_ref(), from);
            let desc = history.edit_tracks(project, playhead, |p| {
                let label = track_label(p, *track);
                let t = p
                    .timeline
                    .track_mut(*track)
                    .ok_or(EditError::TrackNotFound(*track))?;
                t.muted = !t.muted;
                let muted = t.muted;
                Ok(format!(
                    "{label} {}",
                    if muted { "muted" } else { "unmuted" }
                ))
            })?;
            Ok(Outcome {
                desc,
                changed: true,
                playhead: None,
            })
        }
        Action::TrackRm { track } => track_rm(project, history, *track, from),
        Action::AssetRm { id } => {
            let playhead = frame_of(project.fps.as_ref(), from);
            let desc = history.remove_asset(project, *id, playhead)?;
            Ok(Outcome {
                desc,
                changed: true,
                playhead: None,
            })
        }
        Action::Undo => {
            let o = history.undo(project)?;
            Ok(Outcome {
                desc: format!("undo: {}", o.desc),
                changed: true,
                playhead: Some(o.playhead),
            })
        }
        Action::Redo => {
            let o = history.redo(project)?;
            Ok(Outcome {
                desc: format!("redo: {}", o.desc),
                changed: true,
                playhead: Some(o.playhead),
            })
        }
    }
}

/// Apply a batch as one undo step (the playhead `from` is recorded so
/// undo returns there).
fn apply(
    project: &mut Project,
    history: &mut History,
    batch: edit::Batch,
    from: f64,
) -> Result<Outcome, EditError> {
    let desc = batch.desc.clone();
    let at_frame = frame_of(project.fps.as_ref(), from);
    history.apply(project, batch, at_frame)?;
    Ok(Outcome {
        desc,
        changed: true,
        playhead: None,
    })
}

/// The track an action's target lives on. Position and segment-index
/// targets use the action's own track; a clip id resolves wherever the
/// clip is (ids are unique across tracks), so `--clip N` needs no
/// `--track`.
fn resolve_track(project: &Project, track: usize, target: Target) -> Result<usize, EditError> {
    match target {
        Target::Clip(id) => project
            .timeline
            .tracks
            .iter()
            .position(|t| t.clip_index(id).is_some())
            .ok_or(EditError::ClipNotFound(id)),
        _ => {
            if project.timeline.track(track).is_none() {
                return Err(EditError::TrackNotFound(track));
            }
            Ok(track)
        }
    }
}

/// The delete op for a target: under a position, a clip by id, or a
/// segment by index.
fn delete_batch(
    project: &Project,
    track: usize,
    target: Target,
    ripple: bool,
) -> Result<edit::Batch, EditError> {
    match target {
        Target::At(t) => {
            if ripple {
                edit::delete_ripple_on(project, track, t)
            } else {
                edit::delete_under_on(project, track, t)
            }
        }
        Target::Clip(id) => {
            if ripple {
                edit::delete_clip_ripple_by_id_on(project, track, id)
            } else {
                edit::delete_clip_by_id_on(project, track, id)
            }
        }
        Target::Segment(index) => edit::delete_segment_on(project, track, index, ripple),
    }
}

/// The segment a target addresses, for yank/extend resolution.
fn segment_at(project: &Project, track: usize, target: Target) -> Result<Segment, EditError> {
    let (index, _) = segment_index_at(project, track, target)?;
    Ok(project.timeline.tracks[track].segments[index].clone())
}

/// The segment index a target addresses (`Target::At` resolves through
/// the frame grid, like every other position).
fn segment_index_at(
    project: &Project,
    track: usize,
    target: Target,
) -> Result<(usize, &Segment), EditError> {
    let t = project
        .timeline
        .track(track)
        .ok_or(EditError::TrackNotFound(track))?;
    let index = match target {
        Target::At(at) => t
            .index_at(grid_time(project.fps.as_ref(), at))
            .ok_or(EditError::NoSegment)?,
        Target::Clip(id) => t.clip_index(id).ok_or(EditError::ClipNotFound(id))?,
        Target::Segment(index) => {
            if index >= t.segments.len() {
                return Err(EditError::SegmentNotFound(index));
            }
            index
        }
    };
    Ok((index, &t.segments[index]))
}

/// Resolve a target + edge to the boundary an extend commits on. A gap
/// has no clip id: its head/tail are the two `Armed` gap forms.
fn arm(project: &Project, track: usize, target: Target, edge: Edge) -> Result<Armed, EditError> {
    let head = edge == Edge::In;
    if let Target::Clip(id) = target {
        return Ok(Armed::Clip { id, head });
    }
    let (index, seg) = segment_index_at(project, track, target)?;
    match seg {
        Segment::Clip(c) => Ok(Armed::Clip { id: c.id, head }),
        Segment::Gap(_) => Ok(if head {
            Armed::GapHead { index }
        } else {
            Armed::GapTail { index }
        }),
    }
}

/// Drop sub-frame stumps, clamp every clip to its media, and snap the
/// tracks onto the frame grid. Recorded as a whole-track-list change, so
/// undo restores exactly what the repair changed.
fn purge_stumps(
    project: &mut Project,
    history: &mut History,
    track: Option<usize>,
    from: f64,
) -> Result<Outcome, EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let indices: Vec<usize> = match track {
        Some(n) => {
            if project.timeline.track(n).is_none() {
                return Err(EditError::TrackNotFound(n));
            }
            vec![n]
        }
        None => (0..project.timeline.tracks.len()).collect(),
    };
    let durations: std::collections::HashMap<u32, Seconds> = project
        .media
        .iter()
        .filter_map(|a| a.duration().map(|d| (a.id, d)))
        .collect();
    let playhead = frame_of(project.fps.as_ref(), from);
    let desc = history.edit_tracks(project, playhead, |p| {
        let mut adjusted = 0usize;
        let mut dropped = 0usize;
        for ti in &indices {
            let t = p
                .timeline
                .track_mut(*ti)
                .ok_or(EditError::TrackNotFound(*ti))?;
            let media = |id: u32| durations.get(&id).copied();
            adjusted += edit::clamp_to_media(t, &media, fps).len();
            let (cleaned, n) = edit::purge_stumps(t, fps);
            t.segments = cleaned;
            dropped += n;
        }
        Ok(format!(
            "purged {dropped} sub-frame segment(s), adjusted {adjusted} clip(s)"
        ))
    })?;
    Ok(Outcome {
        desc,
        changed: true,
        playhead: None,
    })
}

/// Remove a track and its clips. The whole track list is recorded, so the
/// shifted indices are what undo restores — `nle undo` brings the track
/// (and its clips) back.
fn track_rm(
    project: &mut Project,
    history: &mut History,
    track: usize,
    from: f64,
) -> Result<Outcome, EditError> {
    let playhead = frame_of(project.fps.as_ref(), from);
    let desc = history.edit_tracks(project, playhead, |p| {
        let n = p.timeline.tracks.len();
        if track >= n {
            return Err(EditError::TrackNotFound(track));
        }
        let label = track_label(p, track);
        if p.timeline.tracks[track].kind == TrackKind::Video
            && p.timeline.video_tracks().count() <= 1
        {
            return Err(EditError::LastVideoTrack);
        }
        let removed = p.timeline.tracks.remove(track);
        let clips = removed
            .segments
            .iter()
            .filter(|s| matches!(s, Segment::Clip(_)))
            .count();
        Ok(if clips > 0 {
            format!("removed track {label} ({clips} clip(s) deleted)")
        } else {
            format!("removed track {label}")
        })
    })?;
    Ok(Outcome {
        desc,
        changed: true,
        playhead: None,
    })
}

/// Short label for a track: `v1..` for video, `a1..` for audio, numbered
/// within their kind by stacking order (matches the TUI gutter).
pub fn track_label(project: &Project, ti: usize) -> String {
    let (mut v, mut a) = (0, 0);
    for (i, t) in project.timeline.tracks.iter().enumerate() {
        match t.kind {
            TrackKind::Video => v += 1,
            TrackKind::Audio => a += 1,
        }
        if i == ti {
            return match t.kind {
                TrackKind::Video => format!("v{v}"),
                TrackKind::Audio => format!("a{a}"),
            };
        }
    }
    "—".to_string()
}

/// The register description shown on a yank.
fn yank_desc(project: &Project, seg: &Segment) -> String {
    match seg {
        Segment::Clip(c) => format!("yanked clip #{}", c.id),
        Segment::Gap(g) => match project.fps {
            Some(f) => format!(
                "yanked gap (f{})",
                (secs_f64(&g.duration) * secs_f64(&f)).round() as i64
            ),
            None => "yanked gap".to_string(),
        },
    }
}

// ------------------------------------------------------------ the register

/// Register sidecar format version. Bumped on a shape change; a skewed or
/// unreadable file reads as an empty register (the register is
/// convenience, not critical state — the project file is).
/// v2 carries the yanked clip's asset path.
pub const REGISTER_VERSION: u32 = 2;

/// A yanked segment plus, for a clip, the asset path it came from. Asset
/// ids are **project-scoped**, so a register that outlives its project
/// (a rebuilt `nle.json`, a copied sidecar) must be resolved against the
/// pool by path — pasting the wrong media silently would be worse than
/// refusing.
#[derive(Debug, Clone, PartialEq)]
pub struct Register {
    pub segment: Segment,
    pub path: Option<PathBuf>,
}

impl Register {
    /// The register entry for `segment`, resolving a clip's asset path
    /// from `project`.
    pub fn of(project: &Project, segment: Segment) -> Self {
        let path = match &segment {
            Segment::Clip(c) => project.asset(c.asset).map(|a| a.path.clone()),
            Segment::Gap(_) => None,
        };
        Self { segment, path }
    }

    /// Resolve against `project`: same file, possibly under a different
    /// asset id. Refuses when the media isn't in this pool at all.
    pub fn resolve(&self, project: &Project) -> Result<Segment, EditError> {
        let Segment::Clip(c) = &self.segment else {
            return Ok(self.segment.clone());
        };
        let Some(path) = &self.path else {
            return Ok(self.segment.clone());
        };
        if project.asset(c.asset).map(|a| &a.path) == Some(path) {
            return Ok(self.segment.clone()); // same id, same file
        }
        let Some(asset) = project.media.iter().find(|a| &a.path == path) else {
            return Err(EditError::RegisterForeign(path.display().to_string()));
        };
        let mut segment = self.segment.clone();
        if let Segment::Clip(clip) = &mut segment {
            clip.asset = asset.id;
        }
        Ok(segment)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterFile {
    version: u32,
    segment: Segment,
    #[serde(default)]
    path: Option<PathBuf>,
}

/// The register sidecar for a project (`nle.json` → `nle.clipboard.json`).
pub fn register_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("clipboard.json")
}

/// The yanked segment a project's register holds, if any.
pub fn load_register(project_path: &Path) -> Option<Register> {
    let text = std::fs::read_to_string(register_path(project_path)).ok()?;
    let file: RegisterFile = serde_json::from_str(&text).ok()?;
    (file.version == REGISTER_VERSION).then_some(Register {
        segment: file.segment,
        path: file.path,
    })
}

/// Persist the register; `None` clears it (the file goes away). Atomic,
/// like the project save — a crash can't leave half a register.
pub fn save_register(project_path: &Path, register: Option<&Register>) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = register_path(project_path);
    let Some(register) = register else {
        let _ = std::fs::remove_file(path);
        return Ok(());
    };
    let file = RegisterFile {
        version: REGISTER_VERSION,
        segment: register.segment.clone(),
        path: register.path.clone(),
    };
    let mut json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::other(format!("register serialize: {e}")))?;
    json.push('\n');
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

// ------------------------------------------------------------ parsing

/// Boolean options (everything else takes a value).
const BOOL_FLAGS: &[&str] = &["ripple"];

/// Short flags the CLI uses, spelled `-o` in the `:cmd` line too
/// (`:export -o out.mp4`). Only the ones that exist on the CLI.
const SHORT_FLAGS: &[(&str, &str)] = &[("o", "out")];

/// One command line: its verb, `--flag` options, and bare positional
/// words (`:speed 0.5`, `:track mute 2`). The grammar mirrors the CLI
/// flags — an agent that knows `nle delete --at 3 --ripple` knows
/// `:delete --at 3 --ripple`. Public so the front-ends' own (non-edit)
/// commands tokenize the same way.
pub struct Flags {
    opts: Vec<(String, Option<String>)>,
    pos: Vec<String>,
}

/// Split a command line into its verb and arguments.
pub fn split_args(input: &str) -> Result<(String, Flags), ParseError> {
    let mut it = input.split_whitespace();
    let verb = it
        .next()
        .ok_or_else(|| ParseError::Invalid("empty command".into()))?
        .to_lowercase();
    let raw: Vec<&str> = it.collect();
    let (mut opts, mut pos) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < raw.len() {
        let tok = raw[i];
        i += 1;
        let Some(name) = tok.strip_prefix("--") else {
            // `-o out.mp4`: a short flag with a known long form. Anything
            // else single-dashed is a positional (`--to -1` values never
            // reach here — they are consumed by their flag).
            if let Some(stripped) = tok.strip_prefix('-')
                && !stripped.is_empty()
                && !stripped.starts_with('-')
                && !stripped.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                let name = SHORT_FLAGS
                    .iter()
                    .find(|(s, _)| *s == stripped)
                    .map(|(_, long)| (*long).to_string())
                    .ok_or_else(|| ParseError::Invalid(format!("unknown option {tok:?}")))?;
                let v = raw
                    .get(i)
                    .ok_or_else(|| ParseError::Invalid(format!("{tok} needs a value")))?;
                i += 1;
                opts.push((name, Some((*v).to_string())));
                continue;
            }
            pos.push(tok.to_string());
            continue;
        };
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (name.to_string(), None),
        };
        if name.is_empty() {
            return Err(ParseError::Invalid(format!("bad option {tok:?}")));
        }
        let value = if BOOL_FLAGS.contains(&name.as_str()) {
            None
        } else {
            Some(match inline {
                Some(v) => v,
                None => {
                    let v = raw
                        .get(i)
                        .ok_or_else(|| ParseError::Invalid(format!("--{name} needs a value")))?;
                    i += 1;
                    (*v).to_string()
                }
            })
        };
        opts.push((name, value));
    }
    Ok((verb, Flags { opts, pos }))
}

impl Flags {
    pub fn take(&mut self, name: &str) -> Option<String> {
        let i = self.opts.iter().position(|(n, _)| n == name)?;
        self.opts.remove(i).1
    }

    /// Every occurrence of a repeatable flag (`--map A=B --map C=D`).
    pub fn take_all(&mut self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.opts.len() {
            if self.opts[i].0 == name {
                if let Some(v) = self.opts.remove(i).1 {
                    out.push(v);
                }
            } else {
                i += 1;
            }
        }
        out
    }

    pub fn take_bool(&mut self, name: &str) -> bool {
        match self.opts.iter().position(|(n, _)| n == name) {
            Some(i) => {
                self.opts.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn positional(&mut self) -> Option<String> {
        (!self.pos.is_empty()).then(|| self.pos.remove(0))
    }

    /// Error on anything unread — a typo'd flag must not vanish silently.
    pub fn finish(&self, usage: &str) -> Result<(), ParseError> {
        if let Some((n, _)) = self.opts.first() {
            return Err(ParseError::Invalid(format!(
                "unknown option --{n} — {usage}"
            )));
        }
        if let Some(w) = self.pos.first() {
            return Err(ParseError::Invalid(format!(
                "unexpected argument {w:?} — {usage}"
            )));
        }
        Ok(())
    }
}

fn opt_time(f: &mut Flags, name: &str) -> Result<Option<f64>, ParseError> {
    match f.take(name) {
        Some(v) => parse_pos(&v).map(Some),
        None => Ok(None),
    }
}

fn opt_usize(f: &mut Flags, name: &str) -> Result<Option<usize>, ParseError> {
    match f.take(name) {
        Some(v) => v
            .parse::<usize>()
            .map(Some)
            .map_err(|_| ParseError::Invalid(format!("--{name} expects a number, got {v:?}"))),
        None => Ok(None),
    }
}

fn opt_u32(f: &mut Flags, name: &str) -> Result<Option<u32>, ParseError> {
    match f.take(name) {
        Some(v) => v
            .parse::<u32>()
            .map(Some)
            .map_err(|_| ParseError::Invalid(format!("--{name} expects a number, got {v:?}"))),
        None => Ok(None),
    }
}

/// Seconds or timecode, optionally negative (a gap head reaches back
/// past time 0).
fn parse_pos(s: &str) -> Result<f64, ParseError> {
    parse_signed_time(s)
        .map(|t| secs_f64(&t))
        .map_err(|e| ParseError::Invalid(format!("invalid time {s:?} ({e})")))
}

/// `--at`/`--clip`/`--seg` — exactly one, falling back to the front-end's
/// defaults (`--at` → playhead, `--clip` → clip under it).
fn parse_target(f: &mut Flags, d: &ParseDefaults, usage: &str) -> Result<Target, ParseError> {
    let at = opt_time(f, "at")?;
    let clip = opt_u32(f, "clip")?;
    let seg = opt_usize(f, "seg")?;
    let given = [at.is_some(), clip.is_some(), seg.is_some()]
        .iter()
        .filter(|g| **g)
        .count();
    if given > 1 {
        return Err(ParseError::Invalid(format!(
            "use only one of --at/--clip/--seg — {usage}"
        )));
    }
    if let Some(t) = at {
        return Ok(Target::At(t));
    }
    if let Some(c) = clip {
        return Ok(Target::Clip(c));
    }
    if let Some(s) = seg {
        return Ok(Target::Segment(s));
    }
    if let Some(t) = d.at {
        return Ok(Target::At(t));
    }
    if let Some(c) = d.clip {
        return Ok(Target::Clip(c));
    }
    Err(ParseError::Invalid(format!(
        "no position given — pass --at T, --clip N, or --seg N ({usage})"
    )))
}

/// `--track N` (1-based, matching `nle track ls` and the v1/a1 labels),
/// defaulting to the front-end's active track.
fn parse_track(f: &mut Flags, d: &ParseDefaults) -> Result<usize, ParseError> {
    match opt_usize(f, "track")? {
        Some(n) => zero_based(n, "--track"),
        None => Ok(d.track),
    }
}

/// A 1-based CLI index → 0-based model index.
fn zero_based(n: usize, what: &str) -> Result<usize, ParseError> {
    if n >= 1 {
        Ok(n - 1)
    } else {
        Err(ParseError::Invalid(format!("{what} is 1-based, got {n}")))
    }
}

/// Parse one `:cmd` line into an [`Action`].
pub fn parse(input: &str, d: &ParseDefaults) -> Result<Action, ParseError> {
    let (verb, mut f) = split_args(input)?;
    let at = |f: &mut Flags, d: &ParseDefaults| -> Result<f64, ParseError> {
        Ok(opt_time(f, "at")?.unwrap_or(
            d.at.ok_or_else(|| ParseError::Invalid("no position given — pass --at T".to_string()))?,
        ))
    };
    match verb.as_str() {
        "trim-in" => {
            let (a, t, r) = (
                at(&mut f, d)?,
                parse_track(&mut f, d)?,
                f.take_bool("ripple"),
            );
            f.finish(":trim-in [--at T] [--track N] [--ripple]")?;
            Ok(Action::TrimIn {
                track: t,
                at: a,
                ripple: r,
            })
        }
        "trim-out" => {
            let (a, t, r) = (
                at(&mut f, d)?,
                parse_track(&mut f, d)?,
                f.take_bool("ripple"),
            );
            f.finish(":trim-out [--at T] [--track N] [--ripple]")?;
            Ok(Action::TrimOut {
                track: t,
                at: a,
                ripple: r,
            })
        }
        "split" => {
            let (a, t) = (at(&mut f, d)?, parse_track(&mut f, d)?);
            f.finish(":split [--at T] [--track N]")?;
            Ok(Action::Split { track: t, at: a })
        }
        "gap" => {
            let (a, t) = (at(&mut f, d)?, parse_track(&mut f, d)?);
            let len = match (opt_usize(&mut f, "frames")?, opt_time(&mut f, "dur")?) {
                (Some(_), Some(_)) => {
                    return Err(ParseError::Invalid(
                        "use either --frames N or --dur T".to_string(),
                    ));
                }
                (Some(n), None) => GapLen::Frames(n as i64),
                (None, Some(s)) => GapLen::Seconds(s),
                (None, None) => GapLen::Frames(1),
            };
            f.finish(":gap [--at T] [--frames N | --dur T] [--track N]")?;
            Ok(Action::InsertGap {
                track: t,
                at: a,
                len,
            })
        }
        "delete" | "del" => {
            let target = parse_target(&mut f, d, "what to delete")?;
            let (t, r) = (parse_track(&mut f, d)?, f.take_bool("ripple"));
            f.finish(":delete [--at T | --clip N | --seg N] [--track N] [--ripple]")?;
            Ok(Action::Delete {
                track: t,
                target,
                ripple: r,
            })
        }
        "yank" => {
            let target = parse_target(&mut f, d, "what to yank")?;
            let t = parse_track(&mut f, d)?;
            f.finish(":yank [--at T | --clip N | --seg N] [--track N]")?;
            Ok(Action::Yank { track: t, target })
        }
        "paste" => {
            let (a, t, r) = (
                at(&mut f, d)?,
                parse_track(&mut f, d)?,
                f.take_bool("ripple"),
            );
            f.finish(":paste [--at T] [--track N] [--ripple]")?;
            Ok(Action::Paste {
                track: t,
                at: a,
                ripple: r,
                segment: None,
            })
        }
        "speed" => {
            // `:speed 0.5` (the `R` prompt's spelling) or `:speed --rate 0.5 --clip 3`.
            let rate = f
                .positional()
                .or_else(|| f.take("rate"))
                .ok_or_else(|| ParseError::Invalid("usage: :speed RATE [--clip N]".to_string()))?;
            let rate = parse_rate(&rate)?;
            let clip = opt_u32(&mut f, "clip")?.or(d.clip).ok_or_else(|| {
                ParseError::Invalid(
                    ":speed addresses a clip — pass --clip N (or run it with the playhead on one)"
                        .to_string(),
                )
            })?;
            let track = parse_track(&mut f, d)?;
            f.finish(":speed RATE [--clip N] [--track N]")?;
            Ok(Action::SetSpeed { track, clip, rate })
        }
        "extend" => {
            let target = parse_target(&mut f, d, "which edge to move")?;
            let edge = match f.take("edge").as_deref() {
                Some("in") | Some("head") => Edge::In,
                Some("out") | Some("tail") => Edge::Out,
                Some(other) => {
                    return Err(ParseError::Invalid(format!(
                        "--edge expects in|out, got {other:?}"
                    )));
                }
                None => {
                    return Err(ParseError::Invalid(
                        "usage: :extend (--clip N | --at T | --seg N) --edge in|out --to T"
                            .to_string(),
                    ));
                }
            };
            let to = match f.take("to") {
                Some(v) => parse_pos(&v)?,
                None => return Err(ParseError::Invalid("extend needs --to T".to_string())),
            };
            let (t, r) = (parse_track(&mut f, d)?, f.take_bool("ripple"));
            f.finish(":extend … --edge in|out --to T [--track N] [--ripple]")?;
            Ok(Action::Extend {
                track: t,
                target,
                edge,
                to,
                ripple: r,
            })
        }
        "purge" | "clean" | "purge-stumps" => {
            let track = match opt_usize(&mut f, "track")? {
                Some(n) => Some(zero_based(n, "--track")?),
                None => None,
            };
            f.finish(":purge [--track N]")?;
            Ok(Action::PurgeStumps { track })
        }
        "undo" => {
            f.finish(":undo")?;
            Ok(Action::Undo)
        }
        "redo" => {
            f.finish(":redo")?;
            Ok(Action::Redo)
        }
        "media" => match f.positional().as_deref() {
            Some("rm") | Some("remove") => {
                let raw = f
                    .positional()
                    .ok_or_else(|| ParseError::Invalid("usage: :media rm ID".to_string()))?;
                let id = raw.parse::<u32>().map_err(|_| {
                    ParseError::Invalid(format!(":media rm expects an asset id, got {raw:?}"))
                })?;
                f.finish(":media rm ID")?;
                Ok(Action::AssetRm { id })
            }
            Some("ls") => Err(ParseError::Unknown("media ls".to_string())),
            _ => Err(ParseError::Invalid(
                "usage: :media ls · :media rm ID".to_string(),
            )),
        },
        "track" => {
            let sub = f
                .positional()
                .ok_or_else(|| ParseError::Invalid(TRACK_USAGE.to_string()))?;
            match sub.as_str() {
                "add" => {
                    let kind = match f.positional().as_deref() {
                        Some("video") => TrackKind::Video,
                        Some("audio") => TrackKind::Audio,
                        _ => {
                            return Err(ParseError::Invalid(
                                "usage: :track add video|audio".to_string(),
                            ));
                        }
                    };
                    f.finish(":track add video|audio")?;
                    Ok(Action::TrackAdd { kind })
                }
                "mute" => {
                    let track = match f.positional() {
                        Some(n) => zero_based(n.parse().unwrap_or(0), ":track mute [N]")?,
                        None => d.track,
                    };
                    f.finish(":track mute [N]")?;
                    Ok(Action::TrackMute { track })
                }
                "rm" | "remove" => {
                    let raw = f
                        .positional()
                        .ok_or_else(|| ParseError::Invalid(TRACK_USAGE.to_string()))?;
                    let n: usize = raw.parse().map_err(|_| {
                        ParseError::Invalid(format!(":track rm expects a number, got {raw:?}"))
                    })?;
                    f.finish(":track rm N")?;
                    Ok(Action::TrackRm {
                        track: zero_based(n, ":track rm N")?,
                    })
                }
                "ls" => Err(ParseError::Unknown("track ls".to_string())),
                other => Err(ParseError::Invalid(format!(
                    "unknown :track subcommand {other:?} — add · mute · rm · ls"
                ))),
            }
        }
        other => Err(ParseError::Unknown(other.to_string())),
    }
}

const TRACK_USAGE: &str =
    "usage: :track add video|audio · :track mute [N] · :track rm N · :track ls";

/// A speed rate: `n/d` or a positive decimal (`1`, `0.5`, `0.8`, `2`).
fn parse_rate(s: &str) -> Result<Seconds, ParseError> {
    let bad = || ParseError::Invalid(format!("invalid speed {s:?} (try 1, 0.5, 2, or 24/25)"));
    if let Some(r) = crate::time::parse_rate(s) {
        return Ok(r);
    }
    let v: f64 = s.parse().map_err(|_| bad())?;
    if !v.is_finite() || v <= 0.0 {
        return Err(ParseError::Invalid(format!(
            "speed must be positive (got {s:?})"
        )));
    }
    Seconds::approximate_float(v)
        .filter(|r| *r > Seconds::from_integer(0))
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::timeline::{Clip, Gap, TrackKind};

    fn defaults() -> ParseDefaults {
        ParseDefaults {
            at: Some(7.5),
            track: 5,
            clip: Some(42),
        }
    }

    /// Every variant must be reachable from the `:cmd` grammar *and*
    /// from the CLI — the parity tests prove it mechanically with
    /// `Action::slot`.
    #[test]
    fn every_action_has_a_colon_spelling() {
        const SPECS: &[&str] = &[
            "trim-in",
            "trim-out",
            "split",
            "gap",
            "delete",
            "yank",
            "paste",
            "speed 2 --clip 1",
            "extend --clip 1 --edge in --to 2",
            "purge",
            "track add video",
            "track mute",
            "track rm 1",
            "media rm 1",
            "undo",
            "redo",
        ];
        let mut seen = vec![false; SLOTS];
        for spec in SPECS {
            let a = parse(spec, &defaults()).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
            let s = a.slot();
            assert!(!seen[s], "{spec:?} duplicates slot {s}");
            seen[s] = true;
        }
        assert!(seen.iter().all(|&s| s), "uncovered slots: {seen:?}");
    }

    #[test]
    fn the_colon_grammar_mirrors_the_cli_flags() {
        assert_eq!(
            parse("delete --at 3 --ripple --track 2", &defaults()),
            Ok(Action::Delete {
                track: 1,
                target: Target::At(3.0),
                ripple: true,
            })
        );
        assert_eq!(
            parse("yank --clip 9", &defaults()),
            Ok(Action::Yank {
                track: 5,
                target: Target::Clip(9),
            })
        );
        assert_eq!(
            parse("extend --seg 2 --edge out --to -1.5 --ripple", &defaults()),
            Ok(Action::Extend {
                track: 5,
                target: Target::Segment(2),
                edge: Edge::Out,
                to: -1.5,
                ripple: true,
            })
        );
    }

    #[test]
    fn defaults_fill_in_the_playhead_track_and_clip() {
        assert_eq!(
            parse("split", &defaults()),
            Ok(Action::Split { track: 5, at: 7.5 })
        );
        assert_eq!(
            parse("speed 0.5", &defaults()),
            Ok(Action::SetSpeed {
                track: 5,
                clip: 42,
                rate: Seconds::new(1, 2),
            })
        );
        // No playhead to fall back to (the CLI case): refuse instead of
        // guessing a position.
        let bare = ParseDefaults::default();
        assert!(matches!(parse("split", &bare), Err(ParseError::Invalid(_)),));
    }

    #[test]
    fn unknown_verbs_and_bad_flags_stay_apart() {
        // Unknown verbs may be another front-end's command; bad flags on a
        // known verb are always an error.
        assert_eq!(
            parse("export -o out.mp4", &defaults()),
            Err(ParseError::Unknown("export".to_string()))
        );
        assert!(matches!(
            parse("split --bogus 1", &defaults()),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            parse("delete --at 1 --clip 2", &defaults()),
            Err(ParseError::Invalid(_))
        ));
        // The CLI's short flag works in the shared tokenizer too.
        let (verb, mut f) = split_args("export -o out.mp4").unwrap();
        assert_eq!(verb, "export");
        assert_eq!(f.take("out").as_deref(), Some("out.mp4"));
        assert!(f.positional().is_none(), "no stray positionals");
    }

    // ------------------------------------------------------------ run

    /// A file with both streams (like the demo's red.mp4) — clips can go
    /// on a video *or* an audio track, which is what most fixtures need.
    fn probe_report(duration: &str) -> RawProbe {
        RawProbe {
            streams: vec![
                RawStream {
                    index: 0,
                    codec_type: Some("video".into()),
                    codec_name: Some("h264".into()),
                    width: Some(320),
                    height: Some(240),
                    r_frame_rate: Some("30/1".into()),
                    ..Default::default()
                },
                RawStream {
                    index: 1,
                    codec_type: Some("audio".into()),
                    codec_name: Some("aac".into()),
                    sample_rate: Some("48000".into()),
                    channels: Some(2),
                    ..Default::default()
                },
            ],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    /// [Gap 1 @0..1] [Clip #1 1..4] [Gap 2 @4..6] [Clip #2 6..8]
    fn fixture() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        let seg = |id: u32, in_s: i64, out_s: i64| {
            Segment::Clip(Clip {
                id,
                asset: a,
                asset_in: Seconds::from_integer(in_s),
                asset_out: Seconds::from_integer(out_s),
                speed: Seconds::from_integer(1),
            })
        };
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            seg(1, 0, 3),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            seg(2, 0, 2),
        ];
        p
    }

    fn run(p: &mut Project, reg: &mut Option<Register>, a: &Action) -> Result<Outcome, EditError> {
        let mut h = History::default();
        run_action(p, &mut h, reg, a, 0.0)
    }

    #[test]
    fn edits_record_undo_history() {
        let mut p = fixture();
        let mut reg = None;
        let mut h = History::default();
        let out = run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::Split { track: 0, at: 2.5 },
            2.5,
        )
        .unwrap();
        assert!(out.changed && out.playhead.is_none());
        assert!(h.can_undo());
        // Undo returns to the frame the edit was made from.
        let undone = run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
        assert_eq!(undone.playhead, Some(75), "2.5 s @ 30 fps");
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 4);
    }

    #[test]
    fn yank_fills_the_register_and_paste_consumes_it() {
        let mut p = fixture();
        let mut reg = None;
        let out = run(
            &mut p,
            &mut reg,
            &Action::Yank {
                track: 0,
                target: Target::Clip(1),
            },
        )
        .unwrap();
        assert!(!out.changed, "a yank writes the register, not the project");
        assert!(matches!(
            reg,
            Some(Register {
                segment: Segment::Clip(Clip { id: 1, .. }),
                ..
            })
        ));
        // Pasting without a register is an error; with one it inserts.
        assert!(matches!(
            run(
                &mut p,
                &mut None,
                &Action::Paste {
                    track: 0,
                    at: 7.0,
                    ripple: false,
                    segment: None,
                },
            )
            .unwrap_err(),
            EditError::RegisterEmpty
        ));
        run(
            &mut p,
            &mut reg,
            &Action::Paste {
                track: 0,
                at: 7.0,
                ripple: true,
                segment: None,
            },
        )
        .unwrap();
        // Pasting inside clip #2 splits it: gap, #1, gap, #2a, pasted, #2b.
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 6);
    }

    #[test]
    fn track_edits_are_undoable() {
        let mut p = fixture();
        let mut reg = None;
        let mut h = History::default();

        // Add → undo → redo.
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::TrackAdd {
                kind: TrackKind::Audio,
            },
            0.0,
        )
        .unwrap();
        assert_eq!(p.timeline.tracks.len(), 2);
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
        assert_eq!(p.timeline.tracks.len(), 1, "the add undoes");
        run_action(&mut p, &mut h, &mut reg, &Action::Redo, 0.0).unwrap();
        assert_eq!(p.timeline.tracks.len(), 2, "and redoes");

        // Mute → undo restores the flag.
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::TrackMute { track: 1 },
            0.0,
        )
        .unwrap();
        assert!(p.timeline.track(1).unwrap().muted);
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
        assert!(!p.timeline.track(1).unwrap().muted, "the mute undoes");

        // Put a clip on the new track through the catalog (so the stack
        // stays in step), then remove the track: undo brings it back with
        // its clip.
        let a = p.media[0].id;
        let clip = Segment::Clip(Clip {
            id: 0,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
        });
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::Paste {
                track: 1,
                at: 0.0,
                ripple: true,
                segment: Some(clip),
            },
            0.0,
        )
        .unwrap();
        assert_eq!(p.timeline.track(1).unwrap().segments.len(), 1);
        run_action(&mut p, &mut h, &mut reg, &Action::TrackRm { track: 1 }, 0.0).unwrap();
        assert_eq!(p.timeline.tracks.len(), 1);
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
        assert_eq!(p.timeline.tracks.len(), 2, "the track is back");
        assert_eq!(
            p.timeline.track(1).unwrap().segments.len(),
            1,
            "with its clip"
        );
    }

    #[test]
    fn a_refused_track_edit_leaves_the_project_alone() {
        let mut p = fixture();
        let mut h = History::default();
        // Removing the only video track fails validation — and the snapshot
        // is rolled back, not left half-applied.
        let err = h.edit_tracks(&mut p, 0, |p| {
            p.timeline.tracks.remove(0);
            Ok("removed the only video track".to_string())
        });
        assert!(err.is_err(), "a project needs a video track");
        assert_eq!(p.timeline.tracks.len(), 1, "the edit rolled back");
        assert!(!h.can_undo(), "nothing was recorded");
    }

    #[test]
    fn a_track_list_changed_outside_the_stack_refuses_to_undo() {
        let mut p = fixture();
        let mut reg = None;
        let mut h = History::default();
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::TrackAdd {
                kind: TrackKind::Audio,
            },
            0.0,
        )
        .unwrap();
        // Something (a stale sidecar, an out-of-band write) moved the tracks.
        p.timeline.add_track(TrackKind::Video);
        assert!(matches!(
            run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0),
            Err(EditError::History(_))
        ));
    }

    #[test]
    fn purge_is_undoable() {
        let mut p = fixture();
        // Drift clip #1 to 60.4 frames at 30 fps: purge snaps it to 60.
        if let Segment::Clip(c) = &mut p.timeline.tracks[0].segments[1] {
            c.asset_out = c.asset_in + Seconds::new(604, 300);
        }
        let mut reg = None;
        let mut h = History::default();
        let out = run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::PurgeStumps { track: None },
            0.0,
        )
        .unwrap();
        assert!(out.desc.contains("purged"), "{}", out.desc);
        let span = |p: &Project| match &p.timeline.tracks[0].segments[1] {
            Segment::Clip(c) => c.asset_out - c.asset_in,
            other => panic!("expected a clip, got {other:?}"),
        };
        assert_eq!(span(&p), Seconds::from_integer(2), "snapped to 60 frames");
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
        assert_eq!(span(&p), Seconds::new(604, 300), "the drift is back");
    }

    #[test]
    fn a_register_from_another_project_resolves_or_refuses() {
        let p = fixture();
        // The same file pooled here under a different id (asset 1 is
        // red.mp4 in the fixture): the register resolves by path.
        let mut other = p.clone();
        let (spare, _) = other.add_asset("/media/spare.mp4".into(), probe_report("5"));
        let reg = Register::of(
            &other,
            Segment::Clip(Clip {
                id: 7,
                asset: spare,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
        );
        assert_eq!(reg.path.as_deref(), Some(Path::new("/media/spare.mp4")));
        // Not in this pool → refuse, loudly.
        assert!(matches!(
            reg.resolve(&p),
            Err(EditError::RegisterForeign(path)) if path.contains("spare.mp4")
        ));

        // A file this project *does* pool (under a different id) remaps.
        let mut here = p.clone();
        let (id, _) = here.add_asset("/media/other/red.mp4".into(), probe_report("10"));
        let entry = Register {
            segment: Segment::Clip(Clip {
                id: 7,
                asset: 999, // the id it had over there
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
            path: Some(PathBuf::from("/media/other/red.mp4")),
        };
        let resolved = entry.resolve(&here).unwrap();
        assert!(
            matches!(resolved, Segment::Clip(Clip { asset, .. }) if asset == id),
            "the asset id is remapped to this pool's: {resolved:?}"
        );
    }

    #[test]
    fn extend_moves_an_edge_under_the_same_rules_as_the_tui() {
        let mut p = fixture();
        let mut reg = None;
        run(
            &mut p,
            &mut reg,
            &Action::Extend {
                track: 0,
                target: Target::Clip(1),
                edge: Edge::Out,
                to: 5.0,
                ripple: false,
            },
        )
        .unwrap();
        // The tail grew into the gap; the gap shrank to keep the length.
        let segs = &p.timeline.video_track().unwrap().segments;
        assert!(matches!(segs[1], Segment::Clip(Clip { id: 1, .. })));
        assert_eq!(
            crate::extend::clip_span(&p, 1, 0),
            Some((1.0, 5.0)),
            "{segs:?}"
        );
    }

    #[test]
    fn the_register_round_trips_through_its_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        assert_eq!(load_register(&path), None, "no file, no register");
        let reg = Register {
            segment: Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            path: Some(PathBuf::from("/media/red.mp4")),
        };
        save_register(&path, Some(&reg)).unwrap();
        assert_eq!(load_register(&path), Some(reg));
        save_register(&path, None).unwrap();
        assert_eq!(load_register(&path), None, "clearing removes the file");
        assert!(!register_path(&path).exists());
    }
}
