//! Script / TCP control protocol. Spec: docs/specs/S08-frontend-control.md
//!
//! One command per line. Lines whose first non-blank character is `#` are comments; blank lines are
//! ignored; trailing whitespace is never significant.
//!
//! | Command | Effect |
//! |---|---|
//! | `wait <ms>` | Wait `ms` emulated milliseconds. |
//! | `button [ms]` | Hold the menu button (ITU 0x1000000A bit 6), default 100 ms. |
//! | `key <name> [ms]` | Hold a key (`keymap::key_by_name`), default 80 ms, then a 40 ms release gap. |
//! | `type <text>` | Type the text after the separating space, one key at a time (`keymap::key_for_char`). |
//! | `usbkey <name> [ms]` | Hold a key of the USB keyboard (`usb::usage_by_name`), default 80 ms, then the release gap. |
//! | `screen` | Print `render::text_dump` between `--- screen ---` markers. |
//! | `c64screen` | Print `render::c64_text_dump`, the C64 text screen, between `--- c64 ---` markers (docs/specs/S14-c64-trx64.md §9). |
//! | `png <path>` | Render the current snapshot to an RGB PNG (parent directories created). |
//! | `expect <text> [ms]` | Wait until the screen text contains `text`; timeout default 5000 ms emulated. |
//! | `expect-not <text> [ms]` | Wait until the screen text no longer contains `text`. |
//! | `expect-console <text> [ms]` | Wait until the console output after the previous match contains `text`. |
//! | `expect-c64 <text> [ms]` | Wait until the C64 text screen (`c64screen`) contains `text`. |
//! | `usb-sync [--force] [port]` | Write a `--usb-dir` stick's guest changes back to the host now (all sticks without a port); `--force` overrides the mass-deletion guard (docs/status/usb-dir.md). |
//! | `usb-replug [--discard] [port]` | Unplug a USB device and plug it back in; a `--usb-dir` stick is synced and rebuilt from the host in between (`--discard`: its old image is kept aside, not synced). |
//! | `cart-info` | Print the physical expansion port's cartridge (`--cart-slot`) as `key: value` lines: type, banks, lines, bus sharing, flash decode, dirty flag (docs/status/cart-slot.md). |
//! | `cart-save <path>` | Write that cartridge as it is now, every bank with flash and EEPROM contents, to a CRT file. |
//! | `quit` | Stop the emulator. |
//!
//! `<text>` is one word, or a double-quoted string with `\"` and `\\` escapes. An `expect` that times out
//! prints the screen and fails. The screen text ignores overlay visibility (docs/hw/05 T1): a menu hidden with
//! RUN/STOP still matches. `expect-console` sees the firmware console since power-on; each match moves its
//! start past the matched text, so two `expect-console` lines need two occurrences, in order.
//!
//! `button`, `key`, `type` and `usbkey` reach the emulation thread as one [`TimedInputs`] sequence that
//! [`InputTimeline`] applies at exact emulated times. Host speed (`--speed max`) and host scheduling can
//! neither stretch a hold into key repeat (keyboard_c64.cc:286-293) or a long button press (docs/hw/05
//! hazard 6), nor shorten it below a keyboard scan.
//!
//! Scripts are parsed completely before the first command runs. Errors, including a failed `expect`, name the
//! line number and make `ue2emu run --headless --script` exit non-zero. Over TCP every line is answered by its
//! result lines and then `ok`, or by `error line <n>: <message>` (lines counted per connection).

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::sync::PoisonError;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ue2_core::devices::usb::HUB_PORTS;
use ue2_core::host::{DisplaySnapshot, HostInput};
use ue2_core::render::{c64_text_dump, text_dump, Renderer};
use ue2_core::time;

use crate::cartslot::CartRequest;
use crate::keymap::{self, MatrixKey, LSHIFT};
use crate::runner::{Command, ControlHandle};
use crate::usb;
use crate::usbdir::{UsbAction, UsbRequest};

/// Default `button` hold: at least one 15 ms menu-loop poll, released well before the 1 s
/// `buttonDownFor` swap-disk threshold (docs/hw/05-ui-overlay-input.md hazard 6).
const BUTTON_MS: u64 = 100;
/// Default `key` hold: four 20 ms scans (docs/hw/05 §C timing, keyboard_c64.cc:316-319), below the
/// ~340 ms first repeat (keyboard_c64.cc:105-106, :286-293).
const KEY_MS: u64 = 80;
/// Emulated gap after a release. The firmware accepts the same key again only after a scan that saw
/// no key (keyboard_c64.cc:256-258); docs/hw/05 §C asks for a release of at least 40 ms.
const RELEASE_MS: u64 = 40;
/// SHIFT leads a shifted key by one scan period: a scan that saw the key without SHIFT would queue the
/// unshifted character, and a later modifier change on the same key is ignored (keyboard_c64.cc:294-297).
const SHIFT_LEAD_MS: u64 = 20;
/// Default `expect*` timeout, in emulated milliseconds.
const EXPECT_MS: u64 = 5000;
/// Console bytes kept for `expect-console`; the log is trimmed back to this once it holds twice as much.
const CONSOLE_KEEP: usize = 1 << 20;
/// Wall-clock poll interval while waiting on emulated time.
const WAIT_POLL: Duration = Duration::from_millis(1);
/// Wall-clock poll interval of the TCP accept loop (it ends once the emulator stops).
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// One parsed control command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlCmd {
    Wait(u64),
    Button(u64),
    Key(MatrixKey, u64),
    Type(Vec<MatrixKey>),
    /// HID usage and hold time.
    UsbKey(u8, u64),
    Screen,
    C64Screen,
    Png(PathBuf),
    /// Text that must appear on the screen within the timeout (ms).
    Expect(String, u64),
    /// Text that must be gone from the screen within the timeout (ms).
    ExpectNot(String, u64),
    /// Text that must appear in the console output, after the previous match, within the timeout (ms).
    ExpectConsole(String, u64),
    /// Text that must appear on the C64 text screen within the timeout (ms).
    ExpectC64(String, u64),
    /// `usb-sync [--force] [port]`.
    UsbSync { port: Option<u8>, force: bool },
    /// `usb-replug [--discard] [port]`.
    UsbReplug { port: Option<u8>, discard: bool },
    /// `cart-info`.
    CartInfo,
    /// `cart-save <path>`.
    CartSave(PathBuf),
    Quit,
}

/// Whether a session goes on after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// What the executor needs from a running emulator: [`HandleTarget`] in production, a fake in tests.
pub trait Target {
    /// Apply a timed input sequence; returns once it has ended in emulated time.
    fn inputs(&mut self, seq: TimedInputs) -> Result<()>;
    /// Return once `ms` more emulated milliseconds have passed.
    fn wait_ms(&mut self, ms: u64) -> Result<()>;
    /// Emulated milliseconds since power-on.
    fn now_ms(&self) -> u64;
    /// Let the emulator run a moment before the next check; fails once it has stopped.
    fn poll(&mut self) -> Result<()>;
    /// Text dump of the current display snapshot.
    fn screen_text(&mut self) -> Result<String>;
    /// [`ConsoleLog::find_after`] on the console output with this target's own mark.
    fn console_match(&mut self, text: &str) -> bool;
    /// C64 text screen of the current display snapshot; empty without a C64.
    fn c64_text(&mut self) -> Result<String>;
    /// Current display snapshot as 0x00RRGGBB pixels: (pixels, width, height).
    fn frame(&mut self) -> Result<(Vec<u32>, usize, usize)>;
    /// Run a `usb-sync` / `usb-replug` request to its end: result lines, and the error of a request that failed.
    fn usb(&mut self, req: UsbRequest) -> Result<(Vec<String>, Option<String>)>;
    /// Run a `cart-info` / `cart-save` request: its result lines, or the reason it failed.
    fn cart(&mut self, req: CartRequest) -> Result<Vec<String>>;
    /// Ask the emulator to stop.
    fn quit(&mut self);
}

