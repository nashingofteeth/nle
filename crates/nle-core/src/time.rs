//! Rational time — exact fractions instead of floating point.
//!
//! A timeline stored as f64 seconds accumulates rounding error: edit a clip
//! thousands of times and the pieces drift apart. Fractions can't drift —
//! 30000/1001 fps stays 30000/1001, exactly, forever.

use num_rational::Rational64;
use thiserror::Error;

/// A span of time in exact seconds (numerator/denominator).
/// 12.5s = 25/2, one frame at 29.97fps = 1001/30000.
pub type Seconds = Rational64;

#[derive(Debug, Error, PartialEq)]
pub enum TimeError {
    #[error("time string is empty")]
    Empty,
    #[error("invalid time: {0:?}")]
    Invalid(String),
    #[error("expected at most hh:mm:ss, got too many ':' parts")]
    TooManyParts,
}

/// Parse "12", "12.5", "1:02", "0:00:01.250" into exact Seconds.
///
/// Hours/min/sec are folded left-to-right: (((h * 60) + m) * 60) + s.
pub fn parse_time(input: &str) -> Result<Seconds, TimeError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TimeError::Empty);
    }
    if input.starts_with('-') {
        return Err(TimeError::Invalid(input.to_string()));
    }
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() > 3 {
        return Err(TimeError::TooManyParts);
    }

    let mut total = Seconds::from_integer(0);
    for part in &parts {
        total = total * Seconds::from_integer(60) + parse_component(part)?;
    }
    Ok(total)
}

/// One ":"-separated component: whole units, optionally with a decimal part.
fn parse_component(part: &str) -> Result<Seconds, TimeError> {
    let invalid = || TimeError::Invalid(part.to_string());
    if part.is_empty() {
        return Err(invalid());
    }
    let (whole, frac) = match part.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (part, None),
    };
    let whole = if whole.is_empty() { "0" } else { whole };
    if !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let units: i64 = whole.parse().map_err(|_| invalid())?;

    match frac {
        None => Ok(Seconds::from_integer(units)),
        // Up to 9 decimal digits: nanosecond granularity, no precision loss.
        Some(f) if !f.is_empty() && f.len() <= 9 && f.bytes().all(|b| b.is_ascii_digit()) => {
            let denom: i64 = 10i64.pow(f.len() as u32);
            let num: i64 = f.parse().map_err(|_| invalid())?;
            units
                .checked_mul(denom)
                .and_then(|u| u.checked_add(num))
                .ok_or_else(invalid)
                .map(|n| Seconds::new(n, denom))
        }
        _ => Err(invalid()),
    }
}

/// Exact seconds → f64 for position math (playhead, boundaries, spans).
/// The float boundary every caller was open-coding as
/// `*t.numer() as f64 / *t.denom() as f64`.
pub fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

/// Timeline frame index for a position (the grid `snap`/history use).
pub fn frame_of(fps: Option<&Seconds>, t: f64) -> i64 {
    match fps {
        Some(f) => (t * secs_f64(f)).round() as i64,
        None => t.round() as i64,
    }
}

/// A history frame index back to timeline seconds.
pub fn frame_to_secs(fps: Option<&Seconds>, frame: i64) -> f64 {
    match fps {
        Some(f) => frame as f64 / secs_f64(f),
        None => frame as f64,
    }
}

/// Format for display and for ffmpeg arguments (25/2 → "12.5").
/// Math stays rational; we only touch f64 at this boundary.
pub fn format_seconds(t: &Seconds) -> String {
    let value = *t.numer() as f64 / *t.denom() as f64;
    let s = format!("{value:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Parse ffprobe's frame-rate strings like "30000/1001".
/// Returns None for unknown ("0/0") or malformed values.
pub fn parse_rate(s: &str) -> Option<Seconds> {
    let (n, d) = s.split_once('/')?;
    let n: i64 = n.parse().ok()?;
    let d: i64 = d.parse().ok()?;
    if n < 0 || d <= 0 {
        return None;
    }
    Some(Seconds::new(n, d))
}

/// A CMX3600-style timecode `HH:MM:SS:FF` (or `;` before the frame field
/// for drop-frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    pub h: u32,
    pub m: u32,
    pub s: u32,
    pub f: u32,
}

/// Parse `HH:MM:SS:FF` — the frame field may be introduced by `;` (a
/// drop-frame marker in many EDLs).
pub fn parse_timecode(input: &str) -> Result<Timecode, TimeError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TimeError::Empty);
    }
    let parts: Vec<&str> = input.split([':', ';']).collect();
    if parts.len() != 4 {
        return Err(TimeError::Invalid(input.to_string()));
    }
    let mut n = [0u32; 4];
    for (slot, part) in n.iter_mut().zip(&parts) {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(TimeError::Invalid(input.to_string()));
        }
        *slot = part
            .parse()
            .map_err(|_| TimeError::Invalid(input.to_string()))?;
    }
    Ok(Timecode {
        h: n[0],
        m: n[1],
        s: n[2],
        f: n[3],
    })
}

