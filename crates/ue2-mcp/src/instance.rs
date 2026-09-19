//! One emulator instance: a headless `ue2emu run` child with its own run directory, control port and forwards.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;

use crate::ctl::{screen_text, CtlConn};
use crate::proc::{self, Stop};
use crate::ring::Ring;

/// Retained firmware console (the full log is also written to `console.log`).
pub const CONSOLE_RING: usize = 4 << 20;
const STDERR_RING: usize = 1 << 20;

#[derive(Clone, Debug, Serialize)]
pub struct Ports {
    /// ue2emu TCP control protocol.
    pub control: u16,
    /// The emulator's web UI proxy (`--web-port`) to the firmware's HTTP server on guest port 80; it passes REST
    /// through unchanged and makes the web UI's API URLs carry this port (docs/status/network.md).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    /// Guest port 23 (telnet UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telnet: Option<u16>,
    /// Guest port 21 (FTP control).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ftp: Option<u16>,
    /// Guest port 64 (Ultimate DMA socket).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dma: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub emulator: PathBuf,
    pub firmware: PathBuf,
    pub roms: PathBuf,
    pub flash: Option<PathBuf>,
    /// `--c64-roms` directory, absolute.
    pub c64_roms: Option<PathBuf>,
    pub sd: Option<PathBuf>,
    /// `--usb-dir` arguments, paths absolute.
    pub usb_dirs: Vec<String>,
    /// `--usb-dir-work`: shared by all instances so an unsynced image survives the reuse of an instance id.
    pub usb_dir_work: PathBuf,
    /// `--cart-slot` argument (path absolute, options kept), docs/status/cart-slot.md.
    pub cart_slot: Option<String>,
    pub net: bool,
    pub realtime: bool,
    pub extra_args: Vec<String>,
    pub server_pid: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub description: String,
}

pub struct Instance {
    pub id: String,
    pub pid: u32,
    pub run_dir: PathBuf,
    pub ports: Ports,
    pub launch: Launch,
    pub argv: Vec<String>,
    started: Instant,
    started_unix: u64,
    pub stdout: Arc<StdMutex<Ring>>,
    pub stderr: Arc<StdMutex<Ring>>,
    exit: watch::Receiver<Option<ExitInfo>>,
    ctl: Mutex<Option<CtlConn>>,
    shots: AtomicU64,
    /// Write end of the watchdog pipe (see `spawn_watchdog`).
    watchdog: StdMutex<Option<tokio::process::ChildStdin>>,
}

impl Instance {
    /// Start the emulator and connect to its control port (within `ready_timeout`).
    pub async fn spawn(id: String, run_dir: PathBuf, launch: Launch, ready_timeout: Duration) -> Result<Arc<Instance>> {
        std::fs::create_dir_all(run_dir.join("shots")).with_context(|| format!("create {}", run_dir.display()))?;

        let p = free_ports(if launch.net { 5 } else { 1 })?;
        let ports = Ports {
            control: p[0],
            http: launch.net.then(|| p[1]),
            telnet: launch.net.then(|| p[2]),
            ftp: launch.net.then(|| p[3]),
            dma: launch.net.then(|| p[4]),
        };
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut argv: Vec<String> = vec![
            "run".into(),
            "--headless".into(),
            "--speed".into(),
            if launch.realtime { "realtime" } else { "max" }.into(),
            "--control".into(),
            format!("127.0.0.1:{}", ports.control),
            "--firmware".into(),
            path(&launch.firmware),
            "--roms".into(),
            path(&launch.roms),
        ];
        if let Some(flash) = &launch.flash {
            argv.extend(["--flash".into(), path(flash)]);
        }
        if let Some(dir) = &launch.c64_roms {
            argv.extend(["--c64-roms".into(), path(dir)]);
        }
        if let Some(sd) = &launch.sd {
            argv.extend(["--sd".into(), path(sd)]);
        }
        for spec in &launch.usb_dirs {
            argv.extend(["--usb-dir".into(), spec.clone()]);
        }
        if !launch.usb_dirs.is_empty() {
            argv.extend(["--usb-dir-work".into(), path(&launch.usb_dir_work)]);
        }
        if let Some(spec) = &launch.cart_slot {
            argv.extend(["--cart-slot".into(), spec.clone()]);
        }
        if launch.net {
            argv.extend([
                "--net".into(),
                "user".into(),
                "--web-port".into(),
                p[1].to_string(),
                "--hostfwd".into(),
                format!("tcp:127.0.0.1:{}:23,tcp:127.0.0.1:{}:21,tcp:127.0.0.1:{}:64", p[2], p[3], p[4]),
            ]);
        }
        argv.extend(launch.extra_args.iter().cloned());

        let mut cmd = Command::new(&launch.emulator);
        cmd.args(&argv)
            .current_dir(&run_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        proc::detach(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("start {}", launch.emulator.display()))?;
        let pid = child.id().ok_or_else(|| anyhow!("the emulator exited immediately"))?;

        let stdout = Arc::new(StdMutex::new(Ring::new(CONSOLE_RING)));
        let stderr = Arc::new(StdMutex::new(Ring::new(STDERR_RING)));
        let out_pump = tokio::spawn(pump(child.stdout.take().expect("piped"), stdout.clone(), run_dir.join("console.log")));
        let err_pump = tokio::spawn(pump(child.stderr.take().expect("piped"), stderr.clone(), run_dir.join("stderr.log")));
        let (exit_tx, exit_rx) = watch::channel(None);
        tokio::spawn(async move {
            let status = child.wait().await;
            // Publish the exit only after the pipes are drained, so the final stderr (halt report, stats) is there.
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let _ = out_pump.await;
                let _ = err_pump.await;
            })
            .await;
            let info = match status {
                Ok(s) => ExitInfo { code: s.code(), signal: proc::exit_signal(&s), description: s.to_string() },
                Err(e) => ExitInfo { code: None, signal: None, description: format!("wait failed: {e}") },
            };
            let _ = exit_tx.send(Some(info));
        });
        let watchdog = spawn_watchdog(pid, ports.control);

