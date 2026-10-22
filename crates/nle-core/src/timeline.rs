//! The timeline: tracks of sequential segments — clips and gaps.
//!
//! A track is a *sequence*: segments play back-to-back, and a clip's
//! start is DERIVED (the sum of the durations before it). Overlaps are
//! unrepresentable; gaps — leading, interior, trailing — are ordinary
//! segments, not special state. Removing a segment IS a ripple; replacing
//! a clip with a gap IS "leave a gap".

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{Seconds, checked_add, checked_div, checked_sub, format_seconds, rational_ser};

#[derive(Debug, Error)]
pub enum TimelineError {
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no segment at index {0}")]
    SegmentNotFound(usize),
    #[error("timeline corrupt: {0}")]
    Corrupt(String),
    #[error("a track is {len} s long — the timeline limit is {limit} s")]
    TooLong { len: String, limit: i64 },
}

/// The longest timeline we accept, in seconds — a day. Not a workflow
/// limit (nobody edits a day): it is the guard that keeps duration sums
/// (i64 rationals) from overflowing, which wraps a track's length
/// negative in release builds and panics in debug ones.
pub const MAX_TIMELINE_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

/// A per-clip video effect, applied in export after the retime and before
/// the geometry normalize. Typed (not raw ffmpeg fragments) so it stays
/// serializable and safe to hand to the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// `crop`, as percentages of the source frame per edge: `left` moves the
    /// left edge toward the right, `right` the right edge toward the left (and
    /// the same for `top`/`bottom`). 0 is the frame edge, 100 the opposite
    /// edge; the two sides of an axis must sum to less than 100. The kept
    /// picture stays where it is; the trimmed strips become transparent, so
    /// an upper track shows through them (black when nothing is below).
    Crop {
        left: f64,
        right: f64,
        top: f64,
        bottom: f64,
    },
    /// `scale`, as percentages of the source frame (100 = unchanged).
    Scale { w: f64, h: f64 },
    /// `eq=brightness=…:contrast=…:saturation=…`.
    Eq {
        brightness: f64,
        contrast: f64,
        saturation: f64,
    },
    /// `lut3d=file=…`.
    Lut { path: String },
}

impl Effect {
    /// The short name used by notices, `nle show`, and the effects panel.
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Crop { .. } => "crop",
            Effect::Scale { .. } => "scale",
            Effect::Eq { .. } => "eq",
            Effect::Lut { .. } => "lut",
        }
    }

    /// The parameters as one line — the panel's detail column.
    pub fn details(&self) -> String {
        match self {
            Effect::Crop {
                left,
                right,
                top,
                bottom,
            } => format!("l{left} r{right} t{top} b{bottom}"),
            Effect::Scale { w, h } => format!("{w}% × {h}%"),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => format!("b {brightness} · c {contrast} · s {saturation}"),
            // The full path is noise in a one-line row; the file name is the
            // part that identifies the LUT.
            Effect::Lut { path } => std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
        }
    }

    /// The ffmpeg filter fragment for this effect, applied to the frame as it
    /// stands at this point in the chain (`crop` and `scale` need it).
    pub fn filter(&self, frame: (u32, u32)) -> String {
        match self {
            Effect::Crop {
                left,
                right,
                top,
                bottom,
            } => {
                let (w, h, x, y) = crop_geometry(*left, *right, *top, *bottom, frame);
                let (fw, fh) = frame;
                if (w, h, x, y) == (fw, fh, 0, 0) {
                    return String::new(); // no inset: nothing to mask
                }
                // Keep the picture at its original place and size; the cut
                // strips are transparent. A later `overlay` (multi-track
                // export, or the monitor composite) reveals what is below,
                // while the classic single-track graph drops alpha to black.
                format!(
                    "format=yuva420p,crop={w}:{h}:{x}:{y},\
                     pad={fw}:{fh}:{x}:{y}:color=black@0"
                )
            }
            // Percent of the *current* frame, then padded/cropped back to it
            // so the canvas size is unchanged: 50% is a half-size picture on
            // black, 200% is a centre zoom. Without the pad/crop the export's
            // normalize scale would immediately undo a uniform scale.
            Effect::Scale { w, h } => scale_filter(*w, *h, frame),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => format!("eq=brightness={brightness}:contrast={contrast}:saturation={saturation}"),
            // The path is quoted: `:`/`,` in it would otherwise split the
            // filtergraph, and a literal `'` is escaped the ffmpeg way.
            Effect::Lut { path } => format!("lut3d=file='{}'", path.replace('\'', r"'\''")),
        }
    }

    /// The frame size after this effect. Every current effect keeps its
    /// canvas — a crop masks in place, a scale pads/crops back to the frame —
    /// so the tracker stays for the next effect that really does resize.
    pub fn frame_after(&self, frame: (u32, u32)) -> (u32, u32) {
        frame
    }
}

