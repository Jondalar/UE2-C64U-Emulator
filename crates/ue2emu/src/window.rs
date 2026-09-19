//! Native window (winit + softbuffer). Spec: docs/specs/S08-frontend-control.md
//!
//! winit owns the main thread (required on macOS); the emulator runs on the `runner` thread. The window
//! renders the latest `DisplaySnapshot` at 50 Hz into a 4:3 letterboxed area with integer scaling
//! (opens at 2×), shows emulated time and MIPS in the title, feeds host keys through `keymap` as
//! `HostInput::Key` (with `--usb-keyboard` through `usb` as `HostInput::UsbKey` instead), and maps F12 to the
//! menu button and Page Up to the C64 RESTORE key (S14 §6), with or without `--usb-keyboard`. The C64 frame is
//! composited under the overlay by `Renderer::render` (S14 §9). A `--script` runs on its own thread and
//! `--control` serves TCP, both through `control`. The window closes when the emulator stops (script
//! `quit`, fault halt) or after `--max-seconds`; closing it sends Quit, joins, and prints stats.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use softbuffer::Surface;
use ue2_core::host::HostInput;
use ue2_core::machine::MachineConfig;
use ue2_core::render::Renderer;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::control;
use crate::keymap::{self, MatrixKey, LSHIFT};
use crate::runner::{self, Command, ControlHandle, EmuHandle, RunOptions};
use crate::usb::UsbKeys;

/// 4:3 display area at 1×; the window opens at twice this (logical pixels), which fits the 384×272 C64
/// frame at 2× (docs/specs/S14-c64-trx64.md §9).
const BASE_W: u32 = 384;
const BASE_H: u32 = 288;
/// 50 Hz redraw.
const FRAME: Duration = Duration::from_millis(20);
const TITLE_EVERY: Duration = Duration::from_millis(500);

/// Open the emulator window on the main thread; returns when it is closed.
pub fn run_window(cfg: MachineConfig, opts: RunOptions) -> Result<()> {
    let font_path = cfg.rom_dir.join("chars.bin");
    let font =
        std::fs::read(&font_path).with_context(|| format!("window: read font {}", font_path.display()))?;
    let deadline = opts
        .max_seconds
        .map(|s| Duration::try_from_secs_f64(s).with_context(|| format!("--max-seconds {s}")))
        .transpose()?;
    let event_loop = EventLoop::new().context("window: create event loop")?;

    let started = Instant::now();
    let usb_keys = cfg.usb.keyboard.then(UsbKeys::default);
    // `_audio` keeps the SID audio stream (`--audio`, on by default here) playing until the window has closed.
    let EmuHandle { ctl, mips, speed_pct, join, audio: _audio } = runner::spawn(cfg, &opts)?;
    if let Some(addr) = &opts.control {
        if let Err(e) = control::serve(ctl.clone(), addr) {
            let _ = ctl.commands.send(Command::Quit);
            return Err(e);
        }
    }
    if let Some(path) = opts.script.clone() {
        let script_ctl = ctl.clone();
        std::thread::Builder::new().name("ue2-script".into()).spawn(move || {
            // End of file keeps the window open for interactive use; `quit` stops the emulator itself.
            if let Err(e) = control::run_script(&script_ctl, &path) {
                eprintln!("{e:#}");
                let _ = script_ctl.commands.send(Command::Quit);
            }
        })?;
    }

    let mut app = App {
        ctl: ctl.clone(),
        mips: mips.clone(),
        speed_pct,
        renderer: Renderer::new(&font),
        pixels: Vec::new(),
        held: HeldKeys::default(),
        usb_keys,
        menu_down: false,
        restore_down: false,
        window: None,
        surface: None,
        error: None,
        deadline: deadline.map(|d| started + d),
        next_frame: Instant::now(),
        next_title: Instant::now(),
    };
    let looped = event_loop.run_app(&mut app);
    let window_error = app.error.take();
    drop(app);

    // The emulation thread may have stopped on its own; then the send has no receiver.
    let _ = ctl.commands.send(Command::Quit);
    let emulation = join.join().map_err(|_| anyhow!("emulation thread panicked"))?;
    println!(
        "ue2emu: {:.3} s emulated in {:.3} s wall, {} MIPS",
        ctl.now_ms.load(Ordering::Relaxed) as f64 / 1000.0,
        started.elapsed().as_secs_f64(),
        mips.load(Ordering::Relaxed)
    );
    looped.context("window: event loop")?;
    if let Some(e) = window_error {
        return Err(e);
    }
    emulation
}