        let started_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let meta = json!({
            "id": id, "pid": pid, "server_pid": launch.server_pid, "started_unix": started_unix,
            "ports": ports, "emulator": launch.emulator, "argv": argv,
        });
        let _ = std::fs::write(run_dir.join("instance.json"), serde_json::to_vec_pretty(&meta).unwrap_or_default());

        let inst = Arc::new(Instance {
            id,
            pid,
            run_dir,
            ports,
            launch,
            argv,
            started: Instant::now(),
            started_unix,
            stdout,
            stderr,
            exit: exit_rx,
            ctl: Mutex::new(None),
            shots: AtomicU64::new(0),
            watchdog: StdMutex::new(watchdog),
        });

        let marker = format!("control: listening on 127.0.0.1:{}", inst.ports.control);
        let deadline = Instant::now() + ready_timeout;
        loop {
            if let Some(e) = inst.exited() {
                bail!("the emulator exited during startup ({}); stderr:\n{}", e.description, inst.stderr_tail(30));
            }
            if inst.stderr.lock().unwrap().find(0, marker.as_bytes(), false).is_some() {
                break;
            }
            if Instant::now() >= deadline {
                inst.kill(Stop::Kill);
                bail!(
                    "the emulator did not open its control port within {} ms; stderr:\n{}",
                    ready_timeout.as_millis(),
                    inst.stderr_tail(30)
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        match tokio::time::timeout(Duration::from_secs(5), CtlConn::connect(inst.ports.control)).await {
            Ok(Ok(conn)) => *inst.ctl.lock().await = Some(conn),
            other => {
                inst.kill(Stop::Kill);
                let why = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    _ => "timed out".into(),
                };
                bail!("cannot connect to the control port {}: {why}", inst.ports.control);
            }
        }
        Ok(inst)
    }

    pub fn exited(&self) -> Option<ExitInfo> {
        self.exit.borrow().clone()
    }

    pub async fn wait_exit(&self, d: Duration) -> Option<ExitInfo> {
        let mut rx = self.exit.clone();
        let _ = tokio::time::timeout(d, rx.wait_for(|e| e.is_some())).await;
        self.exited()
    }

    /// Stop our own child, which is not reaped while `exited()` is None.
    fn kill(&self, how: Stop) {
        if self.exited().is_none() {
            proc::stop(self.pid, how);
        }
    }

    /// Run one control command; `Err` carries the emulator's message, a timeout, or the exit reason.
    ///
    /// On timeout the connection is dropped (the emulator may still be executing the command); the next call
    /// reconnects.
    pub async fn control(&self, line: &str, timeout: Duration) -> Result<Vec<String>> {
        if let Some(e) = self.exited() {
            bail!("instance {} is not running (exited: {}); stderr tail:\n{}", self.id, e.description, self.stderr_tail(15));
        }
        let deadline = Instant::now() + timeout;
        let mut guard = tokio::time::timeout_at(deadline, self.ctl.lock()).await.map_err(|_| {
            anyhow!(
                "instance {} is busy with another control command; gave up after {} ms",
                self.id,
                timeout.as_millis()
            )
        })?;
        if guard.is_none() {
            let conn = tokio::time::timeout_at(deadline, CtlConn::connect(self.ports.control))
                .await
                .map_err(|_| anyhow!("connecting to the control port timed out"))?
                .context("connect to the control port")?;
            *guard = Some(conn);
        }
        let conn = guard.as_mut().expect("connected above");
        match tokio::time::timeout_at(deadline, conn.command(line)).await {
            Err(_) => {
                *guard = None;
                bail!(
                    "control command `{line}` did not finish within {} ms (connection reset; the emulator may still be executing it)",
                    timeout.as_millis()
                )
            }
            Ok(Err(e)) => {
                *guard = None;
                drop(guard);
                if let Some(x) = self.wait_exit(Duration::from_millis(500)).await {
                    bail!("the emulator exited ({}) during `{line}`; stderr tail:\n{}", x.description, self.stderr_tail(20));
                }
                bail!("control connection failed during `{line}`: {e:#}")
            }
            Ok(Ok(Err(msg))) => bail!("the emulator rejected `{line}`: {msg}"),
            Ok(Ok(Ok(lines))) => Ok(lines),
        }
    }

    pub async fn screen(&self, timeout: Duration) -> Result<String> {
        Ok(screen_text(&self.control("screen", timeout).await?))
    }

    /// Render the display to `path` with the `png` command and return the file's bytes.
    pub async fn render_png(&self, path: &Path, timeout: Duration) -> Result<Vec<u8>> {
        self.control(&format!("png {}", path.display()), timeout).await?;
        tokio::fs::read(path).await.with_context(|| format!("read {}", path.display()))
    }

    pub fn next_shot_path(&self) -> PathBuf {
        let n = self.shots.fetch_add(1, Ordering::Relaxed) + 1;
        self.run_dir.join("shots").join(format!("shot-{n:04}.png"))
    }

    pub fn stderr_tail(&self, lines: usize) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap().tail(lines, 64 * 1024).1).into_owned()
    }

    pub fn console_end(&self) -> u64 {
        self.stdout.lock().unwrap().end()
    }

    /// Stop cleanly with `quit` (the flash image is written back), escalating to SIGTERM and SIGKILL.
    pub async fn stop(&self, timeout: Duration) -> Value {
        let report = self.stop_process(timeout).await;
        // Tell the watchdog this was a deliberate stop.
        let watchdog = self.watchdog.lock().unwrap().take();
        if let Some(mut pipe) = watchdog {
            let _ = pipe.write_all(b"done\n").await;
        }
        if let Some(run_base) = self.run_dir.parent() {
            release(run_base, &self.id, self.launch.server_pid);
        }
        report
    }

    async fn stop_process(&self, timeout: Duration) -> Value {
        if let Some(e) = self.exited() {
            return json!({
                "id": self.id, "already_exited": true, "exit": e, "run_dir": self.run_dir,
                "stderr_tail": self.stderr_tail(12),
            });
        }
        let quit = self.control("quit", Duration::from_secs(5)).await;
        let mut forced = Value::Null;
        let mut exit = self.wait_exit(timeout).await;
        for (how, wait) in [(Stop::Term, 2), (Stop::Kill, 3)] {
            if exit.is_none() {
                self.kill(how);
                forced = how.name().into();
                exit = self.wait_exit(Duration::from_secs(wait)).await;
            }
        }
        json!({
            "id": self.id,
            "graceful_quit": quit.is_ok(),
            "quit_error": quit.err().map(|e| format!("{e:#}")),
            "forced": forced,
            "exit": exit,
            "flash": self.launch.flash,
            "run_dir": self.run_dir,
            "stderr_tail": self.stderr_tail(12),
        })
    }

    pub fn summary(&self) -> Value {
        let exit = self.exited();
        json!({
            "id": self.id,
            "alive": exit.is_none(),
            "exit": exit,
            "pid": self.pid,
            "uptime_s": self.started.elapsed().as_secs(),
            "started_unix": self.started_unix,
            "ports": self.ports,
            "rest_url": self.ports.http.map(|p| format!("http://127.0.0.1:{p}")),
            "firmware": self.launch.firmware,
            "roms": self.launch.roms,
            "flash": self.launch.flash,
            "sd": self.launch.sd,
            "usb_dirs": self.launch.usb_dirs,
            "cart_slot": self.launch.cart_slot,
            "speed": if self.launch.realtime { "realtime" } else { "max" },
            "net": self.launch.net,
            "run_dir": self.run_dir,
            "console_bytes": self.console_end(),
            "command": std::iter::once(self.launch.emulator.to_string_lossy().into_owned())
                .chain(self.argv.iter().cloned())
                .collect::<Vec<_>>(),
        })
    }
}

