//! S38: the time-travel verbs on TRX64's rings, and the monitor's files (docs/specs/S38-trx64-ring.md §5-§6).
//!
//! `whowrote`, `triage` and `revdepth` are the library's and read the machine's rings themselves. The verbs here are
//! the TRX64 daemon's own arms (`rstep`, `chis`, the transport, the marks, `cadence`/`window`, `ringdump`/`ringload`,
//! `asm-file` and the file shell), written against the checkpoint ring the bridge keeps. Their words and output
//! follow the daemon's, so a client reads both the same.

use std::path::{Component, Path, PathBuf};

use c64_bridge::{RingMode, Trx64Backend};
use trx64_core::checkpoint_ring::RuntimeCheckpointRing;
use trx64_monitor::addr_spans::{self, Role, Space};
use trx64_monitor::host::Files;
use trx64_monitor::{verbs, MonitorHost, MonitorSession};
use ue2_core::c64host::C64Backend;
use ue2_core::devices::c64::C64Port;

use super::{trx64, Host};

/// The directory every monitor file verb is confined to (`--monitor-dir`), and the working directory inside it.
///
/// A path names a file under the root: relative to the working directory, or from the root with a leading `/`. Host
/// paths outside the root are refused, `..` and symbolic links included.
pub struct MonitorFiles {
    root: PathBuf,
    /// Relative to `root`.
    cwd: PathBuf,
}

impl Default for MonitorFiles {
    /// The directory ue2emu was started in.
    fn default() -> Self {
        MonitorFiles::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl MonitorFiles {
    pub fn new(root: PathBuf) -> MonitorFiles {
        MonitorFiles { root, cwd: PathBuf::new() }
    }

    /// `path` under the root, or why not. The path need not exist; whatever part of it does must stay inside the root
    /// once symbolic links are followed.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let mut rel = if path.starts_with('/') { PathBuf::new() } else { self.cwd.clone() };
        for c in Path::new(path).components() {
            match c {
                Component::Normal(name) => rel.push(name),
                Component::ParentDir => {
                    if !rel.pop() {
                        return Err(format!("{path}: outside the monitor directory"));
                    }
                }
                Component::CurDir | Component::RootDir => {}
                Component::Prefix(_) => return Err(format!("{path}: outside the monitor directory")),
            }
        }
        let full = self.root.join(&rel);
        let root = self.root.canonicalize().map_err(|e| format!("{}: {e}", self.root.display()))?;
        // The deepest part that exists, with its links followed, must still be inside the root.
        let mut probe = full.as_path();
        loop {
            if let Ok(real) = probe.canonicalize() {
                if !real.starts_with(&root) {
                    return Err(format!("{path}: outside the monitor directory"));
                }
                break;
            }
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            }
        }
        Ok(full)
    }
}

impl Files for MonitorFiles {
    fn cwd(&self) -> String {
        format!("/{}", self.cwd.display())
    }

    fn set_cwd(&mut self, path: &str) -> Result<String, String> {
        let full = self.resolve(path)?;
        if !full.is_dir() {
            return Err(format!("cd: {path}: not a directory"));
        }
        self.cwd = full.strip_prefix(&self.root).map(Path::to_path_buf).unwrap_or_default();
        Ok(self.cwd())
    }

    fn list(&mut self, path: &str) -> Result<String, String> {
        let full = self.resolve(path)?;
        let mut names: Vec<String> = std::fs::read_dir(&full)
            .map_err(|e| format!("ls: {path}: {e}"))?
            .filter_map(Result::ok)
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                match e.metadata() {
                    Ok(m) if m.is_dir() => format!("  {name}/"),
                    Ok(m) => format!("  {name}  {}", m.len()),
                    Err(_) => format!("  {name}"),
                }
            })
            .collect();
        names.sort();
        Ok(names.join("\n"))
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.resolve(path)?).map_err(|e| format!("{path}: {e}"))
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let full = self.resolve(path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
        }
        std::fs::write(&full, bytes).map_err(|e| format!("{path}: {e}"))
    }

    /// The drives are the firmware's: it mounts through its own menu, REST API or command interface.
    fn mount(&mut self, _path: &str, _device: u8) -> Result<String, String> {
        Err("mount: the firmware owns the drives (its menu, REST API or command interface mounts)".into())
    }

    fn unmount(&mut self, _device: u8) -> Result<String, String> {
        Err("eject: the firmware owns the drives (its menu, REST API or command interface ejects)".into())
    }
}

