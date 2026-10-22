//! The expected-behaviour battery: normal workflows, checked against the
//! *result* — exported pixels, durations, frame counts, structure. These are
//! the old `scripts/expect.sh` sections.
//!
//! Each test builds its own sandbox (see `common`), so they run in parallel.
//! On failure the assertion names the expectation in its message.

mod common;

use std::fs;

use common::*;

#[test]
fn baseline_export() {
    let s = Sandbox::new();
    s.base();
    s.export("b.mp4");
    s.assert_px("b.mp4", 1.0, RED, "red 0–2");
    s.assert_px("b.mp4", 3.0, GREEN, "green 2–4");
    s.assert_px("b.mp4", 5.0, BLUE, "blue 4–6");
    s.assert_dur("b.mp4", 6.0, "6 s");
}

#[test]
fn split_and_ripple_delete() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["split", "--at", "1"]);
    s.ok(&["delete", "--clip", "1", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "the red half survives");
    s.assert_px("b.mp4", 2.0, GREEN, "green slid left");
    s.assert_dur("b.mp4", 5.0, "5 s");
}

#[test]
fn trim_in_ripples_trim_out_leaves_a_gap() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["trim-in", "--at", "1", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "red head pulled in");
    s.assert_dur("b.mp4", 5.0, "5 s");

    s.base();
    s.ok(&["trim-out", "--at", "1"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, RED, "red 0–1");
    s.assert_px("b.mp4", 1.5, BLACK, "the trimmed material is a gap");
    s.assert_dur("b.mp4", 6.0, "length preserved");
}

#[test]
fn gaps_insert_merge_delete() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.5, BLACK, "the inserted gap");
    s.assert_px("b.mp4", 4.0, GREEN, "green pushed to 3–5");
    s.assert_dur("b.mp4", 7.0, "7 s");

    // Two adjacent gaps read as one row.
    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.ok(&["gap", "--at", "3", "--frames", "30"]);
    let gaps = s.show().matches("gap").count();
    assert_eq!(gaps, 1, "adjacent gaps coalesced ({gaps} gap rows)");

    s.base();
    s.ok(&["gap", "--at", "2", "--frames", "30"]);
    s.ok(&["delete", "--at", "2.5"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.5, GREEN, "deleting the gap pulled green back");
    s.assert_dur("b.mp4", 6.0, "6 s");
}

#[test]
fn delete_keeps_the_length_the_tail_is_black() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["delete", "--clip", "2"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 3.0, BLACK, "green's span is a gap");
    s.assert_dur("b.mp4", 6.0, "length preserved");
}

#[test]
fn paste_over_vs_ripple() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["yank", "--clip", "1"]);
    s.ok(&["paste", "--at", "1"]);
    let show = s.show();
    assert!(
        show.contains("1 – 3"),
        "the paste covers exactly 1–3:\n{show}"
    );
    assert!(
        show.contains("(src 1–2)"),
        "the covered green was trimmed to its tail:\n{show}"
    );
    s.export("b.mp4");
    s.assert_px("b.mp4", 2.0, RED, "the pasted red");
    s.assert_dur("b.mp4", 6.0, "length unchanged");

    s.base();
    s.ok(&["yank", "--clip", "1"]);
    s.ok(&["paste", "--at", "0", "--ripple"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 1.0, RED, "the ripple paste sits first");
    s.assert_px("b.mp4", 3.0, RED, "the original red follows");
    s.assert_dur("b.mp4", 8.0, "8 s");
}

#[test]
fn speed_source_timeline_math() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["speed", "1", "2"]);
    s.ok(&["trim-in", "--at", "1"]);
    let show = s.show();
    assert!(
        show.contains("(src 2–4)"),
        "1 timeline s at 2× consumes 2 source s:\n{show}"
    );
    s.ok(&["extend", "--clip", "1", "--edge", "out", "--to", "2.5"]);
    let show = s.show();
    assert!(
        show.contains("(src 2–5)"),
        "0.5 timeline s at 2× adds 1 source s:\n{show}"
    );
    s.export("s.mp4");
    s.assert_dur("s.mp4", 2.5, "2.5 s");
    let report = s.export_report("s2.mp4");
    assert!(report.contains("75 frames"), "30 fps × 2.5 s:\n{report}");
}

