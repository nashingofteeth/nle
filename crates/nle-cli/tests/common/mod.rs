//! Shared fixture for the behaviour tests.
//!
//! These are the old shell batteries turned into ordinary `#[test]`s: they
//! drive the built `nle` binary with real ffmpeg, real exports and real
//! pixels, so `cargo test` is the whole verification and there is no second
//! script to keep in sync.
//!
//! Requirements: `ffmpeg`/`ffprobe` on `PATH` (the editor needs them anyway).
//!
//! Layout: media is generated once into `$CARGO_TARGET_TMPDIR/nle-media` and
//! reused across runs (cleared by `cargo clean`); the *project templates* are
//! rebuilt on every run, because a stale one from before a format-version bump
//! would poison every case with "project version … is not supported".

#![allow(dead_code)] // each test binary uses a different subset

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use tempfile::TempDir;

/// The project every helper addresses unless told otherwise.
pub const P: &str = "p.json";

pub const RED: [u8; 3] = [0xfd, 0x00, 0x00];
pub const GREEN: [u8; 3] = [0x00, 0x7f, 0x00];
pub const BLUE: [u8; 3] = [0x00, 0x00, 0xfe];
pub const BLACK: [u8; 3] = [0x00, 0x00, 0x00];

/// What `eq` did to a frame.
pub enum Look {
    Grey,
    Hue,
}

pub struct Sandbox {
    dir: TempDir,
    cache: PathBuf,
}

impl Sandbox {
    /// A throwaway working directory with the generated media copied in (so
    /// commands can name `red.mp4` the way the scripts did), and its own cache
    /// dir (so no test races another, or the user's `~/.cache`).
    pub fn new() -> Sandbox {
        let dir = tempfile::tempdir().expect("a temp dir");
        for name in MEDIA {
            fs::copy(media_dir().join(name), dir.path().join(name))
                .unwrap_or_else(|e| panic!("copy {name}: {e}"));
        }
        let cache = dir.path().join(".cache");
        fs::create_dir_all(&cache).expect("a cache dir");
        Sandbox { dir, cache }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    // ---------------------------------------------------------------- running

    fn command(&self, project: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nle"));
        cmd.current_dir(self.path())
            .env("XDG_CACHE_HOME", &self.cache)
            .arg("--project")
            .arg(project)
            .args(args);
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_on(P, args)
    }

    pub fn run_on(&self, project: &str, args: &[&str]) -> Output {
        self.command(project, args).output().expect("spawn nle")
    }

    /// Run, require success, return stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        self.ok_on(P, args)
    }

    pub fn ok_on(&self, project: &str, args: &[&str]) -> String {
        let out = self.run_on(project, args);
        assert!(
            out.status.success(),
            "nle {args:?} should have succeeded:\n{}",
            merged(&out)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run, require refusal, return stdout+stderr (to assert on the message).
    pub fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            !out.status.success(),
            "nle {args:?} should have refused:\n{}",
            merged(&out)
        );
        merged(&out)
    }

    // -------------------------------------------------------------- projects

    /// The three-clip sequence the result assertions use:
    /// red 0–2, green 2–4, blue 4–6 (one clip each, in order).
    pub fn base(&self) {
        self.seed(sequence_template());
    }

    /// The hostile battery's base: red 0–3, gap 3–4, blue 4–7, playhead 1.5.
    pub fn hostile_base(&self) {
        self.seed(hostile_template());
    }

