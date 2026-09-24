//! The MCP tool surface: emulator instances, UI input, screen/console/REST inspection.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::config::{self, Config};
use crate::http;
use crate::img;
use crate::instance::{self, Instance, Launch};
use crate::ring::{find_bytes, Ring};

type ToolResult = Result<CallToolResult, rmcp::ErrorData>;

const BOOT_MARKER: &str = "All linked modules have been initialized";

const INSTRUCTIONS: &str = "\
Runs the unmodified U64-II / C64 Ultimate firmware in the ue2emu emulator for firmware testing.
Typical session: emu_start (boots and waits for the console line \
'All linked modules have been initialized') -> emu_button (opens the overlay menu) -> emu_key / emu_type \
(navigate) -> emu_expect (assert text on screen or console) -> emu_screenshot -> emu_rest (needs net=true) -> \
emu_stop. Durations named hold_ms/settle_ms/ms are EMULATED milliseconds; timeout_ms/timeout_s are wall clock. \
Instances keep running between calls until emu_stop; the server stops all of them when it exits. \
The emulator has no C64 video yet: the screen tools see the Ultimate overlay (menu) only. \
A physical cartridge: emu_start {cart_slot: {path, mode}} puts a .crt into the expansion port; emu_cart_info shows it, \
emu_cart_save writes it (flash and EEPROM included) to a CRT file.";

// ---------------------------------------------------------------------------------------------------------
// Parameters

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartParams {
    /// Firmware to boot: an ultimate.elf (best, has symbols), ultimate.app, a .ue2 update file, or a 1541ultimate
    /// checkout directory (its target/u64ii/riscv/ultimate/result/ultimate.elf is used). Default: the emulator
    /// repo's firmware/1541ultimate build. Relative paths are relative to the MCP server's working directory.
    pub firmware: Option<String>,
    /// Firmware roms directory (must contain chars.bin). Default: <checkout>/roms of the checkout the firmware
    /// file lives in, else the default firmware tree's roms.
    pub roms: Option<String>,
    /// SPI flash (config storage): "fresh" (default; a new erased image in the instance run dir, overlay UI
    /// seeded), "persist" (one shared image, <run base>/persist-flash.bin, that survives emu_stop/emu_start),
    /// "none" (in memory, nothing saved), or a path to an image file (created erased if missing). The image is
    /// written back when the instance stops cleanly (emu_stop).
    pub flash: Option<String>,
    /// C64 system ROMs into the flash before boot (`ue2emu run --c64-roms`, docs/status/c64.md): true copies KERNAL,
    /// BASIC and CHAR from the roms directory (kernal.901227-03.bin, basic.901226-01.bin, characters.901225-01.bin)
    /// into /flash/roms of the flash image; a string names another directory with those files. The C64 then boots to
    /// BASIC READY instead of the "shipped without System ROMs" screen. A /flash/roms file with other content is kept;
    /// extra_args ["--c64-roms-force"] replaces it. Needs a flash image (not flash "none"). Default false.
    pub c64_roms: Option<C64Roms>,
    /// SD card image file to insert.
    pub sd: Option<String>,
    /// Host directories as USB sticks (`--usb-dir`, docs/status/usb-dir.md), each "PATH[,size=SIZE][,ro]". The
    /// firmware sees a FAT32 volume built from PATH on hub ports 1.. in order (USB0, USB1, ...) and may write to it;
    /// the guest's changes are written back to PATH when the guest has been quiet for 2 s, on emu_usb_sync and on
    /// emu_stop (atomic writes, deletions moved into PATH/.ue2-trash, conflict copies, a mass-deletion guard). Host
    /// changes reach the guest by an automatic unplug/rebuild/plug. ",ro" write-protects the stick. The volumes
    /// live in <run base>/usb-dir.
    pub usb_dirs: Option<Vec<String>>,
    /// A cartridge in the physical expansion port (`--cart-slot`, docs/status/cart-slot.md), served by TRX64's mappers
    /// or the U64 cart logic independently of the internal cartridge emulation, so the firmware's DMA accesses reach
    /// it like a real board: {"path": "game.crt", "mode": "ro"|"rw"|"save", "save_path": "out.crt",
    /// "flash_decode": "11"|"15"|"both"}.
    pub cart_slot: Option<CartSlotParams>,
    /// Wired Ethernet (libslirp user networking, guest gets 10.0.2.15 by DHCP) with per-instance localhost ports:
    /// http (the emulator's web UI proxy to guest port 80, also serving the browser UI), and forwards to guest
    /// ports 23 (telnet), 21 (ftp), 64 (dma). Required for emu_rest. Default false.
    pub net: Option<bool>,
    /// "max" (default; as fast as the host allows, several times hardware speed) or "realtime" (paced like the
    /// hardware; use when timing matters, e.g. network timeouts or key repeat).
    pub speed: Option<String>,
    /// Extra `ue2emu run` arguments, e.g. ["--no-overlay-ui"], ["--caps", "34000222"], ["--log", "unmapped"],
    /// ["--trace"], ["--no-halt"]. Not allowed: flags this tool sets (--control, --firmware, --roms, --flash,
    /// --c64-roms, --sd, --usb-dir, --cart-slot, --net, --hostfwd, --web-port, --speed, --headless, --script). With
    /// ["--gdb", "127.0.0.1:PORT"] the firmware waits for the debugger, so pass wait_for_boot=false.
    pub extra_args: Option<Vec<String>>,
    /// Wait until the console shows boot_marker before returning. Default true.
    pub wait_for_boot: Option<bool>,
    /// Console text that marks a finished boot. Default "All linked modules have been initialized".
    pub boot_marker: Option<String>,
    /// Wall-clock limit for the boot wait, ms. Default 90000.
    pub boot_timeout_ms: Option<u64>,
}

