//! The status-row crocodile: a fixed-width ASCII sprite with time-based
//! action animations.
//!
//! Every frame is exactly nine ASCII cells. The status row renders the art
//! between two spaces, so a shorter/longer frame would shift the timecode
//! and mode text. Action strips are deliberately slow (100–300 ms frames);
//! the idle blink stays fast. Durations are multiples of the 50 ms draw tick.

use std::time::{Duration, Instant};

pub(crate) const IDLE: &str = r#"=";::;>--"#;
const OPEN: &str = r#">";::;>--"#;
const WIDE: &str = r#")";::;>--"#;
/// Eyes shut: used by the fast idle blink and the pause/mute reaction.
const SLEEP: &str = r#"=-;::;>--"#;
const HISS: &str = r#">O;::;>--"#;
// Ambient swim: the body's `;` become `:`, so the croc shows its legs
// without making the sprite any wider.
pub(crate) const SWIM1: &str = r#"="::::>-~"#;
pub(crate) const SWIM2: &str = r#"="::::>~-"#;
// Tail-only frames for actions (no legs: the croc is not ambient-swimming).
const TAIL1: &str = r#"=";::;>-~"#;
const TAIL2: &str = r#"=";::;>~-"#;
const DASH1: &str = r#"=";::;>>-"#;
const DASH2: &str = r#"=";::;>>>"#;
const TAIL_UP: &str = r#"=";::;>!-"#;
const HAPPY_TAIL: &str = r#"=^;::;>-~"#;
const WIDE_ALERT: &str = r#")O;::;>!-"#;
// Spit/vomit: the body wave runs closed, then the eyes shut, then the jaw
// opens in three stages with a star eye.
const SPIT1: &str = r#"=";::,>--"#;
const SPIT2: &str = r#"=";:.;>--"#;
const SPIT3: &str = r#"=";.:;>--"#;
const SPIT4: &str = r#"=",::;>--"#;
const OPEN_STAR: &str = r#">*;::;>--"#;
const WIDE_STAR: &str = r#")*;::;>--"#;

/// A frame strip: (art, milliseconds). `None` is the idle state.
type Strip = &'static [(&'static str, u64)];

const CHOMP: Strip = &[(OPEN, 100), (WIDE, 100), (OPEN, 100), (TAIL_UP, 100)];
/// Adds (paste/insert/track add/extend) swallow with the mouth closed:
/// a happy eye and a tail flick, never an open jaw.
const GULP: Strip = &[(HAPPY_TAIL, 200), (TAIL2, 200)];
const SPIT: Strip = &[
    (SPIT1, 50),
    (SPIT2, 50),
    (SPIT3, 50),
    (SPIT4, 50),
    (SLEEP, 100),
    (OPEN_STAR, 100),
    (WIDE_STAR, 100),
    (OPEN_STAR, 100),
];
/// Redo is the spit run backwards.
const UNSPIT: Strip = &[
    (OPEN_STAR, 100),
    (WIDE_STAR, 100),
    (OPEN_STAR, 100),
    (SLEEP, 100),
    (SPIT4, 50),
    (SPIT3, 50),
    (SPIT2, 50),
    (SPIT1, 50),
];
const SNAP: Strip = &[(OPEN, 100), (WIDE, 100), (OPEN, 100), (WIDE, 100)];
const NIBBLE: Strip = &[(OPEN, 100), (WIDE, 100), (OPEN, 100)];
const DASH_FAST: Strip = &[(TAIL1, 100), (DASH1, 100), (DASH2, 200)];
const DASH_SLOW: Strip = &[(TAIL2, 300), (TAIL1, 300)];
const SLITHER: Strip = &[(TAIL1, 200), (TAIL2, 200), (TAIL1, 200)];
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
    Unspit,
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
            CrocAct::Unspit => Some(UNSPIT),
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
        SWIM1
    } else {
        SWIM2
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
        SPIT1, SPIT2, SPIT3, SPIT4, OPEN_STAR, WIDE_STAR,
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
            CrocAct::Unspit,
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
    fn trim_and_split_keep_their_eyes_open_and_tail_still() {
        for act in [CrocAct::Nibble, CrocAct::Snap] {
            for (art, _) in act.strip().expect("finite act") {
                assert_ne!(*art, SLEEP, "{act:?} must not blink: {art}");
                assert_ne!(art.chars().nth(1), Some('-'), "{act:?}: {art}");
                assert_eq!(&art[6..], ">--", "{act:?} must not move the tail: {art}");
            }
        }
    }

    #[test]
    fn the_spit_sequence_waves_blinks_then_opens() {
        assert_eq!(frame_at(CrocAct::Spit, Duration::ZERO), SPIT1);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(50)), SPIT2);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(100)), SPIT3);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(150)), SPIT4);
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(200)), SLEEP);
        assert_eq!(
            frame_at(CrocAct::Spit, Duration::from_millis(300)),
            OPEN_STAR
        );
        assert_eq!(
            frame_at(CrocAct::Spit, Duration::from_millis(400)),
            WIDE_STAR
        );
        assert_eq!(
            frame_at(CrocAct::Spit, Duration::from_millis(500)),
            OPEN_STAR
        );
        assert_eq!(frame_at(CrocAct::Spit, Duration::from_millis(600)), IDLE);
        // Only one body cell leaves the base at a time, and the wave is shut.
        for (frame, body) in [
            (SPIT1, ";::,"),
            (SPIT2, ";:.;"),
            (SPIT3, ";.:;"),
            (SPIT4, ",::;"),
        ] {
            assert!(frame.starts_with('='), "wave stays shut: {frame}");
            assert_eq!(&frame[2..6], body, "{frame}");
            let changed = body
                .chars()
                .zip(";::;".chars())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(changed, 1, "one body section at a time: {frame}");
        }
        assert_eq!(SLEEP, frame_at(CrocAct::Spit, Duration::from_millis(200)));
    }

    #[test]
    fn redo_reverses_the_spit_sequence() {
        assert_eq!(frame_at(CrocAct::Unspit, Duration::ZERO), OPEN_STAR);
        assert_eq!(
            frame_at(CrocAct::Unspit, Duration::from_millis(100)),
            WIDE_STAR
        );
        assert_eq!(
            frame_at(CrocAct::Unspit, Duration::from_millis(200)),
            OPEN_STAR
        );
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(300)), SLEEP);
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(400)), SPIT4);
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(450)), SPIT3);
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(500)), SPIT2);
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(550)), SPIT1);
        assert_eq!(frame_at(CrocAct::Unspit, Duration::from_millis(600)), IDLE);
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
            assert!(art == SWIM1 || art == SWIM2, "{ms} ms: {art}");
        }
    }
}
