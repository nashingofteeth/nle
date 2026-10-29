use super::*;

/// Every variant must be reachable from the `:cmd` grammar *and*
/// from the CLI — the parity tests prove it mechanically with
/// `Action::slot`.
#[test]
fn every_action_has_a_colon_spelling() {
    const SPECS: &[&str] = &[
        "trim-in",
        "trim-out",
        "split",
        "gap",
        "delete",
        "yank",
        "paste",
        "speed 200 --clip 1",
        "extend --clip 1 --edge in --to 2",
        "purge",
        "track add video",
        "track mute",
        "track rm 1",
        "media rm 1",
        "undo",
        "redo",
        "transition --at 3",
        "effect crop --clip 1 --left 10",
        "effect rm --clip 1 --index 0",
        "effect clear --clip 1",
    ];
    let mut seen = vec![false; SLOTS];
    for spec in SPECS {
        let a = parse(spec, &defaults()).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
        let s = a.slot();
        assert!(!seen[s], "{spec:?} duplicates slot {s}");
        seen[s] = true;
    }
    assert!(seen.iter().all(|&s| s), "uncovered slots: {seen:?}");
}

#[test]
fn the_colon_grammar_mirrors_the_cli_flags() {
    assert_eq!(
        parse("delete --at 3 --ripple --track 2", &defaults()),
        Ok(Action::Delete {
            track: 1,
            target: Target::At(3.0),
            ripple: true,
        })
    );
    assert_eq!(
        parse("yank --clip 9", &defaults()),
        Ok(Action::Yank {
            track: 5,
            target: Target::Clip(9),
        })
    );
    assert_eq!(
        parse("extend --seg 2 --edge out --to -1.5 --ripple", &defaults()),
        Ok(Action::Extend {
            track: 5,
            target: Target::Segment(2),
            edge: Edge::Out,
            to: -1.5,
            ripple: true,
        })
    );
}

#[test]
fn effect_removal_parses() {
    assert_eq!(
        parse("effect rm --clip 4 --index 1 --track 1", &defaults()),
        Ok(Action::RemoveEffect {
            track: 0,
            clip: 4,
            index: Some(1),
        })
    );
    // Without `--index` the last effect is the one dropped.
    assert_eq!(
        parse("effect rm --clip 4", &defaults()),
        Ok(Action::RemoveEffect {
            track: 5,
            clip: 4,
            index: None,
        })
    );
    assert_eq!(
        parse("effect clear --clip 4", &defaults()),
        Ok(Action::ClearEffects { track: 5, clip: 4 })
    );
}

