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
use crate::time::{Seconds, frame_of, frames_to_seconds, grid_time, parse_signed_time, secs_f64};
use crate::timeline::{Effect, MAX_TIMELINE_SECONDS, Segment, TrackKind, TransitionKind};

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

/// How long a transition is or will be. `Percent` is a rate on an existing
/// blend: 50 = twice as long, 200 = half as long (100 = unchanged).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransitionLen {
    Frames(i64),
    Seconds(f64),
    Percent(f64),
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
    /// Retime a clip; the track ripples to fit (`R`, `:speed`). `rate` is the
    /// model rate (1 = normal); the front-ends take a percentage.
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
    /// Add a crossfade at a boundary (`x`, `:transition`). On an existing
    /// blend the same command retimes it (frames/dur, or a `Percent` rate).
    Transition {
        track: usize,
        at: f64,
        kind: TransitionKind,
        len: TransitionLen,
    },
    /// Append an effect to a clip (`:effect`), or replace the one at `index`
    /// (the effects panel's edit gesture).
    AddEffect {
        track: usize,
        clip: u32,
        effect: Effect,
        index: Option<usize>,
    },
    /// Drop one effect from a clip (`:effect rm`).
    RemoveEffect {
        track: usize,
        clip: u32,
        /// `None` = the last effect.
        index: Option<usize>,
    },
    /// Drop every effect from a clip (`:effect clear`).
    ClearEffects {
        track: usize,
        clip: u32,
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
            Action::Transition { .. } => 16,
            Action::AddEffect { .. } => 17,
            Action::RemoveEffect { .. } => 18,
            Action::ClearEffects { .. } => 19,
        }
    }
}