#[test]
fn audio_retiming_keeps_the_audio() {
    let s = Sandbox::new();
    for (rate, want) in [("2", 1.0), ("0.5", 4.0), ("0.4", 5.0), ("3", 0.666667)] {
        s.ok(&["init", "--force"]);
        s.ok(&["import", "red.mp4"]);
        s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
        s.ok(&["speed", "1", rate]);
        s.export("r.mp4");
        s.assert_dur("r.mp4", want, &format!("a 2 s clip at {rate}×"));
        assert!(s.has_audio("r.mp4"), "{rate}× keeps its audio");
    }
}

#[test]
fn sixty_fps_source_follows_the_timeline_rate() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "blue.mp4"]);
    s.ok(&["add", "blue.mp4", "--in", "0", "--out", "2"]);
    let report = s.export_report("f.mp4");
    assert!(
        report.contains("120 frames @ 60000/1001"),
        "2 s of 59.94 = 120 frames:\n{report}"
    );
}

#[test]
fn multi_track_overlay_mute_base_promotion() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.export("m.mp4");
    s.assert_px("m.mp4", 2.0, GREEN, "the overlay is on top");
    s.assert_px(
        "m.mp4",
        0.5,
        RED,
        "the base shows through the overlay's gap",
    );

    s.ok(&["track", "mute", "2"]);
    s.export("m2.mp4");
    s.assert_px("m2.mp4", 2.0, RED, "a muted overlay reveals the base");

    s.ok(&["track", "mute", "2"]); // unmute the overlay again
    s.ok(&["track", "mute", "1"]);
    s.export("m3.mp4");
    s.assert_px("m3.mp4", 2.0, GREEN, "muting the base promotes the overlay");
    s.assert_px(
        "m3.mp4",
        0.5,
        BLACK,
        "…and it is black where the overlay is a gap",
    );
}

#[test]
fn a_crop_masks_in_place_and_reveals_what_is_below() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["effect", "crop", "--clip", "1", "--left", "50"]);
    s.export("solo.mp4");
    // 320×240: the left half is masked, the kept half does not zoom.
    s.assert_px_at(
        "solo.mp4",
        0.5,
        20,
        120,
        BLACK,
        "a crop with nothing below is black",
    );
    s.assert_px_at(
        "solo.mp4",
        0.5,
        300,
        120,
        RED,
        "the kept half stays where it was",
    );

    // A scale-down is transparent too: black on its own.
    s.ok(&["effect", "clear", "--clip", "1"]);
    s.ok(&["effect", "scale", "--clip", "1", "--w", "50", "--h", "50"]);
    s.export("small.mp4");
    s.assert_px_at(
        "small.mp4",
        0.5,
        20,
        120,
        BLACK,
        "a scale-down with nothing below is black",
    );
    s.assert_px_at(
        "small.mp4",
        0.5,
        160,
        120,
        RED,
        "the shrunk picture is centred",
    );

    // The same effects on an overlay: the masked area shows the track below.
    s.ok(&["effect", "clear", "--clip", "1"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["effect", "crop", "--clip", "2", "--left", "50"]);
    s.export("over.mp4");
    s.assert_px_at(
        "over.mp4",
        0.5,
        20,
        120,
        RED,
        "the cropped overlay reveals the base",
    );
    s.assert_px_at(
        "over.mp4",
        0.5,
        300,
        120,
        GREEN,
        "the overlay keeps its right half",
    );

    s.ok(&["effect", "clear", "--clip", "2"]);
    s.ok(&["effect", "scale", "--clip", "2", "--w", "50", "--h", "50"]);
    s.export("scaled.mp4");
    s.assert_px_at(
        "scaled.mp4",
        0.5,
        20,
        120,
        RED,
        "the scale pad reveals the base",
    );
    s.assert_px_at(
        "scaled.mp4",
        0.5,
        160,
        120,
        GREEN,
        "the shrunk overlay is centred",
    );

    s.ok(&["effect", "clear", "--clip", "2"]);
    s.ok(&["effect", "offset", "--clip", "2", "--x", "50"]);
    s.export("offset.mp4");
    s.assert_px_at(
        "offset.mp4",
        0.5,
        20,
        120,
        RED,
        "the offset vacates the base",
    );
    s.assert_px_at(
        "offset.mp4",
        0.5,
        300,
        120,
        GREEN,
        "the moved overlay is still visible",
    );

    // ±100% of the canvas is fully off screen: the base shows everywhere.
    for x in ["100", "-100"] {
        s.ok(&["effect", "offset", "--clip", "2", "--x", x]);
        let out = format!("off{}.mp4", x.replace('-', "l"));
        s.export(&out);
        s.assert_px_at(
            &out,
            0.5,
            20,
            120,
            RED,
            &format!("offset {x}% leaves the base at the left"),
        );
        s.assert_px_at(
            &out,
            0.5,
            300,
            120,
            RED,
            &format!("offset {x}% leaves the base at the right"),
        );
    }
}