struct App {
    ctl: ControlHandle,
    mips: Arc<AtomicU64>,
    speed_pct: Arc<AtomicU64>,
    renderer: Renderer,
    pixels: Vec<u32>,
    held: HeldKeys,
    /// With `--usb-keyboard`, host keys go to the USB keyboard instead of the matrix.
    usb_keys: Option<UsbKeys>,
    /// F12 state, released on focus loss like the matrix keys.
    menu_down: bool,
    /// `keymap::RESTORE_KEY` state, released on focus loss too.
    restore_down: bool,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    error: Option<anyhow::Error>,
    deadline: Option<Instant>,
    next_frame: Instant,
    next_title: Instant,
}

impl App {
    fn create_window(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("ue2emu")
            .with_inner_size(LogicalSize::new(BASE_W * 2, BASE_H * 2))
            .with_min_inner_size(LogicalSize::new(BASE_W, BASE_H));
        let window = Rc::new(el.create_window(attrs).context("window: create")?);
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow!("window: softbuffer context: {e}"))?;
        let surface =
            Surface::new(&context, window.clone()).map_err(|e| anyhow!("window: softbuffer surface: {e}"))?;
        window.request_redraw();
        self.surface = Some(surface);
        self.window = Some(window);
        Ok(())
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        let snap = self.ctl.display.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let (sw, sh) = self.renderer.render(&snap, &mut self.pixels);
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        blit(&self.pixels, sw, sh, &mut buffer, w.get() as usize, h.get() as usize);
        let _ = buffer.present();
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            window.set_title(&format!(
                "ue2emu — {:.1} s — {} % — {} MIPS",
                self.ctl.now_ms.load(Ordering::Relaxed) as f64 / 1000.0,
                self.speed_pct.load(Ordering::Relaxed),
                self.mips.load(Ordering::Relaxed)
            ));
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        // The firmware repeats held keys itself (keyboard_c64.cc:286-300).
        if ev.repeat {
            return;
        }
        let PhysicalKey::Code(code) = ev.physical_key else {
            return;
        };
        let down = ev.state == ElementState::Pressed;
        let inputs = if code == KeyCode::F12 {
            if self.menu_down == down {
                return;
            }
            self.menu_down = down;
            vec![HostInput::MenuButton(down)]
        } else if code == keymap::RESTORE_KEY {
            if self.restore_down == down {
                return;
            }
            self.restore_down = down;
            vec![HostInput::Restore(down)]
        } else if let Some(usb_keys) = &mut self.usb_keys {
            match usb_keys.key(code, down) {
                Some(ev) => vec![ev],
                None => return,
            }
        } else if let Some(key) = keymap::host_key(code) {
            if down {
                self.held.press(code, key)
            } else {
                self.held.release(code)
            }
        } else {
            return;
        };
        self.send(inputs);
    }

    /// macOS delivers no key releases for a window that lost focus; lift everything held.
    fn release_all(&mut self) {
        let mut inputs = self.held.release_all();
        if let Some(usb_keys) = &mut self.usb_keys {
            inputs.extend(usb_keys.release_all());
        }
        if std::mem::take(&mut self.menu_down) {
            inputs.push(HostInput::MenuButton(false));
        }
        if std::mem::take(&mut self.restore_down) {
            inputs.push(HostInput::Restore(false));
        }
        self.send(inputs);
    }

    fn send(&self, inputs: Vec<HostInput>) {
        for ev in inputs {
            // A stopped emulator closes the window on the next `about_to_wait`.
            let _ = self.ctl.commands.send(Command::Input(ev));
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() {
            if let Err(e) = self.create_window(el) {
                self.error = Some(e);
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if accepts(is_synthetic, event.state.is_pressed()) {
                    self.key(&event);
                }
            }
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if !self.ctl.running.load(Ordering::Relaxed) || self.deadline.is_some_and(|d| now >= d) {
            el.exit();
            return;
        }
        if now >= self.next_frame {
            self.next_frame = now + FRAME;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if now >= self.next_title {
            self.next_title = now + TITLE_EVERY;
            self.update_title();
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame.min(self.next_title)));
    }
}

/// Host keys held in the window and the matrix keys they press. Matrix keys are reference counted, so
/// the SHIFT implied by CRSR UP does not lift a physically held Shift key, and a host key pressed twice
/// (no release seen) presses once.
#[derive(Default)]
struct HeldKeys {
    by_host: HashMap<KeyCode, MatrixKey>,
    count: HashMap<(u8, u8), u32>,
}

impl HeldKeys {
    fn press(&mut self, code: KeyCode, key: MatrixKey) -> Vec<HostInput> {
        let mut out = Vec::new();
        if self.by_host.insert(code, key).is_none() {
            if key.shift {
                self.down(LSHIFT, &mut out);
            }
            self.down(key, &mut out);
        }
        out
    }

    fn release(&mut self, code: KeyCode) -> Vec<HostInput> {
        let mut out = Vec::new();
        if let Some(key) = self.by_host.remove(&code) {
            self.up(key, &mut out);
            if key.shift {
                self.up(LSHIFT, &mut out);
            }
        }
        out
    }

    fn release_all(&mut self) -> Vec<HostInput> {
        let codes: Vec<KeyCode> = self.by_host.keys().copied().collect();
        codes.into_iter().flat_map(|code| self.release(code)).collect()
    }

    fn down(&mut self, key: MatrixKey, out: &mut Vec<HostInput>) {
        let n = self.count.entry((key.row, key.col)).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push(HostInput::Key { row: key.row, col: key.col, down: true });
        }
    }

    fn up(&mut self, key: MatrixKey, out: &mut Vec<HostInput>) {
        if let Some(n) = self.count.get_mut(&(key.row, key.col)) {
            *n -= 1;
            if *n == 0 {
                self.count.remove(&(key.row, key.col));
                out.push(HostInput::Key { row: key.row, col: key.col, down: false });
            }
        }
    }
}

