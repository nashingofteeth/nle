//! Media metadata: the parsed ffprobe report as typed data.
//!
//! These types live in nle-core (pure data, no processes) because the
//! project file embeds them — that way re-importing is free and the
//! media pool never needs to re-probe old files.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::time::{parse_rate, parse_time, Seconds};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawProbe {
    #[serde(default)]
    pub streams: Vec<RawStream>,
    #[serde(default)]
    pub format: RawFormat,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawFormat {
    #[serde(default)]
    pub format_name: String,
    /// Seconds as a string ("12.500000") — ffprobe's quirk, we reparse.
    pub duration: Option<String>,
    /// Total size in bytes, as a string.
    pub size: Option<String>,
    /// Overall bit rate (bits/second), as a string.
    pub bit_rate: Option<String>,
    /// Container tags: major_brand, encoder, ...
    pub tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawStream {
    pub index: u32,
    /// "video" | "audio" | "subtitle" | "data"
    pub codec_type: Option<String>,
    /// "h264", "aac", ...
    pub codec_name: Option<String>,
    pub profile: Option<String>,
    // video
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Frame rate as a fraction, e.g. "30000/1001".
    pub r_frame_rate: Option<String>,
    pub pix_fmt: Option<String>,
    // audio
    /// Samples per second as a string ("48000").
    pub sample_rate: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    // either
    pub bit_rate: Option<String>,
}

impl RawProbe {
    pub fn stream_of_kind(&self, kind: &str) -> Option<&RawStream> {
        self.streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some(kind))
    }

    pub fn video(&self) -> Option<&RawStream> {
        self.stream_of_kind("video")
    }

    pub fn audio(&self) -> Option<&RawStream> {
        self.stream_of_kind("audio")
    }

    pub fn has_video(&self) -> bool {
        self.video().is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio().is_some()
    }

    /// Container duration in exact seconds.
    pub fn duration(&self) -> Option<Seconds> {
        self.format.duration.as_deref().and_then(|d| parse_time(d).ok())
    }

    /// Frame rate of the first video stream.
    pub fn fps(&self) -> Option<Seconds> {
        self.video()
            .and_then(|v| v.r_frame_rate.as_deref())
            .and_then(parse_rate)
    }

    /// Resolution of the first video stream.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.video().and_then(|v| v.width.zip(v.height))
    }
}
