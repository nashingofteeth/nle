//! The timeline panel: one row per track plus the marker row, drawn from the
//! same quantizer as the playhead (`marker_col`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use nle_core::project::Project;
use nle_core::timeline::{Segment as TimelineSegment, Track};
use nle_core::track_label;

use super::super::motion::{marker_col, on_boundary, truncate_label};
use super::super::{App, Mode, pool_strip_active, secs};
use super::pool::render_pool_strip;
pub(crate) fn render_timeline(f: &mut Frame, area: Rect, app: &App) {
    // While the pool is open the monitor is off the project, so the timeline
    // row is the selected asset's own strip — playing, paused or ended. The
    // project (and its insert point) returns on close.
    if pool_strip_active(app)
        && let Mode::Pool(st) = &app.mode
    {
        render_pool_strip(f, area, app, st);
        return;
    }
    let block = Block::bordered().title(" timeline ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }

    let tracks = &app.project.timeline.tracks;
    let n = tracks.len();
    // A label gutter only when there is more than one track to tell apart.
    let label_w: u16 = if n > 1 { 4 } else { 0 };
    let bar_w = inner.width.saturating_sub(label_w);
    if bar_w == 0 {
        return;
    }

    // In extend mode the row rescales past the timeline end so the
    // projected range stays visible while navigating outward.
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let total = if in_extend {
        app.total().max(app.playhead).max(0.001)
    } else {
        app.total().max(0.001)
    };

    // One row per visible track; the marker row takes the row below them.
    let capacity = inner.height.saturating_sub(1) as usize;
    let (start, count) = track_window(n, app.active(), capacity.max(1));

    for (row, ti) in (start..start + count).enumerate() {
        let y = inner.y + row as u16;
        if label_w > 0 {
            let active = ti == app.active();
            let style = if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let label = format!("{:>3} ", track_label(&app.project, ti));
            let gutter = Rect {
                x: inner.x,
                y,
                width: label_w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style))),
                gutter,
            );
        }
        let bar = Rect {
            x: inner.x + label_w,
            y,
            width: bar_w,
            height: 1,
        };
        draw_track_row(
            f,
            bar,
            app,
            &tracks[ti],
            total,
            in_extend && ti == app.active(),
        );
    }

    let marker_y = inner.y + count as u16;
    let bar = Rect {
        x: inner.x + label_w,
        y: marker_y,
        width: bar_w,
        height: 1,
    };
    draw_marker_row(f, bar, app, total, in_extend);
}

/// Which tracks to show: all of them when they fit, else a window of
/// `capacity` centred on the active track.
pub(crate) fn track_window(n: usize, active: usize, capacity: usize) -> (usize, usize) {
    if n <= capacity {
        return (0, n);
    }
    let start = active.saturating_sub(capacity / 2).min(n - capacity);
    (start, capacity)
}

/// One track's segments as (duration, label, style) parts, plus a trailing
/// gap for any span short of the timeline end — a track shorter than the
/// timeline reads as gap rather than blank.
/// One row's parts: `(duration, label, style, permeable_left)`. The flag is
/// true when the boundary on this part's left is a blend — a transition is
/// part of both neighbours, so that divider is drawn permeable rather than
/// hard.
pub(crate) fn track_parts(
    project: &Project,
    track: &Track,
    total: f64,
) -> Vec<(f64, String, Style, bool)> {
    let gap_style = Style::default().bg(Color::DarkGray).fg(Color::Black);
    let is_trans = |s: &TimelineSegment| matches!(s, TimelineSegment::Transition(_));
    let mut parts: Vec<(f64, String, Style, bool)> = track
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let soft = is_trans(s) || i > 0 && track.segments.get(i - 1).is_some_and(is_trans);
            match s {
                TimelineSegment::Clip(c) => {
                    let name = project
                        .asset(c.asset)
                        .and_then(|a| a.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| format!("#{}", c.id));
                    // `*` marks a clip with effects — a suffix would be cut
                    // off on a narrow cell, a prefix survives.
                    let name = if c.effects.is_empty() {
                        name
                    } else {
                        format!("*{name}")
                    };
                    (
                        secs(&c.duration()),
                        name,
                        Style::default().bg(Color::Cyan).fg(Color::Black),
                        soft,
                    )
                }
                // A transition gets its own band; the real dissolve is a
                // proxy-render thing (M5), so it's labelled, not blended.
                TimelineSegment::Transition(t) => (
                    secs(&t.dur),
                    "trans".to_string(),
                    Style::default().bg(Color::Magenta).fg(Color::Black),
                    soft,
                ),
                TimelineSegment::Gap(g) => (secs(&g.duration), "gap".to_string(), gap_style, soft),
            }
        })
        .collect();
    let tail = total - secs(&track.end());
    if tail > 1e-9 {
        let soft = track.segments.last().is_some_and(is_trans);
        parts.push((tail, "gap".to_string(), gap_style, soft));
    }
    parts
}