/// `c64_roms`: true / false, or a ROM directory.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum C64Roms {
    Enabled(bool),
    Dir(String),
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StopParams {
    /// Instance id from emu_start, or "all".
    pub id: String,
    /// Wall-clock ms to wait for a clean exit after `quit` before SIGTERM, then SIGKILL. Default 10000.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConsoleParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Absolute byte offset to read from (use next_offset of a previous emu_console/emu_expect/emu_start result
    /// to get only newer output). When omitted, the last tail_lines lines are returned.
    pub since_offset: Option<u64>,
    /// Lines to return when since_offset is omitted. Default 60.
    pub tail_lines: Option<usize>,
    /// Maximum bytes to return. Default 65536.
    pub max_bytes: Option<usize>,
    /// "stdout" (default: the firmware UART console) or "stderr" (emulator diagnostics: control/net startup,
    /// 'halted:' fault reports with backtrace, final stats line).
    pub stream: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScreenParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Also determine whether the overlay is currently displayed (renders one frame). Default true.
    pub check_visible: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScreenshotParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Integer upscale factor 1-4 (nearest neighbour). Default 2.
    pub scale: Option<u32>,
    /// Also write the PNG to this path (parent directories are created).
    pub save_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ButtonParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Emulated ms to hold the button. Default 100. Keep well below 1000 (a long press is a different action).
    pub hold_ms: Option<u64>,
    /// Emulated ms to let the firmware react afterwards. Default 300; 0 skips.
    pub settle_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Keys to press in order. Names: return, down, up, left, right, f1-f8, space, del, inst, home, clr,
    /// runstop, ctrl, cbm, lshift, rshift, pound, uparrow, larrow; or one character ("a"; "A" is SHIFT+A; "1").
    pub keys: Vec<String>,
    /// Emulated ms each key is held. Default 80 (a 40 ms release gap follows automatically).
    pub hold_ms: Option<u64>,
    /// Emulated ms to wait after the last key. Default 300; 0 skips.
    pub settle_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Text to type. "\n" presses RETURN. Only characters that exist on the C64 keyboard can be typed.
    pub text: String,
    /// Emulated ms to wait after typing. Default 300; 0 skips.
    pub settle_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Emulated milliseconds to run.
    pub ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExpectParams {
    /// Instance id from emu_start.
    pub id: String,
    /// Text to look for (plain substring).
    pub text: String,
    /// "screen" (default: overlay screen text, as emu_screen) or "console" (firmware UART output).
    pub source: Option<String>,
    /// Wall-clock ms to keep polling. Default 10000.
    pub timeout_ms: Option<u64>,
    /// Console only: search output from this absolute byte offset (default 0 = everything retained). Pass a
    /// next_offset captured before an action to match only output that the action caused.
    pub since_offset: Option<u64>,
    /// Case-insensitive (ASCII) match. Default false.
    pub ignore_case: Option<bool>,
    /// Invert: screen = wait until the text is gone; console = pass only if the text does NOT appear within
    /// timeout_ms (waits the full timeout). Default false.
    pub absent: Option<bool>,
    /// Screen only: the overlay must also be displayed (screen RAM keeps text while hidden). Default false.
    pub require_visible: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RestParams {
    /// Instance id from emu_start (started with net=true).
    pub id: String,
    /// HTTP method. Default GET.
    pub method: Option<String>,
    /// Request path with query string, e.g. "/v1/info" or "/v1/configs/Audio%20Output%20Settings".
    pub path: String,
    /// Request body as text.
    pub body: Option<String>,
    /// Request body read from this file (binary-safe), instead of body.
    pub body_file: Option<String>,
    /// Content-Type header for the body.
    pub content_type: Option<String>,
    /// Extra request headers.
    pub headers: Option<BTreeMap<String, String>>,
    /// Wall-clock ms, including retries while the firmware's web server is not reachable yet. Default 20000.
    pub timeout_ms: Option<u64>,
    /// Write the response body to this file.
    pub save_body_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MonitorParams {
    /// Instance id from emu_start.
    pub id: String,
    /// One monitor command, e.g. "r", "m c000 c00f", "d e000", "device fw".
    pub command: String,
    /// Wall-clock ms. Default 60000.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ControlParams {
    /// Instance id from emu_start.
    pub id: String,
    /// One control-protocol line, e.g. "key f2 80", "wait 500", "screen", "png /tmp/x.png".
    pub command: String,
    /// Wall-clock ms. Default 60000.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UsbSyncParams {
    /// Instance id from emu_start.
    pub id: String,
    /// USB hub port 1-3 (usb_dirs take ports 1.. in order). Default: every usb_dirs stick.
    pub port: Option<u8>,
    /// Override the mass-deletion guard, which refuses a sync that would delete more than 25 % of the files or
    /// more than 50. Overwrites do not count. Check the stick first. Not with replug.
    pub force: Option<bool>,
    /// Unplug the stick, sync, rebuild the volume from the host directory and plug it back in (usb-replug), so the
    /// firmware sees host changes now. On a port without usb_dirs: just unplug and plug in.
    pub replug: Option<bool>,
    /// With replug: do not sync; keep the old image aside (discarded-*.img in the work directory) and rebuild.
    pub discard: Option<bool>,
    /// Wall-clock ms. Default 120000.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CartSlotParams {
    /// The .crt file. Relative paths are relative to the MCP server's working directory.
    pub path: String,
    /// "ro" (default: the file is never written), "rw" (flash and EEPROM changes are written back into the file while
    /// running, 2 s emulated after the last change, and on emu_stop; the original is kept as FILE.crt.bak) or "save"
    /// (the same into save_path).
    pub mode: Option<String>,
    /// With mode "save": the CRT file the changes go to.
    pub save_path: Option<String>,
    /// Flash command addresses a flash cartridge decodes: "11" ($555/$2AA), "15" ($5555/$2AAA) or "both" (default).
    pub flash_decode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CartInfoParams {
    /// Instance id from emu_start.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CartSaveParams {
    /// Instance id from emu_start.
    pub id: String,
    /// CRT file to write. Relative paths are relative to the MCP server's working directory.
    pub path: String,
    /// Wall-clock ms. Default 60000.
    pub timeout_ms: Option<u64>,
}

// ---------------------------------------------------------------------------------------------------------
// State

pub struct State {
    pub cfg: Config,
    instances: tokio::sync::Mutex<BTreeMap<String, Arc<Instance>>>,
    /// Ids being started (not yet in `instances`).
    reserved: StdMutex<HashSet<String>>,
}

impl State {
    async fn get(&self, id: &str) -> Result<Arc<Instance>> {
        let map = self.instances.lock().await;
        match map.get(id) {
            Some(inst) => Ok(inst.clone()),
            None => bail!("no instance '{id}'; known instances: {}", known(&map)),
        }
    }

    /// Stop every instance (clean quit first). Called when the server exits.
    pub async fn shutdown(&self) {
        let all: Vec<Arc<Instance>> = std::mem::take(&mut *self.instances.lock().await).into_values().collect();
        let mut set = tokio::task::JoinSet::new();
        // usb_dirs instances get longer: their last sync runs before they exit.
        let usb_dirs = all.iter().any(|inst| !inst.launch.usb_dirs.is_empty());
        for inst in all {
            let wait = if inst.launch.usb_dirs.is_empty() { Duration::from_secs(6) } else { Duration::from_secs(60) };
            set.spawn(async move { inst.stop(wait).await });
        }
        let _ = tokio::time::timeout(Duration::from_secs(if usb_dirs { 75 } else { 20 }), async {
            while let Some(r) = set.join_next().await {
                if let Ok(v) = r {
                    eprintln!("ue2-mcp: stopped {} (graceful {})", v["id"], v["graceful_quit"]);
                }
            }
        })
        .await;
    }
}

fn known(map: &BTreeMap<String, Arc<Instance>>) -> String {
    if map.is_empty() {
        "none (start one with emu_start)".into()
    } else {
        map.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

// ---------------------------------------------------------------------------------------------------------
// Helpers

fn text(s: impl Into<String>) -> ContentBlock {
    ContentBlock::text(s)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn finish(r: Result<CallToolResult>) -> ToolResult {
    Ok(r.unwrap_or_else(|e| CallToolResult::error(vec![text(format!("error: {e:#}"))])))
}

fn ok(blocks: Vec<ContentBlock>) -> Result<CallToolResult> {
    Ok(CallToolResult::success(blocks))
}

/// Wall-clock budget for a control command that covers `emulated_ms` of emulated time.
fn budget(emulated_ms: u64) -> Duration {
    Duration::from_millis(20_000u64.saturating_add(emulated_ms.saturating_mul(4)))
}

/// Console text around `offset`, snapped to whole lines.
fn context(ring: &Ring, offset: u64, len: usize) -> String {
    let from = offset.saturating_sub(600).max(ring.start());
    let (start, bytes) = ring.read(from, 1200 + len);
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if start > ring.start() {
        if let Some(nl) = s.find('\n') {
            s.drain(..=nl);
        }
    }
    if let Some(nl) = s.rfind('\n') {
        s.truncate(nl + 1);
    }
    s
}

fn tail_text(ring: &Ring, lines: usize) -> String {
    String::from_utf8_lossy(&ring.tail(lines, 64 * 1024).1).into_owned()
}

// ---------------------------------------------------------------------------------------------------------
// Server

#[derive(Clone)]
pub struct Emu {
    state: Arc<State>,
    #[allow(dead_code, reason = "read through the code #[tool_handler] generates")]
    tool_router: ToolRouter<Emu>,
}

impl Emu {
    pub fn new(cfg: Config) -> Emu {
        let state = State {
            cfg,
            instances: Default::default(),
            reserved: Default::default(),
        };
        Emu { state: Arc::new(state), tool_router: Self::tool_router() }
    }

    pub fn state(&self) -> Arc<State> {
        self.state.clone()
    }
}

#[tool_router]
impl Emu {
    #[tool(description = "Start a new headless emulator instance (a ue2emu child process running the unmodified \
U64-II / C64 Ultimate firmware) and by default wait until the firmware has booted (console line 'All linked modules \
have been initialized'; about 1-3 s wall clock at speed=max). Returns the instance `id` (pass it to every other \
emu_* tool), pid, run_dir (console.log, stderr.log, flash.bin, shots/), ports (control = TCP control protocol; with \
net=true also http = the web UI proxy to guest port 80, and telnet/ftp/dma forwarded to guest ports 23/21/64), \
rest_url (http://127.0.0.1:<http>, which also opens the firmware web UI in a browser), the boot banner (firmware \
version line) and console_offset. After boot the overlay menu is closed: call emu_button to open it. Several \
instances can run side by side (own run dir, flash and ports each). Always emu_stop instances you started.")]
    async fn emu_start(&self, Parameters(p): Parameters<StartParams>) -> ToolResult {
        finish(self.start(p).await)
    }

    #[tool(description = "Stop an instance (id='all' stops every instance). Sends the control-protocol `quit` so \
the emulator exits cleanly and writes its flash image back, then escalates to SIGTERM/SIGKILL after timeout_ms. \
Returns exit status, whether the quit was graceful, and the emulator's final stderr (stats line: instructions, \
emulated seconds, final PC). The run directory with logs and screenshots is kept.")]
    async fn emu_stop(&self, Parameters(p): Parameters<StopParams>) -> ToolResult {
        finish(self.stop(p).await)
    }

    #[tool(description = "List the instances this server manages: id, alive, exit status, pid, uptime, ports, \
rest_url, firmware, flash, sd, speed, run_dir, console byte count and the exact ue2emu command line; plus the \
server's configured paths. An instance whose emulator died (e.g. firmware fault halt) stays listed \
with alive=false until emu_stop, so its console and stderr can still be read.")]
    async fn emu_list(&self) -> ToolResult {
        finish(self.list().await)
    }

    #[tool(description = "Read the firmware's UART console (boot log, debug printf output). Without since_offset \
returns the last tail_lines lines (default 60). With since_offset returns the bytes from that absolute offset (up \
to max_bytes); the result's next_offset is where the next incremental read should start. Take console_offset from \
emu_start or next_offset before an action, act, then read since that offset to see exactly what the action \
printed. stream='stderr' shows emulator diagnostics instead (fault 'halted:' reports, stats). The server keeps the \
newest 4 MB; the complete log is run_dir/console.log.")]
    async fn emu_console(&self, Parameters(p): Parameters<ConsoleParams>) -> ToolResult {
        finish(self.console(p).await)
    }

    #[tool(description = "Dump the Ultimate overlay (menu) screen as text, one line per row (trailing spaces \
trimmed), and report whether the overlay is currently displayed. The text comes from overlay screen RAM, which \
keeps its content while the overlay is hidden, so check the VISIBLE/HIDDEN line. The selection bar is colour-only \
and does not appear in the text: use emu_screenshot to see which entry is selected. Main menu example rows: \
'SD      SD Card', 'Flash   Flash Disk', 'Temp    RAM Disk', status line '/   -F3=HELP-'.")]
    async fn emu_screen(&self, Parameters(p): Parameters<ScreenParams>) -> ToolResult {
        finish(self.screen(p).await)
    }

    #[tool(description = "Render the current overlay display to a PNG and return it as an image (upscaled by \
`scale`, default 2). Shows colours and the selection highlight that emu_screen cannot. The file is kept under \
run_dir/shots/; save_to writes an extra copy. A hidden overlay renders as a uniform dark image \
(overlay_visible=false).")]
    async fn emu_screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> ToolResult {
        finish(self.screenshot(p).await)
    }

    #[tool(description = "Press and release the Ultimate menu button (hold_ms emulated, default 100), then let the \
firmware run settle_ms (default 300). With the overlay closed this opens the main menu (file browser with SD, \
Flash, Temp, Ftp, WiFi); pressing it again closes the menu. Keep hold_ms well below 1000: a long press triggers a \
different firmware action.")]
    async fn emu_button(&self, Parameters(p): Parameters<ButtonParams>) -> ToolResult {
        finish(self.button(p).await)
    }

    #[tool(description = "Press keys in order on the C64 keyboard matrix the Ultimate menu reads. Each key is held \
hold_ms (default 80 emulated ms) and released with a 40 ms gap; settle_ms (default 300) runs after the last key. \
Names: return, down, up, left, right, f1-f8, space, del, inst, home, clr, runstop, ctrl, cbm, lshift, rshift, \
pound, uparrow, larrow, or a single character ('a'; 'A' = SHIFT+A). Menu behaviour seen in the firmware: up/down \
move the selection (separators are skipped), right/return enter a directory or config category, return on a \
config value opens its choices and a typed letter jumps to the first match, left goes back (leaving the config \
menu may ask 'Save changes to Flash?', where return picks Yes), f2 opens the configuration menu, f3 help, runstop \
closes the menu.")]
    async fn emu_key(&self, Parameters(p): Parameters<KeyParams>) -> ToolResult {
        finish(self.key(p).await)
    }

    #[tool(description = "Type text into the firmware UI (file names, search strings, config text fields), one key \
per character (about 120-140 emulated ms each); '\\n' presses RETURN. Characters without a C64 key (e.g. non-ASCII, \
tabs) are rejected. settle_ms (default 300) runs afterwards.")]
    async fn emu_type(&self, Parameters(p): Parameters<TypeParams>) -> ToolResult {
        finish(self.type_text(p).await)
    }

    #[tool(description = "Let the emulator run for `ms` EMULATED milliseconds, then return (at speed=max this takes \
much less wall-clock time). Prefer emu_expect when waiting for a specific screen or console text.")]
    async fn emu_wait(&self, Parameters(p): Parameters<WaitParams>) -> ToolResult {
        finish(self.wait(p).await)
    }

    #[tool(description = "Assert that text appears (or with absent=true, disappears) on the overlay screen \
(source='screen', default) or in the UART console (source='console'), polling until timeout_ms (wall clock, \
default 10000). The first line of the result is PASS or FAIL; then the evidence (final screen text, or the console \
lines around the match / the console tail) and JSON details (elapsed_ms; console: match_offset, next_offset). \
Console searches cover all retained output unless since_offset is given, so to check what an action printed, pass \
the next_offset/console_offset you had before the action. A FAIL is a normal result, not a tool error. Fails early \
if the emulator exits.")]
    async fn emu_expect(&self, Parameters(p): Parameters<ExpectParams>) -> ToolResult {
        finish(self.expect(p).await)
    }

    #[tool(description = "Send an HTTP request to the firmware's web server (REST API under /v1/, web UI files) \
through the instance's http port (the emulator's web UI proxy, which passes REST through unchanged); the instance \
must have been started with net=true. Examples: GET /v1/info \
(product, firmware version), GET /v1/version, GET /v1/configs. Retries while the web server does not answer yet \
(networking comes up a few seconds after boot) until timeout_ms (default 20000). Returns the status line, response \
headers and body (text; binary bodies as base64 or written to save_body_to). 4xx/5xx responses are returned \
normally.")]
    async fn emu_rest(&self, Parameters(p): Parameters<RestParams>) -> ToolResult {
        finish(self.rest(p).await)
    }

    #[tool(description = "Run one command of the C64 monitor and return its text: registers (`r`), memory \
(`m c000 c00f`), disassembly (`d e000`), the debug surfaces (`bk`, `flow`, `bt`), `help` for the full verb list. \
The verbs are TRX64's, the same monitor its own tools speak (docs/specs/S23-monitor.md). `device` selects the CPU: \
the C64 (default), the 1541 (`drive8`), or the firmware's RISC-V (`fw`, registers and memory only). It needs an \
instance started with a C64 (the default). Run control: `g`, `z`/`step`, `n`, `ret`, `until`, `c64 halt|go|step`, \
`fw halt|go|step`.")]
    async fn emu_monitor(&self, Parameters(p): Parameters<MonitorParams>) -> ToolResult {
        finish(self.monitor(p).await)
    }

    #[tool(description = "Send one raw line of ue2emu's TCP control protocol and return its result lines: the \
direct API underneath the other tools. Commands: `wait <ms>`, `button [ms]`, `key <name> [ms]`, `type <text>`, \
`screen`, `png <path>`, plus any command newer emulator builds add (e.g. expect/expect-console). Use it for \
commands no dedicated tool covers; use emu_stop instead of `quit`.")]
    async fn emu_control(&self, Parameters(p): Parameters<ControlParams>) -> ToolResult {
        finish(self.control(p).await)
    }

    #[tool(description = "Sync a usb_dirs stick now: write the guest's changes to its host directory (control \
command usb-sync). With replug=true: unplug the stick, sync, rebuild the volume from the host directory and plug it \
back in (usb-replug), so the firmware sees host changes at once; on a port without usb_dirs this just unplugs and \
plugs in the device. port picks a hub port 1-3, default every usb_dirs stick. A sync that would delete more than \
25 % of the files (or more than 50) is refused and returns FAIL until force=true; overwrites do not count. An \
image that does not parse (firmware crashed mid-write) is never synced. discard=true (with replug) keeps the old \
image aside unsynced. \
Returns PASS with one line per action (written, moved to .ue2-trash, conflict copies).")]
    async fn emu_usb_sync(&self, Parameters(p): Parameters<UsbSyncParams>) -> ToolResult {
        finish(self.usb_sync(p).await)
    }

    #[tool(description = "Describe the cartridge in the instance's physical expansion port (emu_start cart_slot; \
control command cart-info): type, CRT hardware type, model (trx64, trx64-flash or u64-logic), banks, the EXROM/GAME \
lines it drives and the mode they give, U64_CART_DETECT, the bus sharing registers C64_BUS_INTERNAL/EXTERNAL/BRIDGE as \
the firmware last wrote them, the flash command decode, the source file and persist mode, whether flash or EEPROM \
changed and whether that change is written yet.")]
    async fn emu_cart_info(&self, Parameters(p): Parameters<CartInfoParams>) -> ToolResult {
        finish(self.cart_info(p).await)
    }

    #[tool(description = "Write the cartridge in the instance's physical expansion port, as it is now, to a CRT file \
(control command cart-save): the inserted CRT's header and chip packets with the flash and EEPROM contents taken from \
the cartridge, plus packets for flash banks that were programmed outside the inserted CRT. Works in every cart_slot \
mode and never touches the source file. Returns PASS with the path, size and flash generation.")]
    async fn emu_cart_save(&self, Parameters(p): Parameters<CartSaveParams>) -> ToolResult {
        finish(self.cart_save(p).await)
    }
}

