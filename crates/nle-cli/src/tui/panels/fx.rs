//! The effects panel, its per-kind form, and the transition length form.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::super::{App, FieldKind, FxForm, Mode, fx_clip, fx_state};
/// The effects panel takes over the info pane: one row per effect, in the
/// order they apply at export. Adding asks for the values in a labelled form
/// (`render_fx_form`); applying runs the same `AddEffect` as the CLI.
pub(crate) fn render_fx(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{List, ListItem, ListState, Paragraph};
    let st = fx_state(app);
    let clip = fx_clip(app);
    let form = st.and_then(|s| s.form.as_ref());
    let title = match (clip, form) {
        (Some(c), Some(f)) => {
            let verb = if f.editing.is_some() { "edit" } else { "new" };
            format!(" {verb} {} → clip #{} ", f.kind, c.id)
        }
        (Some(c), None) => format!(" effects → clip #{} ", c.id),
        (None, _) => " effects ".to_string(),
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(clip) = clip else {
        f.render_widget(
            Paragraph::new("the clip is gone — esc to close")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    if let Some(form) = form {
        render_fx_form(f, inner, form);
        return;
    }
    if clip.effects.is_empty() {
        f.render_widget(
            Paragraph::new("(no effects)   c crop · s scale · e eq · o offset")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let sel = st.map(|s| s.sel);
    let items: Vec<ListItem> = clip
        .effects
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if sel == Some(i) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>2}  ", i + 1), style),
                Span::styled(format!("{:<6}", e.kind()), style),
                Span::styled(e.details(), style),
            ]))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(sel);
    f.render_stateful_widget(List::new(items), inner, &mut list_state);
}

/// The add form: one labelled field per parameter, the focused value
/// highlighted, and the navigation line underneath.
/// What the form's fields expect: units, ranges, and neutral values.
pub(crate) fn form_hint(kind: &str) -> &'static str {
    match kind {
        "crop" => {
            "edge inset % (0 = frame edge, 100 = opposite edge); \
             the cut area shows below, else black"
        }
        "scale" => {
            "percent of the picture at this point; 100 keeps the size \
             (below 100 reveals the track below)"
        }
        "offset" => {
            "percent of the distance to the edge; 0 keeps the position, \
             ±100 is fully off-screen"
        }
        "eq" => "neutral values: brightness 0 · contrast 1 · saturation 1",
        _ => "",
    }
}

pub(crate) fn render_fx_form(f: &mut Frame, area: Rect, form: &FxForm) {
    let mut lines: Vec<Line> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let value = &field.value;
            let shown = if field.kind == FieldKind::Toggle {
                if form.lock {
                    "x".to_string()
                } else {
                    " ".to_string()
                }
            } else if i == form.focus {
                format!("{value}▏")
            } else if value.is_empty() {
                "…".to_string()
            } else {
                value.clone()
            };
            let style = if i == form.focus {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let label_style = Style::default().fg(Color::DarkGray);
            let mut spans = Vec::new();
            // The first letter is the jump key; underline it so the mnemonic
            // is visible without a separate key legend.
            if field.key.is_some() {
                let mut chars = field.label.chars();
                if let Some(first) = chars.next() {
                    spans.push(Span::styled(
                        first.to_string(),
                        label_style.add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(chars.as_str().to_string(), label_style));
                }
            } else {
                spans.push(Span::styled(field.label.to_string(), label_style));
            }
            let label_len = field.label.chars().count();
            spans.push(Span::raw(" ".repeat(12usize.saturating_sub(label_len))));
            spans.push(Span::styled(format!("[{shown}]"), style));
            if let Some(previous) = &field.previous {
                spans.push(Span::styled(
                    format!("  was {previous}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(form_hint(form.kind)).style(Style::default().fg(Color::DarkGray)));
    lines.push(
        Line::from("letters jump · tab/↑↓ field · space toggles lock · enter applies · ctrl-u clears · esc cancels")
            .style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

pub(crate) fn render_transition_form(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Transition(form) = &app.mode else {
        return;
    };
    let block = Block::bordered().title(format!(" transition #{} ", form.id));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let shown = if form.frames.is_empty() {
        "▏".to_string()
    } else {
        format!("{}▏", form.frames)
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{:<11}", "frames"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("[{shown}]"),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(
                format!("  was {}", form.current),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from("enter applies · esc cancels").style(Style::default().fg(Color::DarkGray)),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}