/// Resolve percentage edge insets against the source frame: each edge moves
/// that percent of the frame dimension toward its opposite edge (0 = the
/// frame edge, 100 = the opposite edge). The box is clamped to the frame so a
/// hand-edited project can't crop past it.
fn crop_geometry(
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    (fw, fh): (u32, u32),
) -> (u32, u32, u32, u32) {
    let inset = |size: u32, pct: f64| ((size as f64 * pct / 100.0).round() as u32).min(size);
    let (l, r) = (inset(fw, left), inset(fw, right));
    let (t, b) = (inset(fh, top), inset(fh, bottom));
    let w = fw.saturating_sub(l.saturating_add(r)).max(2).min(fw.max(2));
    let h = fh.saturating_sub(t.saturating_add(b)).max(2).min(fh.max(2));
    let x = l.min(fw.saturating_sub(w));
    let y = t.min(fh.saturating_sub(h));
    (w, h, x, y)
}

/// Resize to `w`/`h` percent of the current frame, then pad (below 100%) or
/// crop (above 100%) back to it, centred. The pad is opaque black: `scale` is
/// a picture-in-picture, not a mask — `crop` is the effect that lets the layer
/// below show through.
fn scale_filter(w: f64, h: f64, (fw, fh): (u32, u32)) -> String {
    let size =
        |base: u32, pct: f64| ((base as f64 * pct / 100.0).round() as u32).clamp(2, u32::MAX / 4);
    let (sw, sh) = (size(fw, w), size(fh, h));
    if sw == fw && sh == fh {
        return String::new(); // 100% is a no-op; the caller drops empty filters
    }
    // A canvas big enough for both the scaled picture and the frame: pad the
    // smaller axis, then crop the larger back to the frame.
    let (cw, ch) = (sw.max(fw), sh.max(fh));
    let mut chain = format!("scale={sw}:{sh}");
    if cw != sw || ch != sh {
        chain += &format!(",pad={cw}:{ch}:{}:{}:black", (cw - sw) / 2, (ch - sh) / 2);
    }
    if cw != fw || ch != fh {
        chain += &format!(",crop={fw}:{fh}:{}:{}", (cw - fw) / 2, (ch - fh) / 2);
    }
    chain
}

/// One piece of media on a track. Its timeline position is derived from
/// the segments before it — a clip only owns its source range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u32,
    pub asset: u32,
    #[serde(with = "rational_ser")]
    pub asset_in: Seconds,
    #[serde(with = "rational_ser")]
    pub asset_out: Seconds,
    /// Playback rate relative to the source: timeline duration is
    /// `(asset_out - asset_in) / speed`. `1` is normal speed; a Kdenlive
    /// rate conform (e.g. 24 fps project over 29.97 fps media) is
    /// `project_rate / media_rate`. Never zero or negative.
    #[serde(with = "rational_ser")]
    pub speed: Seconds,
    /// Per-clip video effects, applied in order at export.
    #[serde(default)]
    pub effects: Vec<Effect>,
}

impl Clip {
    /// A normal-speed clip (`speed = 1`) with no effects.
    pub fn new(id: u32, asset: u32, asset_in: Seconds, asset_out: Seconds) -> Self {
        Self {
            id,
            asset,
            asset_in,
            asset_out,
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }
    }

