//! The status row: transport state, the crocodile, preview progress.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::keys::extend::projected_total;
use super::super::{
    App, Mode, PREVIEW_NOTICE_DELAY, PreviewProgress, fmt_timecode, is_pool, pool_pos, pool_sel,
    secs,
};
pub(crate) fn logo(app: &App) -> &'static str {
    app.croc.art(Instant::now())
}

pub(crate) fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let name = app
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.path.display().to_string());

    let icon = if app.dead {
        "monitor closed"
    } else if !app.loaded {
        "… loading"
    } else if app.paused {
        // Two ASCII cells each. `⏸` has an emoji presentation in some
        // terminals (a wide white square) while `▶` stays narrow, so the
        // text after the icon drifted by one cell. These can't.
        "||"
    } else {
        "|>"
    };
    let mode = match &app.mode {
        Mode::Normal => "NORMAL".to_string(),
        Mode::Command(buf) => format!(":{buf}▏"),
        Mode::Extend(st) => {
            if st.ripple {
                "EXTEND·RIPPLE".to_string()
            } else {
                "EXTEND".to_string()
            }
        }
        Mode::Pool(_) => "POOL".to_string(),
        Mode::Fx(_) => "EFFECTS".to_string(),
        Mode::Transition(_) => "TRANSITION".to_string(),
    };
    let count = app.count.map(|c| format!("{c}")).unwrap_or_default();

    let style = Style::default().bg(Color::Blue).fg(Color::White);
    // In extend mode show where the timeline WILL end, so the pending
    // commit's effect on length is visible before pressing e.
    let (total, projected) = match &app.mode {
        Mode::Extend(st) => {
            let p = projected_total(app, st);
            (p, (p - app.total()).abs() > 1e-9)
        }
        _ => (app.total(), false),
    };
    let total_style = if projected {
        style.add_modifier(Modifier::BOLD).fg(Color::Yellow)
    } else {
        style
    };

    // In the pool the monitor and the timeline row are the selected asset,
    // so the clock reads the asset position/duration and the mode tag says
    // POOL. The project cursor is hidden until the pool closes.
    if is_pool(app) {
        let sel = pool_sel(app).unwrap_or(0);
        let (pos, dur) = match app.project.media.get(sel) {
            Some(a) => (pool_pos(app), a.duration().map(|d| secs(&d)).unwrap_or(0.0)),
            None => (0.0, 0.0),
        };
        let fs = app.project.media.get(sel).and_then(|a| a.fps());
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", logo(app)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {name} · "), style),
            Span::styled(
                fmt_timecode(pos, fs.as_ref()),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" / {} · ", fmt_timecode(dur, fs.as_ref())), style),
            Span::styled(format!("{icon} · POOL{count}"), style),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let mut spans = vec![
        Span::styled(
            format!(" {} ", logo(app)),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {name} · "), style),
        Span::styled(
            fmt_timecode(app.playhead, app.project.fps.as_ref()),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {} · ", fmt_timecode(total, app.project.fps.as_ref())),
            total_style,
        ),
        Span::styled(format!("{icon} · {mode}{count}"), style),
    ];
    if let Some(p) = app.preview_progress
        && app
            .preview
            .as_ref()
            .is_some_and(|j| j.started.elapsed() >= PREVIEW_NOTICE_DELAY)
    {
        spans.push(Span::styled(
            format!(" · {}", preview_indicator(p, app.preview_dirty)),
            style.add_modifier(Modifier::BOLD).fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A 10-frame spinner for the status row. Time-based, so it animates with the
/// loop's ~50 ms tick without extra app state.
pub(crate) fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let i = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 100) as usize % FRAMES.len())
        .unwrap_or(0);
    FRAMES[i]
}

/// The status-row preview indicator: `⠋ proxies 2/5 · 43% · queued`. The
/// fraction is (finished + this render's progress) / total.
pub(crate) fn preview_indicator(p: PreviewProgress, dirty: bool) -> String {
    let done = p.done.min(p.total);
    let pct = if p.total > 0 {
        (((p.done as f64 + p.frac) / p.total as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u32
    } else {
        0
    };
    let mut s = if p.total > 0 {
        format!("{} proxies {done}/{} · {pct}%", spinner(), p.total)
    } else {
        format!("{} building preview…", spinner())
    };
    if dirty {
        s.push_str(" · queued");
    }
    s
}