#[tool_handler]
impl ServerHandler for Emu {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(INSTRUCTIONS)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Tool implementations

const RESERVED_FLAGS: &[&str] = &[
    "--control",
    "--firmware",
    "--elf",
    "--roms",
    "--flash",
    "--c64-roms",
    "--sd",
    "--net",
    "--hostfwd",
    "--web-port",
    "--speed",
    "--headless",
    "--script",
    "--usb-dir",
    "--usb-dir-work",
    "--cart-slot",
];

/// Default emu_stop wait for an instance with usb_dirs, whose last sync runs before it exits (wall-clock ms).
const USB_DIR_STOP_MS: u64 = 120_000;

/// A `usb_dirs` entry as a `--usb-dir` argument: the path made absolute (the emulator runs in its run directory) and
/// checked to be a directory; trailing `,ro` / `,size=…` options kept.
fn usb_dir_arg(spec: &str) -> Result<String> {
    let mut path = spec;
    let mut options = Vec::new();
    while let Some((head, option)) = path.rsplit_once(',') {
        if option != "ro" && !option.starts_with("size=") {
            break;
        }
        options.push(option);
        path = head;
    }
    let dir = config::resolve(path);
    if !dir.is_dir() {
        bail!("usb_dirs: {} is not a directory", dir.display());
    }
    let mut arg = dir.to_string_lossy().into_owned();
    for option in options.iter().rev() {
        arg.push(',');
        arg.push_str(option);
    }
    Ok(arg)
}

/// `cart_slot` as a `--cart-slot` argument: the paths made absolute (the emulator runs in its run directory), the
/// source checked to be a file, the mode and flash decode checked and appended.
fn cart_slot_arg(p: &CartSlotParams) -> Result<String> {
    let path = config::resolve(&p.path);
    if !path.is_file() {
        bail!("cart_slot: {} is not a file", path.display());
    }
    let mut arg = path.to_string_lossy().into_owned();
    match p.mode.as_deref().unwrap_or("ro") {
        "ro" => {}
        "rw" => arg.push_str(",rw"),
        "save" => {
            let out = p.save_path.as_deref().context("cart_slot: mode \"save\" needs save_path")?;
            arg.push_str(",save=");
            arg.push_str(&config::resolve(out).to_string_lossy());
        }
        other => bail!("cart_slot.mode must be \"ro\", \"rw\" or \"save\", not {other:?}"),
    }
    if p.save_path.is_some() && p.mode.as_deref() != Some("save") {
        bail!("cart_slot.save_path is only used with mode \"save\"");
    }
    if let Some(decode) = &p.flash_decode {
        if !matches!(decode.as_str(), "11" | "15" | "both") {
            bail!("cart_slot.flash_decode must be \"11\", \"15\" or \"both\", not {decode:?}");
        }
        arg.push_str(",flash-decode=");
        arg.push_str(decode);
    }
    Ok(arg)
}

impl Emu {
    async fn start(&self, p: StartParams) -> Result<CallToolResult> {
        let cfg = &self.state.cfg;
        if !cfg.emulator.is_file() {
            bail!(
                "emulator binary {} not found: run `cargo build --release -p ue2emu` in {} (or set UE2EMU_BIN)",
                cfg.emulator.display(),
                cfg.repo.display()
            );
        }
        let (firmware, tree) = config::resolve_firmware(cfg, p.firmware.as_deref())?;
        let roms = match &p.roms {
            Some(r) => config::resolve(r),
            None => tree
                .as_ref()
                .map(|t| t.join("roms"))
                .filter(|r| r.join("chars.bin").is_file())
                .unwrap_or_else(|| cfg.firmware_tree.join("roms")),
        };
        if !roms.join("chars.bin").is_file() {
            bail!("roms directory {} has no chars.bin; pass roms: \"<1541ultimate checkout>/roms\"", roms.display());
        }
        let c64_roms = match &p.c64_roms {
            None | Some(C64Roms::Enabled(false)) => None,
            Some(C64Roms::Enabled(true)) => Some(roms.clone()),
            Some(C64Roms::Dir(dir)) => {
                let dir = config::resolve(dir);
                if !dir.is_dir() {
                    bail!("c64_roms: {} is not a directory", dir.display());
                }
                Some(dir)
            }
        };
        if c64_roms.is_some() && p.flash.as_deref() == Some("none") {
            bail!("c64_roms writes the ROMs into the flash image, and flash \"none\" has none");
        }
        let realtime = match p.speed.as_deref().unwrap_or("max") {
            "max" => false,
            "realtime" => true,
            other => bail!("speed must be \"max\" or \"realtime\", not {other:?}"),
        };
        let sd = p.sd.as_deref().map(config::resolve);
        if let Some(sd) = &sd {
            if !sd.is_file() {
                bail!("SD image {} not found", sd.display());
            }
        }
        let extra_args = p.extra_args.clone().unwrap_or_default();
        for a in &extra_args {
            let flag = a.split('=').next().unwrap_or_default();
            if RESERVED_FLAGS.contains(&flag) {
                bail!("extra_args must not contain {flag}: emu_start sets it (use the matching emu_start parameter)");
            }
        }
        let usb_dirs = p.usb_dirs.iter().flatten().map(|spec| usb_dir_arg(spec)).collect::<Result<Vec<_>>>()?;
        let cart_slot = p.cart_slot.as_ref().map(cart_slot_arg).transpose()?;
        let launch = Launch {
            emulator: cfg.emulator.clone(),
            firmware,
            roms,
            flash: None,
            c64_roms,
            sd,
            usb_dirs,
            usb_dir_work: cfg.run_base.join("usb-dir"),
            cart_slot,
            net: p.net.unwrap_or(false),
            realtime,
            extra_args,
            server_pid: cfg.server_pid,
        };
        let (id, run_dir) = self.reserve_id().await?;
        let result = self.start_reserved(&id, run_dir, launch, &p).await;
        if result.is_err() {
            // Nothing was registered: give the id back to other servers.
            instance::release(&self.state.cfg.run_base, &id, self.state.cfg.server_pid);
        }
        self.state.reserved.lock().unwrap().remove(&id);
        result
    }

