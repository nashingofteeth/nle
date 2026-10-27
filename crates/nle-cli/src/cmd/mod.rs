//! The CLI's command implementations, split by what they touch. `main.rs`
//! keeps the clap surface and the dispatch; every command body lives here so
//! the TUI can call the same work functions.

use std::path::Path;

use anyhow::Result;

use nle_core::action::{EffectArgs, effect_from_args};
use nle_core::edit;
use nle_core::timeline::TrackKind;
use nle_core::{
    Action, Asset, Edge, GapLen, Project, Target, TransitionKind, TransitionLen, format_seconds,
    load_register, parse_signed_time, run_action, save_register,
};

use crate::{
    Cmd, MediaCmd, TrackCmd, TrackKindArg, edit_message, log, parse_speed_arg, say, say_err,
};

pub(crate) mod detect;
pub(crate) mod export;
pub(crate) mod import;
pub(crate) mod media;
pub(crate) mod project;

/// A CLI time: seconds or timecode, optionally negative (an extend target
/// can reach back past time 0).
pub(crate) fn cli_time(s: &str) -> Result<f64, String> {
    parse_signed_time(s)
        .map(|t| *t.numer() as f64 / *t.denom() as f64)
        .map_err(|e| format!("invalid time {s:?} ({e})"))
}

/// A 1-based CLI index (`--track N`, `nle track rm N`) → 0-based model
/// index. Track numbers are 1-based everywhere, matching `v1`/`a1`.
pub(crate) fn cli_index(n: usize, what: &str) -> Result<usize, String> {
    if n >= 1 {
        Ok(n - 1)
    } else {
        Err(format!("{what} is 1-based, got {n}"))
    }
}

/// `--at`/`--clip`/`--seg`: exactly one way to address a segment.
pub(crate) fn cli_target(
    at: &Option<String>,
    clip: Option<u32>,
    seg: Option<usize>,
) -> Result<Target, String> {
    match (at, clip, seg) {
        (Some(_), None, None) => cli_time(at.as_deref().unwrap_or_default()).map(Target::At),
        (None, Some(c), None) => Ok(Target::Clip(c)),
        (None, None, Some(s)) => Ok(Target::Segment(s)),
        (None, None, None) => Err("pass --at T, --clip N, or --seg N".to_string()),
        _ => Err("use only one of --at/--clip/--seg".to_string()),
    }
}

/// `--edge in|out` (head/tail aliases accepted).
pub(crate) fn cli_edge(s: &str) -> Result<Edge, String> {
    match s {
        "in" | "head" => Ok(Edge::In),
        "out" | "tail" => Ok(Edge::Out),
        other => Err(format!("--edge expects in|out, got {other:?}")),
    }
}

