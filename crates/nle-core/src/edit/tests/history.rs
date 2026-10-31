use std::path::{Path, PathBuf};

use super::*;
use crate::time::Seconds;
use crate::timeline::{Clip, Segment};

#[test]
fn failed_batches_leave_no_trace() {
    let p = fixture();
    let h = History::default();
    assert!(trim_in(&p, 99.0).is_err()); // past the end
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    let _ = h;
}

#[test]
fn sidecar_path_maps_beside_the_project() {
    assert_eq!(
        History::sidecar_path(Path::new("nle.json")),
        PathBuf::from("nle.history.json")
    );
    // A project without an extension still gets one.
    assert_eq!(
        History::sidecar_path(Path::new("dir/proj")),
        PathBuf::from("dir/proj.history.json")
    );
}

#[test]
fn sidecar_round_trips_both_stacks() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = trim_in(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    h.undo(&mut p).unwrap(); // done=[split], undone=[trim]
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.history.json");
    h.save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!("\"version\": {HISTORY_VERSION}")),
        "{text}"
    );
    // Both stacks persist: `nile redo` runs in its own process, so a
    // session-only redo stack would be permanently empty.
    assert!(text.contains("undone"), "{text}");
    assert!(!text.contains("30/1"), "{text}");

    let mut loaded = History::load(&path).unwrap();
    assert!(loaded.can_undo());
    assert!(loaded.can_redo());
    let mut q = p.clone();
    loaded.redo(&mut q).unwrap(); // the trim reapplies
    assert_eq!(clip_ids(&q), vec![1, 2, 3]);
    // and both entries still undo, newest first
    loaded.undo(&mut q).unwrap();
    loaded.undo(&mut q).unwrap(); // the split undoes
    assert_eq!(clip_ids(&q), vec![1, 2]);
}

#[test]
fn history_stores_and_returns_the_playhead_frame() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 42).unwrap();
    // trim the tail of clip #1 at 1.0 (it spans [0..2)).
    let batch = trim_out(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 90).unwrap();
    // undo returns the frame its edit was made from
    assert_eq!(h.undo(&mut p).unwrap().playhead, 90);
    assert_eq!(h.undo(&mut p).unwrap().playhead, 42);
    // redo returns the same frame (re-applies that edit)
    assert_eq!(h.redo(&mut p).unwrap().playhead, 42);
    // out-of-range frames can't occur (stored verbatim)
    assert_eq!(h.undo(&mut p).unwrap().playhead, 42);
}

#[test]
fn persisted_edits_undo_by_topology_after_reload() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = trim_in(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let snapshot = clip_by_id(&p, 1);
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.history.json");
    h.save(&path).unwrap();

    // Restart simulation: project and history both reloaded from disk
    // (production saves both after every edit). The reloaded project
    // is in the post-edit state; history is purely from the sidecar.
    let saved_project = serde_json::to_string(&p).unwrap();
    let mut p2: Project = serde_json::from_str(&saved_project).unwrap();
    let mut h2 = History::load(&path).unwrap();
    h2.undo(&mut p2).unwrap(); // paste_ripple
    h2.undo(&mut p2).unwrap(); // trim_in
    let outcome = h2.undo(&mut p2).unwrap(); // split
    assert!(outcome.desc.contains("split"), "{}", outcome.desc);
    assert_eq!(clip_ids(&p2), vec![1, 2]);
    assert_eq!(clip_by_id(&p2, 2).asset_out, Seconds::from_integer(2));
    assert_eq!(p2.timeline.end(), Seconds::from_integer(5));
    // redo was rebuilt live in-session
    assert!(h2.can_redo());
    h2.redo(&mut p2).unwrap();
    assert_eq!(clip_ids(&p2), vec![1, 2, 3]);
}

#[test]
fn stale_history_fails_cleanly_on_edited_project() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.history.json");
    h.save(&path).unwrap();

    // Hand-edit outside undo: renumber a clip id.
    let mut p2 = fixture();
    if let Segment::Clip(c) = &mut p2.timeline.video_track_mut().unwrap().segments[2] {
        c.id = 99;
    }
    let mut h2 = History::load(&path).unwrap();
    assert!(matches!(h2.undo(&mut p2), Err(EditError::History(_))));
    // project untouched by the failed undo
    assert_eq!(clip_ids(&p2), vec![1, 99]);
    assert_eq!(p2.timeline.end(), Seconds::from_integer(5));
}