    /// The source span the clip reads, before the speed factor.
    pub fn source_duration(&self) -> Seconds {
        self.asset_out - self.asset_in
    }

    /// The timeline span the clip occupies (source scaled by `speed`).
    pub fn duration(&self) -> Seconds {
        self.source_duration() / self.speed
    }

    /// Source time at a timeline offset inside the clip (timeline →
    /// source). Edits move in timeline time but `asset_in/out` live in
    /// source time, so crossing that boundary scales by `speed`.
    pub fn source_at(&self, timeline_offset: Seconds) -> Seconds {
        self.asset_in + timeline_offset * self.speed
    }

    /// Timeline offset of a source time inside the clip (source →
    /// timeline) — the inverse of [`Clip::source_at`].
    pub fn timeline_offset_of(&self, source: Seconds) -> Seconds {
        (source - self.asset_in) / self.speed
    }
}

/// A stretch of nothing: black + silence. Leading, interior, and
/// trailing space are all just Gap segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    #[serde(with = "rational_ser")]
    pub duration: Seconds,
}

/// Where one side of a transition comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransitionSource {
    /// A trimmed span of a pooled asset, in source seconds. Rendered into
    /// `dur` of timeline, so the side's rate is implied by the span.
    Clip {
        asset: u32,
        #[serde(with = "rational_ser")]
        src_in: Seconds,
        #[serde(with = "rational_ser")]
        src_out: Seconds,
        /// The neighbour's effects, copied when the side was built and kept in
        /// sync when the neighbour's list changes: the transition renders that
        /// neighbour's material, effects included.
        #[serde(default)]
        effects: Vec<Effect>,
    },
    /// Generated black — toward a gap or the timeline edge.
    Black,
}

impl TransitionSource {
    /// The pooled asset this side reads, if it is a clip side.
    pub fn asset(&self) -> Option<u32> {
        match self {
            TransitionSource::Clip { asset, .. } => Some(*asset),
            TransitionSource::Black => None,
        }
    }
}

/// The crossfade shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    /// `xfade=transition=fade` — blend the outgoing side into the incoming.
    Dissolve,
    /// `xfade=transition=fadeblack` — fade through black.
    FadeBlack,
}

/// A transition between two source sides, occupying `dur` of timeline. It
/// *owns* both source spans (as a clip owns its own), so it never dangles
/// when neighbours change. A transition needs at least one clip side —
/// black↔black is meaningless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub id: u32,
    #[serde(with = "rational_ser")]
    pub dur: Seconds,
    pub kind: TransitionKind,
    /// Outgoing (left) side.
    pub a: TransitionSource,
    /// Incoming (right) side.
    pub b: TransitionSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    Clip(Clip),
    Gap(Gap),
    Transition(Transition),
}

impl Segment {
    pub fn duration(&self) -> Seconds {
        match self {
            Segment::Clip(c) => c.duration(),
            Segment::Gap(g) => g.duration,
            Segment::Transition(t) => t.dur,
        }
    }

    /// [`Segment::duration`], or `None` if the arithmetic would overflow.
    /// `validate` sums with this: a hand-edited project (a speed near zero, an
    /// extreme span) must be rejected, not panic the caller.
    fn checked_duration(&self) -> Option<Seconds> {
        match self {
            Segment::Clip(c) => checked_div(&checked_sub(&c.asset_out, &c.asset_in)?, &c.speed),
            Segment::Gap(g) => Some(g.duration),
            Segment::Transition(t) => Some(t.dur),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// A muted track contributes nothing to the mix. Defaults to `false` so
    /// a track written before this field existed still parses.
    #[serde(default)]
    pub muted: bool,
    pub segments: Vec<Segment>,
}

impl Track {
    /// A new, empty track of `kind`.
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            muted: false,
            segments: Vec::new(),
        }
    }

