//! Background work: the ffmpeg render/import job and the off-thread preview
//! build. Both are polled from the event loop; neither blocks a keystroke.

use std::time::Instant;

use crate::mpv::Mpv;
use crate::preview;

use super::keys::extend::extend_state;
use super::{App, is_pool, reload_project};
/// A background job (an ffmpeg render, a heavy import): its label for
/// notices, whether the project file changed underneath us (reload after),
/// and the channel its worker reports on. One at a time — the monitor
/// keeps playing while it runs.
pub(crate) struct Job {
    pub(crate) label: &'static str,
    pub(crate) reload: bool,
    pub(crate) rx: std::sync::mpsc::Receiver<Result<String, String>>,
}

/// A monitor preview rebuild in flight. Proxy renders are seconds of ffmpeg,
/// so the EDL is built on a worker while the status row shows progress.
pub(crate) struct PreviewJob {
    pub(crate) rx: std::sync::mpsc::Receiver<PreviewMsg>,
    /// When the build started — the status row waits a moment before
    /// announcing it, so a cached/no-proxy rebuild doesn't flash.
    pub(crate) started: Instant,
}

pub(crate) enum PreviewMsg {
    /// `(finished renders, total renders, fraction of the one in flight)`.
    Progress {
        done: usize,
        total: usize,
        frac: f64,
    },
    Done(Result<String, String>),
}

/// What the status row shows while a preview renders.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PreviewProgress {
    pub done: usize,
    pub total: usize,
    pub frac: f64,
}

/// Arm a background job (one at a time) and hand back its sender.
pub(crate) fn start_job(
    app: &mut App,
    label: &'static str,
    reload: bool,
) -> Option<std::sync::mpsc::Sender<Result<String, String>>> {
    if app.job.is_some() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.job = Some(Job { label, reload, rx });
    Some(tx)
}

/// Let a finished worker report in: swap its result into the message
/// line, and reload the project when the job rewrote it.
pub(crate) fn poll_job(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    let Some((label, reload, res)) = app
        .job
        .as_ref()
        .map(|j| (j.label, j.reload, j.rx.try_recv()))
    else {
        return;
    };
    match res {
        Ok(Ok(msg)) => {
            app.job = None;
            if reload && let Err(e) = reload_project(app, player, true) {
                app.fail(&format!("{msg} — reload failed: {e}"));
            } else {
                app.set_message(&msg);
            }
        }
        Ok(Err(e)) => {
            app.job = None;
            app.fail(&format!("{label} failed: {e}"));
        }
        Err(TryRecvError::Disconnected) => {
            app.job = None;
            app.fail(&format!("{label} failed: worker died"));
        }
        Err(TryRecvError::Empty) => {}
    }
}

/// Rebuild the monitor's EDL at the current playhead, off-thread: a proxy
/// render is seconds of ffmpeg, and blocking the key handler froze the UI.
/// A second edit while one runs just marks it dirty — the finished render
/// still warms the cache — and the newest build follows.
pub(crate) fn start_preview(app: &mut App) {
    if !app.preview_enabled {
        return;
    }
    if app.preview.is_some() {
        app.preview_dirty = true;
        return;
    }
    let project = app.project.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_dirty = false;
    app.preview_progress = Some(PreviewProgress::default());
    std::thread::spawn(move || {
        let progress_tx = tx.clone();
        let mut report = move |done, total, frac| {
            let _ = progress_tx.send(PreviewMsg::Progress { done, total, frac });
        };
        let result =
            preview::build_composite_edl_with(&project, &mut report).map_err(|e| e.to_string());
        let _ = tx.send(PreviewMsg::Done(result));
    });
}

/// Drain the preview worker: progress feeds the status row, and a completed
/// (non-stale) build loads the monitor at wherever the playhead is now. While
/// the pool is open its asset keeps the monitor — `close_pool` rebuilds.
pub(crate) fn poll_preview(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    loop {
        let Some(res) = app.preview.as_ref().map(|j| j.rx.try_recv()) else {
            return;
        };
        match res {
            Ok(PreviewMsg::Progress { done, total, frac }) => {
                app.preview_progress = Some(PreviewProgress { done, total, frac });
            }
            Ok(PreviewMsg::Done(result)) => {
                let stale = app.preview_dirty;
                let pool = is_pool(app);
                let extending = extend_state(app).is_some();
                app.preview = None;
                app.preview_progress = None;
                if stale {
                    app.preview_dirty = false;
                    if !pool && !extending {
                        start_preview(app);
                    }
                } else if !pool && !extending {
                    // Extend mode owns the monitor like the pool does: over
                    // the material preview it handed to mpv, whose clock is
                    // translated through `preview_off`. A timeline build
                    // finishing mid-session must not replace that picture
                    // (`exit_extend` reloads when extend leaves).
                    match result {
                        Ok(url) => {
                            // Keep ignoring the pooled clock until the EDL's
                            // own file reports the landing (see `sync`).
                            if app.pool_on_monitor {
                                app.pool_handoff = Some(app.playhead);
                            }
                            player.load(&url, Some(app.playhead));
                            if let Some(paused) = app.pending_pause.take() {
                                player.set_pause(paused);
                            }
                        }
                        Err(e) => preview_failed(app, &e.to_string()),
                    }
                }
                return;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                app.preview = None;
                app.preview_progress = None;
                preview_failed(app, "worker died");
                return;
            }
        }
    }
}

/// Keep the timeline's notice: a monitor problem must not erase what
/// happened to the model (the same rule as the old inline rebuild).
/// A failed rebuild during a hand-off (pool close, extend exit/commit)
/// also gives the hand-off up: the timeline EDL will never land, and
/// `sync` gates on `pool_on_monitor`, so the playhead would ignore mpv
/// forever. The monitor keeps the old (paused) file; the next action's
/// rebuild fixes the picture — and this notice says what didn't.
/// In the pool the pooled asset legitimately owns the monitor: there the
/// flag stands.
pub(crate) fn preview_failed(app: &mut App, why: &str) {
    if !is_pool(app) {
        app.pool_on_monitor = false;
        app.pool_handoff = None;
    }
    let msg = if app.message.is_empty() {
        format!("preview failed: {why}")
    } else {
        format!("{} — preview failed: {why}", app.message)
    };
    app.fail(&msg);
}