// An unused pooled asset can be removed and undone; the whole pool is
// restored (same order/ids).
#[test]
fn asset_addition_is_undoable() {
    let mut p = fixture();
    let before = p.media.clone();

    let mut h = History::default();
    let (id, desc) = h.add_asset(&mut p, "/media/extra.mp4".into(), probe_report("5"), 42);
    assert!(desc.contains(&format!("#{id}")), "{desc}");
    assert!(p.asset(id).is_some(), "pooled");
    assert_eq!(p.media.len(), before.len() + 1);

    let outcome = h.undo(&mut p).unwrap();
    assert_eq!(outcome.desc, desc, "undo names the same entry");
    assert_eq!(outcome.playhead, 42);
    assert!(p.asset(id).is_none(), "the import undoes");

    h.redo(&mut p).unwrap();
    assert!(p.asset(id).is_some(), "and redoes");
}

// Undoing an import that a clip now depends on refuses loudly (validate
// catches the dangling asset) rather than leaving a broken project.
#[test]
fn undoing_an_import_in_use_refuses() {
    let mut p = fixture();
    let mut h = History::default();
    let (id, _) = h.add_asset(&mut p, "/media/extra.mp4".into(), probe_report("5"), 0);
    // A clip of the new asset lands on the timeline — outside the stack,
    // as if an out-of-band write put it there.
    let clip = Segment::Clip(Clip::new(
        99,
        id,
        Seconds::from_integer(0),
        Seconds::from_integer(1),
    ));
    for op in &paste_ripple(&p, &clip, 0.0).unwrap().ops {
        op.apply(&mut p).unwrap();
    }
    let err = h.undo(&mut p).unwrap_err();
    assert!(matches!(err, EditError::Project(_)), "refused: {err}");
    assert!(p.asset(id).is_some(), "the asset survived the refusal");
}

#[test]
fn asset_removal_is_undoable() {
    let mut p = fixture();
    let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
    let before = p.media.clone();

    let mut h = History::default();
    let desc = h.remove_asset(&mut p, spare, 42).unwrap();
    assert!(desc.contains(&format!("#{spare}")), "{desc}");
    assert!(p.asset(spare).is_none(), "gone after removal");
    assert_eq!(p.media.len(), before.len() - 1);

    let outcome = h.undo(&mut p).unwrap();
    assert_eq!(outcome.desc, desc, "undo restores the same description");
    assert_eq!(outcome.playhead, 42, "and the playhead it was made from");
    assert_eq!(p.media, before, "pool restored exactly (order and ids)");

    h.redo(&mut p).unwrap();
    assert!(p.asset(spare).is_none(), "redo removes it again");
}

// If the pool changed outside the stack, an asset undo refuses and leaves
// the project untouched rather than clobbering the foreign change.
#[test]
fn asset_history_refuses_when_pool_changed_outside() {
    let mut p = fixture();
    let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
    let mut h = History::default();
    h.remove_asset(&mut p, spare, 0).unwrap();
    let _ = p.add_asset("/media/extra.mp4".into(), probe_report("5"));
    let live = p.media.clone();

    assert!(matches!(h.undo(&mut p), Err(EditError::History(_))));
    assert_eq!(p.media, live, "failed undo left the pool untouched");
}

// The pool change serializes into the sidecar and undoes after reload.
#[test]
fn asset_removal_sidecar_round_trips() {
    let mut p = fixture();
    let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
    let mut h = History::default();
    h.remove_asset(&mut p, spare, 0).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.history.json");
    h.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"kind\": \"assets\""), "{text}");

    let mut h2 = History::load(&path).unwrap();
    let mut p2 = p.clone();
    h2.undo(&mut p2).unwrap();
    assert!(p2.asset(spare).is_some(), "asset restored after reload");
}

