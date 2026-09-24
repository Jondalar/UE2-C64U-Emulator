//! `--usb-dir PATH[,size=SIZE][,ro]`: host directories as USB sticks (docs/status/usb-dir.md), driven from the
//! emulation thread.
//!
//! Each directory gets a volume ([`ue2_vfat::DirVolume`], built or resumed before the machine runs), a hub port
//! after the `--usb` images, and a [`Worker`] thread for syncs and rebuilds. This thread only watches the guest's
//! write count, copies the image to a snapshot between run slices (so no block write is half done) and swaps media.
//!
//! - **Automatic sync:** when the guest has written since the last sync and then stayed quiet for [`QUIET_MS`]
//!   emulated and [`QUIET_WALL`] wall clock. A sync the guard refused stops automatic syncs until an explicit
//!   `usb-sync`; one that did not parse or could not write everything is retried only after new guest writes.
//! - **Unsynced marker:** created on this thread right after the first guest write of a dirty period, removed only
//!   when a sync wrote everything; a run that dies in between resumes the image next time.
//! - **Host changes:** the worker announces them; once the guest is quiet the stick is unplugged, synced, rebuilt
//!   from the host and plugged back in. A refused, failed or incomplete sync keeps the old image and skips the
//!   rebuild, and the worker announces nothing more until the host changes again. While the image at its current
//!   write count does not parse, no automatic replug is tried at all.
//! - **Control:** `usb-sync [--force] [port]`, `usb-replug [--discard] [port]` ([`UsbDirs::request`]); both wait
//!   until the guest has not written for [`QUIET_MS`] emulated, at most [`EXPLICIT_WAIT_MS`].
//! - **Quit:** the runner keeps the machine running while a guest write is recent ([`UsbDirs::writing`]), then a
//!   clean stop runs a last sync; after a firmware halt nothing is synced and the image is kept for the next run to
//!   resume.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ue2_core::host::HostInput;
use ue2_core::machine::Machine;
use ue2_vfat::backend::CountingBackend;
use ue2_vfat::volume::{self, SyncError};
use ue2_vfat::worker::{HostChanged, Reply, Request, Worker};
use ue2_vfat::{DirSpec, DirVolume};

/// Guest quiet time before an automatic sync or replug, emulated.
pub const QUIET_MS: u64 = 2000;
/// The same, wall clock: at `--speed max` 2 s emulated pass much faster.
pub const QUIET_WALL: Duration = Duration::from_secs(2);
/// Longest wait of `usb-sync` / `usb-replug`, and of a quit, for a guest that keeps writing (emulated ms). After it
/// the stick is synced as it is, with a warning.
pub const EXPLICIT_WAIT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbAction {
    /// Write the guest's changes back; `force` overrides the mass-deletion guard.
    Sync { force: bool },
    /// Unplug, sync, rebuild from the host, plug in. `discard` keeps the old image aside instead of syncing it.
    /// For a port without `--usb-dir` just unplug and plug in.
    Replug { discard: bool },
}

/// A `usb-sync` / `usb-replug` control command; `port` None means every `--usb-dir` stick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbRequest {
    pub action: UsbAction,
    pub port: Option<u8>,
}

/// Result lines, and an error when the request (or part of it) failed.
pub type UsbDone = Sender<(Vec<String>, Option<String>)>;

enum Job {
    Idle,
    /// A sync of the snapshot taken at write count `at`.
    Sync { at: u64, explicit: bool },
    /// Unplugged; the worker syncs (if `synced_at` is Some) and rebuilds.
    Rebuild { synced_at: Option<u64>, explicit: bool },
    /// Plugged back in; waits until the hub port is connected.
    Plugging { explicit: bool, lines: Vec<String>, error: Option<String> },
}

/// An explicit request's part on one port, done.
struct Finished {
    lines: Vec<String>,
    error: Option<String>,
}

