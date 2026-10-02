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

/// Serde glue: Seconds ↔ "numerator/denominator" strings ("25/2").
/// The project file stays human-readable AND exact — no floats anywhere.
pub mod rational_ser {
    use serde::{de, Deserialize, Deserializer, Serializer};

    use super::{parse_rate, Seconds};

    pub fn serialize<S: Serializer>(v: &Seconds, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!("{}/{}", v.numer(), v.denom()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Seconds, D::Error> {
        let s = String::deserialize(d)?;
        parse_rate(&s).ok_or_else(|| de::Error::custom(format!("invalid rational time {s:?}")))
    }

    /// Option<Seconds> variant for fields that may be absent.
    pub mod opt {
        use serde::{de, Deserialize, Deserializer, Serializer};

        use super::{parse_rate, Seconds};

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
}
