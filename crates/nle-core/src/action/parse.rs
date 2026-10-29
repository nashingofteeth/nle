//! The `:cmd` grammar: flags, defaults and the action parser. It is the same
//! grammar the CLI uses — `to_action` calls this, not a second parser.

use crate::edit::EditError;
use crate::time::{Seconds, frames_to_seconds, parse_signed_time, secs_f64};
use crate::timeline::{Effect, MAX_TIMELINE_SECONDS, TrackKind, TransitionKind};

use super::{Action, Edge, GapLen, Target, TransitionLen};

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

/// Boolean options (everything else takes a value).
const BOOL_FLAGS: &[&str] = &["ripple"];

/// Short flags the CLI uses, spelled `-o` in the `:cmd` line too
/// (`:export -o out.mp4`). Only the ones that exist on the CLI.
const SHORT_FLAGS: &[(&str, &str)] = &[("o", "out")];

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

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Unknown(v) => write!(f, "unknown command: {v}"),
            ParseError::Invalid(m) => f.write_str(m),
        }
    }
}

/// One command line: its verb, `--flag` options, and bare positional
/// words (`:speed 50`, `:track mute 2`). The grammar mirrors the CLI
/// flags — an agent that knows `nle delete --at 3 --ripple` knows
/// `:delete --at 3 --ripple`. Public so the front-ends' own (non-edit)
/// commands tokenize the same way.
pub struct Flags {
    opts: Vec<(String, Option<String>)>,
    pos: Vec<String>,
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
pub const EFFECT_USAGE: &str = ":effect [--clip N] [--track N] [--index N] \
     crop --left P --right P --top P --bottom P (%: 0 = edge, 100 = opposite) | \
     scale --w P --h P (%: 100 = unchanged) | \
     offset --x P --y P (% of the way to the edge; ±100 = off-screen) | \
     eq [--brightness --contrast --saturation] | lut --path P | \
     rm [--index N] | clear (one per kind: adding a kind again replaces it; \
     --index N replaces that row instead of appending)";

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
        // The values ride straight into the project JSON, so they must be
        // bounded: a non-finite float serializes as `null` and a project
        // that can no longer load, and anything outside ffmpeg's own
        // accepted ranges would merely fail the render later. Both are
        // refused here, where the CLI and the TUI form meet.
        "eq" => {
            let bounded = |v: Option<f64>, n: &str, default: f64, min: f64, max: f64| {
                let v = v.unwrap_or(default);
                if !v.is_finite() || !(min..=max).contains(&v) {
                    return Err(ParseError::Invalid(format!("{n} must be {min}–{max}")));
                }
                Ok(v)
            };
            Ok(Effect::Eq {
                brightness: bounded(a.brightness, "eq brightness", 0.0, -1.0, 1.0)?,
                contrast: bounded(a.contrast, "eq contrast", 1.0, -1000.0, 1000.0)?,
                saturation: bounded(a.saturation, "eq saturation", 1.0, 0.0, 3.0)?,
            })
        }
        "lut" => Ok(Effect::Lut {
            path: a
                .path
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| ParseError::Invalid("effect needs --path".to_string()))?,
        }),
        _ => Err(ParseError::Invalid(EFFECT_USAGE.to_string())),
    }
}

/// A saturating frame count divided by a non-integer fps would multiply
/// into overflow inside num-rational *before* `add_transition`'s own
/// "whole frames, at least one" check sees it: `--frames 2^63-1` panicked
/// the debug binary ("attempt to multiply with overflow") and wrapped into
/// a wrong-length blend in release. Land the request on the frame count
/// the percent form bounds, and refuse the range ends there.
pub(super) fn transition_frames(frames: i64, fps: &Seconds) -> Result<Seconds, EditError> {
    if frames < 1 {
        return Err(EditError::TooShort);
    }
    let limit = MAX_TIMELINE_SECONDS as f64 * secs_f64(fps);
    if frames as f64 > limit {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    Ok(frames_to_seconds(frames, *fps))
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
                    let index = opt_usize(&mut f, "index")?;
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
                        index,
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

pub const TRACK_USAGE: &str =
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