/// Where an `img_w`×`img_h` frame goes in a `win_w`×`win_h` surface, as (x, y, w, h): the largest
/// integer scale that fits the centred 4:3 display area, centred. A frame larger than that area is
/// shrunk to fit with its aspect ratio kept.
fn place(win_w: u32, win_h: u32, img_w: u32, img_h: u32) -> (u32, u32, u32, u32) {
    let (ww, wh, iw, ih) = (win_w as u64, win_h as u64, img_w.max(1) as u64, img_h.max(1) as u64);
    let (aw, ah) = if ww * 3 >= wh * 4 { (wh * 4 / 3, wh) } else { (ww, ww * 3 / 4) };
    let scale = (aw / iw).min(ah / ih);
    let (w, h) = if scale >= 1 {
        (iw * scale, ih * scale)
    } else if iw * ah >= ih * aw {
        (aw, ih * aw / iw)
    } else {
        (iw * ah / ih, ah)
    };
    (((ww - w) / 2) as u32, ((wh - h) / 2) as u32, w as u32, h as u32)
}

/// Nearest-neighbour copy of `src` (`sw`×`sh`) into `dst` (`dw`×`dh`) at [`place`], black elsewhere.
fn blit(src: &[u32], sw: usize, sh: usize, dst: &mut [u32], dw: usize, dh: usize) {
    dst.fill(0);
    if sw == 0 || sh == 0 || src.len() < sw * sh || dst.len() < dw * dh {
        return;
    }
    let (x0, y0, w, h) = place(dw as u32, dh as u32, sw as u32, sh as u32);
    let (x0, y0, w, h) = (x0 as usize, y0 as usize, w as usize, h as usize);
    let xs: Vec<usize> = (0..w).map(|x| x * sw / w).collect();
    for y in 0..h {
        let src_row = &src[(y * sh / h) * sw..][..sw];
        let dst_row = &mut dst[(y0 + y) * dw + x0..][..w];
        for (d, &sx) in dst_row.iter_mut().zip(&xs) {
            *d = src_row[sx];
        }
    }
}

