use super::super::*;
use super::*;
use crate::timeline::{Effect, Track};

// A transition renders its neighbours' material, so each side carries a
// copy of that neighbour's effects; effect edits keep the copies in sync,
// and undo restores both in one step.
#[test]
fn a_transition_side_follows_its_neighbours_effects() {
    let eq = Effect::Eq {
        brightness: 0.2,
        contrast: 1.0,
        saturation: 1.0,
    };
    let crop = Effect::Crop {
        left: 10.0,
        right: 10.0,
        top: 0.0,
        bottom: 0.0,
    };
    let mut p = clip_pair(); // C1 0..3, C2 3..6
    for seg in &mut p.timeline.video_track_mut().unwrap().segments {
        if let Segment::Clip(c) = seg {
            c.effects = vec![eq.clone()];
        }
    }
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    let sides = |p: &Project| match &p.timeline.video_track().unwrap().segments[1] {
        Segment::Transition(t) => (t.a.clone(), t.b.clone()),
        other => panic!("expected the blend, got {other:?}"),
    };
    let effects = |side: &TransitionSource| match side {
        TransitionSource::Clip { effects, .. } => effects.clone(),
        TransitionSource::Black => panic!("expected a clip side"),
    };
    let (a, b) = sides(&p);
    assert_eq!(effects(&a), vec![eq.clone()], "a copies the preceding clip");
    assert_eq!(effects(&b), vec![eq.clone()], "b copies the following clip");

    // The following clip's head side follows an edit…
    let batch = add_effect_on(&p, 0, 2, crop.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let (_, b) = sides(&p);
    assert_eq!(effects(&b), vec![eq.clone(), crop.clone()]);

    // …and a clear empties it.
    let batch = clear_effects_on(&p, 0, 2).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let (_, b) = sides(&p);
    assert!(effects(&b).is_empty(), "cleared with the clip");

    // The preceding clip's tail side follows too, and undo removes the
    // effect from the clip *and* the side in one step.
    let batch = add_effect_on(&p, 0, 1, crop.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let (a, _) = sides(&p);
    assert_eq!(effects(&a), vec![eq.clone(), crop.clone()]);

    h.undo(&mut p).unwrap();
    let (a, _) = sides(&p);
    assert_eq!(effects(&a), vec![eq.clone()], "undo restored the side too");

    // Editing a row in place syncs the transition side in the same batch.
    let batch = replace_effect_on(&p, 0, 1, 0, crop.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let (a, _) = sides(&p);
    assert_eq!(effects(&a), vec![crop], "the tail side follows the edit");
    h.undo(&mut p).unwrap();
    let (a, _) = sides(&p);
    assert_eq!(effects(&a), vec![eq], "one undo restores the side");
}

/// Every edit that rebuilds a clip carries its effect list over —
/// trimming, splitting, gap-inserting and the paste sweep used to reset
/// it, so a `:effect` on a clip vanished on the next edit.
#[test]
fn edits_preserve_per_clip_effects() {
    let eff = Effect::Scale { w: 10.0, h: 10.0 };
    let set_effects = |p: &mut Project| {
        for seg in &mut p.timeline.video_track_mut().unwrap().segments {
            if let Segment::Clip(c) = seg {
                c.effects = vec![eff.clone()];
            }
        }
    };
    let clip_effects = |p: &Project| {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Clip(c) => Some(c.effects.clone()),
                Segment::Gap(_) | Segment::Transition(_) => None,
            })
            .collect::<Vec<Vec<Effect>>>()
    };
    let applied = |p: &Project, batch: Batch| {
        let mut q = p.clone();
        History::default().apply(&mut q, batch, 0).unwrap();
        q
    };
    let only = vec![eff.clone()];

    // `s` — both halves of the split keep the effect.
    let mut p = fixture();
    set_effects(&mut p);
    let q = applied(&p, split(&p, 1.0).unwrap());
    assert_eq!(
        clip_effects(&q),
        vec![only.clone(), only.clone(), only.clone()],
        "split"
    );

    // `i` / `o` — the trimmed clip keeps it.
    assert_eq!(
        clip_effects(&applied(&p, trim_in(&p, 0.5).unwrap())),
        vec![only.clone(), only.clone()],
        "trim in"
    );
    assert_eq!(
        clip_effects(&applied(&p, trim_out(&p, 1.0).unwrap())),
        vec![only.clone(), only.clone()],
        "trim out"
    );

    // `_` — the split head and tail both keep it.
    let q = applied(&p, insert_gap(&p, 1.0, 3).unwrap());
    assert_eq!(
        clip_effects(&q),
        vec![only.clone(), only.clone(), only.clone()],
        "insert gap"
    );

    // `p` — the sweep leaves a pasted clip (its own list) and a remainder
    // that keeps the target clip's effect.
    let asset = match &p.timeline.video_track().unwrap().segments[0] {
        Segment::Clip(c) => c.asset,
        _ => unreachable!(),
    };
    let snapshot = Segment::Clip(Clip::new(
        9,
        asset,
        Seconds::from_integer(0),
        Seconds::new(1, 2),
    ));
    let q = applied(&p, paste(&p, &snapshot, 1.0).unwrap());
    let got = clip_effects(&q);
    assert_eq!(got.len(), 4, "left piece, paste, remainder, clip #2");
    assert_eq!(got[0], only);
    assert!(got[1].is_empty(), "the pasted clip carries its own list");
    assert_eq!(got[2], vec![eff.clone()], "the swept remainder keeps it");
    assert_eq!(got[3], vec![eff.clone()]);

    // `:purge` — the snapped clip keeps it.
    let track = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![
            Segment::Clip(Clip {
                id: 1,
                asset,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
                effects: vec![eff.clone()],
            }),
            Segment::Gap(Gap {
                duration: Seconds::new(1, 100), // a stump, dropped
            }),
        ],
    };
    let (cleaned, dropped) = purge_stumps(&track, Seconds::from_integer(30));
    assert_eq!(dropped, 1);
    match &cleaned[0] {
        Segment::Clip(c) => assert_eq!(c.effects, vec![eff.clone()], "purge"),
        s => panic!("expected a clip, got {s:?}"),
    }
}