/// One of the verbs here, or `None`.
pub(super) fn verb(host: &mut Host, session: &mut MonitorSession, verb: &str, args: &[&str]) -> Option<Result<String, String>> {
    Some(match verb {
        "rstep" | "reverse" => rstep(host, args),
        "chis" => chis(host, args),
        "mark" => mark(host, args),
        "marks" => marks(host),
        "unmark" => unmark(host, args),
        "goto" => goto(host, args),
        "frame" => frame(host, args),
        "rewind" => rewind(host),
        "cadence" => cadence(host, args),
        "window" => window(host, args),
        "ringdump" => ringdump(host, args),
        "ringload" => ringload(host, args),
        "run" => Ok(run(host)),
        "pwd" => Ok(host.state.files.cwd()),
        "cd" => host.state.files.set_cwd(args.first().copied().unwrap_or("/")),
        "ls" => host.state.files.list(args.first().copied().unwrap_or(".")),
        "asm-file" => asm_file(host, session, args),
        _ => return None,
    })
}

fn backend<'h>(host: &'h mut Host) -> &'h mut Trx64Backend {
    trx64(host.m).expect("a host has a C64")
}

/// The backend with guest DDR lent for the call, as the firmware's accesses lend it: a restore puts UE2's memories
/// (cartridge, REU, GeoRAM) back into it from the write log (S38 §4.1).
fn with_ddr<R>(host: &mut Host, f: impl FnOnce(&mut Trx64Backend) -> R) -> R {
    let bus = &mut host.m.bus;
    let b = bus
        .io
        .get_mut::<C64Port>()
        .and_then(|p| p.backend_mut())
        .and_then(|b| b.as_any_mut())
        .and_then(|a| a.downcast_mut::<Trx64Backend>())
        .expect("a host has a C64");
    b.lend_ddr(Some(&mut bus.ram[..]));
    let out = f(b);
    b.lend_ddr(None);
    out
}

/// What the last restore could not put back, one line each under the status line.
fn with_notes(host: &mut Host, mut line: String) -> String {
    if let Some(ring) = backend(host).ring() {
        for n in &ring.notes {
            line.push_str(&format!("\n  note: {n}"));
        }
    }
    line
}

/// The ring, or the sentence for its absence.
fn ring_of<'h>(host: &'h mut Host, verb: &str) -> Result<&'h mut c64_bridge::Ring, String> {
    backend(host).ring_mut().ok_or_else(|| format!("{verb}: the rings are off (--ring=off)"))
}

/// Let go of the anchor the transport stands on, if it stands on one: the intervention that cuts the future
/// (TRX64 Spec 808 decision 4). The C64 stays where it is, stopped by the monitor's halt, as the daemon's machine
/// stays paused until `run`. The number of anchors cut, or `None` when it stood on none.
pub(super) fn release(host: &mut Host) -> Option<u64> {
    if !backend(host).ring_holds() {
        return None;
    }
    let _ = super::run::set_halted(host, true);
    Some(backend(host).ring_release())
}

/// `run`: the C64 runs on from where the transport stands, released from the monitor's stop as well.
fn run(host: &mut Host) -> String {
    let cut = release(host);
    let _ = super::run::set_halted(host, false);
    match cut {
        Some(n) if n > 0 => format!("RUN \u{2014} free-running.  (-{n} anchors)"),
        _ => "RUN \u{2014} free-running.".into(),
    }
}

/// The daemon's status line (trx64-daemon `transport::status_line`).
fn status_line(host: &mut Host) -> String {
    let b = backend(host);
    let t = b.trx64().timing();
    let Some(ring) = b.ring() else { return "the rings are off (--ring=off)".into() };
    let ids = ring.ids();
    let mode = ring.mode;
    let Some((index, total)) = ring.position() else {
        return format!("{:<7} {}  (no anchors yet)", mode.as_str(), mode.glyph());
    };
    let when = if index + 1 == total {
        "now".to_string()
    } else {
        format!("-{:.1}s", t.seconds(ids[total - 1].2.saturating_sub(ids[index].2)))
    };
    let mut line = format!("{:<7} {}  frame {}/{}   {when}", mode.as_str(), mode.glyph(), index + 1, total);
    if mode == RingMode::Cut && ring.last_cut > 0 {
        line.push_str(&format!("  (-{} anchors)", ring.last_cut));
    }
    line
}