/// May this key event reach the machine? winit on Windows synthesises the whole keyboard state on focus changes:
/// `Pressed` for every key held when the window gains focus, `Released` for all on focus loss. A synthetic release
/// must pass (a key held while switching away would otherwise stay down); a synthetic press must not, it replays
/// keys the user never struck. macOS synthesises nothing (after TRX64 `trx64-cli` window.rs `accepts`).
const fn accepts(is_synthetic: bool, pressed: bool) -> bool {
    !(is_synthetic && pressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_presses_are_dropped_and_releases_pass() {
        assert!(accepts(false, true) && accepts(false, false), "real key events");
        assert!(accepts(true, false), "a synthetic release frees a key held while focus left");
        assert!(!accepts(true, true), "a synthetic press on focus gain is no keystroke");
    }

    fn key(row: u8, col: u8, down: bool) -> HostInput {
        HostInput::Key { row, col, down }
    }

    #[test]
    fn held_keys_press_and_release() {
        let mut held = HeldKeys::default();
        let up = keymap::host_key(KeyCode::ArrowUp).unwrap();
        let shift = keymap::host_key(KeyCode::ShiftLeft).unwrap();

        assert_eq!(held.press(KeyCode::ShiftLeft, shift), vec![key(1, 7, true)]);
        assert_eq!(held.press(KeyCode::ArrowUp, up), vec![key(0, 7, true)], "SHIFT already down");
        assert_eq!(held.press(KeyCode::ArrowUp, up), vec![], "a second press without release is ignored");
        assert_eq!(held.release(KeyCode::ArrowUp), vec![key(0, 7, false)], "held Shift stays down");
        assert_eq!(held.release(KeyCode::ShiftLeft), vec![key(1, 7, false)]);
        assert_eq!(held.release(KeyCode::ShiftLeft), vec![]);

        let left = keymap::host_key(KeyCode::ArrowLeft).unwrap();
        assert_eq!(held.press(KeyCode::ArrowLeft, left), vec![key(1, 7, true), key(0, 2, true)]);
        assert_eq!(held.release(KeyCode::ArrowLeft), vec![key(0, 2, false), key(1, 7, false)]);
    }

    #[test]
    fn release_all_lifts_everything() {
        let mut held = HeldKeys::default();
        for code in [KeyCode::KeyA, KeyCode::ArrowUp, KeyCode::ShiftRight] {
            held.press(code, keymap::host_key(code).unwrap());
        }
        let mut released = held.release_all();
        released.sort_by_key(|ev| format!("{ev:?}"));
        let mut want = vec![key(1, 2, false), key(0, 7, false), key(1, 7, false), key(6, 4, false)];
        want.sort_by_key(|ev| format!("{ev:?}"));
        assert_eq!(released, want);
        assert!(held.by_host.is_empty() && held.count.is_empty());
    }

    #[test]
    fn placement_is_integer_scaled_in_a_4_3_area() {
        // Default 40×25 grid of 8×9 cells in the 2× window.
        assert_eq!(place(640, 480, 320, 225), (0, 15, 640, 450));
        // Retina backing store of the same window.
        assert_eq!(place(1280, 960, 320, 225), (0, 30, 1280, 900));
        // Wide window: the 4:3 area (960×720) limits the scale to 3.
        assert_eq!(place(1280, 720, 320, 225), (160, 22, 960, 675));
        assert_eq!(place(1600, 900, 320, 225), (320, 112, 960, 675));
        // 720p stretch mode (320×400) only fits at 1×.
        assert_eq!(place(640, 480, 320, 400), (160, 40, 320, 400));
        // 1080p big-font grid (480×575) is larger than the area: shrunk, aspect kept.
        assert_eq!(place(640, 480, 480, 575), (120, 0, 400, 480));
    }

    #[test]
    fn blit_scales_and_letterboxes() {
        let src = [0x11, 0x22];
        let mut dst = vec![0xFFu32; 6 * 4];
        blit(&src, 2, 1, &mut dst, 6, 4);
        #[rustfmt::skip]
        let want = [
            0, 0,    0,    0,    0,    0,
            0, 0x11, 0x11, 0x22, 0x22, 0,
            0, 0x11, 0x11, 0x22, 0x22, 0,
            0, 0,    0,    0,    0,    0,
        ];
        assert_eq!(dst, want);

        blit(&[], 0, 0, &mut dst, 6, 4);
        assert!(dst.iter().all(|&p| p == 0), "no frame yet: black");
    }
}
