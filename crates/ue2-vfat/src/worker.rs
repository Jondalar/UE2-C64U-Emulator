//! One volume on its own thread: syncs and rebuilds run there, so the emulation thread only copies the image and
//! swaps media. The host watcher feeds the same thread; after [`DEBOUNCE`] without events it compares the host with
//! the manifest and announces a real change once.
//!
//! No replug loops: after a replug that did not rebuild (its sync failed or was refused, or the rebuild failed) the
//! worker holds a fingerprint of the host tree and announces nothing until the host differs from it or a sync
//! succeeds. A host change that no longer fits the volume size is reported once and held the same way, without a
//! replug.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ue2_core::devices::usb::block::BlockBackend;

use crate::backend::CountingBackend;
use crate::sync::Report;
use crate::volume::{DirVolume, SyncError};
use crate::watch;

/// Quiet time after the last host event before the host is compared with the manifest.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

pub enum Request {
    /// Sync the snapshot image (then remove the snapshot).
    Sync { snapshot: PathBuf, force: bool },
    /// The stick is unplugged: sync the snapshot (unless `discard` or None), rebuild from the host, answer with the
    /// new medium. `discard` keeps the old image as `discarded-*.img` instead of syncing it.
    Replug { snapshot: Option<PathBuf>, force: bool, discard: bool },
}

pub enum Reply {
    Synced(Result<Report, SyncError>),
    Replugged(Replugged),
}

pub struct Replugged {
    /// The rebuilt medium and its image file; None keeps the old ones.
    pub backend: Option<(Box<dyn BlockBackend>, PathBuf)>,
    /// Why the old image was not synced; the volume was not rebuilt then.
    pub sync_error: Option<SyncError>,
    pub lines: Vec<String>,
    /// A rebuild that failed after a successful sync.
    pub error: Option<String>,
}

/// A host change the frontend should bring to the guest with a replug.
pub struct HostChanged(pub String);

enum Msg {
    Request(Request),
    HostEvent,
    Shutdown,
}

pub struct Worker {
    tx: Sender<Msg>,
    replies: Receiver<Reply>,
    notices: Receiver<HostChanged>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Run `volume` on a new thread; rebuilt media count writes into `writes`. With `watch_host`, a file watcher
    /// reports host changes (a watcher that cannot start is reported on stderr, and the stick then only follows the
    /// host on usb-replug).
    pub fn spawn(volume: DirVolume, writes: Arc<AtomicU64>, watch_host: bool) -> std::io::Result<Worker> {
        let (tx, rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        let (notice_tx, notices) = mpsc::channel();
        let events = tx.clone();
        let join = thread::Builder::new().name("ue2-usb-dir".into()).spawn(move || {
            let _watcher = watch_host
                .then(|| {
                    let events = std::sync::Mutex::new(events);
                    watch::watch(volume.root(), move || {
                        let _ = events.lock().map(|tx| tx.send(Msg::HostEvent));
                    })
                    .map_err(|e| eprintln!("usb-dir: cannot watch {}: {e}; host changes need usb-replug", volume.root().display()))
                    .ok()
                })
                .flatten();
            run(volume, writes, &rx, &reply_tx, &notice_tx);
        })?;
        Ok(Worker { tx, replies, notices, join: Some(join) })
    }

    pub fn send(&self, request: Request) {
        let _ = self.tx.send(Msg::Request(request));
    }

    pub fn try_reply(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }

    /// Wait for the next reply; None if the thread is gone.
    pub fn wait_reply(&self) -> Option<Reply> {
        self.replies.recv().ok()
    }

    pub fn try_notice(&self) -> Option<HostChanged> {
        self.notices.try_recv().ok()
    }

    /// Finish queued requests, then stop the thread and release the directory lock.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// When the worker announces host changes.
#[derive(Debug, Default)]
pub struct HostWatch {
    /// A change was announced and no sync or replug has happened since.
    announced: bool,
    /// The host tree's fingerprint when a replug failed or a change did not fit: nothing is announced while the
    /// host still has it.
    hold: Option<String>,
}

/// What to do at a host check.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// Nothing: already announced, or held with the host unchanged.
    Skip,
    /// Compare the host with the manifest.
    Compare,
}

impl HostWatch {
    /// A host check is due; `fingerprint` computes the host's fingerprint (only called while held).
    pub fn check(&mut self, fingerprint: impl FnOnce() -> Option<String>) -> Check {
        if self.announced {
            return Check::Skip;
        }
        if let Some(held) = &self.hold {
            if fingerprint().as_ref() == Some(held) {
                return Check::Skip;
            }
            self.hold = None;
        }
        Check::Compare
    }

    /// The comparison found a change to announce.
    pub fn announce(&mut self) {
        self.announced = true;
    }

    /// Hold at `fingerprint`: a replug failed, or the change cannot be brought to the guest.
    pub fn hold(&mut self, fingerprint: Option<String>) {
        self.announced = false;
        self.hold = fingerprint;
    }

    /// A sync succeeded, or a replug rebuilt the volume: a difference left is a new change.
    pub fn reset(&mut self) {
        self.announced = false;
        self.hold = None;
    }