struct DirPort {
    port: usize,
    root: PathBuf,
    read_only: bool,
    image: PathBuf,
    snapshot: PathBuf,
    /// The `unsynced` marker in the work directory; only this thread creates and removes it.
    marker: PathBuf,
    worker: Worker,
    writes: Arc<AtomicU64>,
    /// Write count at the last change seen, with its emulated ms and wall time.
    seen: u64,
    last_write: Option<(u64, Instant)>,
    /// Write count the current image had at its last successful sync or build; `u64::MAX` for a resumed image.
    synced: u64,
    /// Write count of a sync that failed: no automatic retry before the guest writes again.
    failed: Option<u64>,
    /// That failure was a parse error: a replug cannot sync this image either, so none is started automatically.
    failed_parse: bool,
    /// A host change waiting for such an image was reported.
    waiting_reported: bool,
    /// The guard refused a sync: no automatic sync or replug until an explicit usb-sync succeeds.
    blocked: bool,
    /// The `unsynced` marker exists.
    marked: bool,
    host_changed: Option<String>,
    job: Job,
}

impl DirPort {
    fn tag(&self) -> String {
        format!("usb-dir port {} ({})", self.port, self.root.display())
    }

    fn dirty(&self) -> bool {
        !self.read_only && self.writes.load(Ordering::Relaxed) != self.synced
    }

    fn quiet(&self, now_ms: u64, wall: Instant) -> bool {
        self.last_write.is_none_or(|(ms, at)| now_ms >= ms + QUIET_MS && wall >= at + QUIET_WALL)
    }

    /// No guest write for [`QUIET_MS`] emulated: the guest has finished what it was writing.
    fn quiet_emulated(&self, now_ms: u64) -> bool {
        self.last_write.is_none_or(|(ms, _)| now_ms >= ms + QUIET_MS)
    }

    /// Copy the image to the snapshot file. Called between run slices, so every block write is complete.
    fn take_snapshot(&self) -> std::io::Result<PathBuf> {
        let _ = std::fs::remove_file(&self.snapshot);
        std::fs::copy(&self.image, &self.snapshot)?;
        Ok(self.snapshot.clone())
    }

    fn start_sync(&mut self, force: bool, explicit: bool) -> Result<(), String> {
        let at = self.writes.load(Ordering::Relaxed);
        let snapshot = self.take_snapshot().map_err(|e| format!("copying {} for the sync: {e}", self.image.display()))?;
        self.worker.send(Request::Sync { snapshot, force });
        self.job = Job::Sync { at, explicit };
        Ok(())
    }

    fn start_replug(&mut self, machine: &mut Machine, force: bool, discard: bool, explicit: bool) -> Result<(), String> {
        machine.input(HostInput::UsbPlug { port: self.port as u8, connected: false });
        let at = self.writes.load(Ordering::Relaxed);
        let sync = self.dirty() && !discard;
        let snapshot = if sync {
            match self.take_snapshot() {
                Ok(snapshot) => Some(snapshot),
                Err(e) => {
                    machine.input(HostInput::UsbPlug { port: self.port as u8, connected: true });
                    return Err(format!("copying {} for the sync: {e}; nothing changed", self.image.display()));
                }
            }
        } else {
            None
        };
        self.worker.send(Request::Replug { snapshot, force, discard });
        self.host_changed = None;
        self.job = Job::Rebuild { synced_at: sync.then_some(at), explicit };
        Ok(())
    }

    fn mark(&mut self, unsynced: bool) {
        if self.marked != unsynced {
            match volume::set_marker(&self.marker, unsynced) {
                Ok(()) => self.marked = unsynced,
                Err(e) => eprintln!("{}: the unsynced marker {}: {e}", self.tag(), self.marker.display()),
            }
        }
    }