#[test]
fn a_muted_sole_video_track_has_nothing_to_export() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "mute", "1"]);
    let out = s.err(&["export", "-o", "mm.mp4"]);
    assert!(
        out.contains("every video track is muted"),
        "the export refuses instead of ignoring the mute:\n{out}"
    );
}

#[test]
fn an_empty_upper_video_track_is_transparent() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "video"]);
    s.export("e.mp4");
    s.assert_px("e.mp4", 1.0, RED, "the base still shows");
}

#[test]
fn audio_track_mixes_in() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "audio"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "1",
    ]);
    s.export("a.mp4");
    s.assert_dur("a.mp4", 2.0, "2 s");
    assert!(s.has_audio("a.mp4"), "audio stream present");
}

#[test]
fn undo_redo_round_trip() {
    let s = Sandbox::new();
    s.base();
    let before = fs::read(s.file(P)).expect("read the project");
    s.ok(&["split", "--at", "1"]);
    s.ok(&["trim-out", "--at", "5"]);
    s.ok(&["gap", "--at", "3", "--frames", "15"]);
    s.ok(&["delete", "--clip", "3", "--ripple"]);
    s.ok(&["speed", "1", "2"]);
    let after = fs::read(s.file(P)).expect("read the project");
    for _ in 0..5 {
        s.ok(&["undo"]);
    }
    assert_eq!(
        fs::read(s.file(P)).expect("read the project"),
        before,
        "5 undos restore the start exactly"
    );
    for _ in 0..5 {
        s.ok(&["redo"]);
    }
    assert_eq!(
        fs::read(s.file(P)).expect("read the project"),
        after,
        "5 redos restore the end exactly"
    );
}

#[test]
fn undoable_track_rm_media_rm_imports() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add", "blue.mp4", "--track", "2", "--at", "0", "--in", "0", "--out", "1",
    ]);
    s.ok(&["track", "rm", "2"]);
    s.ok(&["undo"]);
    s.export("b.mp4");
    s.assert_px("b.mp4", 0.5, BLUE, "the removed track's clip came back");

    // A pool with an unused asset: red+green placed, blue only pooled.
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4", "blue.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["add", "green.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["media", "rm", "3"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(!assets.contains("#3"), "blue left the pool:\n{assets}");
    s.ok(&["undo"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(assets.contains("#3"), "undo brought it back:\n{assets}");
    s.ok(&["import", "red.mp4"]);
    let assets = s.ok(&["media", "ls"]);
    assert!(
        assets.contains("3 asset(s)"),
        "re-importing dedupes:\n{assets}"
    );
}

#[test]
fn head_material_reaches_back_past_time_zero() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "1", "--out", "3"]);
    s.ok(&["extend", "--clip", "1", "--edge", "in", "--to", "-1"]);
    s.export("h.mp4");
    s.assert_px("h.mp4", 0.5, RED, "the prepended source plays from 0");
    s.assert_dur("h.mp4", 3.0, "3 s");
}

