//! The effects panel's keys and forms: one form per effect kind, a sticky
//! scale aspect lock, and the one-field transition length form.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use nle_core::action::{EffectArgs, effect_from_args};
use nle_core::timeline::{Effect, TrackKind, TransitionKind};
use nle_core::{Action, EditError, TransitionLen, edit, secs_f64};

use crate::mpv::Mpv;

use super::super::actions::run_action;
use super::super::{
    App, FieldKind, FxField, FxForm, FxState, Mode, TransitionForm, clamp_fx_sel, clip_by_id,
    clip_ending_at, clip_under, fx_state, fx_state_mut, set_fx_form, set_fx_sel,
};
use super::super::{do_redo, do_undo};
/// `x` — open the effects panel on the clip under the playhead (or the clip
/// ending there, so `xx` works at a clip|gap cut). Effects are video filters,
/// so an audio clip gets the model's own refusal.
pub(crate) fn open_fx(app: &mut App) {
    let clip = clip_under(&app.project, app.playhead, app.active())
        .or_else(|| clip_ending_at(&app.project, app.playhead, app.active()))
        .cloned();
    let Some(clip) = clip else {
        app.set_message("move the playhead onto a clip to see its effects");
        return;
    };
    if app
        .project
        .timeline
        .track(app.active())
        .is_some_and(|t| t.kind != TrackKind::Video)
    {
        app.set_message(&crate::edit_message(&EditError::EffectsNeedVideo));
        return;
    }
    app.mode = Mode::Fx(FxState {
        clip: clip.id,
        sel: 0,
        form: None,
    });
    app.set_message("effects");
}

/// The blend at the playhead on the active track: id, start, kind, frames.
pub(crate) fn transition_target(app: &App) -> Option<(u32, f64, TransitionKind, i64)> {
    let fps = app.project.fps?;
    let (_, tr, start) = edit::transition_at(&app.project, app.active(), app.playhead)
        .ok()
        .flatten()?;
    let frames = (tr.dur * fps).round().to_integer();
    Some((tr.id, secs_f64(&start), tr.kind, frames))
}

/// Open the transition length form, optionally pre-filled with a count.
pub(crate) fn open_transition_form(app: &mut App, prefill: Option<i64>) {
    let Some((id, start, kind, current)) = transition_target(app) else {
        app.set_message("move the playhead onto a transition to change its length");
        return;
    };
    app.mode = Mode::Transition(TransitionForm {
        id,
        start,
        kind,
        frames: prefill.map(|n| n.to_string()).unwrap_or_default(),
        current,
    });
    app.set_message(&format!("transition #{id} — enter frames, enter applies"));
}

/// Transition form keys: digits edit the frame count; enter applies.
pub(crate) fn transition_form_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.set_message("timeline");
        }
        KeyCode::Enter => apply_transition_form(app, player),
        KeyCode::Backspace => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.pop();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.clear();
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.push(c);
            }
        }
        _ => {}
    }
}

pub(crate) fn apply_transition_form(app: &mut App, player: &dyn Mpv) {
    let Some(form) = (match &app.mode {
        Mode::Transition(f) => Some(f.clone()),
        _ => None,
    }) else {
        return;
    };
    let text = form.frames.trim();
    let frames = if text.is_empty() {
        form.current
    } else {
        match text.parse::<i64>() {
            Ok(n) => n,
            Err(_) => {
                app.set_message("frames must be a whole number");
                return;
            }
        }
    };
    if frames < 1 {
        app.set_message("frames must be at least 1");
        return;
    }
    if frames == form.current {
        app.mode = Mode::Normal;
        app.set_message(&format!("transition #{} unchanged", form.id));
        return;
    }
    if run_action(
        app,
        player,
        &Action::Transition {
            track: app.active(),
            at: form.start,
            kind: form.kind,
            len: TransitionLen::Frames(frames),
        },
    ) {
        app.mode = Mode::Normal;
    }
}

