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
    /// `scale`, as percentages of the picture at this point in the chain
    /// (100 = unchanged). Below 100% it shrinks about its centre and pads
    /// transparently, so a lower track shows through; above 100% is a zoom.
    Scale { w: f64, h: f64 },
    /// `offset`, as percentages of the distance from the picture's current
    /// position to the edge: `x` moves right (negative left), `y` moves down
    /// (negative up). 0 is no move and ±100 puts it fully off-screen; the
    /// vacated area is transparent, so a lower track shows through.
    Offset { x: f64, y: f64 },
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
            Effect::Offset { .. } => "offset",
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
            Effect::Offset { x, y } => format!("x {x}% · y {y}%"),
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

    /// The ffmpeg filters for a whole chain, comma-joined (empty when nothing
    /// renders). The picture box is threaded through: `scale` resizes the
    /// current box about its centre, `crop` insets the box's edges, and colour
    /// effects leave it alone. Anything outside the box is transparent, so a
    /// lower track shows through (black when none does — the classic
    /// single-track graph drops alpha at the end).
    pub fn chain(effects: &[Effect], canvas: (u32, u32)) -> String {
        let mut content = Content::full(canvas);
        let mut parts: Vec<String> = Vec::new();
        for effect in effects {
            let (filter, next) = effect.apply(canvas, content);
            if !filter.is_empty() {
                parts.push(filter);
            }
            content = next;
        }
        parts.join(",")
    }

    /// Does the chain leave any transparent canvas? The monitor composites the
    /// layer below when it does; the export's overlay handles it directly.
    pub fn masks(effects: &[Effect], canvas: (u32, u32)) -> bool {
        let mut content = Content::full(canvas);
        for effect in effects {
            content = effect.apply(canvas, content).1;
        }
        !content.is_full(canvas)
    }

    /// One effect: its filter and the picture box it leaves behind. Computing
    /// both together keeps the emitted geometry and the tracked box from
    /// drifting apart.
    fn apply(&self, canvas: (u32, u32), content: Content) -> (String, Content) {
        match self {
            Effect::Crop {
                left,
                right,
                top,
                bottom,
            } => {
                if content.is_empty() {
                    return (String::new(), content); // already invisible
                }
                let next = crop_content(*left, *right, *top, *bottom, content);
                if next == content {
                    return (String::new(), content); // no inset: nothing to mask
                }
                let (fw, fh) = canvas;
                let (x, y) = (next.x, next.y);
                (
                    format!(
                        "format=yuva420p,crop={}:{}:{x}:{y},\
                         pad={fw}:{fh}:{x}:{y}:color=black@0",
                        next.w, next.h,
                    ),
                    next,
                )
            }
            Effect::Scale { w, h } => {
                if content.is_empty() {
                    (String::new(), content) // already invisible
                } else {
                    scale_content(*w, *h, canvas, content)
                }
            }
            Effect::Offset { x, y } => offset_content(*x, *y, canvas, content),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => (
                format!("eq=brightness={brightness}:contrast={contrast}:saturation={saturation}"),
                content,
            ),
            // The path is quoted: `:`/`,` in it would otherwise split the
            // filtergraph, and a literal `'` is escaped the ffmpeg way.
            Effect::Lut { path } => (
                format!("lut3d=file='{}'", path.replace('\'', r"'\''")),
                content,
            ),
        }
    }
}

/// The visible picture inside the effect canvas, in canvas pixels. `x`/`y`
/// are non-negative and the box is clipped to the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Content {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Content {
    fn full((w, h): (u32, u32)) -> Self {
        Self {
            x: 0,
            y: 0,
            w: i64::from(w),
            h: i64::from(h),
        }
    }

    /// No picture left on the canvas: the frame is fully transparent.
    fn empty() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    fn is_full(&self, canvas: (u32, u32)) -> bool {
        *self == Self::full(canvas)
    }
}

/// A frame that is entirely transparent black. Used when a geometry effect
/// leaves nothing on the canvas (e.g. offset 100%). Zeroing RGB as well as
/// alpha matters: the single-track export drops alpha to black.
fn transparent_filter() -> String {
    "format=yuva420p,colorchannelmixer=rr=0:gg=0:bb=0:aa=0".to_string()
}

/// Inset `content` by percentages of its *own* width/height: each edge moves
/// that percent of the box toward its opposite edge. Clamped so a hand-edited
/// project can't crop past the picture.
fn crop_content(left: f64, right: f64, top: f64, bottom: f64, content: Content) -> Content {
    if content.is_empty() {
        return content;
    }
    let inset =
        |size: i64, pct: f64| ((size as f64 * pct / 100.0).round() as i64).clamp(0, size.max(0));
    let (l, r) = (inset(content.w, left), inset(content.w, right));
    let (t, b) = (inset(content.h, top), inset(content.h, bottom));
    let w = (content.w - l - r).clamp(2, content.w.max(2));
    let h = (content.h - t - b).clamp(2, content.h.max(2));
    Content {
        x: content.x + l.min((content.w - w).max(0)),
        y: content.y + t.min((content.h - h).max(0)),
        w,
        h,
    }
}