    /// Handle a worker reply; returns the finished part of an explicit request.
    fn on_reply(&mut self, machine: &mut Machine, reply: Reply) -> Option<Finished> {
        let tag = self.tag();
        match (std::mem::replace(&mut self.job, Job::Idle), reply) {
            (Job::Sync { at, explicit }, Reply::Synced(result)) => {
                let (lines, error) = match result {
                    Ok(report) => {
                        self.synced = at;
                        self.failed = None;
                        self.failed_parse = false;
                        self.blocked = false;
                        if self.writes.load(Ordering::Relaxed) == at {
                            self.mark(false);
                        }
                        let mut lines = report.lines.clone();
                        if report.lines != ["no guest changes"] {
                            lines.push(report.summary());
                            if !explicit {
                                for line in &lines {
                                    eprintln!("{tag}: {line}");
                                }
                            }
                        }
                        (lines, None)
                    }
                    Err(e) => {
                        self.note_sync_error(&e, at);
                        let mut lines = Vec::new();
                        if let SyncError::Incomplete(report) = &e {
                            lines.clone_from(&report.lines);
                            lines.push(report.summary());
                            // Loud whether explicit or not: guest data exists only in the image.
                            for line in &lines {
                                eprintln!("{tag}: {line}");
                            }
                        }
                        eprintln!("{tag}: {e}");
                        (lines, Some(e.to_string()))
                    }
                };
                explicit.then_some(Finished { lines, error })
            }
            (Job::Rebuild { synced_at, explicit }, Reply::Replugged(r)) => {
                let mut error = r.error;
                if let Some(e) = &r.sync_error {
                    self.note_sync_error(e, synced_at.unwrap_or(u64::MAX));
                    error = Some(e.to_string());
                } else if let Some(at) = synced_at {
                    self.synced = at;
                    self.failed = None;
                    self.failed_parse = false;
                    self.blocked = false;
                }
                if let Some((backend, image)) = r.backend {
                    match machine.usb_replace_backend(self.port, backend) {
                        Ok(_old) => {
                            self.image = image;
                            self.synced = self.writes.load(Ordering::Relaxed);
                            self.seen = self.synced;
                            self.mark(false);
                        }
                        Err(e) => error = Some(format!("swapping the medium: {e}")),
                    }
                } else if !self.dirty() {
                    self.mark(false);
                }
                let incomplete = matches!(r.sync_error, Some(SyncError::Incomplete(_)));
                if !explicit || error.is_some() || incomplete {
                    for line in &r.lines {
                        eprintln!("{tag}: {line}");
                    }
                    if let Some(e) = &error {
                        eprintln!("{tag}: {e}; the stick is plugged back in with its old image");
                    }
                }
                machine.input(HostInput::UsbPlug { port: self.port as u8, connected: true });
                self.job = Job::Plugging { explicit, lines: r.lines, error };
                None
            }
            (job, _) => {
                self.job = job;
                eprintln!("{tag}: unexpected worker reply");
                None
            }
        }
    }

    fn note_sync_error(&mut self, e: &SyncError, at: u64) {
        match e {
            SyncError::Guard { .. } => self.blocked = true,
            SyncError::Parse(_) | SyncError::Io(_) | SyncError::Incomplete(_) => {
                self.failed = Some(at);
                self.failed_parse = matches!(e, SyncError::Parse(_));
                self.waiting_reported = false;
            }
        }
    }
}

/// An explicit request: its remaining ports, collected lines and errors.
struct Active {
    action: UsbAction,
    ports: VecDeque<usize>,
    lines: Vec<String>,
    errors: Vec<String>,
    done: UsbDone,
    /// A port without `--usb-dir` being replugged.
    plain: bool,
    started: bool,
    /// Emulated ms when the request was queued: the quiet wait ends [`EXPLICIT_WAIT_MS`] later.
    since_ms: u64,
}

/// Every `--usb-dir` stick of a machine, plus replugs of the other hub ports.
#[derive(Default)]
pub struct UsbDirs {
    ports: Vec<DirPort>,
    queue: VecDeque<Active>,
    /// The emulator is stopping: no new automatic syncs or replugs.
    stopping: bool,
}