/// The nominal integer frame rate (29.97 → 30, 59.94 → 60), used to label
/// timecode fields.
pub fn nominal_fps(fps: Seconds) -> i64 {
    (fps.numer() + fps.denom() / 2) / fps.denom()
}

/// Convert a timecode to an absolute frame index at `fps`. Drop-frame
/// timecodes skip two (or four at 59.94) frame numbers per minute except
/// every tenth minute.
pub fn timecode_to_frames(tc: Timecode, fps: Seconds, drop_frame: bool) -> i64 {
    let nominal = nominal_fps(fps);
    let total_minutes = 60 * tc.h as i64 + tc.m as i64;
    let base = (tc.h as i64 * 3600 + tc.m as i64 * 60 + tc.s as i64) * nominal + tc.f as i64;
    if drop_frame {
        let drop = if nominal == 60 { 4 } else { 2 };
        base - drop * (total_minutes - total_minutes / 10)
    } else {
        base
    }
}

/// Exact seconds for a frame index at `fps`.
pub fn frames_to_seconds(frames: i64, fps: Seconds) -> Seconds {
    Seconds::new(frames, 1) / fps
}

/// Serde glue: Seconds ↔ "numerator/denominator" strings ("25/2").
/// The project file stays human-readable AND exact — no floats anywhere.
pub mod rational_ser {
    use serde::{Deserialize, Deserializer, Serializer, de};

    use super::{Seconds, parse_rate};

    pub fn serialize<S: Serializer>(v: &Seconds, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!("{}/{}", v.numer(), v.denom()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Seconds, D::Error> {
        let s = String::deserialize(d)?;
        parse_rate(&s).ok_or_else(|| de::Error::custom(format!("invalid rational time {s:?}")))
    }

    /// Option<Seconds> variant for fields that may be absent.
    pub mod opt {
        use serde::{Deserialize, Deserializer, Serializer, de};

        use super::{Seconds, parse_rate};

        pub fn serialize<S: Serializer>(v: &Option<Seconds>, s: S) -> Result<S::Ok, S::Error> {
            match v {
                Some(t) => super::serialize(t, s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Seconds>, D::Error> {
            match Option::<String>::deserialize(d)? {
                None => Ok(None),
                Some(t) => parse_rate(&t)
                    .map(Some)
                    .ok_or_else(|| de::Error::custom(format!("invalid rational time {t:?}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_seconds() {
        assert_eq!(parse_time("12").unwrap(), Seconds::from_integer(12));
    }

    #[test]
    fn decimal_seconds() {
        assert_eq!(parse_time("12.5").unwrap(), Seconds::new(25, 2));
        assert_eq!(parse_time(".5").unwrap(), Seconds::new(1, 2));
    }

    #[test]
    fn minutes_and_hours() {
        assert_eq!(parse_time("1:02").unwrap(), Seconds::from_integer(62));
        assert_eq!(parse_time("1:02:03").unwrap(), Seconds::from_integer(3723));
        assert_eq!(parse_time("0:00:01.250").unwrap(), Seconds::new(5, 4));
    }

    #[test]
    fn round_trip_format() {
        assert_eq!(format_seconds(&Seconds::new(25, 2)), "12.5");
        assert_eq!(format_seconds(&Seconds::from_integer(62)), "62");
        assert_eq!(format_seconds(&Seconds::new(1001, 30000)), "0.033367");
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "  ", "abc", "-5", "1:2:3:4", "1..2", "12.", "1.5.2"] {
            assert!(parse_time(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn frame_rates() {
        assert_eq!(parse_rate("30000/1001"), Some(Seconds::new(30000, 1001)));
        assert_eq!(parse_rate("30/1"), Some(Seconds::from_integer(30)));
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("nonsense"), None);
    }

    #[test]
    fn parses_timecodes() {
        assert_eq!(
            parse_timecode("01:00:00:00").unwrap(),
            Timecode {
                h: 1,
                m: 0,
                s: 0,
                f: 0
            }
        );
        // `;` introduces the frame field in many EDLs.
        assert_eq!(
            parse_timecode("00:00:05;12").unwrap(),
            Timecode {
                h: 0,
                m: 0,
                s: 5,
                f: 12
            }
        );
        for bad in ["", "1:2:3", "00:00:00", "aa:bb:cc:dd", "00:00:00:0x"] {
            assert!(parse_timecode(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn timecode_frames_non_drop() {
        let fps = Seconds::from_integer(30);
        assert_eq!(nominal_fps(fps), 30);
        let tc = parse_timecode("00:00:01:15").unwrap();
        assert_eq!(timecode_to_frames(tc, fps, false), 45);
    }

    #[test]
    fn timecode_frames_drop() {
        // 29.97 drop-frame: 2 frames dropped each minute except every 10th.
        let fps = Seconds::new(30000, 1001);
        assert_eq!(nominal_fps(fps), 30);
        let tc = parse_timecode("00:01:00;02").unwrap();
        assert_eq!(timecode_to_frames(tc, fps, true), 1800);
        let tc = parse_timecode("00:10:00;00").unwrap();
        assert_eq!(timecode_to_frames(tc, fps, true), 17982);
        assert_eq!(frames_to_seconds(30, fps), Seconds::new(1001, 1000));
    }
}