/// Copy a child pipe into a ring buffer and a log file.
async fn pump(mut r: impl AsyncRead + Unpin, ring: Arc<StdMutex<Ring>>, log: PathBuf) {
    let mut file = tokio::fs::File::create(&log).await.ok();
    let mut buf = vec![0u8; 16384];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                ring.lock().unwrap().push(&buf[..n]);
                if let Some(f) = file.as_mut() {
                    if f.write_all(&buf[..n]).await.is_err() {
                        file = None;
                    }
                }
            }
        }
    }
    if let Some(f) = file.as_mut() {
        let _ = f.flush().await;
    }
}

/// Distinct free localhost TCP ports (all bound at once so none repeats; released on return).
fn free_ports(n: usize) -> Result<Vec<u16>> {
    let listeners = (0..n)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()
        .context("allocate a free localhost port")?;
    listeners.iter().map(|l| Ok(l.local_addr()?.port())).collect()
}

/// A detached `ue2-mcp --watchdog PID PORT` that stops the emulator when this server dies without cleaning up
/// (killed, crashed); see `proc::watchdog`. Returns the write end of its stdin.
fn spawn_watchdog(child: u32, control_port: u16) -> Option<tokio::process::ChildStdin> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--watchdog")
        .arg(child.to_string())
        .arg(control_port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    proc::detach(&mut cmd);
    let mut c = cmd.spawn().ok()?;
    let stdin = c.stdin.take();
    tokio::spawn(async move {
        let _ = c.wait().await;
    });
    stdin
}

