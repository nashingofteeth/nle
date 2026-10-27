use super::super::motion::{
    cut_points, fmt_timecode, marker_col, next_clip_start, next_cut, next_gap_start, on_boundary,
    prev_clip_start, prev_cut, prev_gap_start, snap_to_cut, truncate_label,
};
use super::super::panels::timeline::render_timeline;
use super::super::*;
use nle_core::time::Seconds;

use super::super::keys::handle_key;
use super::*;
use ratatui::layout::Rect;
use ratatui::widgets::Block;

// Clip-edge dividers on the track row land on the same columns as the
// cut positions the marker row/playhead use (one quantizer).
#[test]
fn clip_edges_align_with_cut_positions() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    // Four equal clips on the shared asset → cuts at 3,6,9,12.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = (1..=4)
        .map(|id| {
            TimelineSegment::Clip(Clip {
                id,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(3),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            })
        })
        .collect();
    let app = App::new(p, PathBuf::from("t.json"));
    let width = 80u16;
    let mut term = Terminal::new(TestBackend::new(width, 4)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width,
                height: 4,
            },
            &app,
        )
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let inner = Block::bordered().inner(Rect {
        x: 0,
        y: 0,
        width,
        height: 4,
    });

    // Every drawn divider is a `▏` glyph at a clip boundary; collect
    // its column (buffer x, offset by the border) and compare to the
    // quantized cut.
    let divider_cols: Vec<u16> = (0..width)
        .filter(|&x| buf[(x, 1)].symbol() == "▏")
        .collect();
    for cut in [3.0, 6.0, 9.0] {
        let want = inner.x + marker_col(cut, 12.0, inner.width);
        assert!(
            divider_cols.contains(&want),
            "divider at cut {cut} → col {want}, got {divider_cols:?}"
        );
    }
}

#[test]
fn timecode_formatting() {
    let fps30 = Seconds::new(30, 1);
    let fps60 = Seconds::new(60, 1);
    assert_eq!(fmt_timecode(0.0, Some(&fps30)), "00:00:00");
    assert_eq!(fmt_timecode(8.2, Some(&fps60)), "00:08:12");
    assert_eq!(fmt_timecode(62.25, Some(&fps30)), "01:02:08");
    assert_eq!(fmt_timecode(3661.0, Some(&fps30)), "1:01:01:00");
    assert_eq!(fmt_timecode(-5.0, Some(&fps30)), "00:00:00");
    assert_eq!(fmt_timecode(62.25, None), "01:02.2");
    assert_eq!(fmt_timecode(0.0, None), "00:00.0");
}

#[test]
fn cut_points_boundaries() {
    let pts = cut_points(&project(), 0);
    assert_eq!(pts, vec![0.0, 3.0, 5.0, 7.0]);
    let eps = 1e-6;
    assert_eq!(next_cut(&pts, 0.5, eps), Some(3.0));
    assert_eq!(next_cut(&pts, 3.0, eps), Some(5.0));
    assert_eq!(next_cut(&pts, 7.0, eps), None);
    assert_eq!(prev_cut(&pts, 4.0, eps), Some(3.0));
    assert_eq!(prev_cut(&pts, 0.0, eps), None);
    assert_eq!(next_cut(&pts, 2.9999999, 0.0167), Some(5.0));
}

#[test]
fn motions_tolerate_playhead_drift() {
    let pts = vec![0.0, 3.0, 5.0, 7.0];
    let eps = 0.5 / 30.0;
    assert_eq!(prev_cut(&pts, 3.0005, eps), Some(0.0));
    assert_eq!(prev_cut(&pts, 3.1, eps), Some(3.0));
    assert_eq!(prev_cut(&pts, 0.033, eps), Some(0.0));
    assert_eq!(next_cut(&pts, 2.9995, eps), Some(5.0));
    assert_eq!(next_clip_start(&project(), 2.9995, eps, 0), Some(5.0));
    assert_eq!(prev_clip_start(&project(), 3.0005, eps, 0), Some(0.0));
}

#[test]
fn gap_navigation_skips_clip_boundaries() {
    // project(): [C1 0..3][gap 3..5][C2 5..7]
    let eps = 0.5 / 30.0;
    assert_eq!(next_gap_start(&project(), 0.0, eps, 0), Some(3.0));
    assert_eq!(next_gap_start(&project(), 2.0, eps, 0), Some(3.0));
    // Within half a frame of the gap we're "at" it, so there is no next
    // one (same drift tolerance as `j`/`k`).
    assert_eq!(next_gap_start(&project(), 2.9995, eps, 0), None);
    assert_eq!(next_gap_start(&project(), 3.0, eps, 0), None);
    assert_eq!(next_gap_start(&project(), 6.0, eps, 0), None);
    assert_eq!(prev_gap_start(&project(), 7.0, eps, 0), Some(3.0));
    assert_eq!(prev_gap_start(&project(), 4.0, eps, 0), Some(3.0));
    assert_eq!(prev_gap_start(&project(), 2.0, eps, 0), None);
}

#[test]
fn n_and_shift_n_jump_to_gaps() {
    let mut app = App::new(project(), PathBuf::from("t.json"));
    app.loaded = true;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('n'));
    assert_eq!(app.playhead, 3.0, "n → the gap start");
    // From the gap start there is no earlier gap; the playhead stays.
    handle_key(&mut app, &fake, key('N'));
    assert_eq!(app.playhead, 3.0);
    // From a clip past the gap, N comes back to it.
    app.playhead = 6.0;
    handle_key(&mut app, &fake, key('N'));
    assert_eq!(app.playhead, 3.0, "N → the previous gap start");
}

#[test]
fn on_boundary_detects_cuts_and_edges() {
    let p = project(); // cuts at 3 and 5; edges 0 and 7
    assert!(on_boundary(&p, 0.0, 0), "timeline start");
    assert!(on_boundary(&p, 3.0, 0));
    assert!(on_boundary(&p, 5.0, 0));
    assert!(on_boundary(&p, 7.0, 0), "timeline end");
    assert!(!on_boundary(&p, 1.5, 0));
    assert!(!on_boundary(&p, 4.0, 0));
}

#[test]
fn total_includes_the_sequence_tail() {
    // no tail concept in the segment model: total == sum of segments
    let app = App::new(project(), PathBuf::from("t.json"));
    assert_eq!(app.total(), 7.0);
}

// Frame numbering is 0-based everywhere: timecode FF is 0-based.
#[test]
fn timecode_frames_are_zero_based() {
    let fps = Seconds::new(30, 1);
    assert_eq!(fmt_timecode(0.0, Some(&fps)), "00:00:00");
    assert_eq!(fmt_timecode(1.0, Some(&fps)), "00:01:00");
    assert_eq!(fmt_timecode(1.0 / 30.0, Some(&fps)), "00:00:01");
}

#[test]
fn snap_to_cut_is_magnetic_within_one_and_a_half_frames() {
    let mut p = project();
    p.fps = Some(Seconds::new(30, 1));
    let radius = 1.5 / 30.0;
    assert_eq!(snap_to_cut(3.0 + radius, &p, 0), 3.0);
    assert_eq!(snap_to_cut(3.0 - radius, &p, 0), 3.0);
    assert_eq!(snap_to_cut(3.5, &p, 0), 3.5);
    assert_eq!(snap_to_cut(1.0, &p, 0), 1.0);
}

#[test]
fn label_truncation_and_padding() {
    assert_eq!(truncate_label("red.mp4", 10), " red.mp4  ");
    assert_eq!(truncate_label("very-long-name.mp4", 8), "very-lon");
    assert_eq!(truncate_label("ab", 2), "ab");
}