const EDL: &str = "\
TITLE: e
FCM: NON-DROP FRAME

001  AX  V  C  00:00:00:00 00:00:01:00 01:00:00:00 01:00:01:00
* FROM CLIP NAME: red.mp4
002  AX  V  C  00:00:00:00 00:00:01:00 01:00:02:00 01:00:03:00
* FROM CLIP NAME: blue.mp4
";

#[test]
fn edl_import_round_trip() {
    let s = Sandbox::new();
    s.write("e.edl", EDL);
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    s.export("edl.mp4");
    s.assert_px("edl.mp4", 0.5, RED, "EDL event 1");
    s.assert_px("edl.mp4", 1.5, BLACK, "the record gap");
    s.assert_px("edl.mp4", 2.5, BLUE, "EDL event 2");
}

/// An EDL whose source out-point runs past the file is clamped to the media
/// (as OTIO and detect-edits are), so the model and the export agree.
#[test]
fn edl_import_clamps_to_the_media() {
    let s = Sandbox::new();
    // red.mp4 is 5 s; this event claims a 9 s source span.
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:09:00 01:00:00:00 01:00:09:00\n\
         * FROM CLIP NAME: red.mp4\n",
    );
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    let show = s.show();
    assert!(
        show.contains("5 s"),
        "the model is clamped to the file: {show}"
    );
    s.export("edl-clamp.mp4");
    s.assert_dur("edl-clamp.mp4", 5.0, "the export matches the clamped model");
}

/// A conform EDL with a dissolve and a wipe imports as hard cuts (each
/// reported), rather than sinking the whole list.
#[test]
fn edl_import_degrades_transitions_to_cuts() {
    let s = Sandbox::new();
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C       00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n\
         * FROM CLIP NAME: red.mp4\n\
         002  AX  V  D  030  00:00:00:00 00:00:02:00 01:00:02:00 01:00:04:00\n\
         * FROM CLIP NAME: blue.mp4\n\
         003  AX  V  W001 030  00:00:00:00 00:00:02:00 01:00:04:00 01:00:06:00\n\
         * FROM CLIP NAME: green.mp4\n",
    );
    let out = s.run(&["import-edl", "e.edl", "--media", ".", "--force"]);
    let text = merged(&out);
    assert!(out.status.success(), "the import should succeed:\n{text}");
    assert!(
        text.contains("imported as a cut"),
        "the non-cut events are reported:\n{text}"
    );
    // The record span is kept whole: three adjacent 2 s cuts.
    let show = s.show();
    assert!(show.contains("6 s"), "the timeline is 6 s: {show}");
    s.export("edl-degrade.mp4");
    s.assert_dur("edl-degrade.mp4", 6.0, "three 2 s cuts");
    s.assert_px("edl-degrade.mp4", 1.0, RED, "event 1");
    s.assert_px("edl-degrade.mp4", 3.0, BLUE, "the dissolve, now a cut");
    s.assert_px("edl-degrade.mp4", 5.0, GREEN, "the wipe, now a cut");
}

/// A black event (`BL`) is empty space, not a clip — the fuzzy reel matcher
/// used to latch it onto `blue.mp4`.
#[test]
fn edl_black_reel_becomes_a_gap() {
    let s = Sandbox::new();
    s.write(
        "e.edl",
        "TITLE: t\nFCM: NON-DROP FRAME\n\n\
         001  AX  V  C  00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n\
         * FROM CLIP NAME: red.mp4\n\
         002  BL  V  C  00:00:00:00 00:00:01:00 01:00:02:00 01:00:03:00\n",
    );
    s.ok(&["import-edl", "e.edl", "--media", ".", "--force"]);
    s.export("edl-black.mp4");
    s.assert_dur("edl-black.mp4", 3.0, "red + a 1 s black span");
    s.assert_px("edl-black.mp4", 1.0, RED, "event 1");
    s.assert_px(
        "edl-black.mp4",
        2.5,
        BLACK,
        "the BL event is black, not blue",
    );
}