/// The buffer in numbers (trx64-daemon `transport_range_line`).
fn range_line(host: &mut Host) -> String {
    let b = backend(host);
    let t = b.trx64().timing();
    let live = b.trx64().c64_core.clk;
    let Some(ring) = b.ring() else { return String::new() };
    let ids = ring.ids();
    if ids.is_empty() {
        return "RINGBUFFER SIZE: empty (fills while the machine runs)".into();
    }
    let (first, last, n) = (ids[0].2, ids[ids.len() - 1].2, ids.len());
    let span = last.saturating_sub(first);
    let at = match (&ring.cursor, ring.position()) {
        (Some(_), Some((i, _))) => format!("Cycle {} Frame {}", ids[i].2, i + 1),
        _ => format!("Cycle {live} (live head)"),
    };
    let expect = t.cycles_per_frame * ring.cadence_frames.max(1);
    // An anchor lands on the first instruction boundary after the frame boundary, so neighbours differ by a few
    // cycles either way; a gap is a missing frame, more than a raster line late.
    let late = expect + u64::from(t.cycles_per_line);
    let (mut gaps, mut worst, mut worst_at) = (0usize, 0u64, 0u64);
    for w in ids.windows(2) {
        let d = w[1].2.saturating_sub(w[0].2);
        if d > late {
            gaps += 1;
            if d > worst {
                (worst, worst_at) = (d, w[0].2);
            }
        }
    }
    let continuity = if gaps == 0 {
        format!("continuous (every {expect} cyc)")
    } else {
        format!(
            "{gaps} GAP(S) \u{2014} worst {worst} cyc ({:.2}s) after Cycle {worst_at}, expected {expect}",
            t.seconds(worst)
        )
    };
    format!(
        "RINGBUFFER SIZE {n} (starts at Cycle {first} Frame 1 // ends at Cycle {last} Frame {n})\n           span {span} cyc = {:.2}s   \u{00b7}   cursor at {at}   \u{00b7}   machine now Cycle {live}\n           reaches {:.2}s back from now   \u{00b7}   {continuity}",
        t.seconds(span),
        t.seconds(live.saturating_sub(first)),
    )
}

/// Stand the C64 on anchor `index` of the list.
fn move_to(host: &mut Host, index: usize, mode: RingMode) -> Result<String, String> {
    let ids = ring_of(host, "transport")?.ids();
    if ids.is_empty() {
        return Err("transport: the checkpoint ring is empty \u{2014} nothing to rewind to (it fills while the C64 runs at 1 MHz)".into());
    }
    let id = ids[index.min(ids.len() - 1)].0.clone();
    with_ddr(host, |b| b.ring_goto(&id, mode)).map_err(|e| format!("transport: {e}"))?;
    let line = status_line(host);
    Ok(with_notes(host, line))
}

/// `goto <frame> | goto c<cycle> | goto @<cycle> | goto <mark>` (trx64-daemon `transport_goto`).
fn goto(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let [target] = args else { return Err("goto: usage: goto <frame> | goto c<cycle>".into()) };
    let ring = ring_of(host, "goto")?;
    let ids = ring.ids();
    if ids.is_empty() {
        return Err("transport: the checkpoint ring is empty".into());
    }
    let t = target.trim();
    let here = ring.position().map_or(0, |(i, _)| i);
    if let Some(id) = ring.anchors.mark_id(t) {
        if let Some(i) = ids.iter().position(|(x, _, _)| *x == id) {
            return move_to(host, i, if i < here { RingMode::StepBack } else { RingMode::StepFwd });
        }
    }
    let index = if let Some(cyc) = t.strip_prefix('c').or_else(|| t.strip_prefix('@')) {
        let want: u64 =
            cyc.trim().parse().map_err(|_| format!("goto: `{t}` is not a cycle (use c<number> or @<number>)"))?;
        ids.iter().rposition(|(_, _, c)| *c <= want).ok_or_else(|| {
            format!("goto: cycle {want} is older than the ring (oldest anchor is at cycle {})", ids[0].2)
        })?
    } else {
        let n: usize = t.parse().map_err(|_| format!("goto: `{t}` is neither a frame number nor c<cycle>"))?;
        if n == 0 || n > ids.len() {
            return Err(format!("goto: frame {n} is outside 1..={}", ids.len()));
        }
        n - 1
    };
    move_to(host, index, if index < here { RingMode::StepBack } else { RingMode::StepFwd })
}

