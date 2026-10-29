//! The status-row crocodile: a fixed-width ASCII sprite with time-based
//! action animations.
//!
//! Every frame is exactly nine ASCII cells. The status row renders the art
//! between two spaces, so a shorter/longer frame would shift the timecode
//! and mode text. Durations are multiples of the 50 ms draw tick.

use std::time::{Duration, Instant};

pub(crate) const IDLE: &str = r#"=";::;>--"#;
const OPEN: &str = r#">";::;>--"#;
const WIDE: &str = r#")";::;>--"#;
const BLINK: &str = r#"=';::;>--"#;
const SLEEP: &str = r#"=-;::;>--"#;
const HISS: &str = r#">O;::;>--"#;
const SWIM1: &str = r#"=";::;>-~"#;
const SWIM2: &str = r#"=";::;>~-"#;
const DASH1: &str = r#"=";::;>>-"#;
const DASH2: &str = r#"=";::;>>>"#;
const TAIL_UP: &str = r#"=";::;>!-"#;
const HAPPY_TAIL: &str = r#"=^;::;>-~"#;
const WIDE_ALERT: &str = r#")O;::;>!-"#;
const YAWN_OPEN: &str = r#")-;::;>--"#;

/// A frame strip: (art, milliseconds). `None` is the idle state.
type Strip = &'static [(&'static str, u64)];

const CHOMP: Strip = &[
    (OPEN, 50),
    (WIDE, 50),
    (OPEN, 50),
    (TAIL_UP, 50),
    (IDLE, 50),
];
const GULP: Strip = &[(OPEN, 50), (WIDE, 50), (HAPPY_TAIL, 100), (IDLE, 50)];
const SPIT: Strip = &[(WIDE, 100), (SWIM2, 100), (IDLE, 100)];
const SNAP: Strip = &[(OPEN, 50), (BLINK, 50), (OPEN, 50), (TAIL_UP, 50)];
const NIBBLE: Strip = &[(OPEN, 50), (BLINK, 50), (SWIM2, 50), (IDLE, 50)];
const DASH_FAST: Strip = &[(SWIM1, 50), (DASH1, 50), (DASH2, 100), (IDLE, 50)];
const DASH_SLOW: Strip = &[(SWIM2, 100), (SWIM1, 100), (IDLE, 150)];
const SLITHER: Strip = &[(SWIM1, 100), (SWIM2, 100), (SWIM1, 100), (IDLE, 50)];
const WINK: Strip = &[(BLINK, 100), (HAPPY_TAIL, 100), (IDLE, 50)];
const YAWN: Strip = &[(YAWN_OPEN, 100), (SLEEP, 200), (IDLE, 100)];
const HISSED: Strip = &[(HISS, 100), (WIDE_ALERT, 150), (HISS, 100), (IDLE, 100)];
const SWIM: Strip = &[(SWIM1, 100), (SWIM2, 100), (SWIM1, 100), (IDLE, 100)];

const BLINK_AT_MS: u128 = 6_800;
const IDLE_PERIOD_MS: u128 = 7_000;

/// One reaction the status crocodile can play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrocAct {
    Idle,
    Chomp,
    Gulp,
    Spit,
    Snap,
    Nibble,
    DashFast,
    DashSlow,
    Slither,
    Wink,
    Yawn,
    Hiss,
    Swim,
}

impl CrocAct {
    fn strip(self) -> Option<Strip> {
        match self {
            CrocAct::Idle => None,
            CrocAct::Chomp => Some(CHOMP),
            CrocAct::Gulp => Some(GULP),
            CrocAct::Spit => Some(SPIT),
            CrocAct::Snap => Some(SNAP),
            CrocAct::Nibble => Some(NIBBLE),
            CrocAct::DashFast => Some(DASH_FAST),
            CrocAct::DashSlow => Some(DASH_SLOW),
            CrocAct::Slither => Some(SLITHER),
            CrocAct::Wink => Some(WINK),
            CrocAct::Yawn => Some(YAWN),
            CrocAct::Hiss => Some(HISSED),
            CrocAct::Swim => Some(SWIM),
        }
    }
}

/// What the crocodile is doing. `idle_since` is deliberately independent of
/// `since`: action animations must not reset the slow idle-blink clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CrocState {
    act: CrocAct,
    since: Instant,
    idle_since: Instant,
}