/// Parse one line. `Ok(None)` for blank and comment lines.
pub fn parse_line(line: &str) -> Result<Option<ControlCmd>, String> {
    let body = line.trim_end().trim_start();
    if body.is_empty() || body.starts_with('#') {
        return Ok(None);
    }
    let (word, rest) = match body.char_indices().find(|(_, c)| c.is_whitespace()) {
        Some((i, c)) => (&body[..i], &body[i + c.len_utf8()..]),
        None => (body, ""),
    };
    let args: Vec<&str> = rest.split_whitespace().collect();
    let arity = |min: usize, max: usize| {
        if args.len() < min || args.len() > max {
            let want = if min == max { format!("{min}") } else { format!("{min}-{max}") };
            return Err(format!("'{word}' takes {want} argument(s), got {}", args.len()));
        }
        Ok(())
    };
    let ms = |i: usize, default: u64| match args.get(i) {
        None => Ok(default),
        Some(s) => s.parse::<u64>().map_err(|_| format!("'{s}' is not a number of milliseconds")),
    };
    let cmd = match word {
        "wait" => {
            arity(1, 1)?;
            ControlCmd::Wait(ms(0, 0)?)
        }
        "button" => {
            arity(0, 1)?;
            ControlCmd::Button(ms(0, BUTTON_MS)?)
        }
        "key" => {
            arity(1, 2)?;
            let key = keymap::key_by_name(args[0]).ok_or_else(|| format!("unknown key '{}'", args[0]))?;
            ControlCmd::Key(key, ms(1, KEY_MS)?)
        }
        "type" => {
            if rest.is_empty() {
                return Err("'type' needs text".into());
            }
            let keys = rest
                .chars()
                .map(|c| keymap::key_for_char(c).ok_or_else(|| format!("cannot type {c:?}")))
                .collect::<Result<Vec<_>, _>>()?;
            ControlCmd::Type(keys)
        }
        "usbkey" => {
            arity(1, 2)?;
            let usage = usb::usage_by_name(args[0]).ok_or_else(|| format!("unknown USB key '{}'", args[0]))?;
            ControlCmd::UsbKey(usage, ms(1, KEY_MS)?)
        }
        "screen" => {
            arity(0, 0)?;
            ControlCmd::Screen
        }
        "c64screen" => {
            arity(0, 0)?;
            ControlCmd::C64Screen
        }
        "png" => {
            if rest.trim().is_empty() {
                return Err("'png' needs a path".into());
            }
            ControlCmd::Png(PathBuf::from(rest.trim()))
        }
        "expect" | "expect-not" | "expect-console" | "expect-c64" => {
            let (text, after) = split_text(rest)?;
            if text.is_empty() {
                return Err(format!("'{word}' needs text"));
            }
            let timeout = match after.split_whitespace().collect::<Vec<_>>()[..] {
                [] => EXPECT_MS,
                [s] => s.parse::<u64>().map_err(|_| {
                    format!("'{s}' is not a number of milliseconds (quote text that contains spaces)")
                })?,
                _ => return Err(format!("'{word}' takes a text and an optional timeout; quote text with spaces")),
            };
            match word {
                "expect" => ControlCmd::Expect(text, timeout),
                "expect-not" => ControlCmd::ExpectNot(text, timeout),
                "expect-c64" => ControlCmd::ExpectC64(text, timeout),
                _ => ControlCmd::ExpectConsole(text, timeout),
            }
        }
        "usb-sync" | "usb-replug" => {
            let flag = if word == "usb-sync" { "--force" } else { "--discard" };
            let (mut port, mut flagged) = (None, false);
            for arg in &args {
                if *arg == flag && !flagged {
                    flagged = true;
                } else if port.is_none() && !arg.starts_with('-') {
                    let valid = arg.parse::<u8>().ok().filter(|p| (1..=HUB_PORTS as u8).contains(p));
                    port = Some(valid.ok_or_else(|| format!("'{arg}' is not a USB hub port (1-{HUB_PORTS})"))?);
                } else {
                    return Err(format!("'{word}' takes [{flag}] [port]"));
                }
            }
            if word == "usb-sync" {
                ControlCmd::UsbSync { port, force: flagged }
            } else {
                ControlCmd::UsbReplug { port, discard: flagged }
            }
        }
        "cart-info" => {
            arity(0, 0)?;
            ControlCmd::CartInfo
        }
        "cart-save" => {
            if rest.trim().is_empty() {
                return Err("'cart-save' needs a path".into());
            }
            ControlCmd::CartSave(PathBuf::from(rest.trim()))
        }
        "quit" => {
            arity(0, 0)?;
            ControlCmd::Quit
        }
        other => return Err(format!("unknown command '{other}'")),
    };
    Ok(Some(cmd))
}

/// Split off a text argument: one word, or a double-quoted string with `\"` and `\\` escapes. Returns the
/// text and what follows it.
fn split_text(rest: &str) -> Result<(String, &str), String> {
    let rest = rest.trim_start();
    let Some(quoted) = rest.strip_prefix('"') else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return Ok((rest[..end].to_string(), &rest[end..]));
    };
    let mut text = String::new();
    let mut chars = quoted.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((text, &quoted[i + 1..])),
            '\\' => match chars.next() {
                Some((_, e @ ('"' | '\\'))) => text.push(e),
                _ => return Err(r#"only \" and \\ escapes are allowed in quoted text"#.into()),
            },
            c => text.push(c),
        }
    }
    Err("unterminated quoted text".into())
}

/// Parse a whole script, keeping 1-based line numbers.
fn parse_script(text: &str) -> Result<Vec<(usize, ControlCmd)>> {
    let mut cmds = Vec::new();
    for (i, line) in text.lines().enumerate() {
        match parse_line(line) {
            Ok(Some(cmd)) => cmds.push((i + 1, cmd)),
            Ok(None) => {}
            Err(e) => bail!("line {}: {e}", i + 1),
        }
    }
    Ok(cmds)
}

