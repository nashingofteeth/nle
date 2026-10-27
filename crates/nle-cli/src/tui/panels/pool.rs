//! The media-pool panel and the strip that replaces the timeline while the
//! pool is open.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::motion::marker_col;
use nle_core::time::Seconds;
use nle_core::timeline::{Clip, Segment as TimelineSegment, Track, TrackKind};
use nle_core::track_label;

use super::super::{App, PoolState, pool_sel, secs};
use super::timeline::draw_track_row;
/// The pool takes over the info pane: asset list on the left, probe
/// metadata for the selection on the right.
pub(crate) fn render_pool(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{List, ListState, Paragraph, Wrap};
    let [list_area, meta_area] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(0)]).areas(area);

    // Left: the asset list. The title names the track `enter` inserts into.
    let target = track_label(&app.project, app.active());
    let list_block = Block::bordered().title(format!(" media pool → {target} "));
    let list_inner = list_block.inner(list_area);
    f.render_widget(list_block, list_area);
    let items: Vec<ratatui::widgets::ListItem> = app
        .project
        .media
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let used = app.project.asset_usage(a.id);
            let mark = if used.is_empty() { "○" } else { "●" };
            let style = if i == pool_sel(app).unwrap_or(0) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} #{:<3} ", a.id), style),
                Span::styled(asset_line(a), style),
            ]))
        })
        .collect();
    // The list scrolls to keep the selected row visible: `ListState` owns the
    // viewport offset, while the selection itself lives in the mode
    // (`pool_sel`), so the widget state is rebuilt each frame, not persisted.
    let mut list_state = ListState::default();
    list_state.select(pool_sel(app));
    f.render_stateful_widget(List::new(items), list_inner, &mut list_state);

    // Right: probe metadata for the selected asset.
    let meta_block = Block::bordered().title(" metadata ");
    let meta_inner = meta_block.inner(meta_area);
    f.render_widget(meta_block, meta_area);
    let lines = match app.project.media.get(pool_sel(app).unwrap_or(0)) {
        Some(a) => asset_metadata(a),
        None => vec!["(no asset selected)".to_string()],
    };
    let para = Paragraph::new(
        lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Gray))))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    f.render_widget(para, meta_inner);
}

/// Plain-text summary for the pool row (path basename + duration).
pub(crate) fn asset_line(a: &nle_core::Asset) -> String {
    let name = a
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| a.path.display().to_string());
    let dur = a
        .duration()
        .map(|d| format!("{}s", secs(&d)))
        .unwrap_or_default();
    format!("{name} {dur}")
}

/// Human-readable probe metadata for the metadata pane (all from the
/// stored `RawProbe` — the file is never re-probed).
pub(crate) fn asset_metadata(a: &nle_core::Asset) -> Vec<String> {
    let p = &a.probe;
    let mut out = vec![
        format!("path   {}", a.path.display()),
        format!("id     #{}", a.id),
    ];
    let fmt = &p.format;
    out.push(format!("format {}", fmt.format_name));
    if let Some(d) = a.duration() {
        out.push(format!("dur    {}s", secs(&d)));
    }
    if let Some(sz) = fmt.size.as_deref() {
        out.push(format!("size   {sz} bytes"));
    }
    if let Some(b) = fmt.bit_rate.as_deref() {
        out.push(format!("bitrate {b}"));
    }
    if let Some((w, h)) = a.video_size() {
        out.push(format!("size   {w}×{h}"));
    }
    if let Some(f) = a.fps() {
        out.push(format!("fps    {}/{}", f.numer(), f.denom()));
    }
    out.push(String::new());
    for s in &p.streams {
        let kind = s.codec_type.as_deref().unwrap_or("?");
        let codec = s.codec_name.as_deref().unwrap_or("?");
        let mut line = format!("stream {} {kind}/{codec}", s.index);
        if let Some(w) = s.width
            && let Some(h) = s.height
        {
            line += &format!(" {w}×{h}");
        }
        if let Some(r) = &s.r_frame_rate {
            line += &format!(" {r}fps");
        }
        if let Some(pf) = &s.pix_fmt {
            line += &format!(" {pf}");
        }
        if let Some(sr) = s.sample_rate.as_deref() {
            line += &format!(" {sr}Hz");
        }
        if let Some(ch) = s.channels {
            line += &format!(" {ch}ch");
        }
        out.push(line);
    }
    out
}

/// The pool previews a raw file, so the monitor is not on the project
/// timeline. Replace the timeline row with the selected asset's own strip (a
/// single clip `0..duration`) and put the marker at the preview clock.
/// `panel_heights` already sized this panel to one track + marker.
pub(crate) fn render_pool_strip(f: &mut Frame, area: Rect, app: &App, st: &PoolState) {
    let Some(asset) = app.project.media.get(st.sel) else {
        f.render_widget(Block::bordered().title(" preview "), area);
        return;
    };
    let block = Block::bordered().title(format!(" preview — {} ", asset_line(asset)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }
    let Some(dur) = asset.duration() else {
        // No known duration means no scale to draw against: name only.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                asset_line(asset),
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    };
    let total = secs(&dur).max(0.001);
    // Reuse the project row renderer so the name truncates and the edges
    // quantize through the same `marker_col` as the playhead marker.
    let track = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![TimelineSegment::Clip(Clip::new(
            0,
            asset.id,
            Seconds::from_integer(0),
            dur,
        ))],
    };
    let row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    draw_track_row(f, row, app, &track, total, false);
    let marker = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    draw_pool_marker_row(f, marker, total, st.pos, app.eof);
}

/// A bare marker row for the pool strip: red `▏`, `▕` at the file's end.
pub(crate) fn draw_pool_marker_row(f: &mut Frame, bar: Rect, total: f64, pos: f64, eof: bool) {
    if bar.width == 0 {
        return;
    }
    let ph = marker_col(pos, total, bar.width) as usize;
    let mut line = Line::default();
    if ph > 0 {
        line.push_span(Span::raw(" ".repeat(ph)));
    }
    let ch = if eof { '▕' } else { '▏' };
    line.push_span(Span::styled(
        ch.to_string(),
        Style::default().fg(Color::Red),
    ));
    f.render_widget(Paragraph::new(line), bar);
}
