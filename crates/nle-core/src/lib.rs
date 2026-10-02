//! nle-core: the pure heart of the editor.
//!
//! Everything in here is *logic* — time math, (later) the EDL data model
//! and edit operations. No files, no processes, no ffmpeg.
//! Keeping it pure means it can be unit-tested exhaustively and instantly.

pub mod time;

pub use time::{format_seconds, parse_rate, parse_time, Seconds};