// `add_effect` is the first-video-track wrapper; the `_on` form refuses a
// track whose clips the graph never applies effects to.
#[test]
fn effects_are_refused_on_an_audio_track() {
    let mut p = fixture();
    let batch = add_effect(&p, 1, Effect::Scale { w: 2.0, h: 2.0 }).unwrap();
    assert!(batch.desc.contains("clip #1"), "{}", batch.desc);

    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[ai].segments = vec![Segment::Clip(Clip::new(
        9,
        1,
        Seconds::from_integer(0),
        Seconds::from_integer(2),
    ))];
    assert!(matches!(
        add_effect_on(&p, ai, 9, Effect::Scale { w: 2.0, h: 2.0 }),
        Err(EditError::EffectsNeedVideo)
    ));
}

// One effect per kind: re-adding a kind replaces the row in place (same
// index, one undo step) instead of stacking a second copy.
#[test]
fn adding_the_same_kind_replaces_the_previous_one() {
    let effects = |p: &Project| match &p.timeline.video_track().unwrap().segments[0] {
        Segment::Clip(c) => c.effects.clone(),
        other => panic!("expected a clip, got {other:?}"),
    };
    let mut p = fixture();
    let mut h = History::default();
    let eq = Effect::Eq {
        brightness: 0.1,
        contrast: 1.0,
        saturation: 1.0,
    };
    let first = Effect::Crop {
        left: 10.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };
    let second = Effect::Crop {
        left: 30.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };
    for effect in [eq.clone(), first.clone()] {
        let batch = add_effect(&p, 1, effect).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
    }
    assert_eq!(effects(&p), vec![eq.clone(), first.clone()]);

    let batch = add_effect(&p, 1, second.clone()).unwrap();
    assert!(batch.desc.contains("edit crop"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        effects(&p),
        vec![eq.clone(), second.clone()],
        "the crop row is replaced in place, after eq"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(effects(&p), vec![eq, first]);
}

// `:effect rm` drops one effect (default: the last) and `:effect clear`
// drops the rest — each is one undo step, and both refuse an empty clip
// rather than recording a no-op.
#[test]
fn effects_can_be_removed_and_cleared() {
    let effects = |p: &Project| match &p.timeline.video_track().unwrap().segments[0] {
        Segment::Clip(c) => c.effects.clone(),
        other => panic!("expected a clip, got {other:?}"),
    };
    let scale = Effect::Scale { w: 2.0, h: 2.0 };
    let crop = Effect::Crop {
        left: 10.0,
        right: 10.0,
        top: 0.0,
        bottom: 0.0,
    };

    let mut p = fixture();
    let mut h = History::default();
    let batch = add_effect(&p, 1, scale.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = add_effect(&p, 1, crop.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(effects(&p), vec![scale.clone(), crop.clone()]);

    // An explicit index drops that row (0-based), and the notice names it.
    let batch = remove_effect_on(&p, 0, 1, Some(0)).unwrap();
    assert!(batch.desc.contains("scale"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(effects(&p), vec![crop.clone()]);

    // `None` drops the last, and an empty list is an error.
    let batch = remove_effect_on(&p, 0, 1, None).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert!(effects(&p).is_empty());
    assert!(matches!(
        remove_effect_on(&p, 0, 1, None),
        Err(EditError::NoEffects(1))
    ));
    assert!(matches!(
        remove_effect_on(&p, 0, 1, Some(3)),
        Err(EditError::EffectNotFound(1, 3))
    ));
    assert!(matches!(
        clear_effects_on(&p, 0, 1),
        Err(EditError::NoEffects(1))
    ));

    // Clear drops every row in one batch; undo restores them together.
    let batch = add_effect(&p, 1, crop.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = add_effect(&p, 1, scale.clone()).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = clear_effects_on(&p, 0, 1).unwrap();
    assert!(batch.desc.contains("2 effect"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert!(effects(&p).is_empty());
    h.undo(&mut p).unwrap();
    assert_eq!(effects(&p), vec![crop.clone(), scale.clone()]);

    // Replacing a row is one batch; undo restores the old effect. A row
    // may swap to a kind no other row holds, but a rewrite into a kind
    // another row already holds is refused — one effect per kind runs
    // in reverse too (`add_effect_on` replaces in place; stacking a
    // second scale and silently orphaning the crop is the failure,
    // and the CLI's `--index` is the only path that changes kinds).
    assert!(matches!(
        replace_effect_on(&p, 0, 1, 0, scale.clone()),
        Err(EditError::DuplicateEffect(1, "scale"))
    ));
    let eq = Effect::Eq {
        brightness: 0.0,
        contrast: 1.0,
        saturation: 1.0,
    };
    let batch = replace_effect_on(&p, 0, 1, 0, eq.clone()).unwrap();
    assert!(batch.desc.contains("edit eq"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(effects(&p), vec![eq, scale.clone()]);
    h.undo(&mut p).unwrap();
    assert_eq!(effects(&p), vec![crop, scale]);
    assert!(matches!(
        replace_effect_on(&p, 0, 1, 5, Effect::Scale { w: 50.0, h: 50.0 }),
        Err(EditError::EffectNotFound(1, 5))
    ));
}
