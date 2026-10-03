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
    delete_clip, delete_gap, delete_gap_ripple, delete_ripple, paste, split, trim_gap_in,
    trim_gap_out, trim_gap_out_ripple, trim_in, trim_in_ripple, trim_out, trim_out_ripple,
    Batch, EditError, EditOp, History,
};
pub use media::{RawFormat, RawProbe, RawStream};
pub use project::{Asset, Project, ProjectError, VideoSettings};
pub use time::{format_seconds, parse_rate, parse_time, Seconds};
pub use timeline::{Clip, Timeline, TimelineError, Track, TrackKind};