impl CrocState {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            act: CrocAct::Idle,
            since: now,
            idle_since: now,
        }
    }

    /// Start a reaction. The latest trigger wins; frames are never queued.
    pub(crate) fn play(&mut self, act: CrocAct) {
        self.act = act;
        self.since = Instant::now();
    }

    pub(crate) fn art(&self, now: Instant) -> &'static str {
        if let Some(art) = frame_in(self.act, now.saturating_duration_since(self.since)) {
            return art;
        }
        idle_frame(now.saturating_duration_since(self.idle_since))
    }

    #[cfg(test)]
    pub(crate) fn act(&self) -> CrocAct {
        self.act
    }

    /// Test-only: make an action look `elapsed` old.
    #[cfg(test)]
    pub(crate) fn set_elapsed(&mut self, elapsed: Duration) {
        self.since = Instant::now() - elapsed;
    }

    /// Test-only: pin the first frame so a rendered status row is not a race
    /// with the 50 ms frame clock.
    #[cfg(test)]
    pub(crate) fn hold_first_frame(&mut self) {
        self.since = Instant::now() + Duration::from_secs(3_600);
    }

    /// Test-only: move the idle blink clock.
    #[cfg(test)]
    pub(crate) fn set_idle_elapsed(&mut self, elapsed: Duration) {
        self.idle_since = Instant::now() - elapsed;
    }
}

fn frame_in(act: CrocAct, elapsed: Duration) -> Option<&'static str> {
    let strip = act.strip()?;
    let elapsed = elapsed.as_millis();
    let mut end = 0;
    for (art, ms) in strip {
        end += *ms as u128;
        if elapsed < end {
            return Some(art);
        }
    }
    None
}

/// Blink for the last 200 ms of every 7 s. A visible, low-key sign of life.
fn idle_frame(elapsed: Duration) -> &'static str {
    if elapsed.as_millis() % IDLE_PERIOD_MS >= BLINK_AT_MS {
        BLINK
    } else {
        IDLE
    }
}

#[cfg(test)]
fn frame_at(act: CrocAct, elapsed: Duration) -> &'static str {
    frame_in(act, elapsed).unwrap_or(IDLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ART: &[&str] = &[
        IDLE, OPEN, WIDE, BLINK, SLEEP, HISS, SWIM1, SWIM2, DASH1, DASH2, TAIL_UP, HAPPY_TAIL,
        WIDE_ALERT, YAWN_OPEN,
    ];

    #[test]
    fn every_frame_is_nine_ascii_cells() {
        for art in ALL_ART {
            assert_eq!(art.chars().count(), 9, "{art:?}");
            assert!(art.is_ascii(), "{art:?}");
        }
    }

    #[test]
    fn strips_are_tick_sized_and_end_closed() {
        for act in [
            CrocAct::Chomp,
            CrocAct::Gulp,
            CrocAct::Spit,
            CrocAct::Snap,
            CrocAct::Nibble,
            CrocAct::DashFast,
            CrocAct::DashSlow,
            CrocAct::Slither,
            CrocAct::Wink,
            CrocAct::Yawn,
            CrocAct::Hiss,
            CrocAct::Swim,
        ] {
            let strip = act.strip().expect("finite act");
            assert!(strip.iter().all(|(_, ms)| *ms % 50 == 0), "{act:?}");
            let total: u64 = strip.iter().map(|(_, ms)| *ms).sum();
            assert_eq!(frame_at(act, Duration::from_millis(total)), IDLE);
        }
    }

    #[test]
    fn a_chomp_opens_then_closes() {
        assert_eq!(frame_at(CrocAct::Chomp, Duration::ZERO), OPEN);
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(60)), WIDE);
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(120)), OPEN);
        assert_eq!(
            frame_at(CrocAct::Chomp, Duration::from_millis(180)),
            TAIL_UP
        );
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(250)), IDLE);
    }

    #[test]
    fn idle_blinks_at_the_end_of_its_period() {
        assert_eq!(idle_frame(Duration::ZERO), IDLE);
        assert_eq!(idle_frame(Duration::from_millis(6_799)), IDLE);
        assert_eq!(idle_frame(Duration::from_millis(6_800)), BLINK);
        assert_eq!(idle_frame(Duration::from_millis(6_999)), BLINK);
        assert_eq!(idle_frame(Duration::from_millis(7_000)), IDLE);
    }
}