const OTIO: &str = r#"{
  "OTIO_SCHEMA": "Timeline.1",
  "tracks": {
    "OTIO_SCHEMA": "Stack.1",
    "children": [
      {
        "OTIO_SCHEMA": "Track.1", "kind": "Video",
        "children": [
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "red.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } },
          { "OTIO_SCHEMA": "Gap.1",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 30, "rate": 30 } } },
          { "OTIO_SCHEMA": "Clip.1",
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "green.mp4" },
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": 0, "rate": 30 },
              "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 60, "rate": 30 } } }
        ]
      }
    ]
  }
}"#;

#[test]
fn otio_import_round_trip() {
    let s = Sandbox::new();
    s.write("e.otio", OTIO);
    s.ok(&["import-otio", "e.otio", "--force"]);
    s.export("otio.mp4");
    s.assert_px("otio.mp4", 1.0, RED, "OTIO clip 1");
    s.assert_px("otio.mp4", 2.5, BLACK, "the OTIO gap");
    s.assert_px("otio.mp4", 4.0, GREEN, "OTIO clip 2");
    s.assert_dur("otio.mp4", 5.0, "5 s");
}

#[test]
fn transitions() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.export("t.mp4");
    // Each neighbour gives up 15 f (0.5 s) and the transition takes its own
    // 0.5 s.
    s.assert_dur(
        "t.mp4",
        5.5,
        "a clip|clip transition shortens the timeline by its length",
    );
    s.assert_px("t.mp4", 1.0, RED, "red before the seam");
    s.assert_px("t.mp4", 5.0, BLUE, "blue still follows");
    // The old cut at 2.0 is now a blend: neither side is pure there.
    s.assert_px_ne("t.mp4", 1.75, RED, "the seam is no longer a cut");
    s.assert_px_ne("t.mp4", 1.75, GREEN, "…and not the incoming side either");
    // `--dur` and the fade kind are the same code path.
    s.base();
    s.ok(&["transition", "fade", "--at", "2", "--dur", "0.5"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 5.5, "--dur and the fade kind agree");
}

#[test]
fn removing_a_transition_gives_the_frames_back() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["delete", "--at", "1.7"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 6.0, "the timeline is whole again");
    s.assert_px("t.mp4", 1.0, RED, "red's tail is restored");
    s.assert_px("t.mp4", 3.0, GREEN, "green's head is restored");
    s.assert_px("t.mp4", 5.0, BLUE, "blue is untouched");

    // The same by segment index (`nle show` prints [1] for the transition).
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["delete", "--seg", "1"]);
    s.export("t.mp4");
    s.assert_dur("t.mp4", 6.0, "delete --seg restores the neighbours too");
    s.assert_px("t.mp4", 3.0, GREEN, "…green's head is back");
}

#[test]
fn effects_eq_neutral_values() {
    let s = Sandbox::new();
    // No eq parameter given: contrast/saturation must stay neutral (this used
    // to come back flat and grey).
    s.base();
    s.ok(&["effect", "eq", "--clip", "1"]);
    s.export("fx.mp4");
    s.assert_px("fx.mp4", 1.0, RED, "eq with no arguments is a no-op");

    // Asking only for brightness must not desaturate either (the defaults for
    // the other two used to be 0 = flat grey).
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--brightness", "-0.2"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.0, Look::Hue, "brightness alone keeps the hue");

    // …and saturation 0 is the deliberate desaturation.
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.0, Look::Grey, "saturation 0 desaturates");

    // Effects and a retime compose: 2 s at 2x is 1 s, still grey.
    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["speed", "1", "2"]);
    s.export("fx.mp4");
    s.assert_dur("fx.mp4", 5.0, "an effect clip retimes (1 s + 2 s + 2 s)");
    s.assert_look("fx.mp4", 0.5, Look::Grey, "…and the effect still applies");

    // An effect clip can't be expressed by `edl://`: the preview plays its
    // proxy. The first call renders it — and must render it *quietly*, since
    // the monitor runs the same code with the TUI on stderr.
    let first = s.run(&["edl", "--track", "1"]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        !stderr.contains("ffmpeg"),
        "the proxy render is quiet:\n{stderr}"
    );
    let edl = s.edl();
    assert!(
        !edl.contains("red.mp4"),
        "an effect clip plays its proxy:\n{edl}"
    );
    assert!(edl.contains("/proxy/"), "…from the proxy cache:\n{edl}");
}