    /// Total duration: the sum of segment durations.
    pub fn end(&self) -> Seconds {
        self.segments
            .iter()
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// Start time of the segment at `index` (prefix sum of durations).
    pub fn start_of(&self, index: usize) -> Seconds {
        self.segments
            .iter()
            .take(index)
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// Index of the segment containing `t` (start ≤ t < end).
    pub fn index_at(&self, t: Seconds) -> Option<usize> {
        let mut acc = Seconds::from_integer(0);
        for (i, s) in self.segments.iter().enumerate() {
            let end = acc + s.duration();
            if acc <= t && t < end {
                return Some(i);
            }
            acc = end;
        }
        None
    }

    /// The clip segment containing `t`, if any.
    pub fn clip_at(&self, t: Seconds) -> Option<&Clip> {
        let i = self.index_at(t)?;
        match &self.segments[i] {
            Segment::Clip(c) => Some(c),
            Segment::Gap(_) | Segment::Transition(_) => None,
        }
    }

    /// Index of the clip segment with the given id.
    pub fn clip_index(&self, id: u32) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| matches!(s, Segment::Clip(c) if c.id == id))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub tracks: Vec<Track>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// A timeline with one empty video track.
    pub fn new() -> Self {
        Self {
            tracks: vec![Track::new(TrackKind::Video)],
        }
    }

    /// Track at `ti` (by position — the stacking order).
    pub fn track(&self, ti: usize) -> Option<&Track> {
        self.tracks.get(ti)
    }

    pub fn track_mut(&mut self, ti: usize) -> Option<&mut Track> {
        self.tracks.get_mut(ti)
    }

    /// All video tracks, in stacking order.
    pub fn video_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Video)
    }

    /// All audio tracks, in stacking order.
    pub fn audio_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    /// Append a track of `kind`; returns its index.
    pub fn add_track(&mut self, kind: TrackKind) -> usize {
        self.tracks.push(Track::new(kind));
        self.tracks.len() - 1
    }

    /// The first video track. Editing still assumes a single video track
    /// until the ops take an explicit index (M4 step 2).
    pub fn video_track(&self) -> Option<&Track> {
        self.video_tracks().next()
    }

    pub fn video_track_mut(&mut self) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.kind == TrackKind::Video)
    }

    /// End of the timeline (across all tracks).
    pub fn end(&self) -> Seconds {
        self.tracks
            .iter()
            .map(|t| t.end())
            .max()
            .unwrap_or_else(|| Seconds::from_integer(0))
    }

    /// Remove a clip by id, searching all tracks.
    pub fn remove_clip(&mut self, id: u32) -> Result<(), TimelineError> {
        for track in &mut self.tracks {
            if let Some(i) = track.clip_index(id) {
                track.segments.remove(i);
                return Ok(());
            }
        }
        Err(TimelineError::ClipNotFound(id))
    }

    /// Structural sanity: at least one video track, positive spans and
    /// gaps. Valid asset refs are the PROJECT's job (it owns the pool).
    /// Overlaps are unrepresentable.
    pub fn validate(&self) -> Result<(), TimelineError> {
        if self.video_tracks().next().is_none() {
            return Err(TimelineError::NoVideoTrack);
        }
        let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
        for (ti, track) in self.tracks.iter().enumerate() {
            // Each segment's own invariants first: a zero `speed` must not
            // reach the length arithmetic below (it divides by it).
            for (si, seg) in track.segments.iter().enumerate() {
                let bad =
                    |what: &str| TimelineError::Corrupt(format!("track {ti} segment {si}: {what}"));
                match seg {
                    Segment::Clip(c) => {
                        if c.asset_out <= c.asset_in {
                            return Err(bad("clip has a non-positive source span"));
                        }
                        if c.speed <= Seconds::from_integer(0) {
                            return Err(bad("clip has a non-positive speed"));
                        }
                    }
                    Segment::Gap(g) => {
                        if g.duration <= Seconds::from_integer(0) {
                            return Err(bad("gap has a non-positive duration"));
                        }
                    }
                    Segment::Transition(t) => {
                        if t.dur <= Seconds::from_integer(0) {
                            return Err(bad("transition has a non-positive duration"));
                        }
                        if t.a.asset().is_none() && t.b.asset().is_none() {
                            return Err(bad("transition has no clip side"));
                        }
                        for side in [&t.a, &t.b] {
                            if let TransitionSource::Clip {
                                src_in, src_out, ..
                            } = side
                                && src_out <= src_in
                            {
                                return Err(bad("transition side has a non-positive source span"));
                            }
                        }
                    }
                }
            }
            // Now the length. The durations and their sum are exact rationals,
            // so a hand-edited value that overflows either is corrupt — not
            // "long", and certainly not a panic.
            let mut end = Seconds::from_integer(0);
            for (si, seg) in track.segments.iter().enumerate() {
                let Some(d) = seg.checked_duration() else {
                    return Err(TimelineError::Corrupt(format!(
                        "track {ti} segment {si}: duration overflows"
                    )));
                };
                end = checked_add(&end, &d).ok_or_else(|| {
                    TimelineError::Corrupt(format!("track {ti}: length overflows"))
                })?;
            }
            if end > limit {
                // Not "corrupt": an edit simply asked for too much.
                return Err(TimelineError::TooLong {
                    len: format_seconds(&end),
                    limit: MAX_TIMELINE_SECONDS,
                });
            }
        }
        Ok(())
    }
}