    async fn reserve_id(&self) -> Result<(String, PathBuf)> {
        let running: HashSet<String> = self.state.instances.lock().await.keys().cloned().collect();
        let mut reserved = self.state.reserved.lock().unwrap();
        for n in 1..10_000 {
            let id = format!("emu{n}");
            let dir = self.state.cfg.run_base.join(&id);
            if running.contains(&id)
                || reserved.contains(&id)
                || instance::dir_in_use(&dir)
                || !instance::claim(&self.state.cfg.run_base, &id, self.state.cfg.server_pid)
            {
                continue;
            }
            reserved.insert(id.clone());
            return Ok((id, dir));
        }
        bail!("no free instance id")
    }

    async fn start_reserved(&self, id: &str, run_dir: PathBuf, mut launch: Launch, p: &StartParams) -> Result<CallToolResult> {
        let cfg = &self.state.cfg;
        launch.flash = match p.flash.as_deref().unwrap_or("fresh") {
            "fresh" => Some(run_dir.join("flash.bin")),
            "none" => None,
            "persist" => Some(cfg.run_base.join("persist-flash.bin")),
            path => Some(config::resolve(path)),
        };
        if let Some(flash) = &launch.flash {
            let map = self.state.instances.lock().await;
            if let Some(other) = map.values().find(|i| i.exited().is_none() && i.launch.flash.as_ref() == Some(flash)) {
                bail!("flash image {} is in use by running instance {}; stop it first", flash.display(), other.id);
            }
        }
        if run_dir.exists() {
            std::fs::remove_dir_all(&run_dir).with_context(|| format!("clear {}", run_dir.display()))?;
        }
        std::fs::create_dir_all(&run_dir).with_context(|| format!("create {}", run_dir.display()))?;
        if let Some(parent) = launch.flash.as_ref().and_then(|f| f.parent()) {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }

        let mut attempt = 0;
        let inst = loop {
            attempt += 1;
            match Instance::spawn(id.to_string(), run_dir.clone(), launch.clone(), Duration::from_secs(30)).await {
                Ok(inst) => break inst,
                // Another process took a port between allocation and bind: retry with fresh ports.
                Err(e) if attempt < 3 && format!("{e:#}").contains("in use") => continue,
                Err(e) => return Err(e),
            }
        };
        self.state.instances.lock().await.insert(id.to_string(), inst.clone());

        let t0 = Instant::now();
        let marker = p.boot_marker.clone().unwrap_or_else(|| BOOT_MARKER.to_string());
        let timeout = Duration::from_millis(p.boot_timeout_ms.unwrap_or(90_000));
        let mut booted = None;
        if p.wait_for_boot.unwrap_or(true) {
            let deadline = t0 + timeout;
            let mut scan = 0;
            booted = Some(loop {
                let (found, end) = {
                    let ring = inst.stdout.lock().unwrap();
                    (ring.find(scan, marker.as_bytes(), false).is_some(), ring.end())
                };
                if found {
                    break true;
                }
                if inst.exited().is_some() {
                    // The pipes are drained before the exit is published: one last look.
                    break inst.stdout.lock().unwrap().find(scan, marker.as_bytes(), false).is_some();
                }
                if Instant::now() >= deadline {
                    break false;
                }
                scan = end.saturating_sub(marker.len() as u64);
                tokio::time::sleep(Duration::from_millis(50)).await;
            });
        }
        let boot_ms = t0.elapsed().as_millis() as u64;
        let (banner, console_tail) = {
            let ring = inst.stdout.lock().unwrap();
            let head = String::from_utf8_lossy(&ring.read(0, 512 * 1024).1).into_owned();
            let banner: Vec<String> = head.lines().filter(|l| l.contains("***")).take(8).map(str::to_string).collect();
            (banner, tail_text(&ring, 40))
        };
        let mut v = inst.summary();
        v["booted"] = json!(booted);
        v["boot_wait_ms"] = json!(booted.map(|_| boot_ms));
        v["banner"] = json!(banner);
        v["console_offset"] = json!(inst.console_end());
        let exit = inst.exited();
        let headline = match (booted, &exit) {
            (Some(true), _) => format!("STARTED {id}: firmware booted ({boot_ms} ms wall clock)"),
            (_, Some(e)) => format!(
                "FAILED: {id} started but the emulator EXITED ({}) — see stderr below; call emu_stop {{\"id\": \"{id}\"}} after reading",
                e.description
            ),
            (Some(false), None) => format!(
                "BOOT TIMEOUT: {id} is running but {marker:?} did not appear within {} ms — inspect with emu_console/emu_screen, then emu_stop",
                timeout.as_millis()
            ),
            (None, None) => format!("STARTED {id} (not waiting for boot)"),
        };
        let mut blocks = vec![text(headline), text(pretty(&v))];
        if booted == Some(false) {
            blocks.push(text(format!("console tail:\n{console_tail}")));
            blocks.push(text(format!("stderr tail:\n{}", inst.stderr_tail(30))));
            return Ok(CallToolResult::error(blocks));
        }
        ok(blocks)
    }