#[test]
fn effect_eq_defaults_to_a_neutral_filter() {
    // ffmpeg's `eq` is neutral at contrast 1 / saturation 1. Defaulting
    // them to 0 flattened and desaturated the clip even when only
    // `--brightness` was asked for.
    let Action::AddEffect { effect, .. } =
        parse("effect --clip 1 eq --brightness -0.2", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(
        effect,
        Effect::Eq {
            brightness: -0.2,
            contrast: 1.0,
            saturation: 1.0
        }
    );
    assert_eq!(
        Effect::chain(std::slice::from_ref(&effect), (0, 0)),
        "eq=brightness=-0.2:contrast=1:saturation=1",
        "the neutral values reach the graph"
    );
}

// `--index` on an add is a replace, not an append (the CLI and `:cmd`
// share this grammar).
#[test]
fn effect_index_replaces_a_row() {
    let Action::AddEffect { effect, index, .. } =
        parse("effect eq --clip 1 --index 0", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(index, Some(0));
    assert_eq!(
        effect,
        Effect::Eq {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0
        }
    );
}

// Crop edges are percentages toward the opposite side: 0 = the frame
// edge, 100 = the other edge. 25 off each side of 1920×1080 masks the
// outer quarter but keeps the middle half in place.
#[test]
fn crop_is_percentage_insets() {
    let Action::AddEffect { effect, .. } = parse(
        "effect crop --clip 1 --left 25 --right 25 --top 25 --bottom 25",
        &defaults(),
    )
    .unwrap() else {
        panic!("expected an effect")
    };
    assert_eq!(
        effect,
        Effect::Crop {
            left: 25.0,
            right: 25.0,
            top: 25.0,
            bottom: 25.0
        }
    );
    assert_eq!(
        Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
        "format=yuva420p,crop=960:540:480:270,\
         pad=1920:1080:480:270:color=black@0"
    );
    assert_eq!(effect.details(), "l25 r25 t25 b25");

    // Omitting all four leaves the frame unchanged (defaults 0) — and
    // unlike the old zoom crop, it renders no filter at all.
    let Action::AddEffect { effect, .. } = parse("effect crop --clip 1", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(Effect::chain(&[effect], (1920, 1080)), "");

    // Asymmetric insets: 10% of 1920 = 192; 25% of 1080 = 270.
    let Action::AddEffect { effect, .. } =
        parse("effect crop --clip 1 --left 10 --top 25", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(
        Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
        "format=yuva420p,crop=1728:810:192:270,\
         pad=1920:1080:192:270:color=black@0"
    );
}

// Scale is a percentage of the picture at that point; 100 (the default) is
// a no-op, and below 100 pads transparently so a lower track shows through.
#[test]
fn scale_is_percentages_with_a_100_default() {
    let Action::AddEffect { effect, .. } = parse("effect scale --clip 1", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(effect, Effect::Scale { w: 100.0, h: 100.0 });
    assert_eq!(
        Effect::chain(&[effect], (1920, 1080)),
        "",
        "100% renders nothing (the caller drops empty filters)"
    );

    let Action::AddEffect { effect, .. } =
        parse("effect scale --clip 1 --w 50 --h 200", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    // 50%×200% of 1920×1080: a 960×2160 picture clipped to the centre
    // 1080 rows, then padded left/right to the canvas.
    assert_eq!(
        Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
        "format=yuva420p,scale=960:2160,crop=960:1080:0:540,\
         pad=1920:1080:480:0:color=black@0"
    );
    assert_eq!(effect.details(), "50% × 200%");

    let Action::AddEffect { effect, .. } =
        parse("effect scale --clip 1 --w 50 --h 50", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(
        Effect::chain(std::slice::from_ref(&effect), (1920, 1080)),
        "format=yuva420p,scale=960:540,pad=1920:1080:480:270:color=black@0"
    );
}

// Crop after scale insets the *scaled picture*, not the full canvas:
// "scale 50, crop 30% left" removes 30% of the small picture.
#[test]
fn crop_after_scale_resolves_against_the_scaled_picture() {
    let effects = vec![
        Effect::Scale { w: 50.0, h: 50.0 },
        Effect::Crop {
            left: 30.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        },
    ];
    // 320×240 → a 160×120 picture at (80,60); 30% of 160 = 48 px.
    assert_eq!(
        Effect::chain(&effects, (320, 240)),
        "format=yuva420p,scale=160:120,pad=320:240:80:60:color=black@0,\
         format=yuva420p,crop=112:120:128:60,pad=320:240:128:60:color=black@0"
    );
    assert!(Effect::masks(&effects, (320, 240)));
}

// Offset moves the picture by a percentage of the canvas; the vacated
// area is transparent and the overflow is clipped.
#[test]
fn offset_moves_the_picture_on_the_canvas() {
    let Action::AddEffect { effect, .. } =
        parse("effect offset --clip 1 --x 10 --y -5", &defaults()).unwrap()
    else {
        panic!("expected an effect")
    };
    assert_eq!(effect, Effect::Offset { x: 10.0, y: -5.0 });
    assert_eq!(effect.details(), "x 10% · y -5%");
    // +10% of 640 = 64 px right: the left 64 px is vacated, the right
    // 64 px of the picture is clipped.
    let right = Effect::Offset { x: 10.0, y: 0.0 };
    assert_eq!(
        Effect::chain(std::slice::from_ref(&right), (640, 360)),
        "format=yuva420p,crop=576:360:0:0,pad=640:360:64:0:color=black@0"
    );
    // -5% of 360 = 18 px up.
    let up = Effect::Offset { x: 0.0, y: -5.0 };
    assert_eq!(
        Effect::chain(std::slice::from_ref(&up), (640, 360)),
        "format=yuva420p,crop=640:342:0:18,pad=640:360:0:0:color=black@0"
    );
    assert!(Effect::masks(&[effect], (640, 360)));

    // ±100% of the canvas is fully off screen: a transparent layer, not
    // a 2 px sliver at the edge.
    let off = "format=yuva420p,colorchannelmixer=rr=0:gg=0:bb=0:aa=0";
    for x in [100.0, -100.0] {
        let shifted = Effect::Offset { x, y: 0.0 };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&shifted), (640, 360)),
            off,
            "offset x={x}%"
        );
        assert!(Effect::masks(&[shifted], (640, 360)));
    }
    for y in [100.0, -100.0] {
        let shifted = Effect::Offset { x: 0.0, y };
        assert_eq!(
            Effect::chain(std::slice::from_ref(&shifted), (640, 360)),
            off,
            "offset y={y}%"
        );
    }

    // 0/0 is a no-op.
    assert_eq!(
        Effect::chain(&[Effect::Offset { x: 0.0, y: 0.0 }], (640, 360)),
        ""
    );
}

// On a picture that scale/crop already moved right, 80% is 80% of the
// remaining distance to the edge, so part of it is still visible.
#[test]
fn offset_counts_the_distance_to_the_edge() {
    let effects = vec![
        Effect::Scale { w: 50.0, h: 50.0 },
        Effect::Crop {
            left: 30.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        },
        Effect::Offset { x: 80.0, y: 0.0 },
    ];
    // The 160×120 scale is cropped to 112×120 at (128,60); 80% of the
    // 192 px to the right edge is 154 px, leaving a 38 px strip.
    let chain = Effect::chain(&effects, (320, 240));
    assert!(
        chain.ends_with("crop=38:120:0:0,pad=320:240:282:60:color=black@0"),
        "{chain}"
    );

    // 100% covers the whole distance: fully off-screen.
    let off = vec![
        Effect::Scale { w: 50.0, h: 50.0 },
        Effect::Crop {
            left: 30.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        },
        Effect::Offset { x: 100.0, y: 0.0 },
    ];
    assert!(
        Effect::chain(&off, (320, 240)).ends_with("colorchannelmixer=rr=0:gg=0:bb=0:aa=0"),
        "{chain}"
    );
}

#[test]
fn bad_effect_percentages_are_refused() {
    let d = ParseDefaults::default();
    for spec in [
        "effect crop --clip 1 --left 101",
        "effect crop --clip 1 --left 50 --right 50",
        "effect crop --clip 1 --top 60 --bottom 40",
        "effect scale --clip 1 --w 0",
        "effect scale --clip 1 --h 1001",
        "effect offset --clip 1 --x 101",
        "effect offset --clip 1 --y -101",
    ] {
        assert!(
            matches!(parse(spec, &d), Err(ParseError::Invalid(_))),
            "{spec:?} was accepted"
        );
    }
}

#[test]
fn defaults_fill_in_the_playhead_track_and_clip() {
    assert_eq!(
        parse("split", &defaults()),
        Ok(Action::Split { track: 5, at: 7.5 })
    );
    assert_eq!(
        parse("speed 50", &defaults()),
        Ok(Action::SetSpeed {
            track: 5,
            clip: 42,
            rate: Seconds::new(1, 2),
        })
    );
    // `:effect` falls back to the clip under the playhead too (it used to
    // demand an explicit `--clip`, unlike every other clip-addressed verb).
    match parse("effect eq", &defaults()) {
        Ok(Action::AddEffect { track, clip, .. }) => {
            assert_eq!((track, clip), (5, 42), "the playhead's clip fills in")
        }
        other => panic!("expected an effect action, got {other:?}"),
    }
    // No playhead to fall back to (the CLI case): refuse instead of
    // guessing a position.
    let bare = ParseDefaults::default();
    assert!(matches!(parse("split", &bare), Err(ParseError::Invalid(_)),));
}

#[test]
fn a_gap_needs_a_positive_length() {
    let d = ParseDefaults::default();
    assert!(matches!(
        parse("gap --at 1 --frames 0", &d),
        Err(ParseError::Invalid(msg)) if msg.contains("at least one frame")
    ));
    assert!(matches!(
        parse("gap --at 1 --dur -2", &d),
        Err(ParseError::Invalid(msg)) if msg.contains("positive length")
    ));
    assert!(parse("gap --at 1 --frames 3", &d).is_ok());
}

/// An explicit `--at` works with no playhead default (the parser used
/// to evaluate its fallback eagerly, so `--at 1` reported "no position
/// given" whenever the front-end had no default position).
#[test]
fn explicit_flags_work_without_defaults() {
    let bare = ParseDefaults::default();
    assert_eq!(
        parse("split --at 1.5", &bare),
        Ok(Action::Split { track: 0, at: 1.5 })
    );
    assert_eq!(
        parse("delete --clip 3 --ripple", &bare),
        Ok(Action::Delete {
            track: 0,
            target: Target::Clip(3),
            ripple: true,
        })
    );
}

#[test]
fn unknown_verbs_and_bad_flags_stay_apart() {
    // Unknown verbs may be another front-end's command; bad flags on a
    // known verb are always an error.
    assert_eq!(
        parse("export -o out.mp4", &defaults()),
        Err(ParseError::Unknown("export".to_string()))
    );
    assert!(matches!(
        parse("split --bogus 1", &defaults()),
        Err(ParseError::Invalid(_))
    ));
    assert!(matches!(
        parse("delete --at 1 --clip 2", &defaults()),
        Err(ParseError::Invalid(_))
    ));
    // The CLI's short flag works in the shared tokenizer too.
    let (verb, mut f) = split_args("export -o out.mp4").unwrap();
    assert_eq!(verb, "export");
    assert_eq!(f.take("out").as_deref(), Some("out.mp4"));
    assert!(f.positional().is_none(), "no stray positionals");
}

// ------------------------------------------------------------ run