    fn seed(&self, template: &Path) {
        fs::copy(template, self.file(P)).expect("seed the project");
        for sidecar in ["p.history.json", "p.clipboard.json"] {
            let _ = fs::remove_file(self.file(sidecar));
        }
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.file(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    pub fn write(&self, name: &str, text: &str) {
        fs::write(self.file(name), text).unwrap_or_else(|e| panic!("write {name}: {e}"));
    }

    // --------------------------------------------------------------- exports

    /// Export and return the report line (the export summary).
    pub fn export_report(&self, name: &str) -> String {
        let out = self.run(&["export", "-o", name]);
        assert!(out.status.success(), "export failed:\n{}", merged(&out));
        merged(&out)
    }

    pub fn export(&self, name: &str) -> PathBuf {
        self.export_report(name);
        self.file(name)
    }

    // ---------------------------------------------------------------- pixels

    /// The first pixel of the frame at `t` seconds, as `ffmpeg` decodes it.
    pub fn px(&self, file: &str, t: f64) -> [u8; 3] {
        let out = Command::new("ffmpeg")
            .current_dir(self.path())
            .args([
                "-v",
                "error",
                "-y",
                "-ss",
                &format!("{t}"),
                "-i",
                file,
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .output()
            .expect("spawn ffmpeg");
        assert!(
            out.status.success(),
            "ffmpeg sampling {file} at {t} s:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.len() >= 3, "no frame at {t} s in {file}");
        [out.stdout[0], out.stdout[1], out.stdout[2]]
    }

    pub fn assert_px(&self, file: &str, t: f64, rgb: [u8; 3], label: &str) {
        let got = self.px(file, t);
        assert_eq!(got, rgb, "{label} ({file} @ {t} s)");
    }

    pub fn assert_px_ne(&self, file: &str, t: f64, rgb: [u8; 3], label: &str) {
        let got = self.px(file, t);
        assert_ne!(got, rgb, "{label} ({file} @ {t} s)");
    }

    /// `Eq` is spatial: a desaturated frame has r≈g≈b (and must not be black,
    /// which a missing clip would be); a hue survives otherwise.
    pub fn assert_look(&self, file: &str, t: f64, look: Look, label: &str) {
        let [r, g, b] = self.px(file, t);
        let hi = r.max(g).max(b);
        let grey = hi - r.min(g).min(b) <= 8 && hi > 20;
        match look {
            Look::Grey => assert!(grey, "{label}: {r:02x}{g:02x}{b:02x} is not grey"),
            Look::Hue => assert!(!grey, "{label}: {r:02x}{g:02x}{b:02x} is grey"),
        }
    }

    // -------------------------------------------------------------- probing

    fn probe(&self, file: &str, args: &[&str]) -> String {
        let out = Command::new("ffprobe")
            .current_dir(self.path())
            .args(["-v", "error"])
            .args(args)
            .arg(file)
            .output()
            .expect("spawn ffprobe");
        assert!(
            out.status.success(),
            "ffprobe {args:?} {file}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn duration(&self, file: &str) -> f64 {
        self.probe(
            file,
            &["-show_entries", "format=duration", "-of", "csv=p=0"],
        )
        .parse()
        .unwrap_or_else(|_| panic!("duration of {file}"))
    }

    pub fn assert_dur(&self, file: &str, secs: f64, label: &str) {
        let got = self.duration(file);
        assert!(
            (got - secs).abs() < 0.06,
            "{label}: {file} is {got} s, want {secs} s"
        );
    }

    pub fn frames(&self, file: &str) -> u64 {
        self.probe(
            file,
            &[
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=nb_frames",
                "-of",
                "csv=p=0",
            ],
        )
        .parse()
        .unwrap_or_else(|_| panic!("frame count of {file}"))
    }

    pub fn assert_frames(&self, file: &str, n: u64, label: &str) {
        assert_eq!(self.frames(file), n, "{label} ({file})");
    }

    pub fn has_audio(&self, file: &str) -> bool {
        !self
            .probe(
                file,
                &[
                    "-select_streams",
                    "a",
                    "-show_entries",
                    "stream=codec_type",
                    "-of",
                    "csv=p=0",
                ],
            )
            .is_empty()
    }

    // ------------------------------------------------------------------ views

    pub fn show(&self) -> String {
        self.ok(&["show"])
    }

    /// The EDL the monitor plays for track 1.
    pub fn edl(&self) -> String {
        self.ok(&["edl", "--track", "1"])
    }

    /// Every `edl://` entry must be the sequence's size: mpv plays them all
    /// into one window, so an entry at another size is re-framed (bars) and
    /// the framing jumps when the monitor crosses into it.
    pub fn assert_edl_geometry(&self, edl: &str, w: u32, h: u32, label: &str) {
        let want = format!("{w},{h}");
        let entries = edl_paths(edl);
        assert!(!entries.is_empty(), "{label}: the EDL has no entries");
        for path in entries {
            let got = self.probe(
                &path.display().to_string(),
                &[
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=width,height",
                    "-of",
                    "csv=p=0",
                ],
            );
            assert_eq!(got, want, "{label}: {} is {got}", path.display());
        }
    }

    // ------------------------------------------------------------- invariant

    /// One step of the hostile battery: no panic, and an exit code of 0
    /// (accepted), 1 (refused) or 2 (usage). Returns the merged output for
    /// context if a later assertion fails.
    pub fn insist(&self, label: &str, project: &str, argv: &[&str]) -> String {
        let out = self.run_on(project, argv);
        let text = merged(&out);
        assert!(
            !text.contains("panicked"),
            "[{label}] panicked on {argv:?}:\n{text}"
        );
        let code = out.status.code();
        assert!(
            matches!(code, Some(0..=2)),
            "[{label}] exited {code:?} on {argv:?}:\n{text}"
        );
        text
    }

    /// The whole invariant: a fresh hostile base, the steps run in order, then
    /// the project must still load with a finite, non-negative length.
    pub fn invariant(&self, label: &str, steps: &[&[&str]]) {
        self.hostile_base();
        let mut last = String::new();
        for argv in steps {
            last = self.insist(label, P, argv);
        }
        self.assert_loads(label, &last);
    }

    pub fn assert_loads(&self, label: &str, context: &str) {
        let project = nle_core::Project::load(&self.file(P)).unwrap_or_else(|e| {
            panic!("[{label}] left a project that no longer loads: {e}\n{context}")
        });
        let secs = nle_core::secs_f64(&project.timeline.end());
        assert!(
            (0.0..1e9).contains(&secs),
            "[{label}] left a corrupt length {secs}"
        );
    }

    // ------------------------------------------------------- output plumbing

    /// Run with stdout going to `sink` (e.g. `/dev/full`), returning the exit
    /// status.
    pub fn run_with_stdout(&self, args: &[&str], sink: Stdio) -> std::process::ExitStatus {
        let mut cmd = self.command(P, args);
        cmd.stderr(Stdio::null()).stdout(sink);
        cmd.status().expect("spawn nle")
    }

    /// Spawn, read one line, close the pipe, and return the exit status.
    pub fn run_with_closed_pipe(&self, args: &[&str]) -> std::process::ExitStatus {
        use std::io::{BufRead, BufReader};
        let mut child = self
            .command(P, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nle");
        {
            let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
            assert!(lines.next().is_some(), "no first line");
        } // dropping the reader closes the pipe
        child.wait().expect("wait")
    }

    /// Every generated file under the sandbox's cache (proxy or black filler).
    pub fn cached_mp4s(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "mp4") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.cache, &mut out);
        out.sort();
        out
    }
}

/// stdout then stderr, for failure messages and message assertions.
pub fn merged(out: &Output) -> String {
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text
}

/// The paths in an `edl://` URL (`%len%path,start,length;…`). Walks the
/// length prefixes rather than splitting on `;`, because a quoted path may
/// contain one.
fn edl_paths(edl: &str) -> Vec<PathBuf> {
    let text = edl.strip_prefix("edl://").expect("an edl:// URL");
    let bytes = text.as_bytes();
    let mut paths = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        assert_eq!(bytes[i], b'%', "unexpected EDL quote at {i}");
        let close = i + 1 + text[i + 1..].find('%').expect("a length prefix");
        let len: usize = text[i + 1..close].parse().expect("the length prefix");
        paths.push(PathBuf::from(&text[close + 1..close + 1 + len]));
        match text[close + 1 + len..].find(';') {
            Some(off) => i = close + 1 + len + off + 1,
            None => break,
        }
    }
    paths
}

// ------------------------------------------------------------------ fixtures

const MEDIA: &[&str] = &[
    "red.mp4",
    "green.mp4",
    "blue.mp4",
    "wide169.mp4",
    "video_only.mp4",
    "audio_only.m4a",
    "tiny.mp4",
    "empty.mp4",
    "notmedia.txt",
];

fn media_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nle-media");
        fs::create_dir_all(&dir).expect("the media dir");
        let d = dir.as_path();
        // red/green at 30 fps, blue at 59.94 (the rate-mismatch case), a 16:9
        // source (the off-sequence case), and the odd files the hostile
        // battery hands the CLI.
        gen_av(d, "red.mp4", "red", "320x240", "30", "5", "440");
        gen_av(d, "green.mp4", "green", "320x240", "30", "5", "660");
        gen_av(d, "blue.mp4", "blue", "320x240", "60000/1001", "5", "880");
        gen_av(d, "wide169.mp4", "blue", "640x360", "30", "5", "880");
        gen_v(d, "video_only.mp4", "orange", "2");
        gen_a(d, "audio_only.m4a", "2");
        gen_v(d, "tiny.mp4", "gray", "0.1");
        fs::write(d.join("empty.mp4"), b"").expect("empty.mp4");
        fs::write(d.join("notmedia.txt"), b"not media").expect("notmedia.txt");
        dir
    })
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn gen_av(dir: &Path, name: &str, color: &str, size: &str, rate: &str, dur: &str, freq: &str) {
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    let picture = format!("color=c={color}:s={size}:d={dur}:r={rate}");
    let tone = format!("sine=frequency={freq}:duration={dur}");
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &picture,
        "-f",
        "lavfi",
        "-i",
        &tone,
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-crf",
        "23",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        path.to_str().expect("a UTF-8 path"),
    ]);
}