    async fn stop(&self, p: StopParams) -> Result<CallToolResult> {
        let targets: Vec<Arc<Instance>> = {
            let mut map = self.state.instances.lock().await;
            if p.id == "all" {
                std::mem::take(&mut *map).into_values().collect()
            } else {
                match map.remove(&p.id) {
                    Some(inst) => vec![inst],
                    None => bail!("no instance '{}'; known instances: {}", p.id, known(&map)),
                }
            }
        };
        let mut set = tokio::task::JoinSet::new();
        for inst in targets {
            // usb_dirs instances run a last sync before they exit.
            let default = if inst.launch.usb_dirs.is_empty() { 10_000 } else { USB_DIR_STOP_MS };
            let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(default));
            set.spawn(async move { inst.stop(timeout).await });
        }
        let mut reports = Vec::new();
        while let Some(r) = set.join_next().await {
            reports.push(r.unwrap_or_else(|e| json!({ "error": e.to_string() })));
        }
        ok(vec![text(format!("stopped {} instance(s)", reports.len())), text(pretty(&json!(reports)))])
    }

    async fn list(&self) -> Result<CallToolResult> {
        let instances: Vec<Value> = self.state.instances.lock().await.values().map(|i| i.summary()).collect();
        let cfg = &self.state.cfg;
        let v = json!({
            "instances": instances,
            "config": {
                "repo": cfg.repo, "emulator": cfg.emulator, "default_firmware_tree": cfg.firmware_tree,
                "run_base": cfg.run_base,
            },
        });
        ok(vec![text(pretty(&v))])
    }

