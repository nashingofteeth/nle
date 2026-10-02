//! nle-ffmpeg: everything that talks to ffmpeg/ffprobe.
//!
//! The rule here: this crate knows *media* (codecs, processes, devices)
//! but nothing about commands or timelines beyond what it's handed.
//! nle-core knows timelines but never touches a process.

pub mod graph;
pub mod probe;
pub mod trim;

pub use graph::{compile_export, export, CompileError, ExportError, ExportPlan, ExportSpec};
// Media types live in core; re-exported here for callers that treat
// this crate as the media facade.
pub use nle_core::media::{RawFormat, RawProbe, RawStream};
pub use probe::{probe, ProbeError};
pub use trim::{trim, Encoder, FfmpegError, TrimError, TrimSpec};
