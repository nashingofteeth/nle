//! nle-ffmpeg: everything that talks to ffmpeg/ffprobe.
//!
//! The rule here: this crate knows *media* (codecs, processes, devices)
//! but nothing about commands or timelines beyond what it's handed.
//! nle-core knows timelines but never touches a process.

pub mod detect;
pub mod graph;
pub mod probe;
pub mod trim;

pub use detect::{DetectError, DetectOptions, Detection, detect};
pub use graph::{
    AudioCodec, CompileError, ExportError, ExportPlan, ExportSpec, compile_export, export,
    export_quiet, export_quiet_progress, export_range_quiet,
};
// Media types live in core; re-exported here for callers that treat
// this crate as the media facade.
pub use nle_core::media::{RawFormat, RawProbe, RawStream};
pub use probe::{ProbeError, probe};
pub use trim::{Encoder, FfmpegError, TrimError, TrimSpec, trim};
