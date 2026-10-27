//! Panel layout and dispatch: the whole frame, the hint lines, the message
//! line, and the height budget that decides how many track rows fit.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use self::fx::{form_hint, render_fx, render_transition_form};
use self::info::render_info;
use self::pool::render_pool;
use self::status::render_status;
use self::timeline::render_timeline;
use super::{App, Mode, fx_state, is_fx, is_pool, pool_strip_active};

pub(super) mod fx;
pub(super) mod info;
pub(super) mod pool;
pub(super) mod status;
pub(super) mod timeline;
pub(crate) fn render(f: &mut Frame, app: &App) {
    // Size the timeline to show every track, then give what's left to the
    // info panel and the bottom panels. The bottom panels are dropped before
    // the timeline is allowed to hide a track. While the pool is open the
    // panel is that asset's one-clip strip, so one row + marker is enough and
    // the freed rows go to the pool list below.
    let track_count = if pool_strip_active(app) {
        1
    } else {
        app.project.timeline.tracks.len()
    };
    let (timeline_h, keys, msg_h) = panel_heights(track_count, f.area().height);
    // The add form carries its own key hints and needs the rows for its
    // fields; drop the bottom cheat-sheet while it is open.
    let keys = if is_fx(app) && fx_state(app).is_some_and(|st| st.form.is_some()) {
        0
    } else {
        keys
    };
    let [status, timeline, info, keys_area, msg_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(timeline_h),
        Constraint::Min(0),
        Constraint::Length(keys),
        Constraint::Length(msg_h),
    ])
    .areas(f.area());

    render_status(f, status, app);
    render_timeline(f, timeline, app);
    if is_pool(app) {
        render_pool(f, info, app);
    } else if is_fx(app) {
        render_fx(f, info, app);
    } else if matches!(app.mode, Mode::Transition(_)) {
        render_transition_form(f, info, app);
    } else {
        render_info(f, info, app);
    }
    if keys > 0 {
        render_hints(f, keys_area, app);
    }
    if msg_h > 0 {
        render_message(f, msg_area, app);
    }
}

/// Rows for the timeline panel and the two bottom panels: the timeline gets
/// one row per track plus the marker row and two borders, and the keys/message
/// panels are dropped before a track has to be hidden. Returns
/// `(timeline_h, keys_h, msg_h)`. Pure so the sizing is testable.
pub(crate) fn panel_heights(track_count: usize, height: u16) -> (u16, u16, u16) {
    let track_rows = track_count.max(1) as u16;
    // Two borders + one row per track + the marker row.
    let want_tl = track_rows.saturating_add(3);
    // The status row is always present; keep a few rows for the info panel.
    let min_info = 3u16;
    let max_tl = height.saturating_sub(1 + min_info).max(4);
    let timeline_h = want_tl.min(max_tl).max(4);
    // Bottom panels only when they fit without squeezing the tracks.
    let leftover = height.saturating_sub(1 + timeline_h + min_info);
    let msg_h = if leftover >= 3 { 3 } else { 0 };
    let keys_h = if leftover >= 3 + 8 { 8 } else { 0 };
    (timeline_h, keys_h, msg_h)
}

/// The keys cheat-sheet (mode-specific; see `hints_for`).
pub(crate) fn render_hints(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" keys ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines: Vec<Line> = hints_for(app)
        .into_iter()
        .map(|h| Line::from(Span::styled(h, Style::default().fg(Color::DarkGray))))
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The latest message/feedback line, in its own panel.
pub(crate) fn render_message(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" message ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (msg, style) = if !app.message.is_empty() {
        (app.message.clone(), Style::default().fg(Color::Yellow))
    } else {
        ("—".to_string(), Style::default().fg(Color::DarkGray))
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(msg, style))).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The mode-specific key cheat-sheet (single source of truth for hints).
pub(crate) fn hints_for(app: &App) -> Vec<&'static str> {
    if let Mode::Command(_) = app.mode {
        return vec![
            "type a command · enter run · esc cancel · flags: --at T --track N --ripple",
            "edits   :split :trim-in|out :delete :yank :paste :gap :extend :speed",
            "history :undo :redo · tracks :track add video|audio · :track rm N",
            "media   :import PATH… · :add FILE · :media ls · :media rm N",
            "more    :show :seek :purge :detect :edl-import :otio-import :export",
        ];
    }
    if matches!(app.mode, Mode::Extend(_)) {
        return vec![
            "extend mode — move the armed edge, then commit",
            "h/l ±1s · H/L ±5s · f/F frame · j/k cuts · w/b clips · n/N gaps · gg/G/0",
            "e commit · E ripple on/off · [ ] side · < > (, .) end · Esc cancel",
        ];
    }
    if is_pool(app) {
        return vec![
            "media pool — the monitor previews the selected file",
            "j/k assets · { } ends · h/l ±1s · H/L ±5s · f/F frame · gg/0 start · G/$ end",
            "space play · enter insert · i import · d remove · u undo · m/esc close",
            "tracks     t/T target · <N>t jump · insert goes to the active track",
        ];
    }
    if matches!(app.mode, Mode::Transition(_)) {
        return vec![
            "transition length — frames, then enter (blank keeps it)",
            "digits edit · ctrl-u clears · esc cancels",
        ];
    }
    if is_fx(app) {
        if let Some(form) = fx_state(app).and_then(|st| st.form.as_ref()) {
            return vec![
                form_hint(form.kind),
                "letters jump · tab/↑↓ field · space toggles lock · enter applies",
                "ctrl-u clears · esc cancels",
            ];
        }
        return vec![
            "effects — rows apply in order at export; the monitor re-renders",
            "j/k select · 1-9 edit · d drop · D clear all · u undo · Ctrl-r redo",
            "c/s/e/o add/edit crop/scale/eq/offset · x transition · q close",
        ];
    }
    vec![
        "transport  space play · h/l ±1s · H/L ±5s · f/F frame · gg/G/0/$ ends",
        "motion     j/k cuts · w/b clips · n/N gaps · < > (, .) clip start/end",
        "edit       i/o trim · I/O ripple · s split · _ gap · dd/D del · yy/p/P",
        "           r speed · X transition · e/E extend · [ ] side · m pool",
        "effects    x panel · c/s/e/o add/edit · d/D drop · u undo · Ctrl-r redo",
        "tracks     t/T cycle · <N>t jump · :track add · : cmd · Esc · q quit",
    ]
}
