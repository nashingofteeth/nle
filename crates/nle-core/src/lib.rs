//! nle-core: the pure heart of the editor.
//!
//! Everything in here is *logic* — time math, media metadata, the EDL
//! data model and edit operations. No files are executed, no processes
//! spawned (the project save/load is I/O but pure-data I/O).
//! Keeping it pure means it can be unit-tested exhaustively and instantly.

pub mod edit;
pub mod media;
pub mod project;
pub mod time;
pub mod timeline;

pub use edit::{
    Atom, Batch, Change, EditError, EditOp, History, absorb_outward, delete_clip_by_id,
    delete_ripple, delete_under, extend_edge, extend_gap_end, paste, paste_ripple, split, trim_in,
    trim_in_ripple, trim_out, trim_out_ripple,
};
pub use media::{RawFormat, RawProbe, RawStream};
pub use project::{Asset, Project, ProjectError, VideoSettings};
pub use time::{Seconds, format_seconds, parse_rate, parse_time};
pub use timeline::{Clip, Timeline, TimelineError, Track, TrackKind};
