//! Media metadata: the parsed ffprobe report as typed data.
//!
//! These types live in nle-core (pure data, no processes) because the
//! project file embeds them — that way re-importing is free and the
//! media pool never needs to re-probe old files.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::time::{Seconds, parse_rate, parse_time};

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
    /// Container tags: major_brand, encoder, ... A `BTreeMap` so the
    /// project file is *byte-stable*: a `HashMap`'s iteration order varies
    /// per process, which made identical projects diff against each other
    /// (noisy for humans, and for anything comparing files).
    pub tags: Option<BTreeMap<String, String>>,
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
        self.format
            .duration
            .as_deref()
            .and_then(|d| parse_time(d).ok())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A verbatim `ffprobe -print_format json -show_format -show_streams`
    /// report. Kept as text (not constructed) because the point of these
    /// tests is the *shape* ffprobe emits: string-valued numbers, a
    /// comma-joined `format_name`, stream fields we ignore.
    const REPORT: &str = r#"{
      "streams": [
        {
          "index": 0,
          "codec_name": "h264",
          "codec_type": "video",
          "profile": "High",
          "width": 1920,
          "height": 1080,
          "pix_fmt": "yuv420p",
          "r_frame_rate": "30000/1001",
          "bit_rate": "5000000"
        },
        {
          "index": 1,
          "codec_name": "aac",
          "codec_type": "audio",
          "sample_rate": "48000",
          "channels": 2,
          "channel_layout": "stereo",
          "bit_rate": "128000"
        }
      ],
      "format": {
        "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
        "duration": "12.500000",
        "size": "7812500",
        "bit_rate": "5000000",
        "tags": { "major_brand": "isom", "encoder": "Lavf60.16.100" }
      }
    }"#;

    fn parse(json: &str) -> RawProbe {
        serde_json::from_str(json).expect("ffprobe output should parse")
    }

    #[test]
    fn parses_an_ffprobe_report_into_exact_times() {
        let p = parse(REPORT);
        // ffprobe prints duration as a string; going through `f64` would
        // lose the exactness the whole model depends on.
        assert_eq!(p.duration(), Some(Seconds::new(25, 2)));
        assert_eq!(p.fps(), Some(Seconds::new(30000, 1001)));
        assert_eq!(p.size(), Some((1920, 1080)));
        assert!(p.has_video() && p.has_audio());
        assert_eq!(p.video().unwrap().codec_name.as_deref(), Some("h264"));
        assert_eq!(p.audio().unwrap().channels, Some(2));
        assert!(
            p.stream_of_kind("subtitle").is_none(),
            "no subtitle stream in this file"
        );
    }

    #[test]
    fn unknown_fields_are_ignored_so_a_newer_ffprobe_still_works() {
        let p = parse(r#"{"streams":[],"format":{"duration":"1","programs":[]}}"#);
        assert_eq!(p.duration(), Some(Seconds::from_integer(1)));
    }

    #[test]
    fn an_empty_report_has_nothing_not_a_panic() {
        let p = parse("{}");
        assert_eq!(p.duration(), None);
        assert_eq!(p.fps(), None);
        assert_eq!(p.size(), None);
        assert!(!p.has_video() && !p.has_audio());
    }

    #[test]
    fn a_missing_or_na_duration_is_none() {
        assert_eq!(parse(r#"{"format":{}}"#).duration(), None);
        // `-show_format` on a live/streaming source prints N/A.
        assert_eq!(parse(r#"{"format":{"duration":"N/A"}}"#).duration(), None);
        assert_eq!(parse(r#"{"format":{"duration":""}}"#).duration(), None);
    }

    #[test]
    fn a_zero_or_missing_rate_is_not_a_frame_rate() {
        // ffprobe reports 0/0 for a still image or a stream with no frames.
        assert_eq!(
            parse(r#"{"streams":[{"index":0,"codec_type":"video","r_frame_rate":"0/0"}]}"#).fps(),
            None
        );
        assert_eq!(
            parse(r#"{"streams":[{"index":0,"codec_type":"video"}]}"#).fps(),
            None
        );
    }

    #[test]
    fn size_needs_both_dimensions() {
        let p = parse(r#"{"streams":[{"index":0,"codec_type":"video","width":640}]}"#);
        assert_eq!(p.size(), None);
    }

    #[test]
    fn tag_order_in_the_input_does_not_change_the_output() {
        // The struct uses a BTreeMap precisely so two probes carrying the
        // same tags serialize byte-identically; a HashMap would make
        // `nile.json` diff against itself run to run.
        let one = parse(r#"{"format":{"tags":{"encoder":"x","major_brand":"isom"}}}"#);
        let two = parse(r#"{"format":{"tags":{"major_brand":"isom","encoder":"x"}}}"#);
        assert_eq!(one, two);
        assert_eq!(
            serde_json::to_string(&one).unwrap(),
            serde_json::to_string(&two).unwrap()
        );
    }

    #[test]
    fn a_probe_round_trips_through_the_project_file() {
        let p = parse(REPORT);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<RawProbe>(&json).unwrap(), p);
    }
}