impl UsbDirs {
    /// Open, build (or resume) and attach every `--usb-dir` on hub ports `first_port`.. in order. Runs on the
    /// emulation thread before the machine starts.
    pub fn attach(machine: &mut Machine, specs: &[DirSpec], work: &Path, first_port: usize) -> Result<UsbDirs> {
        let mut dirs = UsbDirs::default();
        for (i, spec) in specs.iter().enumerate() {
            let port = first_port + i;
            let mut volume = DirVolume::open(spec, work)?;
            let prepared = volume.prepare().with_context(|| format!("--usb-dir {}", spec.path.display()))?;
            let root = volume.root().to_path_buf();
            for line in &prepared.lines {
                eprintln!("usb-dir: {}: {line}", root.display());
            }
            let writes = Arc::new(AtomicU64::new(0));
            let backend = CountingBackend::open(&prepared.image, spec.read_only, writes.clone())
                .with_context(|| format!("opening {}", prepared.image.display()))?;
            machine.usb_attach_storage(port, Box::new(backend)).map_err(|e| anyhow!("--usb-dir {}: {e}", root.display()))?;
            eprintln!(
                "usb-dir: {} is USB{} on hub port {port}{}; image {}",
                root.display(),
                port - 1,
                if spec.read_only { ", read-only" } else { "" },
                prepared.image.display()
            );
            let snapshot = volume.snapshot_path();
            let marker = volume.marker_path();
            let worker = Worker::spawn(volume, writes.clone(), true).context("starting the usb-dir worker")?;
            dirs.ports.push(DirPort {
                port,
                root,
                read_only: spec.read_only,
                image: prepared.image,
                snapshot,
                marker,
                worker,
                writes,
                seen: 0,
                last_write: None,
                synced: if prepared.resumed { u64::MAX } else { 0 },
                failed: None,
                failed_parse: false,
                waiting_reported: false,
                blocked: false,
                marked: prepared.resumed,
                host_changed: None,
                job: Job::Idle,
            });
        }
        Ok(dirs)
    }

    /// Once per run slice: track guest writes, collect worker replies and host changes, finish replugs, run
    /// explicit requests, then automatic syncs and replugs.
    pub fn poll(&mut self, machine: &mut Machine) {
        if self.ports.is_empty() && self.queue.is_empty() {
            return;
        }
        let (now_ms, wall) = (machine.now_ms(), Instant::now());
        let mut finished = Vec::new();
        for p in &mut self.ports {
            let writes = p.writes.load(Ordering::Relaxed);
            if writes != p.seen {
                p.seen = writes;
                p.last_write = Some((now_ms, wall));
            }
            if p.dirty() {
                p.mark(true);
            }
            while let Some(HostChanged(reason)) = p.worker.try_notice() {
                eprintln!("{}: {reason}", p.tag());
                p.host_changed = Some(reason);
            }
            if let Some(reply) = p.worker.try_reply() {
                if let Some(f) = p.on_reply(machine, reply) {
                    finished.push((p.port, f));
                }
            }
            if let Job::Plugging { .. } = p.job {
                if machine.usb_port(p.port).is_some_and(|info| info.connected) {
                    if let Job::Plugging { explicit: true, lines, error } = std::mem::replace(&mut p.job, Job::Idle) {
                        finished.push((p.port, Finished { lines, error }));
                    }
                }
            }
        }
        for (port, f) in finished {
            self.finish_part(port, f);
        }
        self.pump(machine);

        if self.stopping {
            return;
        }
        let wanted = self.queue.front().and_then(|a| a.ports.front().copied());
        for p in &mut self.ports {
            if !matches!(p.job, Job::Idle) || wanted == Some(p.port) || !p.quiet(now_ms, wall) {
                continue;
            }
            let writes = p.writes.load(Ordering::Relaxed);
            // This image at this write count did not parse: a replug's sync would fail the same way.
            let unparsable = p.failed_parse && p.failed == Some(writes);
            if p.host_changed.is_some() && unparsable {
                if !p.waiting_reported {
                    p.waiting_reported = true;
                    eprintln!(
                        "{}: host changes wait: the image does not parse cleanly and holds unsynced guest writes; fix it in \
                         the guest, or run usb-replug --discard to keep it aside and rebuild from the host",
                        p.tag()
                    );
                }
            } else if p.host_changed.is_some() && !p.blocked {
                eprintln!("{}: bringing host changes to the guest (unplug, sync, rebuild, plug in)", p.tag());
                if let Err(e) = p.start_replug(machine, false, false, false) {
                    eprintln!("{}: {e}", p.tag());
                    p.host_changed = None;
                }
            } else if p.dirty() && !p.blocked && p.failed != Some(writes) {
                if let Err(e) = p.start_sync(false, false) {
                    eprintln!("{}: {e}", p.tag());
                    p.failed = Some(writes);
                }
            }
        }
    }