/// Close the panel. Unlike the pool it hasn't moved the monitor, so there is
/// nothing to reload.
pub(crate) fn close_fx(app: &mut App) {
    app.mode = Mode::Normal;
    app.set_message("timeline");
}

/// The "add" form for an effect kind. Every input starts blank; the neutral
/// value is shown as "was N" beside it and is used if the field is left blank.
/// Each numeric field's first letter is its jump key.
pub(crate) fn form_for(kind: &'static str) -> FxForm {
    let field = |label, previous: Option<&str>, kind, key: Option<char>| FxField {
        label,
        value: String::new(),
        previous: previous.map(str::to_string),
        kind,
        key,
    };
    let fields = match kind {
        "crop" => vec![
            field("left %", Some("0"), FieldKind::Float, Some('l')),
            field("right %", Some("0"), FieldKind::Float, Some('r')),
            field("top %", Some("0"), FieldKind::Float, Some('t')),
            field("bottom %", Some("0"), FieldKind::Float, Some('b')),
        ],
        "scale" => vec![
            field("width %", Some("100"), FieldKind::Float, Some('w')),
            field("height %", Some("100"), FieldKind::Float, Some('h')),
            // A real field so tab/arrows reach it; `a` jumps here and space
            // toggles it.
            field("aspect lock", None, FieldKind::Toggle, Some('a')),
        ],
        "offset" => vec![
            field("x %", Some("0"), FieldKind::Float, Some('x')),
            field("y %", Some("0"), FieldKind::Float, Some('y')),
        ],
        "eq" => vec![
            field("brightness", Some("0"), FieldKind::Float, Some('b')),
            field("contrast", Some("1"), FieldKind::Float, Some('c')),
            field("saturation", Some("1"), FieldKind::Float, Some('s')),
        ],
        _ => Vec::new(),
    };
    FxForm {
        kind,
        fields,
        focus: 0,
        editing: None,
        // Scale is aspect-locked by default; the App's sticky preference
        // overrides this when the form is opened.
        lock: kind == "scale",
        aspect: 1.0,
    }
}

/// An "edit" form: same fields, but the inputs start blank and each shows the
/// applied value beside it. A blank field keeps that value on apply.
pub(crate) fn form_for_effect(effect: &Effect, index: usize) -> FxForm {
    let mut form = form_for(effect.kind());
    form.editing = Some(index);
    if let Effect::Scale { w, h } = effect
        && *h > 0.0
    {
        form.aspect = *w / *h;
    }
    let values: Vec<String> = match effect {
        Effect::Crop {
            left,
            right,
            top,
            bottom,
        } => [left, right, top, bottom]
            .into_iter()
            .map(|v| v.to_string())
            .collect(),
        Effect::Scale { w, h } => vec![w.to_string(), h.to_string()],
        Effect::Offset { x, y } => vec![x.to_string(), y.to_string()],
        Effect::Eq {
            brightness,
            contrast,
            saturation,
        } => vec![
            brightness.to_string(),
            contrast.to_string(),
            saturation.to_string(),
        ],
        Effect::Lut { path } => vec![path.clone()],
    };
    for (field, previous) in form.fields.iter_mut().zip(values) {
        field.value.clear();
        field.previous = Some(previous);
    }
    form
}

pub(crate) fn open_effect_form(app: &mut App, kind: &'static str) {
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    // One effect per kind: if this kind is already on the clip, its add key
    // opens that row for editing instead of stacking a duplicate.
    let index = clip_by_id(&app.project, clip).and_then(|c| {
        if c.effects.get(sel).is_some_and(|e| e.kind() == kind) {
            Some(sel)
        } else {
            c.effects.iter().position(|e| e.kind() == kind)
        }
    });
    if let Some(index) = index {
        edit_effect_at(app, clip, index);
        return;
    }
    let mut form = form_for(kind);
    if kind == "scale" {
        form.lock = app.fx_scale_lock;
    }
    set_fx_form(app, Some(form));
    app.set_message(&format!(
        "new {kind} — letters jump, tab moves, enter applies"
    ));
}