/// Execute one command, writing its result lines to `out`.
pub fn execute(t: &mut dyn Target, cmd: &ControlCmd, out: &mut dyn Write) -> Result<Flow> {
    match cmd {
        ControlCmd::Wait(ms) => t.wait_ms(*ms)?,
        ControlCmd::Button(ms) => t.inputs(TimedInputs {
            events: vec![(0, HostInput::MenuButton(true)), (*ms, HostInput::MenuButton(false))],
            len_ms: *ms,
        })?,
        ControlCmd::Key(key, ms) => {
            let mut seq = TimedInputs::default();
            seq.tap(*key, *ms);
            t.inputs(seq)?;
        }
        ControlCmd::Type(keys) => {
            let mut seq = TimedInputs::default();
            for key in keys {
                seq.tap(*key, KEY_MS);
            }
            t.inputs(seq)?;
        }
        ControlCmd::UsbKey(usage, ms) => t.inputs(TimedInputs {
            events: vec![
                (0, HostInput::UsbKey { usage: *usage, down: true }),
                (*ms, HostInput::UsbKey { usage: *usage, down: false }),
            ],
            len_ms: *ms + RELEASE_MS,
        })?,
        ControlCmd::Screen => print_screen(t, out)?,
        ControlCmd::C64Screen => print_block(out, "c64", &t.c64_text()?)?,
        ControlCmd::Png(path) => {
            let (pixels, w, h) = t.frame()?;
            write_png(path, &pixels, w, h)?;
        }
        ControlCmd::Expect(text, ms) => {
            if !poll_until(t, *ms, |t| Ok(t.screen_text()?.contains(text.as_str())))? {
                print_screen(t, out)?;
                bail!("expect {text:?}: not on the screen within {ms} ms emulated");
            }
        }
        ControlCmd::ExpectNot(text, ms) => {
            if !poll_until(t, *ms, |t| Ok(!t.screen_text()?.contains(text.as_str())))? {
                print_screen(t, out)?;
                bail!("expect-not {text:?}: still on the screen after {ms} ms emulated");
            }
        }
        ControlCmd::ExpectConsole(text, ms) => {
            if !poll_until(t, *ms, |t| Ok(t.console_match(text)))? {
                print_screen(t, out)?;
                bail!("expect-console {text:?}: not in the console output within {ms} ms emulated");
            }
        }
        ControlCmd::ExpectC64(text, ms) => {
            if !poll_until(t, *ms, |t| Ok(t.c64_text()?.contains(text.as_str())))? {
                print_block(out, "c64", &t.c64_text()?)?;
                bail!("expect-c64 {text:?}: not on the C64 screen within {ms} ms emulated");
            }
        }
        ControlCmd::UsbSync { port, force } => {
            usb_request(t, out, UsbRequest { action: UsbAction::Sync { force: *force }, port: *port })?
        }
        ControlCmd::UsbReplug { port, discard } => {
            usb_request(t, out, UsbRequest { action: UsbAction::Replug { discard: *discard }, port: *port })?
        }
        ControlCmd::CartInfo => print_lines(out, &t.cart(CartRequest::Info)?)?,
        ControlCmd::CartSave(path) => print_lines(out, &t.cart(CartRequest::Save(path.clone()))?)?,
        ControlCmd::Quit => {
            t.quit();
            return Ok(Flow::Quit);
        }
    }
    out.flush()?;
    Ok(Flow::Continue)
}

/// Run a USB stick request and print its lines; its error fails the command.
fn usb_request(t: &mut dyn Target, out: &mut dyn Write, req: UsbRequest) -> Result<()> {
    let (lines, error) = t.usb(req)?;
    let mut text = String::new();
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    out.write_all(text.as_bytes())?;
    out.flush()?;
    match error {
        Some(e) => bail!("{e}"),
        None => Ok(()),
    }
}