    async fn console(&self, p: ConsoleParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let use_stderr = match p.stream.as_deref().unwrap_or("stdout") {
            "stdout" | "console" => false,
            "stderr" => true,
            other => bail!("stream must be \"stdout\" or \"stderr\", not {other:?}"),
        };
        let max = p.max_bytes.unwrap_or(65_536).clamp(1, 1 << 20);
        let (start, bytes, end, retained_from) = {
            let ring = if use_stderr { inst.stderr.lock().unwrap() } else { inst.stdout.lock().unwrap() };
            let (s, b) = match p.since_offset {
                Some(off) => ring.read(off, max),
                None => ring.tail(p.tail_lines.unwrap_or(60), max),
            };
            (s, b, ring.end(), ring.start())
        };
        let next = start + bytes.len() as u64;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let meta = json!({
            "stream": if use_stderr { "stderr" } else { "stdout" },
            "start_offset": start,
            "next_offset": next,
            "end_offset": end,
            "more_available": next < end,
            "lost_bytes": p.since_offset.map_or(0, |o| retained_from.saturating_sub(o)),
            "alive": inst.exited().is_none(),
            "exit": inst.exited(),
            "log_file": inst.run_dir.join(if use_stderr { "stderr.log" } else { "console.log" }),
        });
        let body = if body.is_empty() { "(no output in this range)".to_string() } else { body };
        ok(vec![text(body), text(pretty(&meta))])
    }

    async fn visible(&self, inst: &Instance) -> Result<bool> {
        let path = inst.run_dir.join("probe.png");
        let bytes = inst.render_png(&path, budget(0)).await?;
        Ok(img::overlay_visible(&img::decode_rgb(&bytes)?.2))
    }

    async fn screen(&self, p: ScreenParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let screen = inst.screen(budget(0)).await?;
        let visible = if p.check_visible.unwrap_or(true) { Some(self.visible(&inst).await?) } else { None };
        let head = match visible {
            Some(true) => "overlay: VISIBLE",
            Some(false) => "overlay: HIDDEN (the text is overlay screen RAM, not on display; emu_button opens the menu)",
            None => "overlay: visibility not checked",
        };
        ok(vec![text(format!("{head}\n--- screen ({} rows) ---\n{screen}--- end ---", screen.lines().count()))])
    }