/// `frame [-N|+N]`: the status line, or N anchors back or forward; the ends stop it.
fn frame(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let Some(arg) = args.first() else { return Ok(status_line(host)) };
    let delta: i64 = arg.trim().parse().map_err(|_| format!("frame: `{arg}`? usage: frame -N | frame +N"))?;
    let ring = ring_of(host, "frame")?;
    let Some((index, total)) = ring.position() else {
        return Err("transport: no anchors yet \u{2014} the ring fills while the C64 runs at 1 MHz (`rewind` shows the count)".into());
    };
    if delta > 0 && ring.cursor.is_none() {
        return Err("frame: at the head \u{2014} the C64 runs with the firmware here; `c64 step` steps it".into());
    }
    let want = index as i64 + delta;
    let next = want.clamp(0, total as i64 - 1) as usize;
    let mut line = move_to(host, next, if delta < 0 { RingMode::StepBack } else { RingMode::StepFwd })?;
    if next as i64 != want {
        line.push_str(&format!("   (END OF BUFFER)\n{}", range_line(host)));
    }
    Ok(line)
}

/// `rewind`: where the transport stands, the buffer in numbers, and the settings.
fn rewind(host: &mut Host) -> Result<String, String> {
    let ring = ring_of(host, "rewind")?;
    let (cap, cadence, window, empty, (pictures, picture_bytes)) = (
        ring.anchors.max_entries(),
        ring.cadence_frames.max(1),
        ring.window_seconds,
        ring.ids().is_empty(),
        ring.pictures(),
    );
    let mut lines = vec![status_line(host), range_line(host)];
    lines.push(format!("  cap {cap} anchors   cadence every {cadence} frame(s)   window {window:.1}s"));
    let (recs, bytes) = backend(host).ue2_log();
    lines.push(format!(
        "  UE2 write log: {recs} write(s), {:.1} MiB (cartridge, REU, GeoRAM, EEPROM)",
        bytes as f64 / (1024.0 * 1024.0)
    ));
    lines.push(format!(
        "  pictures: {pictures} distinct, {:.1} MiB packed",
        picture_bytes as f64 / (1024.0 * 1024.0)
    ));
    if empty {
        lines.push("  nothing captured yet \u{2014} the ring fills while the C64 runs at 1 MHz".into());
    }
    lines.push("  frame -N|+N \u{00b7} goto <frame|c<cycle>|mark> \u{00b7} run".into());
    Ok(lines.join("\n"))
}

/// `mark <name>`: name and pin the anchor the transport stands on, the newest at the head.
fn mark(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let [name] = args else { return Err("mark: usage: mark <name>  (names the anchor you are standing on)".into()) };
    let ring = ring_of(host, "mark")?;
    let ids = ring.ids();
    let (index, _) =
        ring.position().ok_or_else(|| "mark: no anchors yet \u{2014} the ring fills while the machine runs".to_string())?;
    let r = ring.anchors.set_mark(&ids[index].0, name).map_err(|e| format!("mark: {e}"))?;
    let used = ring.anchors.marks().len();
    Ok(format!(
        "MARK {} @ Cycle {} Frame {}   \u{00b7}   {used} of {} marks",
        r.label.as_deref().unwrap_or(name),
        r.cycles,
        index + 1,
        RuntimeCheckpointRing::MAX_MARKS
    ))
}