#[test]
fn an_effect_survives_the_edits_that_rebuild_a_clip() {
    // trim/split/gap/paste rebuilt the clip with `effects: Vec::new()`, so the
    // next edit silently dropped it. Each case exports and checks the pixel.
    let s = Sandbox::new();

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["trim-out", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 0.5, Look::Grey, "the effect survives trim-out");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["split", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        0.5,
        Look::Grey,
        "the effect survives split (head)",
    );
    s.assert_look("fx.mp4", 1.5, Look::Grey, "…and the tail");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["trim-in", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look("fx.mp4", 1.5, Look::Grey, "the effect survives trim-in");

    s.base();
    s.ok(&["effect", "eq", "--clip", "1", "--saturation", "0"]);
    s.ok(&["gap", "--at", "1", "--frames", "15"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        0.5,
        Look::Grey,
        "the effect survives insert-gap (head)",
    );
    s.assert_look("fx.mp4", 2.0, Look::Grey, "…and the tail");

    // The paste sweep left the remainder past it without the effect.
    s.base();
    s.ok(&["effect", "eq", "--clip", "2", "--saturation", "0"]);
    s.ok(&["yank", "--clip", "2"]);
    s.ok(&["paste", "--at", "1"]);
    s.export("fx.mp4");
    s.assert_look(
        "fx.mp4",
        3.5,
        Look::Grey,
        "the remainder past a paste keeps its effect",
    );
}

#[test]
fn a_retimed_clip_previews_from_a_proxy() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["speed", "1", "2"]);
    let edl = s.edl();
    assert!(
        !edl.contains("red.mp4"),
        "a 2x clip plays its proxy (an EDL entry can't retime):\n{edl}"
    );
    assert!(edl.contains("/proxy/"), "…from the proxy cache:\n{edl}");
    assert!(
        edl.contains("green.mp4"),
        "an untouched clip still plays its source:\n{edl}"
    );
}

#[test]
fn retiming_a_neighbour_retimes_the_transition_side() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["transition", "--at", "2", "--frames", "15"]);
    s.ok(&["speed", "1", "2"]);
    // red 0..2 at 2x is 1 s, and the dissolve owns the last 0.5 s of it: the
    // clip keeps 0.5 s and the transition's side plays source 1..2, also at
    // 2x. Before, the clip kept 0.75 s and the transition played its span at
    // 1x — a speed step at the seam.
    let show = s.show();
    assert!(
        show.contains("4.5 s"),
        "the retimed contribution (1 s) plus green + blue:\n{show}"
    );
    s.export("ts.mp4");
    // 15 (red) + 15 (transition) + 45 (green) + 60 (blue).
    s.assert_frames("ts.mp4", 135, "the render is the model's 135 frames");
}

#[test]
fn a_rate_that_misses_the_frame_grid_snaps_the_timeline() {
    let s = Sandbox::new();
    s.base();
    s.ok(&["speed", "1", "0.7"]);
    // 2 s / 0.7 = 2.857 s = 85.714 frames. The clip takes 86 frames and the
    // rate becomes 30/43, so every boundary stays on the grid and the source
    // span — the material the user picked — is untouched.
    let show = s.show();
    assert!(
        show.contains("2.866667"),
        "the clip lands on frame 86:\n{show}"
    );
    assert!(
        show.contains("@0.697674"),
        "…at the rate that fits:\n{show}"
    );
    s.export("fr.mp4");
    // The model's frames: 86 (0.7x) + 60 + 60. A source span nudged to fit 86
    // frames would read one extra source frame and render 207.
    s.assert_frames(
        "fr.mp4",
        206,
        "the render is the model's 206 frames (86+60+60)",
    );
}