/// The edit-shaped CLI commands as catalog [`Action`]s — the single
/// mapping behind both `main`'s dispatch and the CLI-parity test, so a
/// command can't exist without its action. `None` means "not an action"
/// (probe, export, imports, queries); `Some(Err)` is an argument problem.
///
/// `nle add` is the one exception: it is a pool write + paste in one
/// step, so it builds its `Paste` action after probing.
pub(crate) fn to_action(cmd: &Cmd) -> Option<Result<Action, String>> {
    match cmd {
        Cmd::TrimIn { at, track, ripple } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).map(|at| Action::TrimIn {
                track,
                at,
                ripple: *ripple,
            })
        })),
        Cmd::TrimOut { at, track, ripple } => {
            Some(cli_index(*track, "--track").and_then(|track| {
                cli_time(at).map(|at| Action::TrimOut {
                    track,
                    at,
                    ripple: *ripple,
                })
            }))
        }
        Cmd::Split { at, track } => Some(
            cli_index(*track, "--track")
                .and_then(|track| cli_time(at).map(|at| Action::Split { track, at })),
        ),
        Cmd::Gap {
            at,
            frames,
            dur,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let at = cli_time(at)?;
            let len = match (frames, dur) {
                (Some(_), Some(_)) => return Err("use either --frames or --dur".to_string()),
                (Some(n), None) => GapLen::Frames(*n),
                (None, Some(d)) => cli_time(d).map(GapLen::Seconds)?,
                (None, None) => GapLen::Frames(1),
            };
            match len {
                GapLen::Frames(n) if n < 1 => {
                    return Err("a gap needs at least one frame".to_string());
                }
                GapLen::Seconds(s) if s <= 0.0 => {
                    return Err("a gap needs a positive length".to_string());
                }
                _ => {}
            }
            Ok(Action::InsertGap { track, at, len })
        })),
        Cmd::Delete {
            at,
            clip,
            seg,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_target(at, *clip, *seg).map(|target| Action::Delete {
                track,
                target,
                ripple: *ripple,
            })
        })),
        Cmd::Yank {
            at,
            clip,
            seg,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_target(at, *clip, *seg).map(|target| Action::Yank { track, target })
        })),
        Cmd::Paste { at, track, ripple } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).map(|at| Action::Paste {
                track,
                at,
                ripple: *ripple,
                segment: None,
            })
        })),
        Cmd::Extend {
            clip,
            seg,
            at,
            edge,
            to,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let target = cli_target(at, *clip, *seg)?;
            let edge = cli_edge(edge)?;
            let to = cli_time(to)?;
            Ok(Action::Extend {
                track,
                target,
                edge,
                to,
                ripple: *ripple,
            })
        })),
        Cmd::Remove { id, ripple } => Some(Ok(Action::Delete {
            track: 0,
            target: Target::Clip(*id),
            ripple: *ripple,
        })),
        Cmd::Speed { id, rate, track } => Some(cli_index(*track, "--track").and_then(|track| {
            parse_speed_arg(rate)
                .map_err(|e| e.to_string())
                .map(|rate| Action::SetSpeed {
                    track,
                    clip: *id,
                    rate,
                })
        })),
        Cmd::PurgeStumps { track } => Some(
            track
                .map(|n| cli_index(n, "--track"))
                .transpose()
                .map(|track| Action::PurgeStumps { track }),
        ),
        Cmd::Transition {
            kind,
            at,
            frames,
            dur,
            percent,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let at = cli_time(at)?;
            let kind = match kind.as_deref() {
                None | Some("dissolve") => TransitionKind::Dissolve,
                Some("fade") => TransitionKind::FadeBlack,
                Some(other) => {
                    return Err(format!("unknown kind {other:?} — use dissolve or fade"));
                }
            };
            let len = match (frames, dur, percent) {
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                    return Err("use only one of --frames, --dur, or --percent".to_string());
                }
                (Some(n), None, None) => TransitionLen::Frames(*n),
                (None, Some(d), None) => TransitionLen::Seconds(cli_time(d)?),
                (None, None, Some(p)) => {
                    if !p.is_finite() || *p <= 0.0 {
                        return Err(format!("transition percent must be positive (got {p})"));
                    }
                    TransitionLen::Percent(*p)
                }
                (None, None, None) => TransitionLen::Seconds(1.0),
            };
            Ok(Action::Transition {
                track,
                at,
                kind,
                len,
            })
        })),
        Cmd::Effect {
            kind,
            clip,
            track,
            index,
            w,
            h,
            left,
            right,
            top,
            bottom,
            x,
            y,
            brightness,
            contrast,
            saturation,
            path,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            match kind.as_str() {
                "rm" => Ok(Action::RemoveEffect {
                    track,
                    clip: *clip,
                    index: *index,
                }),
                "clear" => Ok(Action::ClearEffects { track, clip: *clip }),
                _ => effect_from_args(
                    kind,
                    EffectArgs {
                        w: *w,
                        h: *h,
                        left: *left,
                        right: *right,
                        top: *top,
                        bottom: *bottom,
                        x: *x,
                        y: *y,
                        brightness: *brightness,
                        contrast: *contrast,
                        saturation: *saturation,
                        path: path.clone(),
                    },
                )
                .map_err(|e| e.to_string())
                .map(|effect| Action::AddEffect {
                    track,
                    clip: *clip,
                    effect,
                    index: *index,
                }),
            }
        })),
        Cmd::Undo {} => Some(Ok(Action::Undo)),
        Cmd::Redo {} => Some(Ok(Action::Redo)),
        Cmd::Track { action } => match action {
            TrackCmd::Ls => None,
            TrackCmd::Add { kind } => Some(Ok(Action::TrackAdd {
                kind: match kind {
                    TrackKindArg::Video => TrackKind::Video,
                    TrackKindArg::Audio => TrackKind::Audio,
                },
            })),
            TrackCmd::Mute { index } => Some(
                index
                    .map(|n| cli_index(n, "track index"))
                    .transpose()
                    .map(|track| Action::TrackMute {
                        track: track.unwrap_or(0),
                    }),
            ),
            TrackCmd::Rm { index } => {
                Some(cli_index(*index, "track index").map(|track| Action::TrackRm { track }))
            }
        },
        Cmd::Media { action } => match action {
            MediaCmd::Ls => None,
            MediaCmd::Rm { id } => Some(Ok(Action::AssetRm { id: *id })),
        },
        _ => None,
    }
}