/// Result lines in one write.
fn print_lines(out: &mut dyn Write, lines: &[String]) -> Result<()> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// `text` between `--- <marker> ---` lines, in one write so console output from the emulation thread cannot land
/// inside the block.
fn print_block(out: &mut dyn Write, marker: &str, text: &str) -> Result<()> {
    let mut block = format!("--- {marker} ---\n");
    for line in text.lines() {
        block.push_str(line);
        block.push('\n');
    }
    block.push_str(&format!("--- {marker} ---\n"));
    out.write_all(block.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Write the screen text between `--- screen ---` markers.
fn print_screen(t: &mut dyn Target, out: &mut dyn Write) -> Result<()> {
    let text = t.screen_text()?;
    print_block(out, "screen", &text)
}

/// Check `met` until it holds (true) or `ms` emulated milliseconds have passed (false).
fn poll_until(t: &mut dyn Target, ms: u64, mut met: impl FnMut(&mut dyn Target) -> Result<bool>) -> Result<bool> {
    let deadline = t.now_ms().saturating_add(ms);
    loop {
        // Time before the check: a check that fails at or after the deadline is the last one.
        let now = t.now_ms();
        if met(t)? {
            return Ok(true);
        }
        if now >= deadline {
            return Ok(false);
        }
        t.poll()?;
    }
}

/// A timed input sequence for the emulation thread (`runner::Command::Inputs`), applied by [`InputTimeline`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimedInputs {
    /// `(offset_ms, event)` from the start of the sequence, offsets non-decreasing.
    events: Vec<(u64, HostInput)>,
    /// The sequence ends this many emulated ms after its start, not before its last event.
    len_ms: u64,
}

impl TimedInputs {
    /// Append a tap of `key`: SHIFT first when it needs one, hold `ms`, release, then the release gap.
    fn tap(&mut self, key: MatrixKey, ms: u64) {
        let ev = |k: MatrixKey, down: bool| HostInput::Key { row: k.row, col: k.col, down };
        let mut at = self.len_ms;
        if key.shift {
            self.events.push((at, ev(LSHIFT, true)));
            at += SHIFT_LEAD_MS;
        }
        self.events.push((at, ev(key, true)));
        at += ms;
        self.events.push((at, ev(key, false)));
        if key.shift {
            self.events.push((at, ev(LSHIFT, false)));
        }
        self.len_ms = at + RELEASE_MS;
    }
}

/// The emulation thread's queue of [`TimedInputs`]. A sequence starts when it arrives, or when the previous
/// sequence ends if that is later, so its holds keep their emulated length however late it was sent.
#[derive(Default)]
pub struct InputTimeline {
    /// Queued events at absolute emulated ms, in order.
    events: VecDeque<(u64, HostInput)>,
    /// End ms of each queued sequence with the sender its `Target::inputs` call waits on, in order.
    ends: VecDeque<(u64, Sender<()>)>,
    /// End of the last queued sequence.
    end_ms: u64,
}

impl InputTimeline {
    /// Queue `seq`, arriving at `now_ms`; `done` receives `()` when it ends.
    pub fn push(&mut self, seq: TimedInputs, done: Sender<()>, now_ms: u64) {
        let start = self.end_ms.max(now_ms);
        self.events.extend(seq.events.into_iter().map(|(at, ev)| (start + at, ev)));
        self.end_ms = start + seq.len_ms;
        self.ends.push_back((self.end_ms, done));
    }

    /// Apply every event due at `now_ms` in order, then signal the sequences that have ended.
    pub fn advance(&mut self, now_ms: u64, mut apply: impl FnMut(HostInput)) {
        while self.events.front().is_some_and(|&(at, _)| at <= now_ms) {
            if let Some((_, ev)) = self.events.pop_front() {
                apply(ev);
            }
        }
        while self.ends.front().is_some_and(|&(end, _)| end <= now_ms) {
            if let Some((_, done)) = self.ends.pop_front() {
                // The waiting client may be gone (TCP disconnect); the sequence has run all the same.
                let _ = done.send(());
            }
        }
    }

    /// Instructions to run from `now_clocks` so the slice ends at the next queued event or sequence end,
    /// between 1 and `max`.
    pub fn slice_insns(&self, now_clocks: u64, clocks_per_insn: u64, max: u64) -> u64 {
        let next = self.events.front().map(|e| e.0).into_iter().chain(self.ends.front().map(|e| e.0)).min();
        match next {
            None => max,
            Some(ms) => {
                time::ms_to_clocks(ms).saturating_sub(now_clocks).div_ceil(clocks_per_insn.max(1)).clamp(1, max)
            }
        }
    }
}

/// Firmware console output since power-on (the emulation thread also copies it to stdout), kept for
/// `expect-console`. Offsets count every byte ever pushed; bytes older than [`CONSOLE_KEEP`] may be dropped.
#[derive(Debug, Default)]
pub struct ConsoleLog {
    /// Bytes dropped from the front.
    dropped: u64,
    bytes: Vec<u8>,
}

impl ConsoleLog {
    pub fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > 2 * CONSOLE_KEEP {
            let cut = self.bytes.len() - CONSOLE_KEEP;
            self.bytes.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    /// True when non-empty `text` starts at or after offset `*mark`; `*mark` then moves just past the first
    /// such occurrence.
    pub fn find_after(&self, text: &str, mark: &mut u64) -> bool {
        let needle = text.as_bytes();
        let start = mark.saturating_sub(self.dropped).min(self.bytes.len() as u64) as usize;
        match self.bytes[start..].windows(needle.len().max(1)).position(|w| w == needle) {
            Some(i) => {
                *mark = self.dropped + (start + i + needle.len()) as u64;
                true
            }
            None => false,
        }
    }
}

/// Encode 0x00RRGGBB pixels as an 8-bit RGB PNG, creating parent directories.
pub fn write_png(path: &Path, pixels: &[u32], w: usize, h: usize) -> Result<()> {
    if w == 0 || h == 0 || pixels.len() < w * h {
        bail!("png: no frame to write ({w}x{h}, {} pixels)", pixels.len());
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("png: create {}", dir.display()))?;
    }
    let file = File::create(path).with_context(|| format!("png: create {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let rgb: Vec<u8> =
        pixels[..w * h].iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgb)?;
    writer.finish()?;
    Ok(())
}

/// [`Target`] over a running emulator's [`ControlHandle`].
pub struct HandleTarget {
    ctl: ControlHandle,
    /// Built from `chars.bin` in `ControlHandle::rom_dir` on the first `png`.
    renderer: Option<Renderer>,
    /// Console offset just past the last `expect-console` match.
    console_mark: u64,
}

impl HandleTarget {
    pub fn new(ctl: ControlHandle) -> Self {
        HandleTarget { ctl, renderer: None, console_mark: 0 }
    }

    fn snapshot(&self) -> DisplaySnapshot {
        self.ctl.display.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Target for HandleTarget {
    fn inputs(&mut self, seq: TimedInputs) -> Result<()> {
        let (done, ended) = mpsc::channel();
        self.ctl.commands.send(Command::Inputs { seq, done }).map_err(|_| anyhow!("emulator stopped"))?;
        // An emulation thread that stops first drops the queued sequence and with it `done`.
        ended.recv().map_err(|_| anyhow!("emulator stopped"))
    }

    fn wait_ms(&mut self, ms: u64) -> Result<()> {
        let until = self.now_ms().saturating_add(ms);
        while self.now_ms() < until {
            self.poll()?;
        }
        Ok(())
    }

    fn now_ms(&self) -> u64 {
        self.ctl.now_ms.load(Ordering::Relaxed)
    }

    fn poll(&mut self) -> Result<()> {
        if !self.ctl.running.load(Ordering::Relaxed) {
            bail!("emulator stopped");
        }
        thread::sleep(WAIT_POLL);
        Ok(())
    }

    fn screen_text(&mut self) -> Result<String> {
        Ok(text_dump(&self.snapshot()))
    }

    fn console_match(&mut self, text: &str) -> bool {
        self.ctl.console.lock().unwrap_or_else(PoisonError::into_inner).find_after(text, &mut self.console_mark)
    }

    fn c64_text(&mut self) -> Result<String> {
        Ok(c64_text_dump(&self.snapshot()))
    }

    fn frame(&mut self) -> Result<(Vec<u32>, usize, usize)> {
        let renderer = match self.renderer.take() {
            Some(r) => r,
            None => {
                let path = self.ctl.rom_dir.join("chars.bin");
                let font = fs::read(&path).with_context(|| format!("png: read font {}", path.display()))?;
                Renderer::new(&font)
            }
        };
        let snap = self.snapshot();
        let mut pixels = Vec::new();
        let (w, h) = self.renderer.insert(renderer).render(&snap, &mut pixels);
        Ok((pixels, w, h))
    }

    fn usb(&mut self, req: UsbRequest) -> Result<(Vec<String>, Option<String>)> {
        let (done, result) = mpsc::channel();
        self.ctl.commands.send(Command::Usb { req, done }).map_err(|_| anyhow!("emulator stopped"))?;
        result.recv().map_err(|_| anyhow!("emulator stopped"))
    }

    fn cart(&mut self, req: CartRequest) -> Result<Vec<String>> {
        let (done, result) = mpsc::channel();
        self.ctl.commands.send(Command::Cart { req, done }).map_err(|_| anyhow!("emulator stopped"))?;
        result.recv().map_err(|_| anyhow!("emulator stopped"))?.map_err(|e| anyhow!(e))
    }

    fn quit(&mut self) {
        // The emulation thread may already be gone; then there is nothing left to stop.
        let _ = self.ctl.commands.send(Command::Quit);
    }
}

/// Execute a control script; returns after `quit` or end of file.
pub fn run_script(ctl: &ControlHandle, path: &Path) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("read script {}", path.display()))?;
    let mut target = HandleTarget::new(ctl.clone());
    run_text(&mut target, &text, &mut std::io::stdout()).with_context(|| format!("script {}", path.display()))
}

/// Parse `text` completely, then execute it until `quit` or the end.
fn run_text(t: &mut dyn Target, text: &str, out: &mut dyn Write) -> Result<()> {
    for (line, cmd) in parse_script(text)? {
        if execute(t, &cmd, out).with_context(|| format!("line {line}"))? == Flow::Quit {
            break;
        }
    }
    Ok(())
}

/// Serve the control protocol on `addr` in a background thread.
pub fn serve(ctl: ControlHandle, addr: &str) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(addr).with_context(|| format!("control: bind {addr}"))?;
    eprintln!("control: listening on {}", listener.local_addr()?);
    serve_listener(ctl, listener)
}

/// Accept one client at a time until a client sends `quit` or the emulator stops.
fn serve_listener(ctl: ControlHandle, listener: TcpListener) -> Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    let handle = thread::Builder::new().name("ue2-control".into()).spawn(move || {
        let mut target = HandleTarget::new(ctl.clone());
        while ctl.running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => match session(&mut target, stream) {
                    Ok(Flow::Quit) => break,
                    Ok(Flow::Continue) => {}
                    Err(e) => eprintln!("control: {peer}: {e:#}"),
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(e) => {
                    eprintln!("control: accept: {e}");
                    thread::sleep(ACCEPT_POLL);
                }
            }
        }
    })?;
    Ok(handle)
}

