//! The info panel: project, active track and the clip under the playhead.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::super::motion::fmt_timecode;
use nle_core::timeline::Segment as TimelineSegment;
use nle_core::{Armed, track_label};

use super::super::actions::{clip_under, grid_time};
use super::super::{App, ExtendState, Mode, gap_under, secs};
pub(crate) fn render_info(f: &mut Frame, area: Rect, app: &App) {
    let t = app.playhead;

    let first_line = {
        let label = track_label(&app.project, app.active());
        let body = match &app.mode {
            Mode::Extend(st) => extend_status(app, st),
            _ => clip_line(app, t),
        };
        format!("[{label}] {body}")
    };
    fn clip_line(app: &App, t: f64) -> String {
        clip_under(&app.project, t, app.active())
            .map(|c| {
                let asset = app
                    .project
                    .asset(c.asset)
                    .map(|a| a.path.display().to_string())
                    .unwrap_or_else(|| "?".into());
                let fps = app.project.fps.as_ref();
                let (seg_start, seg_end) = {
                    let track = app.track().unwrap();
                    let si = track.index_at(grid_time(&app.project, t)).unwrap();
                    let s = track.start_of(si);
                    (s, s + track.segments[si].duration())
                };
                let clip_fps = app
                    .project
                    .asset(c.asset)
                    .and_then(|a| a.fps())
                    .map(|f| format!(" · {:.2}fps", secs(&f)))
                    .unwrap_or_default();
                format!(
                    "clip #{}  {}   {} – {} · len {}{clip_fps}   (src {} – {})",
                    c.id,
                    asset,
                    fmt_timecode(secs(&seg_start), fps),
                    fmt_timecode(secs(&seg_end), fps),
                    fmt_timecode(secs(&c.duration()), fps),
                    fmt_timecode(secs(&c.asset_in), fps),
                    fmt_timecode(secs(&c.asset_out), fps)
                )
            })
            .unwrap_or_else(|| match gap_under(&app.project, t, app.active()) {
                Some((gs, ge)) => {
                    let fps = app.project.fps.as_ref();
                    format!(
                        "— gap —  {} – {} · len {}",
                        fmt_timecode(secs(&gs), fps),
                        fmt_timecode(secs(&ge), fps),
                        fmt_timecode(secs(&(ge - gs)), fps)
                    )
                }
                None => "— gap —".to_string(),
            })
    }

    /// First info line in extend mode: armed edge, committable range,
    /// available material, and the mode keys.
    fn extend_status(app: &App, st: &ExtendState) -> String {
        let fps = app.project.fps.as_ref();
        let hi = st
            .hi
            .map(|h| fmt_timecode(h, fps))
            .unwrap_or_else(|| "…".to_string());
        let range = format!("{}–{}", fmt_timecode(st.lo, fps), hi);
        let flag = if st.ripple { " · RIPPLE" } else { "" };
        match &st.armed {
            Armed::Clip { id, head } => {
                let mat = match material_remaining(app, *id, *head) {
                    Some(s) => format!(" · material {s:.1}s left"),
                    None => String::new(),
                };
                format!(
                    "EXTEND {} of clip #{} · edge {} · range {range}{mat}{flag}",
                    if *head { "head" } else { "tail" },
                    id,
                    fmt_timecode(st.edge_pos, fps),
                )
            }
            Armed::GapTail { .. } => format!("EXTEND gap end · range {range} · free growth{flag}"),
            Armed::GapHead { .. } => {
                format!("EXTEND gap start · range {range} · overwrites left{flag}")
            }
        }
    }

    /// Source seconds still unused on the extend side of a clip edge:
    /// before the in-point (head) or past the out-point (tail). `None`
    /// when the asset duration is unknown.
    fn material_remaining(app: &App, id: u32, head: bool) -> Option<f64> {
        let track = app.track()?;
        let clip = track
            .segments
            .iter()
            .filter_map(|seg| match seg {
                TimelineSegment::Clip(c) if c.id == id => Some(c),
                _ => None,
            })
            .next()?;
        let asset = app.project.asset(clip.asset)?;
        let dur = asset.duration()?;
        Some(if head {
            secs(&clip.asset_in)
        } else {
            (secs(&dur) - secs(&clip.asset_out)).max(0.0)
        })
    }

    let stats = format!(
        "{} clip(s) · {} · {} fps · undo depth {}",
        app.track()
            .map(|t| {
                t.segments
                    .iter()
                    .filter(|s| matches!(s, TimelineSegment::Clip(_)))
                    .count()
            })
            .unwrap_or(0),
        fmt_timecode(app.total(), app.project.fps.as_ref()),
        app.project
            .fps
            .as_ref()
            .map(|f| format!("{}/{}", f.numer(), f.denom()))
            .unwrap_or_else(|| "?".into()),
        app.history.depth()
    );

    let mut lines = vec![
        Line::from(first_line),
        Line::from(""),
        Line::from(Span::styled(stats, Style::default().fg(Color::Gray))),
    ];
    if let Some(err) = &app.mpv_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("mpv: {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" info ")),
        area,
    );
}