    pub fn held(&self) -> bool {
        self.hold.is_some()
    }
}

fn run(mut volume: DirVolume, writes: Arc<AtomicU64>, rx: &Receiver<Msg>, replies: &Sender<Reply>, notices: &Sender<HostChanged>) {
    let mut check_at: Option<Instant> = None;
    let mut watch = HostWatch::default();
    loop {
        let msg = match check_at {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Msg::HostEvent) => check_at = Some(Instant::now() + DEBOUNCE),
            Ok(Msg::Request(Request::Sync { snapshot, force })) => {
                let result = volume.sync(&snapshot, force);
                let _ = std::fs::remove_file(&snapshot);
                if result.is_ok() {
                    watch.reset();
                    check_at = Some(Instant::now() + DEBOUNCE);
                }
                let _ = replies.send(Reply::Synced(result));
            }
            Ok(Msg::Request(Request::Replug { snapshot, force, discard })) => {
                let replugged = replug(&mut volume, &writes, snapshot, force, discard);
                if replugged.backend.is_some() {
                    watch.reset();
                    check_at = Some(Instant::now() + DEBOUNCE);
                } else {
                    // Its own writes (a partial sync) are part of the fingerprint, so they announce nothing.
                    watch.hold(volume.host_fingerprint().ok());
                    check_at = None;
                }
                let _ = replies.send(Reply::Replugged(replugged));
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                check_at = None;
                if watch.check(|| volume.host_fingerprint().ok()) == Check::Skip {
                    continue;
                }
                match volume.host_changed() {
                    Ok(Some(reason)) => match volume.rebuild_problem() {
                        Some(problem) => {
                            eprintln!(
                                "usb-dir: {}: {reason}, but the host changes cannot reach the guest: {problem}",
                                volume.root().display()
                            );
                            watch.hold(volume.host_fingerprint().ok());
                        }
                        None => {
                            watch.announce();
                            let _ = notices.send(HostChanged(reason));
                        }
                    },
                    Ok(None) => {}
                    Err(e) => eprintln!("usb-dir: scanning {}: {e}", volume.root().display()),
                }
            }
        }
    }
}

fn replug(volume: &mut DirVolume, writes: &Arc<AtomicU64>, snapshot: Option<PathBuf>, force: bool, discard: bool) -> Replugged {
    let mut lines = Vec::new();
    if discard {
        match volume.discard_image() {
            Ok(Some(kept)) => lines.push(format!("guest changes not synced; the old image is kept as {}", kept.display())),
            Ok(None) => {}
            Err(e) => {
                let error = Some(format!("keeping the old image failed: {e}; nothing changed"));
                return Replugged { backend: None, sync_error: None, lines, error };
            }
        }
    } else if let Some(snapshot) = &snapshot {
        let result = volume.sync(snapshot, force);
        let _ = std::fs::remove_file(snapshot);
        match result {
            Ok(report) => {
                lines.extend(report.lines.iter().cloned());
                lines.push(report.summary());
            }
            Err(e) => {
                if let SyncError::Incomplete(report) = &e {
                    lines.extend(report.lines.iter().cloned());
                    lines.push(report.summary());
                }
                return Replugged { backend: None, sync_error: Some(e), lines, error: None };
            }
        }
    }
    let old = volume.image_path();
    match volume.build() {
        Ok(prepared) => match CountingBackend::open(&prepared.image, volume.read_only(), writes.clone()) {
            Ok(backend) => {
                lines.extend(prepared.lines);
                if let Some(old) = old.filter(|_| !discard) {
                    volume.remove_old_image(&old);
                }
                Replugged { backend: Some((Box::new(backend), prepared.image)), sync_error: None, lines, error: None }
            }
            Err(e) => Replugged { backend: None, sync_error: None, lines, error: Some(format!("opening the new image: {e}")) },
        },
        Err(e) => Replugged { backend: None, sync_error: None, lines, error: Some(format!("rebuilding the volume failed: {e:#}")) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_replug_holds_until_the_host_changes() {
        let mut w = HostWatch::default();
        assert_eq!(w.check(|| unreachable!("not held")), Check::Compare);
        w.announce();
        assert_eq!(w.check(|| unreachable!()), Check::Skip, "announced once");

        // The replug failed: the same host announces nothing, however many events its own writes cause.
        w.hold(Some("fp1".into()));
        for _ in 0..3 {
            assert_eq!(w.check(|| Some("fp1".into())), Check::Skip);
        }
        assert!(w.held());
        // A real host change compares again (and may announce one more replug).
        assert_eq!(w.check(|| Some("fp2".into())), Check::Compare);
        assert!(!w.held());
        w.announce();
        w.hold(Some("fp2".into()));
        // A successful sync lifts the hold.
        w.reset();
        assert_eq!(w.check(|| unreachable!("not held")), Check::Compare);
        // A fingerprint that cannot be taken holds nothing.
        w.hold(None);
        assert_eq!(w.check(|| unreachable!("not held")), Check::Compare);
    }
}