fn gen_v(dir: &Path, name: &str, color: &str, dur: &str) {
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    let picture = format!("color=c={color}:s=320x240:d={dur}:r=30");
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &picture,
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-an",
        path.to_str().expect("a UTF-8 path"),
    ]);
}

fn gen_a(dir: &Path, name: &str, dur: &str) {
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    let tone = format!("sine=frequency=300:duration={dur}");
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &tone,
        "-c:a",
        "aac",
        path.to_str().expect("a UTF-8 path"),
    ]);
}

/// `nle` on one project, for building the templates (no sandbox yet).
fn cli(dir: &Path, project: &Path, args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_nle"))
        .current_dir(dir)
        .arg("--project")
        .arg(project)
        .args(args)
        .output()
        .expect("spawn nle");
    assert!(
        out.status.success(),
        "template setup nle {args:?}:\n{}",
        merged(&out)
    );
}

/// red 0–2, green 2–4, blue 4–6 (the result assertions' sequence).
fn sequence_template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let dir = media_dir();
        let project = dir.join("base_sequence.json");
        cli(dir, &project, &["init", "--force"]);
        cli(
            dir,
            &project,
            &["import", "red.mp4", "green.mp4", "blue.mp4"],
        );
        cli(
            dir,
            &project,
            &["add", "red.mp4", "--in", "0", "--out", "2"],
        );
        cli(
            dir,
            &project,
            &["add", "green.mp4", "--at", "2", "--in", "0", "--out", "2"],
        );
        cli(
            dir,
            &project,
            &["add", "blue.mp4", "--at", "4", "--in", "0", "--out", "2"],
        );
        project
    })
}

/// red 0–3, gap 3–4, blue 4–7, playhead 1.5 (the hostile battery's base).
fn hostile_template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let dir = media_dir();
        let project = dir.join("base_hostile.json");
        cli(dir, &project, &["init", "--force"]);
        cli(dir, &project, &["import", "red.mp4", "blue.mp4"]);
        cli(
            dir,
            &project,
            &["add", "red.mp4", "--in", "0", "--out", "3"],
        );
        cli(
            dir,
            &project,
            &["add", "blue.mp4", "--at", "4", "--in", "0", "--out", "3"],
        );
        cli(dir, &project, &["seek", "1.5"]);
        project
    })
}
