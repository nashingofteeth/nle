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
//! processes (probe, export, detect, EDL/OTIO import) stay in the CLI crate
//! and speak to this module only through the values they construct.

use crate::edit::{self, EditError, History};
use crate::extend::{self, Armed};
use crate::project::Project;
use crate::time::{Seconds, frame_of, grid_time, secs_f64};
use crate::timeline::{Effect, Segment, TrackKind, TransitionKind};

/// What an action addresses: the segment under a timeline position, a
/// clip by id (stable across edits), or a segment by index (as printed
/// by `nile show`).
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
    /// explicit `segment` (media-pool insert, `nile add`) skips the
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
    /// Remove an unused asset from the media pool (`nile media rm`);
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
                // only retime an existing blend. The saturating cast (a
                // `--dur 1e300` → `i64::MAX` frame count) is refused by
                // `transition_frames` before any rational math runs.
                let requested = match len {
                    TransitionLen::Frames(n) => *n,
                    TransitionLen::Seconds(s) => {
                        if s.is_finite() {
                            (*s * secs_f64(&fps)).round() as i64
                        } else {
                            0
                        }
                    }
                    TransitionLen::Percent(_) => {
                        return Err(EditError::NoTransition(format!("{at} s")));
                    }
                };
                let dur = transition_frames(requested, &fps)?;
                // `add_transition` ids the blend with the next segment id;
                // keep it so the playhead can land in the new blend below.
                let blend_id = project.next_clip_id();
                let batch = edit::add_transition(project, *track, *at, *kind, dur)?;
                let mut out = apply(project, history, batch, from)?;
                land_on_blend(project, *track, blend_id, &mut out);
                return Ok(out);
            };
            // An existing blend is retimed; its kind stays.
            let requested = match len {
                TransitionLen::Frames(n) => *n,
                TransitionLen::Seconds(s) => {
                    if s.is_finite() {
                        (*s * secs_f64(&fps)).round() as i64
                    } else {
                        0
                    }
                }
                TransitionLen::Percent(p) => {
                    if !p.is_finite() || *p <= 0.0 {
                        return Err(EditError::BadSpeed(format!("{p}")));
                    }
                    let old_frames = (transition.dur * fps).round().to_integer();
                    ((old_frames as f64 * 100.0 / p).round() as i64).max(1)
                }
            };
            let dur = transition_frames(requested, &fps)?;
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
        // The midpoint snaps to a frame; on a one-frame blend that lands a
        // frame *past* it (start + half a frame rounds up). Stay within the
        // blend: its last frame is the latest spot that still shows it.
        let fps = project.fps.as_ref();
        let start = secs_f64(&tr.start_of(i));
        let dur = secs_f64(&tr.segments[i].duration());
        let one_frame = fps.map_or(1.0, |f| 1.0 / secs_f64(f));
        let mid = (start + dur / 2.0).min(start + dur - one_frame);
        out.playhead = Some(frame_of(fps, mid));
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
/// shifted indices are what undo restores — `nile undo` brings the track
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

mod parse;
mod register;

use parse::transition_frames;

pub use parse::{
    EFFECT_USAGE, EffectArgs, Flags, ParseDefaults, ParseError, TRACK_USAGE, effect_from_args,
    parse, split_args,
};
pub use register::{REGISTER_VERSION, Register, load_register, register_path, save_register};

// ------------------------------------------------------------ parsing

#[cfg(test)]
mod tests;
