//! mpv as a programmable monitor: child process + JSON IPC client.
//!
//! mpv opens a unix socket (`--input-ipc-server`); we speak newline-framed
//! JSON: commands in (`{"command": [...]}`), events out. A reader thread
//! owns the receiving half of the socket and keeps a shared `PlaybackState`
//! fresh; the TUI reads it every tick and sends commands through a separate
//! cloned stream (readers and writers don't contend).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MpvError {
    #[error("could not start mpv: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("mpv's IPC socket never appeared at {0} — is another nle running?")]
    SocketTimeout(PathBuf),
    #[error("mpv IPC failed: {0}")]
    Ipc(String),
}

/// What the reader thread knows about playback. Read by the TUI every tick.
#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    /// mpv's reported position — for an EDL this is TIMELINE time. Seconds.
    pub time_pos: Option<f64>,
    /// Total length of the loaded timeline (seconds).
    pub duration: Option<f64>,
    pub paused: bool,
    /// A file finished loading and is ready for commands.
    pub file_loaded: bool,
    /// The socket closed: mpv was closed or crashed.
    pub dead: bool,
    /// Last error mpv reported (e.g. loadfile failure).
    pub last_error: Option<String>,
}

/// A running mpv instance acting as the preview monitor.
pub struct MpvPlayer {
    child: Child,
    socket_path: PathBuf,
    writer: Mutex<UnixStream>,
    state: Arc<Mutex<PlaybackState>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl MpvPlayer {
    /// Spawn mpv in idle mode with an IPC socket and connect to it.
    pub fn spawn(title: &str) -> Result<Self, MpvError> {
        let socket_path = std::env::temp_dir().join(format!("nle-mpv-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path); // stale socket from a crash

        let child = Command::new("mpv")
            .args([
                "--no-terminal",
                "--idle=yes",           // stay alive before/after files
                "--keep-open=always",   // at EOF: pause at the end, don't quit
                "--pause",              // start paused at 0
                "--hr-seek=yes",        // every seek is frame-exact
                "--force-window=yes",
                &format!("--input-ipc-server={}", socket_path.display()),
                &format!("--title={title}"),
            ])
            .spawn()?;

        // The socket appears shortly after spawn.
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if !socket_path.exists() {
            return Err(MpvError::SocketTimeout(socket_path));
        }

        let stream = UnixStream::connect(&socket_path)?;
        let reader_stream = stream.try_clone().map_err(|e| MpvError::Ipc(e.to_string()))?;
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let reader_state = Arc::clone(&state);
        let reader = std::thread::spawn(move || read_loop(reader_stream, reader_state));

        Ok(Self {
            child,
            socket_path,
            writer: Mutex::new(stream),
            state,
            reader: Some(reader),
        })
    }

    /// Subscribe to the properties the TUI cares about.
    pub fn observe(&self) -> Result<(), MpvError> {
        for (id, name) in [(1, "time-pos"), (2, "pause"), (3, "duration"), (4, "eof-reached")] {
            self.send(serde_json::json!(["observe_property", id, name]))?;
        }
        Ok(())
    }

    fn send(&self, command: serde_json::Value) -> Result<(), MpvError> {
        let mut writer = self.writer.lock().map_err(|e| MpvError::Ipc(e.to_string()))?;
        writeln!(writer, "{}", serde_json::json!({ "command": command }))
            .map_err(|e| MpvError::Ipc(e.to_string()))
    }

    /// Load (or replace) the file to play. `url` may be an edl:// URI.
    pub fn load(&self, url: &str) {
        let _ = self.send(serde_json::json!(["loadfile", url, "replace"]));
    }

    pub fn toggle_pause(&self) {
        let _ = self.send(serde_json::json!(["cycle", "pause"]));
    }

    pub fn seek_relative(&self, delta: f64) {
        let _ = self.send(serde_json::json!(["seek", delta, "relative"]));
    }

    pub fn seek_absolute(&self, target: f64) {
        let _ = self.send(serde_json::json!(["seek", target, "absolute"]));
    }

    pub fn frame_step(&self) {
        let _ = self.send(serde_json::json!(["frame_step"]));
    }

    pub fn frame_back_step(&self) {
        let _ = self.send(serde_json::json!(["frame_back_step"]));
    }

    /// Snapshot of the reader thread's knowledge.
    pub fn state(&self) -> PlaybackState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ask mpv to exit cleanly.
    pub fn quit(&self) {
        let _ = self.send(serde_json::json!(["quit"]));
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.quit();
        // Give it a moment to exit on its own; kill as a fallback.
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if self.child.try_wait().map(|s| s.is_none()).unwrap_or(true) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

// ------------------------------------------------------------- IPC parsing

/// Either a response to one of our commands, or an event.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IpcMessage {
    Response { error: String },
    Event {
        event: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        data: Option<serde_json::Value>,
        #[serde(default)]
        reason: Option<String>,
    },
}

fn read_loop(stream: UnixStream, state: Arc<Mutex<PlaybackState>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                if let Ok(mut st) = state.lock() {
                    st.dead = true;
                }
                break;
            }
            Ok(_) => {
                let Ok(msg) = serde_json::from_str::<IpcMessage>(line.trim()) else {
                    continue;
                };
                let Ok(mut st) = state.lock() else { break };
                match msg {
                    IpcMessage::Response { error } => {
                        if error != "success" {
                            st.last_error = Some(error);
                        }
                    }
                    IpcMessage::Event { event, name, data, reason } => match event.as_str() {
                        "property-change" => apply_property(&mut st, name.as_deref(), data),
                        "file-loaded" => st.file_loaded = true,
                        "end-file" => {
                            st.file_loaded = false;
                            if let Some(r) = reason
                                && r != "eof" && r != "stop" && r != "redirect" {
                                    st.last_error = Some(format!("playback ended: {r}"));
                                }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

fn apply_property(st: &mut PlaybackState, name: Option<&str>, data: Option<serde_json::Value>) {
    match name {
        Some("time-pos") => st.time_pos = data.as_ref().and_then(|v| v.as_f64()),
        Some("duration") => st.duration = data.as_ref().and_then(|v| v.as_f64()),
        Some("pause") => st.paused = data.as_ref().and_then(|v| v.as_bool()).unwrap_or(false),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_property_change_events() {
        let json = r#"{"event":"property-change","id":1,"name":"time-pos","data":12.5}"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        match msg {
            IpcMessage::Event { event, name, data, .. } => {
                assert_eq!(event, "property-change");
                assert_eq!(name.as_deref(), Some("time-pos"));
                assert_eq!(data.unwrap().as_f64(), Some(12.5));
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn parses_error_responses() {
        let json = r#"{"error":"loading failed","request_id":0}"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        match msg {
            IpcMessage::Response { error } => assert_eq!(error, "loading failed"),
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn ignores_unknown_lines() {
        assert!(serde_json::from_str::<IpcMessage>("not json").is_err());
    }

    #[test]
    fn state_defaults_are_sane() {
        let st = PlaybackState::default();
        assert!(!st.paused && !st.file_loaded && !st.dead);
        assert!(st.time_pos.is_none());
    }
}