/// How many slots the catalog has — the coverage tests assert exactly
/// this many are reachable from each spelling.
pub const SLOTS: usize = 20;

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
        Action::Transition {
            track,
            at,
            kind,
            len,
        } => {
            let fps = project.fps.ok_or(EditError::NoFps)?;
            let existing = edit::transition_at(project, *track, *at)?;
            let Some((_, transition, _)) = existing else {
                // No blend at the position: frames/dur add one; a rate can
                // only retime an existing blend.
                let dur = match len {
                    TransitionLen::Frames(n) => frames_to_seconds(*n, fps),
                    TransitionLen::Seconds(s) => {
                        frames_to_seconds((s * secs_f64(&fps)).round() as i64, fps)
                    }
                    TransitionLen::Percent(_) => {
                        return Err(EditError::NoTransition(format!("{at} s")));
                    }
                };
                // `add_transition` ids the blend with the next segment id;
                // keep it so the playhead can land in the new blend below.
                let blend_id = project.next_clip_id();
                let batch = edit::add_transition(project, *track, *at, *kind, dur)?;
                let mut out = apply(project, history, batch, from)?;
                land_on_blend(project, *track, blend_id, &mut out);
                return Ok(out);
            };
            // An existing blend is retimed; its kind stays.
            let dur = match len {
                TransitionLen::Frames(n) => frames_to_seconds(*n, fps),
                TransitionLen::Seconds(s) => {
                    frames_to_seconds((s * secs_f64(&fps)).round() as i64, fps)
                }
                TransitionLen::Percent(p) => {
                    if !p.is_finite() || *p <= 0.0 {
                        return Err(EditError::BadSpeed(format!("{p}")));
                    }
                    let old_frames = (transition.dur * fps).round().to_integer();
                    let frames = ((old_frames as f64 * 100.0 / p).round() as i64).max(1);
                    if frames as f64 > MAX_TIMELINE_SECONDS as f64 * secs_f64(&fps) {
                        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
                    }
                    frames_to_seconds(frames, fps)
                }
            };
            let blend_id = transition.id;
            let Some(batch) = edit::retime_transition_on(project, *track, *at, dur)? else {
                return Ok(Outcome {
                    desc: "transition unchanged".to_string(),
                    changed: false,
                    playhead: None,
                });
            };
            let mut out = apply(project, history, batch, from)?;
            land_on_blend(project, *track, blend_id, &mut out);
            Ok(out)
        }
        Action::AddEffect {
            track,
            clip,
            effect,
            index,
        } => {
            // A clip id is unique across tracks, so `--clip N` finds its own
            // track (as delete/yank/speed already do) — the `--track` default
            // must not hide a clip that lives elsewhere.
            let ti = resolve_track(project, *track, Target::Clip(*clip))?;
            let batch = match index {
                Some(i) => edit::replace_effect_on(project, ti, *clip, *i, effect.clone())?,
                None => edit::add_effect_on(project, ti, *clip, effect.clone())?,
            };
            apply(project, history, batch, from)
        }
        Action::RemoveEffect { track, clip, index } => {
            let ti = resolve_track(project, *track, Target::Clip(*clip))?;
            let batch = edit::remove_effect_on(project, ti, *clip, *index)?;
            apply(project, history, batch, from)
        }
        Action::ClearEffects { track, clip } => {
            let ti = resolve_track(project, *track, Target::Clip(*clip))?;
            let batch = edit::clear_effects_on(project, ti, *clip)?;
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
            // Clip ids are unique across tracks, so a clip-addressed action
            // finds its own track — same as delete/yank/extend (`--track` is
            // the default for position-addressed targets, not a constraint).
            let ti = resolve_track(project, *track, Target::Clip(*clip))?;
            let batch = edit::set_speed_on(project, ti, *clip, *rate)?;
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

/// Park the playhead on the middle of blend `blend_id` after adding/retiming
/// it: the cut it covers is the overlap point, not either edge.
fn land_on_blend(project: &Project, track: usize, blend_id: u32, out: &mut Outcome) {
    if let Some(tr) = project.timeline.track(track)
        && let Some(i) = tr
            .segments
            .iter()
            .position(|s| matches!(s, Segment::Transition(t) if t.id == blend_id))
    {
        let mid = secs_f64(&tr.start_of(i)) + secs_f64(&tr.segments[i].duration()) / 2.0;
        out.playhead = Some(frame_of(project.fps.as_ref(), mid));
    }
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
        // A transition isn't material to extend; phase 1 refuses it.
        Segment::Transition(_) => Err(EditError::Transition),
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
        Segment::Transition(t) => format!("yanked transition #{}", t.id),
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
            // A transition carries its own asset ids; resolving those across
            // projects is out of scope (register paste of a transition).
            Segment::Transition(_) => None,
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
/// words (`:speed 50`, `:track mute 2`). The grammar mirrors the CLI
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

/// The `:effect` / `nle effect` usage string — one spelling for both
/// front-ends.
pub const EFFECT_USAGE: &str = ":effect [--clip N] [--track N] \
     crop --left P --right P --top P --bottom P (%: 0 = edge, 100 = opposite) | \
     scale --w P --h P (%: 100 = unchanged) | \
     offset --x P --y P (% of the way to the edge; ±100 = off-screen) | \
     eq [--brightness --contrast --saturation] | lut --path P | \
     rm [--index N] | clear (one per kind: adding a kind again replaces it)";

/// The parameters of an effect call, before the kind picks which of them
/// apply. Shared by the `:cmd` grammar and the CLI so a new kind can't drift
/// between them.
#[derive(Debug, Clone, Default)]
pub struct EffectArgs {
    /// scale: width/height as percentages of the source frame (default 100).
    pub w: Option<f64>,
    pub h: Option<f64>,
    /// crop: edge insets as percentages (0 = the frame edge, 100 = the
    /// opposite edge; the two sides of an axis must sum to under 100).
    pub left: Option<f64>,
    pub right: Option<f64>,
    pub top: Option<f64>,
    pub bottom: Option<f64>,
    /// offset: position shift as percentages of the canvas (0 = no move).
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub brightness: Option<f64>,
    pub contrast: Option<f64>,
    pub saturation: Option<f64>,
    pub path: Option<String>,
}

/// Build a typed [`Effect`] from `kind` and its parameters. The one place the
/// defaults and the percentage ranges live.
pub fn effect_from_args(kind: &str, a: EffectArgs) -> Result<Effect, ParseError> {
    let pct = |v: Option<f64>, n: &str, default: f64, min: f64, max: f64| {
        let v = v.unwrap_or(default);
        if !(min..=max).contains(&v) {
            return Err(ParseError::Invalid(format!("{n} must be {min}–{max} (%)")));
        }
        Ok(v)
    };
    match kind {
        "crop" => {
            let left = pct(a.left, "crop left", 0.0, 0.0, 100.0)?;
            let right = pct(a.right, "crop right", 0.0, 0.0, 100.0)?;
            let top = pct(a.top, "crop top", 0.0, 0.0, 100.0)?;
            let bottom = pct(a.bottom, "crop bottom", 0.0, 0.0, 100.0)?;
            if left + right >= 100.0 {
                return Err(ParseError::Invalid(
                    "crop left + right must be under 100%".to_string(),
                ));
            }
            if top + bottom >= 100.0 {
                return Err(ParseError::Invalid(
                    "crop top + bottom must be under 100%".to_string(),
                ));
            }
            Ok(Effect::Crop {
                left,
                right,
                top,
                bottom,
            })
        }
        "scale" => Ok(Effect::Scale {
            w: pct(a.w, "scale width", 100.0, 1.0, 1000.0)?,
            h: pct(a.h, "scale height", 100.0, 1.0, 1000.0)?,
        }),
        "offset" => Ok(Effect::Offset {
            x: pct(a.x, "offset x", 0.0, -100.0, 100.0)?,
            y: pct(a.y, "offset y", 0.0, -100.0, 100.0)?,
        }),
        // ffmpeg's `eq` is neutral at brightness 0 / contrast 1 /
        // saturation 1: 0 contrast and saturation mean flat and grey.
        "eq" => Ok(Effect::Eq {
            brightness: a.brightness.unwrap_or(0.0),
            contrast: a.contrast.unwrap_or(1.0),
            saturation: a.saturation.unwrap_or(1.0),
        }),
        "lut" => Ok(Effect::Lut {
            path: a
                .path
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| ParseError::Invalid("effect needs --path".to_string()))?,
        }),
        _ => Err(ParseError::Invalid(EFFECT_USAGE.to_string())),
    }
}

/// Parse one `:cmd` line into an [`Action`].
pub fn parse(input: &str, d: &ParseDefaults) -> Result<Action, ParseError> {
    let (verb, mut f) = split_args(input)?;
    let at = |f: &mut Flags, d: &ParseDefaults| -> Result<f64, ParseError> {
        match opt_time(f, "at")? {
            Some(t) => Ok(t),
            None => d
                .at
                .ok_or_else(|| ParseError::Invalid("no position given — pass --at T".to_string())),
        }
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
            match len {
                GapLen::Frames(n) if n < 1 => {
                    return Err(ParseError::Invalid(
                        "a gap needs at least one frame".to_string(),
                    ));
                }
                GapLen::Seconds(s) if s <= 0.0 => {
                    return Err(ParseError::Invalid(
                        "a gap needs a positive length".to_string(),
                    ));
                }
                _ => {}
            }
            f.finish(":gap [--at T] [--frames N | --dur T] [--track N]")?;
            Ok(Action::InsertGap {
                track: t,
                at: a,
                len,
            })
        }
        "transition" => {
            let (a, t) = (at(&mut f, d)?, parse_track(&mut f, d)?);
            let kind = match f.positional().as_deref() {
                None | Some("dissolve") => TransitionKind::Dissolve,
                Some("fade") => TransitionKind::FadeBlack,
                Some(other) => {
                    return Err(ParseError::Invalid(format!(
                        "unknown kind {other:?} — use dissolve or fade"
                    )));
                }
            };
            let len = match (
                opt_usize(&mut f, "frames")?,
                opt_time(&mut f, "dur")?,
                opt_time(&mut f, "percent")?,
            ) {
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                    return Err(ParseError::Invalid(
                        "use only one of --frames N, --dur T, or --percent P".to_string(),
                    ));
                }
                (Some(n), None, None) => TransitionLen::Frames(n as i64),
                (None, Some(s), None) => TransitionLen::Seconds(s),
                (None, None, Some(p)) => {
                    if !p.is_finite() || p <= 0.0 {
                        return Err(ParseError::Invalid(format!(
                            "transition percent must be positive (got {p})"
                        )));
                    }
                    TransitionLen::Percent(p)
                }
                (None, None, None) => TransitionLen::Seconds(1.0),
            };
            f.finish(
                ":transition [dissolve|fade] [--at T] [--track N] \
                 [--frames N | --dur T | --percent P]",
            )?;
            Ok(Action::Transition {
                track: t,
                at: a,
                kind,
                len,
            })
        }
        "effect" => {
            let track = parse_track(&mut f, d)?;
            let clip = opt_u32(&mut f, "clip")?.or(d.clip).ok_or_else(|| {
                ParseError::Invalid(
                    "effect addresses a clip — pass --clip N (or run it with the playhead on one)"
                        .to_string(),
                )
            })?;
            let kind = f
                .positional()
                .ok_or_else(|| ParseError::Invalid(EFFECT_USAGE.to_string()))?;
            match kind.as_str() {
                "rm" => {
                    let index = opt_usize(&mut f, "index")?;
                    f.finish(":effect rm [--index N] [--clip N] [--track N]")?;
                    Ok(Action::RemoveEffect { track, clip, index })
                }
                "clear" => {
                    f.finish(":effect clear [--clip N] [--track N]")?;
                    Ok(Action::ClearEffects { track, clip })
                }
                _ => {
                    let args = EffectArgs {
                        w: opt_time(&mut f, "w")?,
                        h: opt_time(&mut f, "h")?,
                        left: opt_time(&mut f, "left")?,
                        right: opt_time(&mut f, "right")?,
                        top: opt_time(&mut f, "top")?,
                        bottom: opt_time(&mut f, "bottom")?,
                        x: opt_time(&mut f, "x")?,
                        y: opt_time(&mut f, "y")?,
                        brightness: opt_time(&mut f, "brightness")?,
                        contrast: opt_time(&mut f, "contrast")?,
                        saturation: opt_time(&mut f, "saturation")?,
                        path: f.take("path"),
                    };
                    let effect = effect_from_args(&kind, args)?;
                    f.finish(EFFECT_USAGE)?;
                    Ok(Action::AddEffect {
                        track,
                        clip,
                        effect,
                        index: None,
                    })
                }
            }
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
            // `:speed 50` (the `R` prompt's spelling) or `:speed --rate 50 --clip 3`.
            // The value is a percentage: 100 = normal, 50 = half, 200 = double.
            let rate = f.positional().or_else(|| f.take("rate")).ok_or_else(|| {
                ParseError::Invalid("usage: :speed PERCENT [--clip N]".to_string())
            })?;
            let rate = parse_rate(&rate)?;
            let clip = opt_u32(&mut f, "clip")?.or(d.clip).ok_or_else(|| {
                ParseError::Invalid(
                    ":speed addresses a clip — pass --clip N (or run it with the playhead on one)"
                        .to_string(),
                )
            })?;
            let track = parse_track(&mut f, d)?;
            f.finish(":speed PERCENT [--clip N] [--track N]")?;
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

/// A speed percentage: `100` = normal, `50` = half, `200` = double. Returns
/// the model's rate (`percent / 100`, so 1 = normal).
fn parse_rate(s: &str) -> Result<Seconds, ParseError> {
    let bad = || ParseError::Invalid(format!("invalid speed {s:?} (try 100, 200, or 50)"));
    let percent: f64 = s.parse().map_err(|_| bad())?;
    if !percent.is_finite() || percent <= 0.0 {
        return Err(ParseError::Invalid(format!(
            "speed must be positive (got {s:?})"
        )));
    }
    Seconds::approximate_float(percent / 100.0)
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
            "speed 200 --clip 1",
            "extend --clip 1 --edge in --to 2",
            "purge",
            "track add video",
            "track mute",
            "track rm 1",
            "media rm 1",
            "undo",
            "redo",
            "transition --at 3",
            "effect crop --clip 1 --left 10",
            "effect rm --clip 1 --index 0",
            "effect clear --clip 1",
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
    fn effect_removal_parses() {
        assert_eq!(
            parse("effect rm --clip 4 --index 1 --track 1", &defaults()),
            Ok(Action::RemoveEffect {
                track: 0,
                clip: 4,
                index: Some(1),
            })
        );
        // Without `--index` the last effect is the one dropped.
        assert_eq!(
            parse("effect rm --clip 4", &defaults()),
            Ok(Action::RemoveEffect {
                track: 5,
                clip: 4,
                index: None,
            })
        );
        assert_eq!(
            parse("effect clear --clip 4", &defaults()),
            Ok(Action::ClearEffects { track: 5, clip: 4 })
        );
    }

    #[test]
    fn effect_eq_defaults_to_a_neutral_filter() {
        // ffmpeg's `eq` is neutral at contrast 1 / saturation 1. Defaulting
        // them to 0 flattened and desaturated the clip even when only
        // `--brightness` was asked for.
        let Action::AddEffect { effect, .. } =
            parse("effect --clip 1 eq --brightness -0.2", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(
            effect,
            Effect::Eq {
                brightness: -0.2,
                contrast: 1.0,
                saturation: 1.0
            }
        );
        assert_eq!(
            Effect::chain(std::slice::from_ref(&effect), (0, 0)),
            "eq=brightness=-0.2:contrast=1:saturation=1",
            "the neutral values reach the graph"
        );
    }

    // Crop edges are percentages toward the opposite side: 0 = the frame
    // edge, 100 = the other edge. 25 off each side of 1920×1080 masks the
    // outer quarter but keeps the middle half in place.
    #[test]
    fn crop_is_percentage_insets() {
        let Action::AddEffect { effect, .. } = parse(
            "effect crop --clip 1 --left 25 --right 25 --top 25 --bottom 25",
            &defaults(),
        )
        .unwrap() else {
            panic!("expected an effect")
        };
        assert_eq!(
            effect,
            Effect::Crop {
                left: 25.0,
                right: 25.0,
                top: 25.0,
                bottom: 25.0
            }
        );
        assert_eq!(
            Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
            "format=yuva420p,crop=960:540:480:270,\
             pad=1920:1080:480:270:color=black@0"
        );
        assert_eq!(effect.details(), "l25 r25 t25 b25");

        // Omitting all four leaves the frame unchanged (defaults 0) — and
        // unlike the old zoom crop, it renders no filter at all.
        let Action::AddEffect { effect, .. } = parse("effect crop --clip 1", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(Effect::chain(&[effect], (1920, 1080)), "");

        // Asymmetric insets: 10% of 1920 = 192; 25% of 1080 = 270.
        let Action::AddEffect { effect, .. } =
            parse("effect crop --clip 1 --left 10 --top 25", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
            "format=yuva420p,crop=1728:810:192:270,\
             pad=1920:1080:192:270:color=black@0"
        );
    }

    // Scale is a percentage of the picture at that point; 100 (the default) is
    // a no-op, and below 100 pads transparently so a lower track shows through.
    #[test]
    fn scale_is_percentages_with_a_100_default() {
        let Action::AddEffect { effect, .. } = parse("effect scale --clip 1", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(effect, Effect::Scale { w: 100.0, h: 100.0 });
        assert_eq!(
            Effect::chain(&[effect], (1920, 1080)),
            "",
            "100% renders nothing (the caller drops empty filters)"
        );

        let Action::AddEffect { effect, .. } =
            parse("effect scale --clip 1 --w 50 --h 200", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        // 50%×200% of 1920×1080: a 960×2160 picture clipped to the centre
        // 1080 rows, then padded left/right to the canvas.
        assert_eq!(
            Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
            "format=yuva420p,scale=960:2160,crop=960:1080:0:540,\
             pad=1920:1080:480:0:color=black@0"
        );
        assert_eq!(effect.details(), "50% × 200%");

        let Action::AddEffect { effect, .. } =
            parse("effect scale --clip 1 --w 50 --h 50", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
            "format=yuva420p,scale=960:540,pad=1920:1080:480:270:color=black@0"
        );
    }

    // Crop after scale insets the *scaled picture*, not the full canvas:
    // "scale 50, crop 30% left" removes 30% of the small picture.
    #[test]
    fn crop_after_scale_resolves_against_the_scaled_picture() {
        let effects = vec![
            Effect::Scale { w: 50.0, h: 50.0 },
            Effect::Crop {
                left: 30.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
        ];
        // 320×240 → a 160×120 picture at (80,60); 30% of 160 = 48 px.
        assert_eq!(
            Effect::chain(&effects, (320, 240)),
            "format=yuva420p,scale=160:120,pad=320:240:80:60:color=black@0,\
             format=yuva420p,crop=112:120:128:60,pad=320:240:128:60:color=black@0"
        );
        assert!(Effect::masks(&effects, (320, 240)));
    }

    // Offset moves the picture by a percentage of the canvas; the vacated
    // area is transparent and the overflow is clipped.
    #[test]
    fn offset_moves_the_picture_on_the_canvas() {
        let Action::AddEffect { effect, .. } =
            parse("effect offset --clip 1 --x 10 --y -5", &defaults()).unwrap()
        else {
            panic!("expected an effect")
        };
        assert_eq!(effect, Effect::Offset { x: 10.0, y: -5.0 });
        assert_eq!(effect.details(), "x 10% · y -5%");
        // +10% of 640 = 64 px right: the left 64 px is vacated, the right
        // 64 px of the picture is clipped.
        let right = Effect::Offset { x: 10.0, y: 0.0 };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&right), (640, 360)),
            "format=yuva420p,crop=576:360:0:0,pad=640:360:64:0:color=black@0"
        );
        // -5% of 360 = 18 px up.
        let up = Effect::Offset { x: 0.0, y: -5.0 };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&up), (640, 360)),
            "format=yuva420p,crop=640:342:0:18,pad=640:360:0:0:color=black@0"
        );
        assert!(Effect::masks(&[effect], (640, 360)));

        // ±100% of the canvas is fully off screen: a transparent layer, not
        // a 2 px sliver at the edge.
        let off = "format=yuva420p,colorchannelmixer=rr=0:gg=0:bb=0:aa=0";
        for x in [100.0, -100.0] {
            let shifted = Effect::Offset { x, y: 0.0 };
            assert_eq!(
                Effect::chain(std::slice::from_ref(&shifted), (640, 360)),
                off,
                "offset x={x}%"
            );
            assert!(Effect::masks(&[shifted], (640, 360)));
        }
        for y in [100.0, -100.0] {
            let shifted = Effect::Offset { x: 0.0, y };
            assert_eq!(
                Effect::chain(std::slice::from_ref(&shifted), (640, 360)),
                off,
                "offset y={y}%"
            );
        }

        // 0/0 is a no-op.
        assert_eq!(
            Effect::chain(&[Effect::Offset { x: 0.0, y: 0.0 }], (640, 360)),
            ""
        );
    }

    // On a picture that scale/crop already moved right, 80% is 80% of the
    // remaining distance to the edge, so part of it is still visible.
    #[test]
    fn offset_counts_the_distance_to_the_edge() {
        let effects = vec![
            Effect::Scale { w: 50.0, h: 50.0 },
            Effect::Crop {
                left: 30.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            Effect::Offset { x: 80.0, y: 0.0 },
        ];
        // The 160×120 scale is cropped to 112×120 at (128,60); 80% of the
        // 192 px to the right edge is 154 px, leaving a 38 px strip.
        let chain = Effect::chain(&effects, (320, 240));
        assert!(
            chain.ends_with("crop=38:120:0:0,pad=320:240:282:60:color=black@0"),
            "{chain}"
        );

        // 100% covers the whole distance: fully off-screen.
        let off = vec![
            Effect::Scale { w: 50.0, h: 50.0 },
            Effect::Crop {
                left: 30.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            Effect::Offset { x: 100.0, y: 0.0 },
        ];
        assert!(
            Effect::chain(&off, (320, 240)).ends_with("colorchannelmixer=rr=0:gg=0:bb=0:aa=0"),
            "{chain}"
        );
    }

    #[test]
    fn bad_effect_percentages_are_refused() {
        let d = ParseDefaults::default();
        for spec in [
            "effect crop --clip 1 --left 101",
            "effect crop --clip 1 --left 50 --right 50",
            "effect crop --clip 1 --top 60 --bottom 40",
            "effect scale --clip 1 --w 0",
            "effect scale --clip 1 --h 1001",
            "effect offset --clip 1 --x 101",
            "effect offset --clip 1 --y -101",
        ] {
            assert!(
                matches!(parse(spec, &d), Err(ParseError::Invalid(_))),
                "{spec:?} was accepted"
            );
        }
    }

    #[test]
    fn defaults_fill_in_the_playhead_track_and_clip() {
        assert_eq!(
            parse("split", &defaults()),
            Ok(Action::Split { track: 5, at: 7.5 })
        );
        assert_eq!(
            parse("speed 50", &defaults()),
            Ok(Action::SetSpeed {
                track: 5,
                clip: 42,
                rate: Seconds::new(1, 2),
            })
        );
        // `:effect` falls back to the clip under the playhead too (it used to
        // demand an explicit `--clip`, unlike every other clip-addressed verb).
        match parse("effect eq", &defaults()) {
            Ok(Action::AddEffect { track, clip, .. }) => {
                assert_eq!((track, clip), (5, 42), "the playhead's clip fills in")
            }
            other => panic!("expected an effect action, got {other:?}"),
        }
        // No playhead to fall back to (the CLI case): refuse instead of
        // guessing a position.
        let bare = ParseDefaults::default();
        assert!(matches!(parse("split", &bare), Err(ParseError::Invalid(_)),));
    }

    #[test]
    fn a_gap_needs_a_positive_length() {
        let d = ParseDefaults::default();
        assert!(matches!(
            parse("gap --at 1 --frames 0", &d),
            Err(ParseError::Invalid(msg)) if msg.contains("at least one frame")
        ));
        assert!(matches!(
            parse("gap --at 1 --dur -2", &d),
            Err(ParseError::Invalid(msg)) if msg.contains("positive length")
        ));
        assert!(parse("gap --at 1 --frames 3", &d).is_ok());
    }

    /// An explicit `--at` works with no playhead default (the parser used
    /// to evaluate its fallback eagerly, so `--at 1` reported "no position
    /// given" whenever the front-end had no default position).
    #[test]
    fn explicit_flags_work_without_defaults() {
        let bare = ParseDefaults::default();
        assert_eq!(
            parse("split --at 1.5", &bare),
            Ok(Action::Split { track: 0, at: 1.5 })
        );
        assert_eq!(
            parse("delete --clip 3 --ripple", &bare),
            Ok(Action::Delete {
                track: 0,
                target: Target::Clip(3),
                ripple: true,
            })
        );
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
                effects: Vec::new(),
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

    // `--clip` finds the clip's own track: `effect` used to need an explicit
    // `--track`, unlike delete/yank/speed.
    #[test]
    fn add_effect_finds_the_clips_own_track() {
        let mut p = fixture();
        let vi = p.timeline.add_track(TrackKind::Video);
        p.timeline.tracks[vi].segments = vec![Segment::Clip(Clip {
            id: 9,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(1),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let mut reg = None;
        let out = run(
            &mut p,
            &mut reg,
            &Action::AddEffect {
                track: 0, // the default track; clip #9 lives on the second
                clip: 9,
                effect: Effect::Eq {
                    brightness: 0.0,
                    contrast: 1.0,
                    saturation: 1.0,
                },
                index: None,
            },
        )
        .unwrap();
        assert!(out.changed, "{}", out.desc);
        match &p.timeline.tracks[vi].segments[0] {
            Segment::Clip(c) => assert_eq!(c.effects.len(), 1, "the effect landed"),
            other => panic!("expected a clip, got {other:?}"),
        }
    }

    #[test]
    fn a_transition_action_retimes_an_existing_blend() {
        let mut p = fixture();
        let mut reg = None;
        let action = |len| Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len,
        };
        let dur = |p: &Project| {
            p.timeline
                .video_track()
                .unwrap()
                .segments
                .iter()
                .find_map(|s| match s {
                    Segment::Transition(t) => Some(t.dur),
                    _ => None,
                })
                .unwrap()
        };
        run(&mut p, &mut reg, &action(TransitionLen::Frames(30))).unwrap();
        assert_eq!(dur(&p), Seconds::from_integer(1));

        // The original cut is the blend's far edge, and still addresses it.
        let out = run(&mut p, &mut reg, &action(TransitionLen::Frames(15))).unwrap();
        assert!(out.changed);
        assert!(out.desc.contains("15 frames"), "{}", out.desc);
        assert_eq!(dur(&p), Seconds::new(1, 2));

        // A rate is relative to the current length: 50% doubles it.
        run(&mut p, &mut reg, &action(TransitionLen::Percent(50.0))).unwrap();
        assert_eq!(dur(&p), Seconds::from_integer(1));

        // 100% is a no-op, not a history entry.
        let out = run(&mut p, &mut reg, &action(TransitionLen::Percent(100.0))).unwrap();
        assert!(!out.changed);
        assert!(out.desc.contains("unchanged"), "{}", out.desc);

        // A rate with no blend at the position refuses.
        let out = run(
            &mut p,
            &mut reg,
            &Action::Transition {
                track: 0,
                at: 0.5,
                kind: TransitionKind::Dissolve,
                len: TransitionLen::Percent(50.0),
            },
        );
        assert!(matches!(out, Err(EditError::NoTransition(_))));
    }

    // The cut a transition is added to sits in the middle of the new blend
    // (the neighbours overlap there), so the playhead lands on its midpoint —
    // not on either edge.
    #[test]
    fn a_transition_lands_on_its_midpoint() {
        let mut p = fixture(); // [Gap][Clip #1 1..4][Gap][Clip #2 6..8]
        let mut reg = None;
        let out = run(
            &mut p,
            &mut reg,
            &Action::Transition {
                track: 0,
                at: 4.0,
                kind: TransitionKind::Dissolve,
                len: TransitionLen::Frames(30),
            },
        )
        .unwrap();
        // The clip gives up 1 s, so the blend owns [3, 4): midpoint 3.5.
        assert_eq!(out.playhead, Some(105), "the middle of the blend");

        // Deleting at the blend's first frame still removes the blend and
        // hands the trimmed frames back.
        let out = run(
            &mut p,
            &mut reg,
            &Action::Delete {
                track: 0,
                target: Target::At(3.0),
                ripple: false,
            },
        )
        .unwrap();
        assert!(out.desc.contains("transition"), "{}", out.desc);
        let segs = &p.timeline.video_track().unwrap().segments;
        assert!(
            !segs.iter().any(|s| matches!(s, Segment::Transition(_))),
            "the blend is gone: {segs:?}"
        );
        match &segs[..] {
            [
                Segment::Gap(_),
                Segment::Clip(c),
                Segment::Gap(_),
                Segment::Clip(_),
            ] => {
                assert_eq!(
                    (c.asset_in, c.asset_out),
                    (0.into(), 3.into()),
                    "the clip is whole again"
                );
            }
            other => panic!("expected the clip restored, got {other:?}"),
        }
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
            effects: Vec::new(),
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
                effects: Vec::new(),
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
                effects: Vec::new(),
            }),
            path: Some(PathBuf::from("/media/other/red.mp4")),
        };
        let resolved = entry.resolve(&here).unwrap();
        assert!(
            matches!(resolved, Segment::Clip(Clip { asset, .. }) if asset == id),
            "the asset id is remapped to this pool's: {resolved:?}"
        );
    }

    /// Clip ids are unique across tracks: a clip-addressed action finds its
    /// own track, so `nle speed ID` needs no `--track` (same as
    /// delete/yank/extend).
    #[test]
    fn a_clip_addressed_action_finds_its_own_track() {
        let mut p = fixture();
        let a = p.media[0].id;
        let ai = p.timeline.add_track(TrackKind::Audio);
        let clip = Segment::Clip(Clip {
            id: 0,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        });
        let mut h = History::default();
        let mut reg = None;
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::Paste {
                track: ai,
                at: 0.0,
                ripple: true,
                segment: Some(clip),
            },
            0.0,
        )
        .unwrap();
        let id = match &p.timeline.track(ai).unwrap().segments[0] {
            Segment::Clip(c) => c.id,
            other => panic!("expected a clip, got {other:?}"),
        };
        // The clip lives on the audio track; `track: 0` must still find it.
        run_action(
            &mut p,
            &mut h,
            &mut reg,
            &Action::SetSpeed {
                track: 0,
                clip: id,
                rate: Seconds::from_integer(2),
            },
            0.0,
        )
        .unwrap();
        assert_eq!(
            p.timeline.track(ai).unwrap().segments[0].duration(),
            Seconds::from_integer(1),
            "the audio-track clip was retimed"
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
