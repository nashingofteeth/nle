//! The status-row crocodile: a fixed-width ASCII sprite with time-based
//! action animations.
//!
//! Every frame is exactly nine ASCII cells. The status row renders the art
//! between two spaces, so a shorter/longer frame would shift the timecode
//! and mode text. Action strips are deliberately slow (100–300 ms frames);
//! the idle blink stays fast. Durations are multiples of the 50 ms draw tick.

use std::time::{Duration, Instant};

pub(crate) const IDLE: &str = r#"=";::;>--"#;
/// The status field is wide enough for the swimming legs; other frames are
/// padded so the timecode never shifts when the legs appear/disappear.
pub(crate) const CROC_WIDTH: usize = 13;
const OPEN: &str = r#">";::;>--"#;
const WIDE: &str = r#")";::;>--"#;
/// Eyes shut: used by the fast idle blink and the pause/mute reaction.
const SLEEP: &str = r#"=-;::;>--"#;
const HISS: &str = r#">O;::;>--"#;
pub(crate) const SWIM1: &str = r#"=";::;>-~"#;
pub(crate) const SWIM2: &str = r#"=";::;>~-"#;
pub(crate) const SWIM1_LEGS: &str = r#"=";::;::::>-~"#;
pub(crate) const SWIM2_LEGS: &str = r#"=";::;::::>~-"#;
const DASH1: &str = r#"=";::;>>-"#;
const DASH2: &str = r#"=";::;>>>"#;
const TAIL_UP: &str = r#"=";::;>!-"#;
const HAPPY_TAIL: &str = r#"=^;::;>-~"#;
const WIDE_ALERT: &str = r#")O;::;>!-"#;
// Spit: one body section changes at a time. A comma at the back gives way
// to a single dot moving right-to-left through the middle to the front; the
// jaw opens only after the wave, then shuts again.
const SPIT1: &str = r#"=";::,>--"#;
const SPIT2: &str = r#"=";:.;>--"#;
const SPIT3: &str = r#"=";.:;>--"#;
const SPIT4: &str = r#"=".::;>--"#;
const SPIT_OPEN: &str = r#")".::;>--"#;

/// A frame strip: (art, milliseconds). `None` is the idle state.
type Strip = &'static [(&'static str, u64)];

const CHOMP: Strip = &[(OPEN, 100), (WIDE, 100), (OPEN, 100), (TAIL_UP, 100)];
/// Adds (paste/insert/track add/extend) swallow with the mouth closed:
/// a happy eye and a tail flick, never an open jaw.
const GULP: Strip = &[(HAPPY_TAIL, 200), (SWIM2, 200)];
const SPIT: Strip = &[
    (SPIT1, 100),
    (SPIT2, 100),
    (SPIT3, 100),
    (SPIT4, 100),
    (SPIT_OPEN, 100),
];
const SNAP: Strip = &[(OPEN, 100), (SLEEP, 100), (OPEN, 100), (TAIL_UP, 100)];
const NIBBLE: Strip = &[(OPEN, 100), (SLEEP, 200), (SWIM2, 100)];
const DASH_FAST: Strip = &[(SWIM1, 100), (DASH1, 100), (DASH2, 200)];
const DASH_SLOW: Strip = &[(SWIM2, 300), (SWIM1, 300)];
const SLITHER: Strip = &[(SWIM1, 200), (SWIM2, 200), (SWIM1, 200)];
const WINK: Strip = &[(SLEEP, 200), (HAPPY_TAIL, 200)];
/// Pause/mute: only the eyes close — no open mouth, no yawn.
const SLEEPY: Strip = &[(SLEEP, 400)];
const HISSED: Strip = &[(HISS, 200), (WIDE_ALERT, 300), (HISS, 200)];

const BLINK_AT_MS: u128 = 6_800;
const IDLE_PERIOD_MS: u128 = 7_000;
const SWIM_FRAME_MS: u128 = 200;

/// One reaction the status crocodile can play. Playing is an ambient swim,
/// not an entry here, so it can loop without a state transition.
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
    Sleep,
    Hiss,
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
            CrocAct::Sleep => Some(SLEEPY),
            CrocAct::Hiss => Some(HISSED),
        }
    }
}

/// What the crocodile is doing. `idle_since` is deliberately independent of
/// `since`: action animations must not reset the slow idle-blink clock, and
/// the continuous swim rides the same phase.
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

    /// Drop any reaction now — the ambient state takes over (used when
    /// playback starts, so the croc swims immediately).
    pub(crate) fn clear(&mut self) {
        self.act = CrocAct::Idle;
        self.since = Instant::now();
    }

    /// `playing` is the transport state (loaded, playing, not at EOF). While
    /// playing there is no idle or blink: the tail loops continuously.
    pub(crate) fn art(&self, now: Instant, playing: bool) -> &'static str {
        if let Some(art) = frame_in(self.act, now.saturating_duration_since(self.since)) {
            return art;
        }
        let elapsed = now.saturating_duration_since(self.idle_since);
        if playing {
            swim_frame(elapsed)
        } else {
            idle_frame(elapsed)
        }
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

    /// Test-only: move the idle blink/swim clock.
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