/// Draw one track's segment row. `notch` overlays the extend-mode playhead
/// marker (only the armed track gets it). Boundaries come from the SAME
/// quantizer as the marker row (`marker_col` of the cumulative time), so
/// drawn clip edges and the playhead/cut indicator can never disagree.
pub(crate) fn draw_track_row(
    f: &mut Frame,
    bar: Rect,
    app: &App,
    track: &Track,
    total: f64,
    notch: bool,
) {
    let parts = track_parts(&app.project, track, total);
    let mut cum: f64 = 0.0;
    let mut cells: Vec<(u16, String, Style)> = Vec::new();
    for (i, (dur, label, style, soft)) in parts.iter().enumerate() {
        let start_x = if i == 0 {
            0
        } else {
            marker_col(cum, total, bar.width)
        };
        cum += dur;
        let end_x = if i == parts.len() - 1 {
            bar.width
        } else {
            marker_col(cum, total, bar.width)
        };
        let width = end_x.saturating_sub(start_x);
        if width == 0 {
            continue; // rounds away — no cell, no divider
        }
        let text = if i == 0 || start_x == 0 {
            truncate_label(label, width)
        } else {
            // A blend is part of both neighbours: its boundary is a bare
            // colour change, not a bar. (A dotted box glyph would read as
            // permeable but sits mid-cell, so it never lines up with the
            // left-aligned `▏` dividers or the playhead marker.)
            let bar = if *soft { ' ' } else { '▏' };
            format!("{bar}{}", truncate_label(label, width.saturating_sub(1)))
        };
        cells.push((start_x, text, *style));
    }

    let mut spans: Vec<Span> = Vec::new();
    if notch {
        // Split the containing part's text around the playhead marker.
        let ph = marker_col(app.playhead, total, bar.width);
        let edge = match &app.mode {
            Mode::Extend(st) => st.edge_pos,
            _ => app.playhead,
        };
        let at_edge = (app.playhead - edge).abs() < 1e-9;
        let notch_style = Style::default().fg(if at_edge { Color::White } else { Color::Yellow });
        for (x0, text, style) in &cells {
            let chars: Vec<char> = text.chars().collect();
            let off = (ph as usize).saturating_sub(*x0 as usize);
            if ph >= *x0 && off < chars.len() {
                let before: String = chars[..off].iter().collect();
                let after: String = chars[off + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, *style));
                }
                spans.push(Span::styled("▏".to_string(), notch_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, *style));
                }
            } else {
                spans.push(Span::styled(text.clone(), *style));
            }
        }
    } else {
        for (_, text, style) in &cells {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
}

/// Draw the playhead marker row: red ▏ normally, WHITE exactly on a cut
/// frame, ▕ at EOF. In extend mode it also carries the workable span
/// (yellow) and the material extent (dim), with a yellow playhead.
pub(crate) fn draw_marker_row(f: &mut Frame, bar: Rect, app: &App, total: f64, in_extend: bool) {
    let ph = marker_col(app.playhead, total, bar.width);
    let at_end = app.eof;
    let mut marker_cells: Vec<(char, Style)> = vec![(' ', Style::default()); bar.width as usize];
    if let Mode::Extend(st) = &app.mode {
        let white_here = (app.playhead - st.edge_pos).abs() < 1e-9;
        let color = if white_here {
            Color::White
        } else {
            Color::Yellow
        };
        if let Some((m0, m1)) = st.mat {
            paint_run(
                &mut marker_cells,
                total,
                bar.width,
                m0,
                m1,
                '·',
                Style::default().fg(Color::DarkGray),
            );
        }
        let hi_edge = st.hi.unwrap_or(f64::INFINITY);
        paint_run(
            &mut marker_cells,
            total,
            bar.width,
            st.lo,
            hi_edge,
            '─',
            Style::default().fg(Color::Yellow),
        );
        marker_cells[ph as usize] = (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
    } else {
        let _ = in_extend;
        let color = if on_boundary(&app.project, app.playhead, app.active()) {
            Color::White
        } else {
            Color::Red
        };
        marker_cells[ph as usize] = (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
    }
    let mut marker: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in marker_cells {
        if run_style.as_ref() != Some(&style) {
            if let Some(s) = run_style {
                marker.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(style);
        }
        run.push(ch);
    }
    if let Some(s) = run_style {
        marker.push(Span::styled(run, s));
    }
    f.render_widget(Paragraph::new(Line::from(marker)), bar);
}

/// Paint `glyph` with `style` over the cells spanning `[a, b)` in
/// timeline seconds (`b` may exceed the row — it clamps).
pub(crate) fn paint_run(
    cells: &mut [(char, Style)],
    total: f64,
    width: u16,
    a: f64,
    b: f64,
    glyph: char,
    style: Style,
) {
    if width == 0 || b <= a {
        return;
    }
    let col_a = marker_col(a.max(0.0), total, width) as usize;
    let mut col_b = marker_col(b, total, width) as usize;
    if b >= total - 1e-9 {
        col_b = cells.len();
    }
    for cell in cells.iter_mut().take(col_b).skip(col_a) {
        *cell = (glyph, style);
    }
}
