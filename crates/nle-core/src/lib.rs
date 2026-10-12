//! nle-core: the pure heart of the editor.
//!
//! Everything in here is *logic* — time math, media metadata, the EDL
//! data model and edit operations. No files are executed, no processes
//! spawned (the project save/load is I/O but pure-data I/O).
//! Keeping it pure means it can be unit-tested exhaustively and instantly.

pub mod action;
pub mod detect;
pub mod edit;
pub mod edl;
pub mod extend;
pub mod media;
pub mod otio;
pub mod project;
pub mod time;
pub mod timeline;

pub use action::{
    Action, Edge, GapLen, Outcome, ParseDefaults, ParseError, Register, Target, load_register,
    register_path, run_action, save_register, track_label,
};
pub use detect::{Span, reconstruct};
pub use edit::{
    Atom, Batch, Change, EditError, EditOp, History, absorb_outward, clamp_to_media,
    delete_clip_by_id, delete_ripple, delete_under, extend_edge, extend_gap_end, paste,
    paste_ripple, purge_stumps, set_speed, split, trim_in, trim_in_ripple, trim_out,
    trim_out_ripple,
};
pub use edl::{EdlDoc, EdlError, EdlEvent};
pub use extend::{
    Armed, CommitKind, armed_edge, clip_span, commit_batch, gap_span, plan_commit, resolve_armed,
};
pub use media::{RawFormat, RawProbe, RawStream};
pub use otio::{OtioClip, OtioDoc, OtioError, OtioItem, OtioTrack};
pub use project::{Asset, Project, ProjectError, VideoSettings};
pub use time::{
    Seconds, Timecode, format_seconds, frame_of, frame_to_secs, frames_to_seconds, grid_time,
    nominal_fps, parse_rate, parse_signed_time, parse_time, parse_timecode, secs_f64,
    timecode_to_frames,
};
pub use timeline::{
    Clip, Timeline, TimelineError, Track, TrackKind, Transition, TransitionKind, TransitionSource,
};