    async fn screenshot(&self, p: ScreenshotParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let scale = p.scale.unwrap_or(2).clamp(1, 4);
        let path = inst.next_shot_path();
        let raw = inst.render_png(&path, budget(0)).await?;
        let (w, h, rgb) = img::decode_rgb(&raw)?;
        let visible = img::overlay_visible(&rgb);
        let png = if scale == 1 {
            raw
        } else {
            let (sw, sh, big) = img::scale(w, h, &rgb, scale);
            let encoded = img::encode_rgb(sw, sh, &big)?;
            std::fs::write(&path, &encoded).with_context(|| format!("write {}", path.display()))?;
            encoded
        };
        let saved_to = match &p.save_to {
            Some(dst) => {
                let dst = config::resolve(dst);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(&dst, &png).with_context(|| format!("write {}", dst.display()))?;
                Some(dst)
            }
            None => None,
        };
        let meta = json!({
            "path": path, "saved_to": saved_to, "width": w * scale, "height": h * scale,
            "native_size": [w, h], "scale": scale, "overlay_visible": visible, "png_bytes": png.len(),
        });
        ok(vec![
            ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&png), "image/png"),
            text(pretty(&meta)),
        ])
    }

    async fn settle(&self, inst: &Instance, settle_ms: u64) -> Result<()> {
        if settle_ms > 0 {
            inst.control(&format!("wait {settle_ms}"), budget(settle_ms)).await?;
        }
        Ok(())
    }

    async fn button(&self, p: ButtonParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let hold = p.hold_ms.unwrap_or(100);
        let settle = p.settle_ms.unwrap_or(300);
        inst.control(&format!("button {hold}"), budget(hold)).await?;
        self.settle(&inst, settle).await?;
        ok(vec![text(format!("menu button held {hold} ms, then {settle} ms settle (emulated)"))])
    }

    async fn key(&self, p: KeyParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        if p.keys.is_empty() {
            bail!("keys is empty");
        }
        let hold = p.hold_ms.unwrap_or(80);
        for (i, k) in p.keys.iter().enumerate() {
            let name = if k == " " { "space" } else { k.as_str() };
            if name.is_empty() || name.contains(char::is_whitespace) {
                bail!("bad key name {k:?}");
            }
            inst.control(&format!("key {name} {hold}"), budget(hold + 80))
                .await
                .with_context(|| format!("key {} of {} ({name})", i + 1, p.keys.len()))?;
        }
        let settle = p.settle_ms.unwrap_or(300);
        self.settle(&inst, settle).await?;
        ok(vec![text(format!("pressed {}; {settle} ms settle (emulated)", p.keys.join(", ")))])
    }

    async fn type_text(&self, p: TypeParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        if p.text.is_empty() {
            bail!("text is empty");
        }
        let lines: Vec<&str> = p.text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            let line = line.trim_end_matches('\r');
            // The control parser trims trailing whitespace, so trailing spaces go out as `key space`.
            let core = line.trim_end_matches(' ');
            if !core.is_empty() {
                inst.control(&format!("type {core}"), budget(core.chars().count() as u64 * 160)).await?;
            }
            for _ in 0..line.len() - core.len() {
                inst.control("key space", budget(200)).await?;
            }
            if i + 1 < lines.len() {
                inst.control("key return", budget(200)).await?;
            }
        }
        let settle = p.settle_ms.unwrap_or(300);
        self.settle(&inst, settle).await?;
        ok(vec![text(format!("typed {} character(s); {settle} ms settle (emulated)", p.text.chars().count()))])
    }

    async fn wait(&self, p: WaitParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let t0 = Instant::now();
        inst.control(&format!("wait {}", p.ms), budget(p.ms)).await?;
        ok(vec![text(format!("ran {} ms emulated in {} ms wall clock", p.ms, t0.elapsed().as_millis()))])
    }

    async fn expect(&self, p: ExpectParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        if p.text.is_empty() {
            bail!("text is empty");
        }
        let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(10_000));
        let ignore_case = p.ignore_case.unwrap_or(false);
        let absent = p.absent.unwrap_or(false);
        let t0 = Instant::now();
        let deadline = t0 + timeout;
        let verdict = |passed: bool| if passed { "PASS" } else { "FAIL" };
        match p.source.as_deref().unwrap_or("screen") {
            "screen" => {
                let need_visible = p.require_visible.unwrap_or(false);
                let mut visible = None;
                let (passed, screen) = loop {
                    let screen = inst.screen(budget(0)).await?;
                    let found = find_bytes(screen.as_bytes(), p.text.as_bytes(), ignore_case).is_some();
                    let passed = if absent {
                        !found
                    } else if found && need_visible {
                        let v = self.visible(&inst).await?;
                        visible = Some(v);
                        v
                    } else {
                        found
                    };
                    if passed || Instant::now() >= deadline {
                        break (passed, screen);
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                };
                let elapsed = t0.elapsed().as_millis() as u64;
                let what = match (absent, passed) {
                    (false, true) => format!("{:?} is on the screen", p.text),
                    (false, false) if visible == Some(false) => {
                        format!("{:?} is in screen RAM but the overlay is HIDDEN ({elapsed} ms)", p.text)
                    }
                    (false, false) => format!("{:?} did not appear on the screen within {} ms", p.text, timeout.as_millis()),
                    (true, true) => format!("{:?} is not on the screen", p.text),
                    (true, false) => format!("{:?} is still on the screen after {} ms", p.text, timeout.as_millis()),
                };
                let meta = json!({
                    "passed": passed, "source": "screen", "elapsed_ms": elapsed, "absent": absent,
                    "overlay_visible": visible, "alive": inst.exited().is_none(),
                });
                ok(vec![
                    text(format!("{}: {what}", verdict(passed))),
                    text(format!("--- screen ---\n{screen}--- end ---")),
                    text(pretty(&meta)),
                ])
            }
            "console" => {
                let needle = p.text.as_bytes();
                let from = p.since_offset.unwrap_or(0);
                let mut scan = from;
                let mut exited_early = false;
                let hit = loop {
                    let (found, end) = {
                        let ring = inst.stdout.lock().unwrap();
                        (ring.find(scan, needle, ignore_case), ring.end())
                    };
                    if found.is_some() {
                        break found;
                    }
                    if inst.exited().is_some() {
                        exited_early = true;
                        break inst.stdout.lock().unwrap().find(scan, needle, ignore_case);
                    }
                    if Instant::now() >= deadline {
                        break None;
                    }
                    scan = end.saturating_sub(needle.len() as u64).max(from);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                };
                let passed = hit.is_some() != absent;
                let elapsed = t0.elapsed().as_millis() as u64;
                let (evidence, end) = {
                    let ring = inst.stdout.lock().unwrap();
                    let evidence = match hit {
                        Some(off) => format!("--- console around offset {off} ---\n{}", context(&ring, off, needle.len())),
                        None => format!("--- console tail ---\n{}", tail_text(&ring, 30)),
                    };
                    (evidence, ring.end())
                };
                let what = match (hit, absent) {
                    (Some(off), false) => format!("{:?} found in the console at offset {off} ({elapsed} ms)", p.text),
                    (Some(off), true) => format!("{:?} appeared in the console at offset {off}", p.text),
                    (None, _) if exited_early => format!(
                        "{:?} not in the console and the emulator EXITED ({})",
                        p.text,
                        inst.exited().map(|e| e.description).unwrap_or_default()
                    ),
                    (None, false) => format!("{:?} did not appear in the console within {} ms", p.text, timeout.as_millis()),
                    (None, true) => format!("{:?} did not appear in the console for {} ms", p.text, timeout.as_millis()),
                };
                let meta = json!({
                    "passed": passed, "source": "console", "elapsed_ms": elapsed, "absent": absent,
                    "searched_from": from, "match_offset": hit,
                    "next_offset": hit.map_or(end, |o| o + needle.len() as u64),
                    "console_end": end, "alive": inst.exited().is_none(),
                });
                ok(vec![text(format!("{}: {what}", verdict(passed))), text(evidence), text(pretty(&meta))])
            }
            other => bail!("source must be \"screen\" or \"console\", not {other:?}"),
        }
    }

    async fn rest(&self, p: RestParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let port = inst.ports.http.ok_or_else(|| {
            anyhow!("instance {} was started without net: true, so it has no forwarded HTTP port", inst.id)
        })?;
        let method = p.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
        if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
            bail!("bad HTTP method {method:?}");
        }
        if !p.path.starts_with('/') || p.path.contains(char::is_whitespace) {
            bail!("path must start with '/' and contain no whitespace (percent-encode spaces as %20)");
        }
        let body = match (&p.body, &p.body_file) {
            (Some(_), Some(_)) => bail!("pass body or body_file, not both"),
            (Some(b), None) => b.clone().into_bytes(),
            (None, Some(f)) => {
                let f = config::resolve(f);
                std::fs::read(&f).with_context(|| format!("read {}", f.display()))?
            }
            (None, None) => Vec::new(),
        };
        let mut headers: Vec<(String, String)> = p.headers.clone().unwrap_or_default().into_iter().collect();
        if let Some(ct) = &p.content_type {
            headers.push(("Content-Type".into(), ct.clone()));
        }
        for (k, v) in &headers {
            if k.is_empty() || k.contains([':', '\r', '\n', ' ']) || v.contains(['\r', '\n']) {
                bail!("bad header {k:?}: {v:?}");
            }
        }
        let req = http::Request { port, method: &method, path: &p.path, headers: &headers, body: &body };
        let resp = http::send(&req, Duration::from_millis(p.timeout_ms.unwrap_or(20_000))).await?;
        let saved_to = match &p.save_body_to {
            Some(dst) => {
                let dst = config::resolve(dst);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(&dst, &resp.body).with_context(|| format!("write {}", dst.display()))?;
                Some(dst)
            }
            None => None,
        };
        const TEXT_LIMIT: usize = 256 * 1024;
        let body_text = match std::str::from_utf8(&resp.body) {
            Ok(s) if s.len() <= TEXT_LIMIT => s.to_string(),
            Ok(_) => format!(
                "{}\n[... truncated, {} bytes in total; use save_body_to]",
                String::from_utf8_lossy(&resp.body[..TEXT_LIMIT]),
                resp.body.len()
            ),
            Err(_) if resp.body.len() <= TEXT_LIMIT => format!(
                "[binary body, {} bytes, base64]\n{}",
                resp.body.len(),
                base64::engine::general_purpose::STANDARD.encode(&resp.body)
            ),
            Err(_) => format!("[binary body, {} bytes; use save_body_to]", resp.body.len()),
        };
        let head: String = resp.headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
        let mut first = format!(
            "HTTP {} {} ({} body bytes, {} attempt(s)) for {method} {} via 127.0.0.1:{port}",
            resp.status,
            resp.reason,
            resp.body.len(),
            resp.attempts,
            p.path
        );
        if let Some(dst) = saved_to {
            first.push_str(&format!("; body saved to {}", dst.display()));
        }
        ok(vec![text(format!("{first}\n{head}\n{body_text}"))])
    }

    async fn usb_sync(&self, p: UsbSyncParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let (replug, force, discard) = (p.replug.unwrap_or(false), p.force.unwrap_or(false), p.discard.unwrap_or(false));
        if discard && !replug {
            bail!("discard needs replug: true");
        }
        if force && replug {
            bail!("force is for a sync: run emu_usb_sync {{force: true}} first, then replug");
        }
        let mut line = String::from(if replug { "usb-replug" } else { "usb-sync" });
        if force {
            line.push_str(" --force");
        }
        if discard {
            line.push_str(" --discard");
        }
        if let Some(port) = p.port {
            line.push_str(&format!(" {port}"));
        }
        match inst.control(&line, Duration::from_millis(p.timeout_ms.unwrap_or(120_000))).await {
            Ok(lines) => ok(vec![text(format!("PASS: {line}\n{}", lines.join("\n")))]),
            Err(e) => match format!("{e:#}").split_once("the emulator rejected ") {
                Some((_, reason)) => ok(vec![text(format!("FAIL: {reason}"))]),
                None => Err(e),
            },
        }
    }

    async fn cart_info(&self, p: CartInfoParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let lines = inst.control("cart-info", Duration::from_secs(30)).await?;
        ok(vec![text(lines.join("\n"))])
    }

    async fn cart_save(&self, p: CartSaveParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let path = config::resolve(&p.path);
        let line = format!("cart-save {}", path.display());
        let lines = inst.control(&line, Duration::from_millis(p.timeout_ms.unwrap_or(60_000))).await?;
        ok(vec![text(format!("PASS: {line}\n{}", lines.join("\n")))])
    }

    /// S23 §7: the monitor over the same control line, so a human and an agent read the same text.
    async fn monitor(&self, p: MonitorParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        let line = format!("monitor {}", p.command.trim());
        let lines = inst.control(&line, Duration::from_millis(p.timeout_ms.unwrap_or(60_000))).await?;
        ok(vec![text(if lines.is_empty() { "ok".to_string() } else { lines.join("\n") })])
    }

    async fn control(&self, p: ControlParams) -> Result<CallToolResult> {
        let inst = self.state.get(&p.id).await?;
        if p.command.split_whitespace().next() == Some("quit") {
            bail!("use emu_stop to stop an instance (it also releases the id and ports)");
        }
        let lines = inst.control(&p.command, Duration::from_millis(p.timeout_ms.unwrap_or(60_000))).await?;
        let out = if lines.is_empty() { "ok".to_string() } else { format!("{}\nok", lines.join("\n")) };
        ok(vec![text(out)])
    }
}