/// Run one catalog action against the project file: load, apply through
/// the undo history, persist project + sidecar + register. A CLI edit is
/// undoable (`nle undo`) exactly like a TUI edit — same executor, same
/// history.
pub(crate) fn run_cli_action(project_path: &Path, action: &Action) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let mut history =
        edit::History::load(&edit::History::sidecar_path(project_path)).unwrap_or_default();
    let mut register = load_register(project_path);
    // The edit is "made from" its own position, so undoing it returns
    // there (the TUI passes the playhead instead — its session state).
    let from = match action {
        Action::TrimIn { at, .. }
        | Action::TrimOut { at, .. }
        | Action::Split { at, .. }
        | Action::InsertGap { at, .. }
        | Action::Paste { at, .. } => *at,
        Action::Delete { target, .. } | Action::Yank { target, .. } => match target {
            Target::At(t) => *t,
            _ => 0.0,
        },
        _ => 0.0,
    };
    let outcome = run_action(&mut project, &mut history, &mut register, action, from)
        .map_err(|e| anyhow::Error::msg(edit_message(&e)))?;
    project.save(project_path)?;
    history.save(&edit::History::sidecar_path(project_path))?;
    save_register(project_path, register.as_ref())?;
    log::line(&format!("action {}", outcome.desc));
    match outcome.playhead {
        Some(f) => say!("✓ {} (playhead f{f})", outcome.desc),
        None => say!("✓ {}", outcome.desc),
    }
    Ok(())
}

/// What a work function produced: warnings for stderr (or the log) and
/// the summary line the caller shows. Keeps the TUI free of stray prints
/// — a `say!` would draw over the terminal UI.
pub(crate) struct CommandReport {
    pub(crate) warnings: Vec<String>,
    pub(crate) summary: String,
}

impl CommandReport {
    pub(crate) fn new(summary: String) -> Self {
        Self {
            warnings: Vec::new(),
            summary,
        }
    }

    pub(crate) fn warn(&mut self, msg: String) {
        self.warnings.push(msg);
    }

    /// The CLI's rendering: warnings on stderr, summary on stdout.
    pub(crate) fn print(&self) {
        for w in &self.warnings {
            say_err!("{w}");
        }
        say!("{}", self.summary);
    }
}

pub(crate) fn asset_summary(a: &Asset) -> String {
    let mut s = match a.duration() {
        Some(d) => format!("{} s", format_seconds(&d)),
        None => "? s".to_string(),
    };
    if let Some((w, h)) = a.video_size() {
        s += &format!(" · {w}×{h}");
    }
    if let Some(fps) = a.fps() {
        s += &format!(" @ {}/{}", fps.numer(), fps.denom());
    }
    s += if a.probe.has_audio() {
        " · audio ✓"
    } else {
        " · no audio"
    };
    s
}