/// Enter on a listed effect opens it for editing, prefilled with its values.
pub(crate) fn open_edit_form(app: &mut App) {
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    edit_effect_at(app, clip, sel);
}

/// Open effect `index` of `clip` as an edit form: blank inputs, old values
/// shown beside them.
pub(crate) fn edit_effect_at(app: &mut App, clip: u32, index: usize) {
    let Some(effect) = clip_by_id(&app.project, clip)
        .and_then(|c| c.effects.get(index))
        .cloned()
    else {
        app.set_message("no effect to edit");
        return;
    };
    if matches!(effect, Effect::Lut { .. }) {
        app.set_message("LUT effects are CLI-only for now — use :effect lut");
        return;
    }
    let kind = effect.kind();
    let mut form = form_for_effect(&effect, index);
    if kind == "scale" {
        form.lock = app.fx_scale_lock;
    }
    set_fx_form(app, Some(form));
    app.set_message(&format!(
        "edit {kind} — blank keeps the old value, enter applies"
    ));
}

/// Does a typed character belong in the focused field?
pub(crate) fn field_accepts(kind: FieldKind, c: char) -> bool {
    match kind {
        FieldKind::Float => c.is_ascii_digit() || c == '.' || c == '-',
        FieldKind::Path => !c.is_control(),
        FieldKind::Toggle => false,
    }
}

/// Edit the focused field's string; the borrow ends inside so callers can
/// keep touching `app`.
pub(crate) fn fx_edit_field(app: &mut App, f: impl FnOnce(&mut String)) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    let Some(field) = form.fields.get_mut(form.focus) else {
        return;
    };
    f(&mut field.value);
}

pub(crate) fn fx_move_focus(app: &mut App, delta: isize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    let last = form.fields.len().saturating_sub(1);
    form.focus = form.focus.saturating_add_signed(delta).min(last);
}

/// Focus one field by index (a letter mnemonic).
pub(crate) fn fx_focus(app: &mut App, index: usize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    if index < form.fields.len() {
        form.focus = index;
    }
}

/// Flip the scale form's aspect lock (its toggle field is focused).
pub(crate) fn fx_toggle_lock(app: &mut App) {
    let lock = {
        let Some(st) = fx_state_mut(app) else { return };
        let Some(form) = st.form.as_mut() else { return };
        form.lock = !form.lock;
        form.lock
    };
    app.fx_scale_lock = lock;
}

/// With the scale form's lock on, mirror the field being typed into the other
/// axis using the ratio captured when the form opened.
pub(crate) fn fx_sync_aspect(app: &mut App, edited: usize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    if !form.lock || form.kind != "scale" || form.fields.len() < 2 || form.aspect <= 0.0 {
        return;
    }
    if edited >= 2 {
        return; // the lock toggle itself
    }
    let Some(value) = form
        .fields
        .get(edited)
        .and_then(|f| f.value.trim().parse::<f64>().ok())
    else {
        return;
    };
    if value <= 0.0 {
        return;
    }
    let synced = if edited == 0 {
        value / form.aspect
    } else {
        value * form.aspect
    };
    if let Some(other) = form.fields.get_mut(1 - edited) {
        other.value = fmt_percent(synced);
    }
}