    /// Queue a `usb-sync` / `usb-replug`; `done` gets the result when it has finished.
    /// Whether hub port `port` holds one of the `--usb-dir` sticks.
    pub fn owns(&self, port: usize) -> bool {
        self.ports.iter().any(|p| p.port == port)
    }

    pub fn request(&mut self, machine: &mut Machine, req: UsbRequest, done: UsbDone) {
        let dir_ports: Vec<usize> = self.ports.iter().map(|p| p.port).collect();
        let (ports, plain) = match req.port.map(usize::from) {
            Some(port) if dir_ports.contains(&port) => (vec![port], false),
            Some(port) => match (req.action, machine.usb_port(port).and_then(|info| info.device)) {
                (UsbAction::Replug { discard: false }, Some(_)) => (vec![port], true),
                (UsbAction::Replug { discard: false }, None) => {
                    let _ = done.send((Vec::new(), Some(format!("hub port {port} is empty"))));
                    return;
                }
                _ => {
                    let _ = done.send((Vec::new(), Some(format!("hub port {port} is not a --usb-dir stick"))));
                    return;
                }
            },
            None if dir_ports.is_empty() => {
                let _ = done.send((Vec::new(), Some("no --usb-dir stick attached".into())));
                return;
            }
            None => (dir_ports, false),
        };
        self.queue.push_back(Active {
            action: req.action,
            ports: ports.into(),
            lines: Vec::new(),
            errors: Vec::new(),
            done,
            plain,
            started: false,
            since_ms: machine.now_ms(),
        });
        self.pump(machine);
    }

    /// True while a stick with unsynced writes had a guest write less than [`QUIET_MS`] ago (emulated): a quit then
    /// keeps the machine running, so the guest can finish writing before the last sync. Also stops automatic syncs
    /// and replugs.
    pub fn writing(&mut self, now_ms: u64) -> bool {
        self.stopping = true;
        self.ports.iter().any(|p| p.dirty() && !p.quiet_emulated(now_ms))
    }