/// One client connection: each line → result lines + `ok`, or `error line <n>: …`.
fn session(t: &mut dyn Target, stream: TcpStream) -> Result<Flow> {
    // Accepted sockets inherit the listener's non-blocking mode on BSD/macOS.
    stream.set_nonblocking(false)?;
    let mut out = BufWriter::new(stream.try_clone()?);
    for (i, line) in BufReader::new(stream).lines().enumerate() {
        let result = parse_line(&line?).map_err(|e| anyhow!(e)).and_then(|cmd| match cmd {
            Some(cmd) => execute(t, &cmd, &mut out),
            None => Ok(Flow::Continue),
        });
        match result {
            Ok(flow) => {
                writeln!(out, "ok")?;
                out.flush()?;
                if flow == Flow::Quit {
                    return Ok(Flow::Quit);
                }
            }
            Err(e) => {
                writeln!(out, "error line {}: {e:#}", i + 1)?;
                out.flush()?;
            }
        }
    }
    Ok(Flow::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::mpsc::TryRecvError;
    use std::sync::{Arc, Mutex};

    /// Emulated time one `Fake::poll` advances.
    const FAKE_POLL_MS: u64 = 10;

    fn k(name: &str) -> MatrixKey {
        keymap::key_by_name(name).unwrap()
    }

    fn key_ev(key: MatrixKey, down: bool) -> HostInput {
        HostInput::Key { row: key.row, col: key.col, down }
    }

    /// Emulator stand-in: emulated time only moves through `inputs`, `wait_ms` and `poll`.
    #[derive(Default)]
    struct Fake {
        now: u64,
        log: Vec<(u64, HostInput)>,
        quit: bool,
        screen: String,
        /// `(at_ms, text)` that replaces `screen` once `now` reaches `at_ms`.
        later_screen: Option<(u64, String)>,
        console: ConsoleLog,
        /// `(at_ms, bytes)` appended to `console` once `now` reaches `at_ms`.
        later_console: Option<(u64, Vec<u8>)>,
        console_mark: u64,
        c64: String,
        /// Emulated time at which the emulator "stops".
        stop_at: Option<u64>,
        /// USB requests received; port 3 fails.
        usb: Vec<UsbRequest>,
        /// Cartridge requests received; a save to `fail.crt` fails.
        cart: Vec<CartRequest>,
    }

    impl Fake {
        fn stopped(&self) -> bool {
            self.stop_at.is_some_and(|t| self.now >= t)
        }

        /// Move emulated time on and deliver the `later_*` changes that came due.
        fn advance(&mut self, ms: u64) -> Result<()> {
            self.now += ms;
            let now = self.now;
            if let Some((_, text)) = self.later_screen.take_if(|(at, _)| *at <= now) {
                self.screen = text;
            }
            if let Some((_, bytes)) = self.later_console.take_if(|(at, _)| *at <= now) {
                self.console.push(&bytes);
            }
            if self.stopped() {
                bail!("emulator stopped");
            }
            Ok(())
        }
    }

    impl Target for Fake {
        fn inputs(&mut self, seq: TimedInputs) -> Result<()> {
            if self.stopped() {
                bail!("emulator stopped");
            }
            let now = self.now;
            self.log.extend(seq.events.into_iter().map(|(at, ev)| (now + at, ev)));
            self.advance(seq.len_ms)
        }

        fn wait_ms(&mut self, ms: u64) -> Result<()> {
            self.advance(ms)
        }

        fn now_ms(&self) -> u64 {
            self.now
        }

        fn poll(&mut self) -> Result<()> {
            self.advance(FAKE_POLL_MS)
        }

        fn screen_text(&mut self) -> Result<String> {
            Ok(self.screen.clone())
        }

        fn console_match(&mut self, text: &str) -> bool {
            self.console.find_after(text, &mut self.console_mark)
        }

        fn c64_text(&mut self) -> Result<String> {
            Ok(self.c64.clone())
        }

        fn frame(&mut self) -> Result<(Vec<u32>, usize, usize)> {
            Ok((vec![0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0x00FF_FFFF], 2, 2))
        }

        fn usb(&mut self, req: UsbRequest) -> Result<(Vec<String>, Option<String>)> {
            self.usb.push(req);
            let error = (req.port == Some(3)).then(|| "port 3: refused".to_string());
            Ok((vec![format!("port {:?}: done", req.port)], error))
        }

        fn cart(&mut self, req: CartRequest) -> Result<Vec<String>> {
            self.cart.push(req.clone());
            match req {
                CartRequest::Save(path) if path == Path::new("fail.crt") => bail!("cart-save: refused"),
                CartRequest::Save(path) => Ok(vec![format!("saved: {}", path.display())]),
                CartRequest::Info => Ok(vec!["type: EasyFlash".into(), "dirty: no".into()]),
            }
        }

        fn quit(&mut self) {
            self.quit = true;
        }
    }

    fn run(fake: &mut Fake, text: &str) -> (Result<()>, String) {
        let mut out = Vec::new();
        let r = run_text(fake, text, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn parses_every_command() {
        use ControlCmd::*;
        let ok = |l: &str| parse_line(l).unwrap();
        assert_eq!(ok("wait 4000"), Some(Wait(4000)));
        assert_eq!(ok("button"), Some(Button(100)));
        assert_eq!(ok("button 50"), Some(Button(50)));
        assert_eq!(ok("key down"), Some(Key(k("down"), 80)));
        assert_eq!(ok("key RETURN 30"), Some(Key(k("return"), 30)));
        assert_eq!(ok("key A"), Some(Key(MatrixKey { row: 1, col: 2, shift: true }, 80)));
        assert_eq!(ok("type Hi 1"), Some(Type(vec![k("H"), k("i"), k("space"), k("1")])));
        assert_eq!(ok("type  x"), Some(Type(vec![k("space"), k("x")])), "one separator, rest verbatim");
        assert_eq!(ok("type #1"), Some(Type(vec![k("#"), k("1")])));
        assert_eq!(ok("screen"), Some(Screen));
        assert_eq!(ok("c64screen"), Some(C64Screen));
        assert_eq!(ok("png run/menu.png"), Some(Png("run/menu.png".into())));
        assert_eq!(ok("png my shots/a.png "), Some(Png("my shots/a.png".into())));
        assert_eq!(ok("expect Ready"), Some(Expect("Ready".into(), 5000)));
        assert_eq!(ok("expect  Ready  250 "), Some(Expect("Ready".into(), 250)));
        assert_eq!(ok(r#"expect "SD      SD Card  Ready" 3000"#), Some(Expect("SD      SD Card  Ready".into(), 3000)));
        assert_eq!(ok(r#"expect-not "a \"q\" \\ b""#), Some(ExpectNot(r#"a "q" \ b"#.into(), 5000)));
        assert_eq!(ok(r#"expect-console "Page: 0 done." 800"#), Some(ExpectConsole("Page: 0 done.".into(), 800)));
        assert_eq!(ok(r#"expect-c64 "UCI OK" 2000"#), Some(ExpectC64("UCI OK".into(), 2000)));
        assert_eq!(ok(r##"expect "#1""##), Some(Expect("#1".into(), 5000)));
        assert_eq!(ok("quit\r"), Some(Quit));
        assert_eq!(ok("  \twait 1  "), Some(Wait(1)));
        for blank in ["", "   ", "# comment", "  # indented comment", "#wait 5"] {
            assert_eq!(ok(blank), None, "{blank:?}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        let cases = [
            ("bogus", "unknown command 'bogus'"),
            ("WAIT 5", "unknown command 'WAIT'"),
            ("wait", "'wait' takes 1 argument(s), got 0"),
            ("wait 1 2", "'wait' takes 1 argument(s), got 2"),
            ("wait -5", "'-5' is not a number"),
            ("wait 1.5", "'1.5' is not a number"),
            ("button x", "'x' is not a number"),
            ("button 1 2", "'button' takes 0-1 argument(s)"),
            ("key", "'key' takes 1-2 argument(s), got 0"),
            ("key nope", "unknown key 'nope'"),
            ("key a b", "'b' is not a number"),
            ("key a 1 2", "'key' takes 1-2 argument(s), got 3"),
            ("type", "'type' needs text"),
            ("type ok\u{e4}", "cannot type 'ä'"),
            ("screen now", "'screen' takes 0 argument(s)"),
            ("c64screen 1", "'c64screen' takes 0 argument(s)"),
            ("png", "'png' needs a path"),
            ("expect", "'expect' needs text"),
            (r#"expect-not """#, "'expect-not' needs text"),
            (r#"expect "open"#, "unterminated quoted text"),
            (r#"expect "a\n""#, r#"only \" and \\ escapes"#),
            ("expect SD Card", "'Card' is not a number of milliseconds (quote text that contains spaces)"),
            ("expect-console a 1 2", "'expect-console' takes a text and an optional timeout"),
            ("cart-info now", "'cart-info' takes 0 argument(s)"),
            ("cart-save  ", "'cart-save' needs a path"),
            ("quit now", "'quit' takes 0 argument(s)"),
        ];
        for (line, want) in cases {
            let err = parse_line(line).expect_err(line);
            assert!(err.contains(want), "{line:?}: got {err:?}, want {want:?}");
        }
    }

    #[test]
    fn usbkey_holds_a_usb_keyboard_key() {
        assert_eq!(parse_line("usbkey F10 30"), Ok(Some(ControlCmd::UsbKey(0x43, 30))));
        assert_eq!(parse_line("usbkey down"), Ok(Some(ControlCmd::UsbKey(0x51, KEY_MS))));
        assert!(parse_line("usbkey nope").unwrap_err().contains("unknown USB key 'nope'"));
        assert!(parse_line("usbkey").unwrap_err().contains("'usbkey' takes 1-2 argument(s), got 0"));
        let mut fake = Fake::default();
        let (r, _) = run(&mut fake, "usbkey return 30\nwait 5");
        r.unwrap();
        let ev = |down| HostInput::UsbKey { usage: 0x28, down };
        assert_eq!(fake.log, vec![(0, ev(true)), (30, ev(false))]);
        assert_eq!(fake.now, 30 + RELEASE_MS + 5);
    }

    #[test]
    fn usb_sync_and_replug_reach_the_target() {
        let sync = |port, force| Some(ControlCmd::UsbSync { port, force });
        assert_eq!(parse_line("usb-sync"), Ok(sync(None, false)));
        assert_eq!(parse_line("usb-sync --force 2"), Ok(sync(Some(2), true)));
        assert_eq!(parse_line("usb-sync 1 --force"), Ok(sync(Some(1), true)));
        assert_eq!(parse_line("usb-replug --discard"), Ok(Some(ControlCmd::UsbReplug { port: None, discard: true })));
        assert!(parse_line("usb-sync 4").unwrap_err().contains("'4' is not a USB hub port (1-3)"));
        assert!(parse_line("usb-replug --force").unwrap_err().contains("'usb-replug' takes [--discard] [port]"));
        assert!(parse_line("usb-sync 1 2").unwrap_err().contains("'usb-sync' takes [--force] [port]"));

        let mut fake = Fake::default();
        let (r, out) = run(&mut fake, "usb-sync --force 1\nusb-replug\nusb-replug 3\nwait 1\n");
        assert_eq!(format!("{:#}", r.unwrap_err()), "line 3: port 3: refused");
        assert_eq!(out, "port Some(1): done\nport None: done\nport Some(3): done\n", "lines print before the error");
        let replug = |port| UsbRequest { action: UsbAction::Replug { discard: false }, port };
        assert_eq!(fake.usb, [UsbRequest { action: UsbAction::Sync { force: true }, port: Some(1) }, replug(None), replug(Some(3))]);
        assert_eq!(fake.now, 0, "the failed line stops the script");
    }

    #[test]
    fn cart_info_and_cart_save_reach_the_target() {
        assert_eq!(parse_line("cart-info"), Ok(Some(ControlCmd::CartInfo)));
        assert_eq!(parse_line("cart-save run/my cart.crt "), Ok(Some(ControlCmd::CartSave("run/my cart.crt".into()))));
        let mut fake = Fake::default();
        let (r, out) = run(&mut fake, "cart-info\ncart-save out.crt\ncart-save fail.crt\nwait 1\n");
        assert_eq!(format!("{:#}", r.unwrap_err()), "line 3: cart-save: refused");
        assert_eq!(out, "type: EasyFlash\ndirty: no\nsaved: out.crt\n");
        assert_eq!(fake.cart, [CartRequest::Info, CartRequest::Save("out.crt".into()), CartRequest::Save("fail.crt".into())]);
    }

    #[test]
    fn script_parse_error_names_line_and_runs_nothing() {
        let mut fake = Fake::default();
        let (r, _) = run(&mut fake, "wait 10\n# comment\n\nkey nope\nwait 5\n");
        let msg = format!("{:#}", r.unwrap_err());
        assert_eq!(msg, "line 4: unknown key 'nope'");
        assert_eq!((fake.now, fake.log.len()), (0, 0));
    }

    #[test]
    fn script_runtime_error_names_line() {
        let mut fake = Fake { stop_at: Some(50), ..Fake::default() };
        let (r, _) = run(&mut fake, "wait 10\n\nwait 100\nwait 1\n");
        assert_eq!(format!("{:#}", r.unwrap_err()), "line 3: emulator stopped");
    }

    #[test]
    fn wait_and_button_use_emulated_time() {
        let mut fake = Fake::default();
        let (r, out) = run(&mut fake, "wait 4000\nbutton\nbutton 30\n");
        r.unwrap();
        assert_eq!(out, "");
        assert_eq!(
            fake.log,
            vec![
                (4000, HostInput::MenuButton(true)),
                (4100, HostInput::MenuButton(false)),
                (4100, HostInput::MenuButton(true)),
                (4130, HostInput::MenuButton(false)),
            ]
        );
        assert_eq!(fake.now, 4130);
    }

    #[test]
    fn key_holds_then_releases() {
        let mut fake = Fake::default();
        run(&mut fake, "key down\nkey return 200").0.unwrap();
        let (down, ret) = (k("down"), k("return"));
        assert_eq!(
            fake.log,
            vec![
                (0, key_ev(down, true)),
                (80, key_ev(down, false)),
                (120, key_ev(ret, true)),
                (320, key_ev(ret, false)),
            ]
        );
        assert_eq!(fake.now, 360);
    }

    #[test]
    fn shifted_key_leads_with_shift() {
        let mut fake = Fake::default();
        run(&mut fake, "key up").0.unwrap();
        let crsr = MatrixKey { row: 0, col: 7, shift: false };
        assert_eq!(
            fake.log,
            vec![
                (0, key_ev(LSHIFT, true)),
                (20, key_ev(crsr, true)),
                (100, key_ev(crsr, false)),
                (100, key_ev(LSHIFT, false)),
            ]
        );
        assert_eq!(fake.now, 140);
    }

    #[test]
    fn type_taps_each_character() {
        let mut fake = Fake::default();
        run(&mut fake, "type aB").0.unwrap();
        let (a, b) = (k("a"), MatrixKey { row: 3, col: 4, shift: false });
        assert_eq!(
            fake.log,
            vec![
                (0, key_ev(a, true)),
                (80, key_ev(a, false)),
                (120, key_ev(LSHIFT, true)),
                (140, key_ev(b, true)),
                (220, key_ev(b, false)),
                (220, key_ev(LSHIFT, false)),
            ]
        );
        assert_eq!(fake.now, 260);
    }

    #[test]
    fn screen_prints_between_markers() {
        let mut fake = Fake { screen: "HELLO\n  MENU\n".into(), c64: "\n READY.\n".into(), ..Fake::default() };
        let (r, out) = run(&mut fake, "screen\nc64screen");
        r.unwrap();
        assert_eq!(out, "--- screen ---\nHELLO\n  MENU\n--- screen ---\n--- c64 ---\n\n READY.\n--- c64 ---\n");
    }

    #[test]
    fn expect_waits_for_the_text_to_come_and_go() {
        let later = (300, "MENU\nSD      SD Card  Ready\n".to_string());
        let mut fake = Fake { screen: "BOOT\n".into(), later_screen: Some(later), ..Fake::default() };
        let (r, out) = run(&mut fake, "expect \"SD Card  Ready\" 1000\nexpect-not BOOT 0\nexpect MENU 0\n");
        r.unwrap();
        assert_eq!((out.as_str(), fake.now), ("", 300), "passes on the first check that sees the text");

        fake.later_screen = Some((450, "BYE\n".into()));
        run(&mut fake, "expect-not MENU 200\n").0.unwrap();
        assert_eq!(fake.now, 450);
    }

    #[test]
    fn expect_timeout_prints_the_screen_and_names_the_line() {
        let mut fake = Fake { screen: "MENU\n".into(), ..Fake::default() };
        let (r, out) = run(&mut fake, "wait 10\nexpect Ready 500\nquit\n");
        assert_eq!(format!("{:#}", r.unwrap_err()), "line 2: expect \"Ready\": not on the screen within 500 ms emulated");
        assert_eq!(out, "--- screen ---\nMENU\n--- screen ---\n");
        assert!((510..510 + FAKE_POLL_MS).contains(&fake.now), "gave up at the deadline: {}", fake.now);
        assert!(!fake.quit, "nothing after the failed line runs");

        let (r, out) = run(&mut fake, "expect-not MENU 20");
        assert_eq!(format!("{:#}", r.unwrap_err()), "line 1: expect-not \"MENU\": still on the screen after 20 ms emulated");
        assert_eq!(out, "--- screen ---\nMENU\n--- screen ---\n");
    }

    #[test]
    fn expect_c64_reads_the_c64_screen() {
        let mut fake = Fake { screen: "MENU\n".into(), c64: "\n READY.\n".into(), ..Fake::default() };
        run(&mut fake, "expect-c64 READY. 0\n").0.unwrap();
        let (r, out) = run(&mut fake, "expect-c64 MENU 50\n");
        let msg = format!("{:#}", r.unwrap_err());
        assert_eq!(msg, "line 1: expect-c64 \"MENU\": not on the C64 screen within 50 ms emulated");
        assert_eq!(out, "--- c64 ---\n\n READY.\n--- c64 ---\n", "the C64 screen, not the menu");
    }

    #[test]
    fn expect_console_matches_in_order() {
        let mut fake = Fake::default();
        fake.console.push(b"Page: 1 done.\nWriting config store 'User Interface Settings' to flash..");
        fake.later_console = Some((700, b"Page: 0 done.\n".to_vec()));
        let script = "expect-console \"Writing config store 'User Interface Settings'\"\nexpect-console \"Page: 0 done.\" 1000\n";
        run(&mut fake, script).0.unwrap();
        assert_eq!(fake.now, 700);

        let (r, out) = run(&mut fake, "expect-console \"Page: 1 done.\" 50\n");
        let msg = format!("{:#}", r.unwrap_err());
        assert_eq!(msg, "line 1: expect-console \"Page: 1 done.\": not in the console output within 50 ms emulated");
        assert_eq!(out, "--- screen ---\n--- screen ---\n", "an earlier occurrence does not count");
    }

    #[test]
    fn console_log_finds_across_pushes_and_trimming() {
        let mut log = ConsoleLog::default();
        let mut mark = 0;
        log.push(b"abc Pa");
        assert!(!log.find_after("Page", &mut mark));
        log.push(b"ge xyz Page");
        assert!(log.find_after("Page", &mut mark));
        assert_eq!(mark, 8);
        assert!(log.find_after("Page", &mut mark));
        assert_eq!(mark, 17);
        assert!(!log.find_after("Page", &mut mark));

        log.push(&vec![b'.'; 2 * CONSOLE_KEEP]);
        assert_eq!(log.bytes.len(), CONSOLE_KEEP, "trimmed back to CONSOLE_KEEP");
        log.push(b"END");
        assert!(!log.find_after("abc", &mut mark), "dropped bytes are gone");
        assert!(log.find_after("END", &mut mark));
        assert_eq!(mark, 17 + 2 * CONSOLE_KEEP as u64 + 3);
    }

    #[test]
    fn timeline_runs_sequences_back_to_back_at_exact_times() {
        let mut timeline = InputTimeline::default();
        let (done, ended) = mpsc::channel();
        let mut tap = TimedInputs::default();
        tap.tap(k("down"), 80);
        let press = TimedInputs {
            events: vec![(0, HostInput::MenuButton(true)), (100, HostInput::MenuButton(false))],
            len_ms: 100,
        };
        timeline.push(tap, done.clone(), 1000);
        // Arrives while the tap still runs: starts at its end (1000 + 80 + 40).
        timeline.push(press, done, 1010);

        let (mut applied, mut ends) = (Vec::new(), Vec::new());
        for now in 1000..1400 {
            timeline.advance(now, |ev| applied.push((now, ev)));
            ends.extend(ended.try_iter().map(|()| now));
        }
        let down = k("down");
        assert_eq!(
            applied,
            vec![
                (1000, key_ev(down, true)),
                (1080, key_ev(down, false)),
                (1120, HostInput::MenuButton(true)),
                (1220, HostInput::MenuButton(false)),
            ]
        );
        assert_eq!(ends, vec![1120, 1220]);

        // Idle again: the next sequence starts on arrival, and dropping the timeline disconnects its waiter.
        let (done, ended) = mpsc::channel();
        timeline.push(TimedInputs { events: vec![], len_ms: 10 }, done, 5000);
        timeline.advance(5009, |_| unreachable!());
        drop(timeline);
        assert_eq!(ended.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn timeline_slices_end_at_the_next_event() {
        let mut timeline = InputTimeline::default();
        assert_eq!(timeline.slice_insns(0, 4, 100_000), 100_000, "idle: full slice");
        let (done, _ended) = mpsc::channel();
        timeline.push(TimedInputs { events: vec![(1, HostInput::MenuButton(true))], len_ms: 900 }, done, 0);
        timeline.advance(0, |_| unreachable!());
        let clocks_1ms = time::ms_to_clocks(1);
        assert_eq!(timeline.slice_insns(1, 4, 100_000), (clocks_1ms - 1).div_ceil(4), "stops at the event");
        assert_eq!(timeline.slice_insns(0, 0, 1_000_000), clocks_1ms, "0 clocks per instruction counts as 1");
        timeline.advance(1, |_| {});
        assert_eq!(timeline.slice_insns(clocks_1ms, 4, 100_000), 100_000, "the end at 900 ms is further off");
        assert_eq!(timeline.slice_insns(time::ms_to_clocks(900), 4, 100_000), 1, "due now: at least one");
    }

    #[test]
    fn png_writes_rgb_image() {
        let dir = std::env::temp_dir().join(format!("ue2emu-control-{}", std::process::id()));
        let path = dir.join("nested/shot.png");
        let mut fake = Fake::default();
        run(&mut fake, &format!("png {}", path.display())).0.unwrap();

        let reader = png::Decoder::new(File::open(&path).unwrap());
        let mut reader = reader.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height, info.color_type), (2, 2, png::ColorType::Rgb));
        assert_eq!(&buf[..info.buffer_size()], &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        fs::remove_dir_all(&dir).unwrap();

        assert!(write_png(&dir.join("empty.png"), &[], 0, 0).is_err());
    }

    #[test]
    fn quit_ends_the_script() {
        let mut fake = Fake::default();
        run(&mut fake, "quit\nwait 100\n").0.unwrap();
        assert!(fake.quit);
        assert_eq!(fake.now, 0);
    }

    /// Commands the fake emulation thread received or applied, at their emulated ms (`None` = Quit).
    type EmuLog = Vec<(u64, Option<HostInput>)>;

    /// A `ControlHandle` whose "emulation thread" advances `now_ms` by 1 per loop, applies timed inputs through
    /// a real [`InputTimeline`] and logs them. No `runner::spawn`.
    fn fake_emulator() -> (ControlHandle, JoinHandle<EmuLog>) {
        let (tx, rx) = mpsc::channel();
        let ctl = ControlHandle {
            commands: tx,
            display: Arc::new(Mutex::new(DisplaySnapshot::default())),
            now_ms: Arc::new(AtomicU64::new(0)),
            running: Arc::new(AtomicBool::new(true)),
            console: Arc::default(),
            rom_dir: PathBuf::new(),
        };
        let (now_ms, running) = (ctl.now_ms.clone(), ctl.running.clone());
        let emu = thread::spawn(move || {
            let mut log = Vec::new();
            let mut timeline = InputTimeline::default();
            // Bounded, so a broken test fails instead of hanging.
            while now_ms.load(Ordering::Relaxed) < 600_000 {
                let now = now_ms.load(Ordering::Relaxed);
                loop {
                    match rx.try_recv() {
                        Ok(Command::Input(ev)) => log.push((now, Some(ev))),
                        Ok(Command::Inputs { seq, done }) => timeline.push(seq, done, now),
                        Ok(Command::Usb { done, .. }) => {
                            let _ = done.send((Vec::new(), Some("no USB sticks in the fake".into())));
                        }
                        Ok(Command::Cart { done, .. }) => {
                            let _ = done.send(Err("no cartridge in the fake".into()));
                        }
                        Ok(Command::Quit) => {
                            log.push((now, None));
                            running.store(false, Ordering::Relaxed);
                            return log;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return log,
                    }
                }
                timeline.advance(now, |ev| log.push((now, Some(ev))));
                now_ms.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_micros(100));
            }
            running.store(false, Ordering::Relaxed);
            log
        });
        (ctl, emu)
    }

    #[test]
    fn handle_target_waits_on_emulated_time() {
        let (ctl, emu) = fake_emulator();
        let mut t = HandleTarget::new(ctl.clone());
        t.wait_ms(30).unwrap();
        assert!(ctl.now_ms.load(Ordering::Relaxed) >= 30);
        t.quit();
        assert_eq!(emu.join().unwrap().last(), Some(&(ctl.now_ms.load(Ordering::Relaxed), None)));
        assert_eq!(format!("{}", t.wait_ms(1_000).unwrap_err()), "emulator stopped");
        assert_eq!(format!("{}", t.inputs(TimedInputs::default()).unwrap_err()), "emulator stopped");
    }

    #[test]
    fn handle_target_expects_on_console_and_screen() {
        let (ctl, emu) = fake_emulator();
        ctl.console.lock().unwrap().push(b"*** FPGA Capabilities: 34000222 ***\n");
        let mut t = HandleTarget::new(ctl.clone());
        let mut out = Vec::new();
        run_text(&mut t, "expect-console \"FPGA Capabilities\" 50\n", &mut out).unwrap();
        let err = run_text(&mut t, "expect-console Capabilities 30\n", &mut out).unwrap_err();
        assert_eq!(format!("{err:#}"), "line 1: expect-console \"Capabilities\": not in the console output within 30 ms emulated");

        let start = ctl.now_ms.load(Ordering::Relaxed);
        let err = run_text(&mut t, "expect MENU 40\n", &mut out).unwrap_err();
        assert_eq!(format!("{err:#}"), "line 1: expect \"MENU\": not on the screen within 40 ms emulated");
        assert!(ctl.now_ms.load(Ordering::Relaxed) >= start + 40, "waited out the timeout in emulated time");
        run_text(&mut t, "expect-not MENU 0\n", &mut out).unwrap();
        t.quit();
        emu.join().unwrap();
    }

    #[test]
    fn png_font_comes_from_the_handle_rom_dir() {
        let dir = std::env::temp_dir().join(format!("ue2emu-roms-{}", std::process::id()));
        let (mut ctl, emu) = fake_emulator();
        ctl.rom_dir = dir.clone();
        let mut t = HandleTarget::new(ctl);
        let err = format!("{:#}", t.frame().unwrap_err());
        assert!(err.contains(&dir.join("chars.bin").display().to_string()), "{err}");

        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("chars.bin"), [0u8; 2048]).unwrap();
        let (pixels, w, h) = t.frame().unwrap();
        assert!(w > 0 && h > 0 && pixels.len() == w * h, "{w}x{h}, {} pixels", pixels.len());
        fs::remove_dir_all(&dir).unwrap();
        t.quit();
        emu.join().unwrap();
    }

    #[test]
    fn tcp_protocol_answers_each_line() {
        let (ctl, emu) = fake_emulator();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_listener(ctl, listener).unwrap();

        let stream = TcpStream::connect(addr).unwrap();
        let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
        for (line, want) in [
            ("# hello", "ok"),
            ("key up", "ok"),
            ("bogus", "error line 3: unknown command 'bogus'"),
            ("wait 5\r", "ok"),
            ("expect-not anything 0", "ok"),
            ("expect missing 3", "--- screen ---"),
            ("", "--- screen ---"),
            ("", "error line 6: expect \"missing\": not on the screen within 3 ms emulated"),
            ("quit", "ok"),
        ] {
            if !line.is_empty() {
                writeln!(&stream, "{line}").unwrap();
            }
            assert_eq!(lines.next().unwrap().unwrap(), want, "{line:?}");
        }
        assert!(lines.next().is_none(), "server closes the connection after quit");
        server.join().unwrap();

        let log = emu.join().unwrap();
        let crsr = MatrixKey { row: 0, col: 7, shift: false };
        let events: Vec<_> = log.iter().map(|(_, ev)| ev.clone()).collect();
        assert_eq!(
            events,
            vec![
                Some(key_ev(LSHIFT, true)),
                Some(key_ev(crsr, true)),
                Some(key_ev(crsr, false)),
                Some(key_ev(LSHIFT, false)),
                None,
            ]
        );
        assert_eq!(log[1].0 - log[0].0, SHIFT_LEAD_MS, "shift lead timed on the emulation thread: {log:?}");
        assert_eq!(log[2].0 - log[1].0, KEY_MS, "hold timed on the emulation thread: {log:?}");
        assert!(log[4].0 - log[3].0 >= RELEASE_MS + 5, "release gap and wait 5 elapsed: {log:?}");
    }
}