/// A percentage for a mirrored field: two decimals, no trailing zeros.
pub(crate) fn fmt_percent(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Turn the form's strings into the typed effect and apply it. The values go
/// through `effect_from_args` — the CLI's parser — so the required fields and
/// neutral defaults can't drift between the two front-ends.
pub(crate) fn apply_fx_form(app: &mut App, player: &dyn Mpv) {
    let Some(clip) = fx_state(app).map(|s| s.clip) else {
        return;
    };
    let Some(form) = fx_state(app).and_then(|s| s.form.clone()) else {
        return;
    };
    let effect = match form_effect(&form) {
        Ok(effect) => effect,
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
    };
    // An edit that changes nothing is not a history entry: just close.
    if let Some(i) = form.editing
        && clip_by_id(&app.project, clip).and_then(|c| c.effects.get(i)) == Some(&effect)
    {
        set_fx_form(app, None);
        app.mode = Mode::Normal;
        app.set_message("effect unchanged");
        return;
    }
    let applied = run_action(
        app,
        player,
        &Action::AddEffect {
            track: app.active(),
            clip,
            effect,
            index: form.editing,
        },
    );
    // Applying is the end of the gesture: leave the panel, keeping the
    // action's notice ("effect on clip #N"). A refused value never reaches
    // here, so the form stays open only while there is something to fix.
    if applied {
        set_fx_form(app, None);
        app.mode = Mode::Normal;
    }
}

pub(crate) fn form_effect(form: &FxForm) -> Result<Effect, String> {
    let value = |i: usize| form.fields.get(i).map_or("", FxField::effective);
    let opt_float = |i: usize, name: &str| -> Result<Option<f64>, String> {
        let v = value(i);
        if v.is_empty() {
            return Ok(None);
        }
        v.parse()
            .map(Some)
            .map_err(|_| format!("{name} must be a number"))
    };
    let args = match form.kind {
        "crop" => EffectArgs {
            left: opt_float(0, "left")?,
            right: opt_float(1, "right")?,
            top: opt_float(2, "top")?,
            bottom: opt_float(3, "bottom")?,
            ..Default::default()
        },
        "scale" => EffectArgs {
            w: opt_float(0, "width")?,
            h: opt_float(1, "height")?,
            ..Default::default()
        },
        "offset" => EffectArgs {
            x: opt_float(0, "x")?,
            y: opt_float(1, "y")?,
            ..Default::default()
        },
        "eq" => EffectArgs {
            brightness: opt_float(0, "brightness")?,
            contrast: opt_float(1, "contrast")?,
            saturation: opt_float(2, "saturation")?,
            ..Default::default()
        },
        other => return Err(format!("unknown effect {other:?}")),
    };
    effect_from_args(form.kind, args).map_err(|e| e.to_string())
}

/// Add a cross-dissolve at the playhead (`X`, or `xx` from Normal — the first
/// `x` opens the panel and the second lands here). A count is the length in
/// frames; without one it is a second.
/// Add a cross-dissolve at the playhead (`X`, or `xx` from Normal — the first
/// `x` opens the panel and the second lands here). Returns whether it landed.
pub(crate) fn add_transition(app: &mut App, player: &dyn Mpv, frames: Option<i64>) -> bool {
    let len = match frames {
        Some(n) => TransitionLen::Frames(n),
        None => TransitionLen::Seconds(1.0),
    };
    run_action(
        app,
        player,
        &Action::Transition {
            track: app.active(),
            at: app.playhead,
            kind: TransitionKind::Dissolve,
            len,
        },
    )
}

/// Effects panel keys: list motion, drop/clear, add (structured form), undo.
pub(crate) fn fx_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    if fx_state(app).is_some_and(|st| st.form.is_some()) {
        fx_form_key(app, player, key);
        return;
    }
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    let Some(c) = clip_by_id(&app.project, clip) else {
        // The clip vanished (an undo of the insert that made it): leave.
        close_fx(app);
        return;
    };
    let n = c.effects.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => close_fx(app),
        KeyCode::Char('j') | KeyCode::Down => {
            set_fx_sel(app, (sel + 1).min(n.saturating_sub(1)));
        }
        KeyCode::Char('k') | KeyCode::Up => set_fx_sel(app, sel.saturating_sub(1)),
        // The rows are numbered 1..n: a digit opens that effect directly.
        KeyCode::Char(digit @ '1'..='9') => {
            let row = digit.to_digit(10).unwrap_or(0) as usize;
            if row <= n {
                set_fx_sel(app, row - 1);
                open_edit_form(app);
            } else {
                app.set_message(&format!("no effect #{row}"));
            }
        }
        // Selecting an applied effect opens it for editing (blank keeps the
        // current value); `d`/`D` remain the way to remove.
        KeyCode::Enter => open_edit_form(app),
        KeyCode::Char('d') => {
            if n == 0 {
                app.set_message("no effects to drop");
                return;
            }
            run_action(
                app,
                player,
                &Action::RemoveEffect {
                    track: app.active(),
                    clip,
                    index: Some(sel.min(n - 1)),
                },
            );
            clamp_fx_sel(app);
        }
        KeyCode::Char('D') => {
            if n == 0 {
                app.set_message("no effects to clear");
                return;
            }
            run_action(
                app,
                player,
                &Action::ClearEffects {
                    track: app.active(),
                    clip,
                },
            );
            set_fx_sel(app, 0);
        }
        // Adding asks for the values in labelled fields instead of leaving a
        // `:effect …` line to complete by hand.
        KeyCode::Char('c') => open_effect_form(app, "crop"),
        KeyCode::Char('s') => open_effect_form(app, "scale"),
        KeyCode::Char('e') => open_effect_form(app, "eq"),
        KeyCode::Char('o') => open_effect_form(app, "offset"),
        // `xx`: the first `x` opened the panel; the second applies the
        // transition and exits the panel (a failure keeps it open to retry).
        KeyCode::Char('x') => {
            if transition_target(app).is_some() {
                open_transition_form(app, None);
            } else if add_transition(app, player, None) {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char('u') => {
            do_undo(app, player);
            clamp_fx_sel(app);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            do_redo(app, player);
            clamp_fx_sel(app);
        }
        _ => {}
    }
}

/// Keys while the add form is open: type into the focused field, tab/enter to
/// move, enter on the last applies, esc cancels.
pub(crate) fn fx_form_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let Some(form) = fx_state(app).and_then(|s| s.form.clone()) else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            set_fx_form(app, None);
            app.set_message("effects");
        }
        KeyCode::Tab | KeyCode::Down => fx_move_focus(app, 1),
        KeyCode::BackTab | KeyCode::Up => fx_move_focus(app, -1),
        // Enter is the commit: it validates every field and applies. Only tab
        // (or the arrows) moves between fields.
        KeyCode::Enter => apply_fx_form(app, player),
        KeyCode::Backspace => {
            fx_edit_field(app, |v| {
                v.pop();
            });
            fx_sync_aspect(app, form.focus);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            fx_edit_field(app, String::clear);
        }
        // Space flips the focused toggle (the scale form's lock).
        KeyCode::Char(' ')
            if form
                .fields
                .get(form.focus)
                .is_some_and(|f| f.kind == FieldKind::Toggle) =>
        {
            fx_toggle_lock(app);
        }
        KeyCode::Char(c) => {
            let focused = form.fields.get(form.focus);
            // A focused toggle has no text: its own jump key flips it.
            if focused.is_some_and(|f| f.kind == FieldKind::Toggle)
                && focused.is_some_and(|f| f.key == Some(c.to_ascii_lowercase()))
            {
                fx_toggle_lock(app);
                return;
            }
            // In a numeric form the field labels double as jump keys. A path
            // field must receive every letter as text, so it is exempt.
            if focused.is_some_and(|f| f.kind != FieldKind::Path)
                && let Some(i) = form
                    .fields
                    .iter()
                    .position(|f| f.key == Some(c.to_ascii_lowercase()))
            {
                fx_focus(app, i);
                return;
            }
            if focused.is_some_and(|f| field_accepts(f.kind, c)) {
                fx_edit_field(app, |v| v.push(c));
                fx_sync_aspect(app, form.focus);
            }
        }
        _ => {}
    }
}