fn marks(host: &mut Host) -> Result<String, String> {
    let b = backend(host);
    let (t, live) = (b.trx64().timing(), b.trx64().c64_core.clk);
    let ring = b.ring().ok_or("marks: the rings are off (--ring=off)")?;
    let marks = ring.anchors.marks();
    if marks.is_empty() {
        return Ok(format!(
            "no marks (0 of {}) \u{2014} `mark <name>` names the anchor you are on",
            RuntimeCheckpointRing::MAX_MARKS
        ));
    }
    let ids = ring.ids();
    let mut lines: Vec<String> = marks
        .iter()
        .map(|m| {
            let idx = ids.iter().position(|(i, _, _)| *i == m.id);
            format!(
                "  {:<16} Cycle {:>12}  Frame {:>5}   -{:.2}s",
                m.label.as_deref().unwrap_or("?"),
                m.cycles,
                idx.map_or(0, |i| i + 1),
                t.seconds(live.saturating_sub(m.cycles))
            )
        })
        .collect();
    let cap = ring.anchors.max_entries().max(1) as f64;
    let secs = ring.window_seconds;
    let lost = marks.len() as f64 / cap * secs;
    lines.push(format!(
        "  {} of {} marks \u{00b7} window {:.1}s of {:.1}s",
        marks.len(),
        RuntimeCheckpointRing::MAX_MARKS,
        secs - lost,
        secs
    ));
    Ok(lines.join("\n"))
}

fn unmark(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let [name] = args else { return Err("unmark: usage: unmark <name>".into()) };
    match ring_of(host, "unmark")?.anchors.drop_mark(name) {
        Some(r) => Ok(format!("UNMARK {name} (was Cycle {}) \u{2014} the anchor is no longer pinned", r.cycles)),
        None => Err(format!("unmark: no mark named `{name}`")),
    }
}

/// `cadence [frames] [seconds]`: the capture rate and the window, set together (TRX64 Spec 807 §4.6).
fn cadence(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let b = backend(host);
    let fps = b.trx64().timing().nominal_fps;
    let ring = b.ring_mut().ok_or("cadence: the rings are off (--ring=off)")?;
    let num = |t: Option<&&str>| t.and_then(|x| x.trim().trim_start_matches('$').parse::<f64>().ok());
    let Some(f) = num(args.first()) else {
        let (cad, secs) = (ring.cadence_frames.max(1), ring.window_seconds);
        return Ok(format!(
            "cadence: every {cad} frame(s) = {:.1} capture(s)/s, window {secs:.1}s \u{2192} cap {} entries ({} held)",
            f64::from(fps) / cad as f64,
            ring.anchors.max_entries(),
            ring.anchors.list().len()
        ));
    };
    let frames = f.clamp(1.0, 3000.0) as u64;
    let secs = num(args.get(1)).filter(|v| v.is_finite() && *v > 0.0).map_or(ring.window_seconds, |v| v.min(600.0));
    ring.cadence_frames = frames;
    ring.window_seconds = secs;
    let cap = ring.retune(fps);
    Ok(format!(
        "cadence: every {frames} frame(s) = {:.1} capture(s)/s, window {secs:.1}s \u{2192} cap {cap} entries ({} held)\n  ~{:.0} MiB when full (~98 KiB/entry, measured)   budget now {} MiB",
        f64::from(fps) / frames as f64,
        ring.anchors.list().len(),
        cap as f64 * 98.0 / 1024.0,
        ring.anchors.budget_bytes() / (1024 * 1024)
    ))
}

/// `window [seconds]`: how far back the ring reaches; keeps the cadence.
fn window(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let secs = args.first().and_then(|t| t.trim().trim_end_matches('s').parse::<f64>().ok());
    let b = backend(host);
    let fps = b.trx64().timing().nominal_fps;
    let ring = b.ring_mut().ok_or("window: the rings are off (--ring=off)")?;
    match secs {
        None => Ok(format!(
            "window: {:.1}s at every-{}-frames \u{2192} cap {} anchors",
            ring.window_seconds,
            ring.cadence_frames.max(1),
            ring.anchors.max_entries()
        )),
        Some(v) if v.is_finite() && v > 0.0 => {
            ring.window_seconds = v.min(600.0);
            let cap = ring.retune(fps);
            Ok(format!(
                "window: {:.1}s at every-{}-frames \u{2192} cap {cap} anchors\n  ~{:.0} MiB when full (~98 KiB/anchor, measured)   budget now {} MiB",
                ring.window_seconds,
                ring.cadence_frames.max(1),
                cap as f64 * 98.0 / 1024.0,
                ring.anchors.budget_bytes() / (1024 * 1024)
            ))
        }
        Some(_) => Err("window: seconds must be > 0 (max 600)".into()),
    }
}

