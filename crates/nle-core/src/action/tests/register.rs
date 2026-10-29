use super::*;

#[test]
fn a_register_from_another_project_resolves_or_refuses() {
    let p = fixture();
    // The same file pooled here under a different id (asset 1 is
    // red.mp4 in the fixture): the register resolves by path.
    let mut other = p.clone();
    let (spare, _) = other.add_asset("/media/spare.mp4".into(), probe_report("5"));
    let reg = Register::of(
        &other,
        Segment::Clip(Clip {
            id: 7,
            asset: spare,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    );
    assert_eq!(reg.path.as_deref(), Some(Path::new("/media/spare.mp4")));
    // Not in this pool → refuse, loudly.
    assert!(matches!(
        reg.resolve(&p),
        Err(EditError::RegisterForeign(path)) if path.contains("spare.mp4")
    ));

    // A file this project *does* pool (under a different id) remaps.
    let mut here = p.clone();
    let (id, _) = here.add_asset("/media/other/red.mp4".into(), probe_report("10"));
    let entry = Register {
        segment: Segment::Clip(Clip {
            id: 7,
            asset: 999, // the id it had over there
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        path: Some(PathBuf::from("/media/other/red.mp4")),
    };
    let resolved = entry.resolve(&here).unwrap();
    assert!(
        matches!(resolved, Segment::Clip(Clip { asset, .. }) if asset == id),
        "the asset id is remapped to this pool's: {resolved:?}"
    );
}

#[test]
fn the_register_round_trips_through_its_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.json");
    assert_eq!(load_register(&path), None, "no file, no register");
    let reg = Register {
        segment: Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        path: Some(PathBuf::from("/media/red.mp4")),
    };
    save_register(&path, Some(&reg)).unwrap();
    assert_eq!(load_register(&path), Some(reg));
    save_register(&path, None).unwrap();
    assert_eq!(load_register(&path), None, "clearing removes the file");
    assert!(!register_path(&path).exists());
}