#[test]
fn history_atom_is_lossless_for_speed_clips() {
    // A rate conform's source range is off the project grid. The old
    // frame-count Atom quantized it on every undo, perturbing the clip
    // until a zero-frame sliver failed validation. The Atom now carries
    // exact source times, so it round-trips bit-for-bit.
    let mut p = fixture(); // fps 30, clip #1 first
    let c = Clip {
        id: 1,
        asset: 1,
        asset_in: Seconds::new(101 * 1001, 30000),
        asset_out: Seconds::new(921 * 1001, 30000),
        speed: Seconds::new(1001, 1250),
        effects: Vec::new(),
    };
    p.timeline.video_track_mut().unwrap().segments[0] = Segment::Clip(c.clone());
    let mut h = History::default();
    let batch = trim_out(&p, 1.0).unwrap(); // edit the same track
    h.apply(&mut p, batch, 0).unwrap();
    h.undo(&mut p).unwrap();
    let Segment::Clip(restored) = &p.timeline.video_track().unwrap().segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(*restored, c, "the conform clip round-trips exactly");
}

#[test]
fn history_depth_is_capped_and_redo_clears() {
    let mut p = fixture();
    let mut h = History::default();
    let snapshot = clip_by_id(&p, 1);
    for _ in 0..(MAX_HISTORY + 5) {
        let end = p.timeline.end();
        let end_f = *end.numer() as f64 / *end.denom() as f64;
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), end_f + 10.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
    }
    assert_eq!(h.depth(), MAX_HISTORY);
    // cap survives a save/load round trip
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.history.json");
    h.save(&path).unwrap();
    assert_eq!(History::load(&path).unwrap().depth(), MAX_HISTORY);

    // a new edit clears the redo stack
    h.undo(&mut p).unwrap();
    assert!(h.can_redo());
    let end = p.timeline.end();
    let end_f = *end.numer() as f64 / *end.denom() as f64;
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), end_f + 10.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert!(!h.can_redo());
}

#[test]
fn corrupt_and_foreign_sidecars_start_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    assert!(History::load(&bad).is_err());

    // v2 stored raw op shapes: rejected, never misread
    let old = dir.path().join("old.json");
    std::fs::write(&old, "{\"version\": 2, \"done\": [], \"undone\": []}").unwrap();
    assert!(History::load(&old).is_err());
    assert!(
        History::restore(HistoryState {
            version: 2,
            done: vec![],
            undone: vec![],
        })
        .depth()
            == 0
    );

    // A corrupt rational (zero denominator) is rejected at parse time —
    // as raw speed_num/speed_den it reached `Ratio::new` and panicked on
    // the first undo instead.
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let good = dir.path().join("good.json");
    h.save(&good).unwrap();
    let text = std::fs::read_to_string(&good).unwrap();
    let bad = dir.path().join("denom.json");
    std::fs::write(
        &bad,
        text.replace("\"speed\": \"1/1\"", "\"speed\": \"5/0\""),
    )
    .unwrap();
    assert!(History::load(&bad).is_err());
}

#[test]
fn failed_apply_restores_project_and_keeps_history_empty() {
    let mut p = fixture();
    let mut h = History::default();
    let before = p.clone();
    // A gap past the timeline limit passes op application but fails validate
    // (clamping does not touch gaps, so it cannot rescue this one).
    let bad = Segment::Gap(Gap {
        duration: Seconds::from_integer(crate::timeline::MAX_TIMELINE_SECONDS + 1),
    });
    let batch = Batch {
        ops: vec![EditOp::Replace {
            track: 0,
            index: 0,
            before: p.timeline.video_track().unwrap().segments[0].clone(),
            after: bad,
        }],
        desc: "bad edit".into(),
    };
    assert!(h.apply(&mut p, batch, 0).is_err());
    assert_eq!(p, before, "project untouched");
    assert_eq!(h.depth(), 0, "nothing pushed");
}

#[test]
fn failed_undo_keeps_its_history_entry() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let expected = p.clone();
    // External mutation: undo must refuse and keep its entry.
    p.timeline.video_track_mut().unwrap().segments.pop();
    assert!(h.undo(&mut p).is_err());
    assert_eq!(h.depth(), 1, "entry preserved for retry");
    assert!(!h.can_redo());
    // Restoring the expected topology makes undo work again.
    p = expected;
    assert!(h.undo(&mut p).is_ok());
    assert_eq!(h.depth(), 0);
}