/// `ringdump <path>`: the three rings into a `.c64rering` under the monitor directory. `ringdump <path> board`: the
/// file the TRX64-Ultimate board loads (S38 §7), from the anchors a board file brought.
fn ringdump(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let usage = || "ringdump: usage: ringdump <path.c64rering> [board]".to_string();
    let path = match args {
        [path] => *path,
        [path, "board"] => return ringdump_board(host, path),
        _ => return Err(usage()),
    };
    let (bytes, info) = backend(host).ring_dump().map_err(|e| format!("ringdump: {e}"))?;
    host.state.files.write(path, &bytes).map_err(|e| format!("ringdump: {e}"))?;
    Ok(format!(
        "ringdump: {} anchor(s), {} delta entr(ies), {} cpu-history \u{2192} {path}  ({} bytes, cycles {}\u{2013}{})",
        info.anchors, info.delta_entries, info.cpu_history, info.file_bytes, info.cycle_first, info.cycle_last
    ))
}

fn ringdump_board(host: &mut Host, path: &str) -> Result<String, String> {
    let (bytes, info) = backend(host).ring_dump_board().map_err(|e| format!("ringdump: {e}"))?;
    host.state.files.write(path, &bytes).map_err(|e| format!("ringdump: {e}"))?;
    let mut line = format!(
        "ringdump: {} board anchor(s), {} marked, current={} \u{2192} {path}  ({} bytes, stored gzip, the board's format)",
        info.anchors, info.marks, info.current_id, info.file_bytes
    );
    if info.not_board > 0 {
        line.push_str(&format!("\n  note: {} anchor(s) of UE2's own left out: no chip block (trx64Hw)", info.not_board));
    }
    if info.over_limit > 0 {
        line.push_str(&format!(
            "\n  note: {} older board anchor(s) left out: the board takes at most {}",
            info.over_limit,
            c64_bridge::BOARD_MAX_ENTRIES
        ));
    }
    Ok(line)
}

/// `ringload <path>`: a `.c64rering` back, and the C64 on its current anchor, held there.
fn ringload(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let [path] = args else { return Err("ringload: usage: ringload <path.c64rering>".into()) };
    let bytes = host.state.files.read(path).map_err(|e| format!("ringload: {e}"))?;
    let info = with_ddr(host, |b| b.ring_load(&bytes)).map_err(|e| format!("ringload: {e}"))?;
    let line = format!(
        "ringload: restored {} anchor(s), {} delta entr(ies), {} cpu-history from {path}  (current={}, cycles {}\u{2013}{})\n  now: `goto`, `frame`, `rstep`, `whowrote`, `chis` work on this buffer; `run` continues from here",
        info.anchors,
        info.delta_entries,
        info.cpu_history,
        info.current_id.as_deref().unwrap_or("none"),
        info.cycle_first,
        info.cycle_last
    );
    Ok(with_notes(host, line))
}

/// `rstep [n]`: undo the last n instructions from the delta ring (TRX64 `Machine::reverse_step`). The C64 is stopped
/// first: the undo restores CPU and memory bytes, not chip counters, so it is for inspection, not for running on.
fn rstep(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let n = args.first().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    ring_of(host, "rstep")?;
    if backend(host).ring_holds() {
        return Err("rstep: the C64 stands on an anchor, and the delta ring ends at the head \u{2014} `run` first, or go back with `frame -1`".into());
    }
    super::run::set_halted(host, true)?;
    // The ring stands while the C64 is stopped; TRX64 undoes only from an armed ring.
    backend(host).arm_rings_for_step();
    let out = host.machine().reverse_step(n)?;
    let l = out.landed;
    let mut lines = vec![format!(
        "reverse-step: undid {} instruction(s) \u{2192} landed @ {}  A=${:02x} X=${:02x} Y=${:02x} SP=${:02x} P=${:02x}  cyc={}",
        out.steps_taken,
        addr_spans::addr4(l.pc, Space::C64, Role::Pc),
        l.a,
        l.x,
        l.y,
        l.sp,
        l.p,
        l.cycle
    )];
    if out.undone_writes.is_empty() {
        lines.push("  (no memory writes were rolled back)".into());
    } else {
        lines.push(format!("  rolled back {} write(s) (newest first):", out.undone_writes.len()));
        for w in out.undone_writes.iter().take(32) {
            lines.push(format!(
                "    {}: ${:02x} <- ${:02x}  (restored old)",
                addr_spans::addr4(w.addr, Space::C64, Role::Memory),
                w.old_value,
                w.new_value
            ));
        }
        if out.undone_writes.len() > 32 {
            lines.push(format!("    \u{2026} +{} more", out.undone_writes.len() - 32));
        }
    }
    lines.push("  NOTE: inspect-backward only \u{2014} CPU+RAM+IO bytes restored, NOT chip counters (VIC raster / CIA timers); the C64 is stopped (`c64 go`). Restore a checkpoint anchor to resume forward.".into());
    Ok(lines.join("\n"))
}