/// Resize the current box to `w`/`h` percent of itself, about its centre. The
/// scaled picture is clipped to the canvas and padded transparently around it,
/// so a lower track shows through; above 100% it is a centre zoom.
fn scale_content(wpct: f64, hpct: f64, canvas: (u32, u32), content: Content) -> (String, Content) {
    if content.is_empty() {
        return (String::new(), content);
    }
    let (fw, fh) = (i64::from(canvas.0), i64::from(canvas.1));
    let size = |base: i64, pct: f64| {
        ((base as f64 * pct / 100.0).round() as i64).clamp(2, i64::from(u32::MAX / 4))
    };
    let (sw, sh) = (size(content.w, wpct), size(content.h, hpct));
    // Keep the box's centre (doubled, so an odd size stays integral).
    let sx = (2 * content.x + content.w - sw) / 2;
    let sy = (2 * content.y + content.h - sh) / 2;
    // Clip the scaled picture to the canvas.
    let ix = sx.max(0);
    let iy = sy.max(0);
    let w = (sx + sw).min(fw) - ix;
    let h = (sy + sh).min(fh) - iy;
    if w < 2 || h < 2 {
        return (transparent_filter(), Content::empty());
    }
    let next = Content {
        x: ix.min(fw - w),
        y: iy.min(fh - h),
        w,
        h,
    };
    // 100% (or a rounding no-op) renders nothing.
    if sw == content.w && sh == content.h && next == content {
        return (String::new(), content);
    }
    let mut chain = String::from("format=yuva420p");
    if !content.is_full(canvas) {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            content.w, content.h, content.x, content.y
        );
    }
    chain += &format!(",scale={sw}:{sh}");
    // The scaled picture can stick out of the canvas: crop the overflow.
    if next.x != sx || next.y != sy || next.w != sw || next.h != sh {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            next.w,
            next.h,
            next.x - sx,
            next.y - sy
        );
    }
    if !next.is_full(canvas) {
        chain += &format!(",pad={fw}:{fh}:{}:{}:color=black@0", next.x, next.y);
    }
    (chain, next)
}

/// Move the current box toward an edge by `x`/`y` percent of the distance to
/// it. 0 is no move, ±100 puts the picture fully off-canvas (a transparent
/// frame); intermediate values leave the rest of the picture on screen.
fn offset_content(xpct: f64, ypct: f64, canvas: (u32, u32), content: Content) -> (String, Content) {
    if content.is_empty() {
        return (String::new(), content);
    }
    let (fw, fh) = (i64::from(canvas.0), i64::from(canvas.1));
    // Right/up use the room to that edge; left/down use the room to the
    // other one, so ±100 always reaches the edge from where the picture is.
    let dx = if xpct >= 0.0 {
        ((fw - content.x) as f64 * xpct / 100.0).round() as i64
    } else {
        -(((content.x + content.w) as f64) * -xpct / 100.0).round() as i64
    };
    let dy = if ypct >= 0.0 {
        ((fh - content.y) as f64 * ypct / 100.0).round() as i64
    } else {
        -(((content.y + content.h) as f64) * -ypct / 100.0).round() as i64
    };
    if dx == 0 && dy == 0 {
        return (String::new(), content);
    }
    // The casts saturate (a hand-edited `1e300` percent becomes
    // `i64::MAX`), so the i64 adds must not assume the add stays in range:
    // `x + i64::MAX` overflow-panicked the debug build and wrapped into
    // garbage crop/pad coordinates in release. Saturate instead — the
    // result is "the picture moved to or past the edge", which is what a
    // huge percentage means.
    let nx = content.x.saturating_add(dx);
    let ny = content.y.saturating_add(dy);
    // The part of the moved picture still on the canvas.
    let ix = nx.max(0);
    let iy = ny.max(0);
    let vw = nx.saturating_add(content.w).min(fw).saturating_sub(ix);
    let vh = ny.saturating_add(content.h).min(fh).saturating_sub(iy);
    if vw < 2 || vh < 2 {
        // Fully (or within a pixel) off canvas: a transparent layer. At
        // ±100% of the canvas the picture is entirely outside.
        return (transparent_filter(), Content::empty());
    }
    // Source offset inside the picture and destination on the canvas.
    let sx = ix.saturating_sub(nx);
    let sy = iy.saturating_sub(ny);
    let next = Content {
        x: ix,
        y: iy,
        w: vw,
        h: vh,
    };
    let mut chain = String::from("format=yuva420p");
    if !content.is_full(canvas) {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            content.w, content.h, content.x, content.y
        );
    }
    if vw != content.w || vh != content.h {
        chain += &format!(",crop={vw}:{vh}:{sx}:{sy}");
    }
    chain += &format!(",pad={fw}:{fh}:{}:{}:color=black@0", next.x, next.y);
    (chain, next)
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
            Effect::chain(
                &[Effect::Lut {
                    path: "/luts/a,b:c.cube".into()
                }],
                (0, 0)
            ),
            "lut3d=file='/luts/a,b:c.cube'",
            "a comma or colon must not split the graph"
        );
        assert_eq!(
            Effect::chain(
                &[Effect::Lut {
                    path: "/luts/it's.cube".into()
                }],
                (0, 0)
            ),
            r"lut3d=file='/luts/it'\''s.cube'",
            "a quote is escaped the ffmpeg way"
        );
    }
}
