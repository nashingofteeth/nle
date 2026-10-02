//! nle-ffmpeg: everything that talks to ffmpeg/ffprobe.
//!
//! The rule here: this crate knows *media* (codecs, processes, devices)
//! but nothing about commands or timelines. nle-core knows timelines
//! but never touches a process. The CLI wires them together.

pub mod probe;
pub mod trim;

pub use probe::{probe, ProbeError, RawFormat, RawProbe, RawStream};
pub use trim::{trim, Encoder, TrimError, TrimSpec};