/// `chis [cycles] | chis <start> <end>`: the CPU history ring, the last 4000 cycles by default.
fn chis(host: &mut Host, args: &[&str]) -> Result<String, String> {
    let is_num = |t: Option<&&str>| t.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let window = (is_num(args.first()) && is_num(args.get(1)))
        .then(|| (args[0].parse::<u64>().unwrap_or(0), args[1].parse::<u64>().unwrap_or(0)));
    let last = if window.is_none() && is_num(args.first()) { args[0].parse().unwrap_or(4000) } else { 4000 };
    let m = host.machine();
    let ring = &m.cpu_history;
    let Some((oldest, newest)) = ring.cycle_span() else {
        return Err("chis: no cpu history \u{2014} the live ring is empty (it records while the C64 runs at 1 MHz)".into());
    };
    let mut entries = Vec::new();
    let header = match window {
        Some((s, e)) => {
            let (lo, hi) = (s.min(e), s.max(e));
            if lo < oldest {
                let ex = m.ring_exhaustion(false);
                let mut msg = format!("chis: cycle {lo} is older than the live ring (oldest {oldest})");
                if ex.ring_exhausted {
                    msg.push_str(&format!("\nring_exhausted: true   revdepth={}s   hint: {}", ex.revdepth_seconds, ex.hint));
                }
                return Err(msg);
            }
            ring.window_by_cycle(lo, hi, &mut entries);
            format!("# cpuhistory (live ring) \u{2014} cycles {lo}\u{2013}{hi}, {} instr", entries.len())
        }
        None => {
            ring.window_by_cycle(newest.saturating_sub(last), newest, &mut entries);
            format!("# cpuhistory (live ring) \u{2014} last {last} cycles, {} instr", entries.len())
        }
    };
    let mut out = header;
    for e in &entries {
        let (pc, op, b1, b2) = (e.pc, e.opcode, e.b1, e.b2);
        let read = move |addr: u16| match addr.wrapping_sub(pc) {
            0 => op,
            1 => b1,
            2 => b2,
            _ => 0,
        };
        let (_, line) = addr_spans::disasm_line(read, pc, Space::C64);
        out.push_str(&format!(
            "\nc{:<10} {}  a={:02x} x={:02x} y={:02x} sp={:02x} p={:02x}",
            e.cycle,
            addr_spans::pad_right(&line, 30),
            e.a,
            e.x,
            e.y,
            e.sp,
            e.p
        ));
    }
    Ok(out)
}

/// `asm-file <path> [addr]`: a file under the monitor directory, assembled by the library's block assembler.
fn asm_file(host: &mut Host, session: &mut MonitorSession, args: &[&str]) -> Result<String, String> {
    let Some(path) = args.first() else { return Err("asm-file: usage: asm-file <path> [addr]".into()) };
    let bytes = host.state.files.read(path).map_err(|e| format!("asm-file: {e}"))?;
    let source = String::from_utf8(bytes).map_err(|_| format!("asm-file: {path} is not text"))?;
    let origin_tok = args.get(1).map(|s| s.to_string());
    let origin = verbs::asm_file_origin(&source, path, origin_tok.as_ref())?;
    verbs::assemble_block_at(session, host, origin, &source, "asm-file")
}