    /// Start the head request's next port when that port is idle; answer requests with no ports left.
    fn pump(&mut self, machine: &mut Machine) {
        while let Some(active) = self.queue.front_mut() {
            let Some(&port) = active.ports.front() else {
                let active = self.queue.pop_front().unwrap();
                let error = (!active.errors.is_empty()).then(|| active.errors.join("; "));
                let _ = active.done.send((active.lines, error));
                continue;
            };
            if active.started {
                if active.plain {
                    let connected = machine.usb_port(port).is_some_and(|info| info.connected && !info.plug_pending);
                    if connected {
                        active.lines.push(format!("port {port}: plugged back in"));
                        active.ports.pop_front();
                        active.started = false;
                        continue;
                    }
                }
                return;
            }
            if active.plain {
                machine.input(HostInput::UsbPlug { port: port as u8, connected: false });
                machine.input(HostInput::UsbPlug { port: port as u8, connected: true });
                active.started = true;
                return;
            }
            let Some(p) = self.ports.iter_mut().find(|p| p.port == port) else { return };
            if !matches!(p.job, Job::Idle) {
                return;
            }
            // Wait until the guest has finished writing, so a half-written file is not synced.
            let now_ms = machine.now_ms();
            if !p.read_only && !p.quiet_emulated(now_ms) {
                if now_ms < active.since_ms + EXPLICIT_WAIT_MS {
                    return;
                }
                active.lines.push(format!(
                    "port {port}: WARNING: the guest was still writing after {} s; the stick is taken as it is now, and a \
                     file it was writing may be incomplete (a replaced host version is in .ue2-trash)",
                    EXPLICIT_WAIT_MS / 1000
                ));
            }
            let started = match active.action {
                UsbAction::Sync { .. } if p.read_only => Err(Some("read-only stick: nothing to sync".to_string())),
                UsbAction::Sync { force } => p.start_sync(force, true).map_err(Some),
                // The guard applies to a replug's sync too; only usb-sync --force overrides it.
                UsbAction::Replug { discard } => p.start_replug(machine, false, discard, true).map_err(Some),
            };
            match started {
                Ok(()) => {
                    active.started = true;
                    return;
                }
                Err(message) => {
                    let message = message.unwrap_or_default();
                    if p.read_only && matches!(active.action, UsbAction::Sync { .. }) {
                        active.lines.push(format!("port {port}: {message}"));
                    } else {
                        active.errors.push(format!("port {port}: {message}"));
                    }
                    active.ports.pop_front();
                }
            }
        }
    }

    fn finish_part(&mut self, port: usize, f: Finished) {
        let Some(active) = self.queue.front_mut().filter(|a| a.started && a.ports.front() == Some(&port)) else { return };
        active.lines.extend(f.lines.into_iter().map(|l| format!("port {port}: {l}")));
        if let Some(e) = f.error {
            active.errors.push(format!("port {port}: {e}"));
        }
        active.ports.pop_front();
        active.started = false;
    }

    /// The emulator stops. `clean` (quit): wait for running jobs, then a last sync of every dirty stick. After a
    /// halt nothing is synced. Queued requests are answered with an error; the workers stop and release their locks.
    pub fn finish(self, machine: &mut Machine, clean: bool) {
        let UsbDirs { ports, queue, .. } = self;
        let now_ms = machine.now_ms();
        for active in queue {
            let _ = active.done.send((active.lines, Some("emulator stopped".into())));
        }
        for mut p in ports {
            while matches!(p.job, Job::Sync { .. } | Job::Rebuild { .. }) {
                match p.worker.wait_reply() {
                    Some(reply) => {
                        p.on_reply(machine, reply);
                    }
                    None => break,
                }
            }
            if clean && p.dirty() {
                eprintln!("{}: syncing guest changes before exit", p.tag());
                if !p.quiet_emulated(now_ms) {
                    eprintln!(
                        "{}: WARNING: the guest was still writing when the emulator stopped; a file it was writing may be \
                         incomplete on the host (a replaced host version is in .ue2-trash)",
                        p.tag()
                    );
                }
                match p.start_sync(false, true) {
                    Ok(()) => match p.worker.wait_reply() {
                        Some(reply) => {
                            if let Some(f) = p.on_reply(machine, reply) {
                                for line in &f.lines {
                                    eprintln!("{}: {line}", p.tag());
                                }
                            }
                        }
                        None => eprintln!("{}: the usb-dir worker is gone", p.tag()),
                    },
                    Err(e) => eprintln!("{}: {e}", p.tag()),
                }
            }
            if p.dirty() {
                eprintln!(
                    "{}: guest changes are NOT synced{}; the image {} is kept and the next run with this --usb-dir \
                     resumes and syncs it",
                    p.tag(),
                    if clean { "" } else { " (the emulator did not stop cleanly)" },
                    p.image.display()
                );
            }
            p.worker.shutdown();
        }
    }
}
