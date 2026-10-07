//! Opt-in debug log.
//!
//! In the TUI, stdout/stderr are the alternate screen and mpv runs
//! `--no-terminal`, so diagnostics vanish. Set `NLE_LOG` to a file path and
//! every user-visible notice and mpv error is appended as a timestamped line;
//! unset, `line` is a no-op.
//!
//! Zero-dependency on purpose (the dep set is frozen): a
//! `OnceLock<Option<Mutex<File>>>` opened on first use.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// The log file, opened once from `NLE_LOG`. `None` when unset or unopenable.
fn sink() -> &'static Option<Mutex<File>> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("NLE_LOG")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
}

/// Append a line to `NLE_LOG`, if set. Never fails the caller.
pub fn line(msg: &str) {
    let Some(sink) = sink() else { return };
    if let Ok(mut file) = sink.lock() {
        let _ = emit(&mut file, msg);
    }
}

/// The actual write, split out so it can be tested without touching the
/// process-wide env/`OnceLock`.
fn emit(file: &mut File, msg: &str) -> std::io::Result<()> {
    writeln!(file, "[{}] {msg}", stamp())
}

/// Seconds-since-epoch with milliseconds — enough to order a debug trail
/// without pulling in a time crate.
fn stamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}.{:03}", d.as_secs(), d.subsec_millis()),
        Err(_) => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_seconds_and_millis() {
        let s = stamp();
        let (secs, millis) = s.split_once('.').expect("a dot");
        assert!(secs.parse::<u64>().is_ok(), "{s}");
        assert_eq!(millis.len(), 3, "{s}");
    }

    #[test]
    fn emit_writes_a_timestamped_line() {
        let path = std::env::temp_dir().join(format!("nle-log-test-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        emit(&mut file, "hello").unwrap();
        drop(file);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with('['), "{text}");
        assert!(text.contains("] hello"), "{text}");
        let _ = std::fs::remove_file(&path);
    }
}