/// Format helper re-exported for op descriptions.
#[allow(unused_imports)]
use format_seconds as _fmt_unused;

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u32, dur: i64) -> Segment {
        Segment::Clip(Clip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    }

    fn gap(dur: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(dur),
        })
    }

    #[test]
    fn source_mapping_scales_by_speed() {
        // 2× clip reading source [1, 5): 1s of timeline = 2s of source.
        let c = Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        };
        assert_eq!(
            c.source_at(Seconds::from_integer(1)),
            Seconds::from_integer(3)
        );
        assert_eq!(
            c.timeline_offset_of(Seconds::from_integer(3)),
            Seconds::from_integer(1)
        );
    }

    #[test]
    fn end_is_the_sum_of_segments() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.end(), Seconds::from_integer(6));
        assert_eq!(t.start_of(0), Seconds::from_integer(0));
        assert_eq!(t.start_of(1), Seconds::from_integer(2));
        assert_eq!(t.start_of(2), Seconds::from_integer(3));
        assert_eq!(t.start_of(3), Seconds::from_integer(6));
    }

    #[test]
    fn index_at_and_clip_at() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.index_at(Seconds::from_integer(0)), Some(0));
        assert_eq!(t.index_at(Seconds::from_integer(2)), Some(1), "gap start");
        assert_eq!(
            t.index_at(Seconds::from_integer(3)),
            Some(2),
            "the clip after the gap"
        );
        assert_eq!(
            t.index_at(Seconds::from_integer(6)),
            None,
            "the end belongs to no segment"
        );
        assert!(t.clip_at(Seconds::from_integer(4)).is_some());
        assert!(
            t.clip_at(Seconds::from_integer(2)).is_none(),
            "inside a gap"
        );
        assert_eq!(t.clip_index(2), Some(2));
        assert_eq!(t.clip_index(9), None);
    }

    #[test]
    fn validate_rejects_bad_spans() {
        let mut p_timeline = Timeline::new();
        p_timeline.tracks[0].segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(2),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        assert!(p_timeline.validate().is_err());
        p_timeline.tracks[0].segments = vec![gap(0)];
        assert!(p_timeline.validate().is_err());
    }

    #[test]
    fn tracks_are_addressed_by_index_and_kind() {
        let mut t = Timeline::new();
        assert_eq!(t.tracks.len(), 1, "one video track to start");
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 0);
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        assert!(t.track(1).is_none());

        let ai = t.add_track(TrackKind::Audio);
        assert_eq!(ai, 1, "add_track returns the new index");
        assert_eq!(t.track(1).unwrap().kind, TrackKind::Audio);
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 1);
        // video_track() is the *first* video, not the last track.
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        t.track_mut(1).unwrap().muted = true;
        assert!(t.track(1).unwrap().muted);
    }

    #[test]
    fn muted_defaults_false_and_round_trips() {
        let t = Track::new(TrackKind::Audio);
        assert!(!t.muted, "new tracks are not muted");
        // A track written before the field existed parses as unmuted.
        let legacy: Track = serde_json::from_str(r#"{"kind":"Video","segments":[]}"#).unwrap();
        assert!(!legacy.muted);
        // And it round-trips once set.
        let mut t = Track::new(TrackKind::Video);
        t.muted = true;
        let json = serde_json::to_string(&t).unwrap();
        let back: Track = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn validate_requires_a_video_track() {
        let no_video = Timeline {
            tracks: vec![Track::new(TrackKind::Audio)],
        };
        assert!(matches!(
            no_video.validate(),
            Err(TimelineError::NoVideoTrack)
        ));

        let mut ok = Timeline::new();
        ok.add_track(TrackKind::Audio);
        assert!(ok.validate().is_ok());
    }

    fn trans(dur: i64, a: TransitionSource) -> Segment {
        Segment::Transition(Transition {
            id: 1,
            dur: Seconds::from_integer(dur),
            kind: TransitionKind::Dissolve,
            a,
            b: TransitionSource::Black,
        })
    }

    #[test]
    fn validate_rejects_bad_transitions() {
        let clip = |in_: i64, out: i64| TransitionSource::Clip {
            asset: 1,
            src_in: Seconds::from_integer(in_),
            src_out: Seconds::from_integer(out),
            effects: Vec::new(),
        };
        let mut t = Timeline::new();

        t.tracks[0].segments = vec![trans(1, TransitionSource::Black)];
        assert!(t.validate().is_err(), "needs a clip side");

        t.tracks[0].segments = vec![trans(0, clip(0, 1))];
        assert!(t.validate().is_err(), "non-positive duration");

        t.tracks[0].segments = vec![trans(1, clip(2, 2))];
        assert!(t.validate().is_err(), "non-positive source span");

        t.tracks[0].segments = vec![trans(1, clip(0, 1))];
        assert!(t.validate().is_ok());
    }

    /// Hand-edited arithmetic that the rational can't hold is rejected, not a
    /// panic: `validate` used to sum durations before checking them (a zero
    /// `speed` divided by zero) and to add with the panicking operator.
    #[test]
    fn validate_rejects_arithmetic_it_cannot_hold() {
        let mut t = Timeline::new();
        let clip = |id, out: Seconds, speed: Seconds| {
            Segment::Clip(Clip {
                id,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: out,
                speed,
                effects: Vec::new(),
            })
        };
        let one = Seconds::from_integer(1);
        let zero = Seconds::from_integer(0);

        t.tracks[0].segments = vec![clip(1, Seconds::from_integer(3), zero)];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("speed")),
            "a zero speed is caught before the length sums"
        );

        // Two tiny slivers whose exact sum overflows the numerator.
        t.tracks[0].segments = vec![
            clip(1, Seconds::new(1, i64::MAX), one),
            clip(2, Seconds::new(1, i64::MAX - 1), one),
        ];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("overflows")),
            "an overflowing sum is corrupt, not a panic"
        );

        // A single duration that can't be represented at all.
        t.tracks[0].segments = vec![clip(1, Seconds::from_integer(3), Seconds::new(1, i64::MAX))];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("overflows")),
            "an overflowing duration is corrupt, not a panic"
        );
    }

    #[test]
    fn a_lut_path_with_filter_metacharacters_is_quoted() {
        assert_eq!(
            Effect::Lut {
                path: "/luts/a,b:c.cube".into()
            }
            .filter((0, 0)),
            "lut3d=file='/luts/a,b:c.cube'",
            "a comma or colon must not split the graph"
        );
        assert_eq!(
            Effect::Lut {
                path: "/luts/it's.cube".into()
            }
            .filter((0, 0)),
            r"lut3d=file='/luts/it'\''s.cube'",
            "a quote is escaped the ffmpeg way"
        );
    }
}