/// Continuous tail motion while the monitor plays. Alternating frames, never
/// an idle frame.
fn swim_frame(elapsed: Duration) -> &'static str {
    if (elapsed.as_millis() / SWIM_FRAME_MS).is_multiple_of(2) {
        SWIM1_LEGS
    } else {
        SWIM2_LEGS
    }
}

/// Blink for the last 200 ms of every 7 s — fast, and eyes shut to `-`.
fn idle_frame(elapsed: Duration) -> &'static str {
    if elapsed.as_millis() % IDLE_PERIOD_MS >= BLINK_AT_MS {
        SLEEP
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
        IDLE, OPEN, WIDE, SLEEP, HISS, SWIM1, SWIM2, DASH1, DASH2, TAIL_UP, HAPPY_TAIL, WIDE_ALERT,
        SPIT1, SPIT2, SPIT3, SPIT4,
    ];

    #[test]
    fn every_frame_fits_the_status_sprite() {
        for art in ALL_ART {
            assert!(matches!(art.chars().count(), 9 | CROC_WIDTH), "{art:?}");
            assert!(art.is_ascii(), "{art:?}");
            assert_eq!(format!("{art:<CROC_WIDTH$}").chars().count(), CROC_WIDTH);
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
            CrocAct::Sleep,
            CrocAct::Hiss,
        ] {
            let strip = act.strip().expect("finite act");
            assert!(strip.iter().all(|(_, ms)| *ms % 50 == 0), "{act:?}");
            let total: u64 = strip.iter().map(|(_, ms)| *ms).sum();
            assert_eq!(frame_at(act, Duration::from_millis(total)), IDLE);
        }
    }

    #[test]
    fn a_chomp_opens_then_closes_slowly() {
        assert_eq!(frame_at(CrocAct::Chomp, Duration::ZERO), OPEN);
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(100)), WIDE);
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(200)), OPEN);
        assert_eq!(
            frame_at(CrocAct::Chomp, Duration::from_millis(300)),
            TAIL_UP
        );
        assert_eq!(frame_at(CrocAct::Chomp, Duration::from_millis(400)), IDLE);
    }

    #[test]
    fn the_gulp_never_opens_the_mouth() {
        for (art, _) in GULP {
            assert!(art.starts_with('='), "adds must keep the mouth shut: {art}");
        }
    }

    #[test]
    fn the_spit_wave_moves_from_back_to_front() {
        assert_eq!(frame_at(CrocAct::Spit, Duration::ZERO), SPIT1);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(100)), SPIT2);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(200)), SPIT3);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(300)), SPIT4);
        assert_eq!(
            frame_at(CrocAct::Spit, Duration::from_millis(400)),
            SPIT_OPEN
        );
        // The jaw is shut while the mark travels, opens after the wave, then
        // the fallback is closed again.
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(500)), IDLE);
        // Only one body cell leaves the base at a time: the mark travels
        // back-to-front instead of painting the whole back.
        for (frame, body) in [
            (SPIT1, ";::,"),
            (SPIT2, ";:.;"),
            (SPIT3, ";.:;"),
            (SPIT4, ".::;"),
            (SPIT_OPEN, ".::;"),
        ] {
            assert_eq!(&frame[2..6], body, "{frame}");
            let changed = body
                .chars()
                .zip(";::;".chars())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(changed, 1, "one body section at a time: {frame}");
        }
        assert!(SPIT1.starts_with('='), "wave starts closed");
        assert!(SPIT_OPEN.starts_with(')'), "jaw opens after the wave");
    }

    #[test]
    fn idle_blinks_fast_with_eyes_shut() {
        assert_eq!(idle_frame(Duration::ZERO), IDLE);
        assert_eq!(idle_frame(Duration::from_millis(6_799)), IDLE);
        assert_eq!(idle_frame(Duration::from_millis(6_800)), SLEEP);
        assert_eq!(idle_frame(Duration::from_millis(6_999)), SLEEP);
        assert_eq!(idle_frame(Duration::from_millis(7_000)), IDLE);
    }

    #[test]
    fn playing_never_idles() {
        let croc = CrocState::new();
        let now = Instant::now();
        for ms in (0..1_000).step_by(100) {
            let art = croc.art(now + Duration::from_millis(ms), true);
            assert!(art == SWIM1_LEGS || art == SWIM2_LEGS, "{ms} ms: {art}");
        }
    }
}