#[test]
fn an_off_sequence_clip_previews_at_the_sequence_size() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "wide169.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]); // 4:3 sets the sequence
    s.ok(&["add", "wide169.mp4", "--at", "2", "--in", "0", "--out", "2"]);
    s.ok(&["transition", "--at", "2", "--frames", "30"]);
    let edl = s.edl();
    // mpv re-frames a raw file to its own aspect (bars) but plays the
    // transition's proxy already scaled to the sequence, so the wide clip must
    // be a proxy too — otherwise the framing jumps as the monitor leaves the
    // dissolve.
    assert!(
        !edl.contains("wide169.mp4"),
        "the 16:9 clip is proxied, not played raw:\n{edl}"
    );
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");
}

#[test]
fn a_multi_track_preview_renders_the_composite() {
    // Two video layers, both with sound: `edl://` can carry only one source's
    // audio per span, so the monitor plays the rendered composite (the export's
    // own overlay + amix) where they overlap.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    let edl = s.edl();
    assert!(
        edl.contains("red.mp4"),
        "the uncovered base plays straight from source:\n{edl}"
    );
    assert!(
        edl.contains("/proxy/"),
        "the overlap is a rendered composite:\n{edl}"
    );
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");

    // The composite is the export's: the opaque overlay wins, and the mix
    // carries both tracks' audio.
    let proxies: Vec<_> = edl_paths(&edl)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/proxy/"))
        .collect();
    assert_eq!(proxies.len(), 1, "one span, one render: {proxies:?}");
    let proxy = proxies[0].display().to_string();
    s.assert_px(&proxy, 1.0, GREEN, "the overlay is on top");
    assert!(s.has_audio(&proxy), "the composite carries the mix");
    s.assert_dur(&proxy, 2.0, "the span is [1,3)");
}

#[test]
fn a_cropped_overlay_previews_the_composite() {
    // A crop is a mask, so the monitor must render the overlay to reveal the
    // layer below — the clip's own proxy would show black margins instead.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["effect", "crop", "--clip", "2", "--left", "50"]);
    let edl = s.edl();
    assert!(
        edl.contains("red.mp4"),
        "the uncovered base still plays from source:\n{edl}"
    );
    let proxies: Vec<_> = edl_paths(&edl)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/proxy/"))
        .collect();
    assert_eq!(proxies.len(), 1, "one composite span: {proxies:?}");
    let proxy = proxies[0].display().to_string();
    s.assert_px_at(&proxy, 0.5, 20, 120, RED, "the crop reveals the base");
    s.assert_px_at(
        &proxy,
        0.5,
        300,
        120,
        GREEN,
        "the overlay keeps its right half",
    );
}

#[test]
fn an_audio_track_is_mixed_into_the_preview() {
    // The monitor used to play the video layer's audio and ignore a separate
    // audio track entirely; its span is now rendered and mixed.
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "2"]);
    s.ok(&["track", "add", "audio"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--in",
        "0",
        "--out",
        "1",
    ]);
    let edl = s.edl();
    let entries = edl_paths(&edl);
    assert_eq!(entries.len(), 2, "one span + the plain tail:\n{edl}");
    assert!(
        entries[0].to_string_lossy().contains("/proxy/"),
        "the audio track's span is rendered:\n{edl}"
    );
    assert!(
        entries[1].ends_with("red.mp4"),
        "the rest still plays from source:\n{edl}"
    );
    let mixed = entries[0].display().to_string();
    assert!(s.has_audio(&mixed), "the mix carries the audio track");
    s.assert_dur(&mixed, 1.0, "the span is [0,1)");
    s.assert_edl_geometry(&edl, 320, 240, "every entry is the sequence's 320x240");
}