/// True when `dir/instance.json` names a live process (an emulator another server started still owns it).
pub fn dir_in_use(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("instance.json")) else { return false };
    serde_json::from_str::<Value>(&text).ok().and_then(|v| v["pid"].as_i64()).is_some_and(proc::alive)
}

fn claim_path(run_base: &Path, id: &str) -> PathBuf {
    run_base.join(format!("{id}.claim"))
}

/// Claim instance id `id` for this server across processes: `<run_base>/<id>.claim` is created exclusively and
/// holds the server pid. A claim whose server is gone (or that names this server, whose in-memory state is
/// authoritative) is taken over. Several MCP servers (Claude sessions) can share one run base this way.
pub fn claim(run_base: &Path, id: &str, server_pid: u32) -> bool {
    use std::io::Write;
    if std::fs::create_dir_all(run_base).is_err() {
        return false;
    }
    let path = claim_path(run_base, id);
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => return write!(f, "{server_pid}").is_ok(),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i64>().ok());
                match holder {
                    Some(pid) if pid != server_pid as i64 && proc::alive(pid) => return false,
                    // Stale, ours, or still being written by its creator: take it over only if unreadable twice.
                    None if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) => return false,
                    _ => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(_) => return false,
        }
    }
    false
}

/// Drop this server's claim on `id`.
pub fn release(run_base: &Path, id: &str, server_pid: u32) {
    let path = claim_path(run_base, id);
    if std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(server_pid) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_exclusive_per_live_server() {
        let base = std::env::temp_dir().join(format!("ue2-mcp-claim-{}", std::process::id()));
        let me = std::process::id();
        assert!(claim(&base, "emu1", me));
        assert!(claim(&base, "emu1", me), "own claim is taken over (in-memory state decides)");
        // A live foreign holder: pid 1 (launchd) always exists.
        std::fs::write(base.join("emu2.claim"), "1").unwrap();
        assert!(!claim(&base, "emu2", me));
        // A dead holder is stale.
        std::fs::write(base.join("emu3.claim"), "999999999").unwrap();
        assert!(claim(&base, "emu3", me));
        release(&base, "emu1", me);
        release(&base, "emu2", me);
        assert!(!base.join("emu1.claim").exists());
        assert!(base.join("emu2.claim").exists(), "release leaves foreign claims alone");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