#[test]
fn a_muted_layer_needs_no_composite() {
    let s = Sandbox::new();
    s.ok(&["init", "--force"]);
    s.ok(&["import", "red.mp4", "green.mp4"]);
    s.ok(&["add", "red.mp4", "--in", "0", "--out", "4"]);
    s.ok(&["track", "add", "video"]);
    s.ok(&[
        "add",
        "green.mp4",
        "--track",
        "2",
        "--at",
        "1",
        "--in",
        "0",
        "--out",
        "2",
    ]);
    s.ok(&["track", "mute", "2"]);
    let edl = s.edl();
    assert!(
        !edl.contains("/proxy/"),
        "a muted layer contributes neither picture nor sound:\n{edl}"
    );
    assert!(edl.contains("red.mp4"), "the base still shows:\n{edl}");
}

#[test]
fn cli_only_guards() {
    let s = Sandbox::new();
    let out = s.err(&["trim", "red.mp4", "--in", "0", "--out", "99", "-o", "t.mp4"]);
    assert!(
        out.contains("beyond the file's duration"),
        "trim refuses an --out past the end:\n{out}"
    );

    s.base();
    let out = s.err(&["effect", "crop", "--clip", "1", "--left", "101"]);
    assert!(
        out.contains("0–100"),
        "an out-of-range crop inset is refused:\n{out}"
    );
    let out = s.err(&["effect", "lut", "--clip", "1", "--path", ""]);
    assert!(
        out.contains("needs --path"),
        "an empty LUT path is refused:\n{out}"
    );
    let out = s.err(&["export", "-o", "."]);
    assert!(
        out.contains("Error opening output"),
        "a failed export names the reason:\n{out}"
    );
    let first = s.ok(&["media", "ls"]);
    assert!(
        first.lines().next().unwrap_or("").contains("asset(s)"),
        "piped output survives an early-closed consumer:\n{first}"
    );

    // Effects are video filters: on an audio track they would be stored but
    // never reach the graph, so the action refuses instead. `--clip` finds the
    // clip's own track (as delete/yank/speed do), so the guard is reached by
    // putting the clip on the audio track.
    s.ok(&["track", "add", "audio"]);
    s.ok(&["add", "red.mp4", "--track", "2", "--in", "0", "--out", "1"]);
    let out = s.err(&["effect", "eq", "--clip", "4"]);
    assert!(
        out.contains("video clips"),
        "an effect on an audio track is refused:\n{out}"
    );
}

// `nle cache prune` is the only thing that deletes proxy renders: age-based
// on the file's mtime, and never implicit.
#[test]
fn cache_prune_is_explicit_and_age_based() {
    use std::time::{Duration, SystemTime};

    let s = Sandbox::new();
    s.base();
    let dir = s.cache_dir().join("nle").join("proxy");
    fs::create_dir_all(&dir).unwrap();
    let now = SystemTime::now();
    let write = |name: &str, len: usize, age: Duration| {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; len]).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now - age))
            .unwrap();
    };
    write("aaaa.mp4", 100, Duration::from_secs(31 * 24 * 3600));
    write("bbbb.mp4", 200, Duration::from_secs(3600));

    // An ordinary command sweeps nothing: pruning is explicit.
    s.ok(&["show"]);
    assert!(dir.join("aaaa.mp4").exists(), "no implicit sweep");

    let out = s.ok(&["cache", "prune", "--dry-run"]);
    assert!(out.contains("would remove 1"), "{out}");
    assert!(dir.join("aaaa.mp4").exists(), "a dry run deletes nothing");

    let out = s.ok(&["cache", "prune"]);
    assert!(out.contains("removed 1"), "{out}");
    assert!(!dir.join("aaaa.mp4").exists(), "the old entry is gone");
    assert!(dir.join("bbbb.mp4").exists(), "the young entry is kept");

    let out = s.err(&["cache", "prune", "--days", "0"]);
    assert!(out.contains("at least 1"), "0 days is refused:\n{out}");
}
