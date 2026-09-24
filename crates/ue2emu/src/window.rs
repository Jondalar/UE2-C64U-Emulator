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
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::control;
use crate::keymap::{self, MatrixKey, LSHIFT};
use crate::runner::{self, Command, ControlHandle, EmuHandle, RunOptions};
use crate::usb::UsbKeys;

/// S32: the host key that lets a captured mouse go again (the window captures it on a click).
const MOUSE_RELEASE_KEY: KeyCode = KeyCode::PageDown;

/// The window opens at twice this (logical pixels) and is never lower than `BASE_H`; its shape then follows
/// the rendered image (see `aspect_snap`).
const BASE_W: u32 = 384;
const BASE_H: u32 = 288;
/// How often the window looks for a new display snapshot; it redraws only when there is one (S26 §3).
const POLL: Duration = Duration::from_millis(4);
const TITLE_EVERY: Duration = Duration::from_millis(500);

/// Open the emulator window on the main thread; returns when it is closed.
pub fn run_window(cfg: MachineConfig, opts: RunOptions) -> Result<()> {
    let font_path = cfg.rom_dir.join("chars.bin");
    let font =
        std::fs::read(&font_path).with_context(|| format!("window: read font {}", font_path.display()))?;
    crate::runner::warn_on_c64_char_rom(&font, &font_path);
    let deadline = opts
        .max_seconds
        .map(|s| Duration::try_from_secs_f64(s).with_context(|| format!("--max-seconds {s}")))
        .transpose()?;
    let event_loop = EventLoop::new().context("window: create event loop")?;

    let started = Instant::now();
    let usb_keys = cfg.usb.keyboard.then(UsbKeys::default);
    let mouse = cfg.usb.mouse.then(MouseCapture::default);
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
        image: (0, 0),
        last_size: (0, 0),
        held: HeldKeys::default(),
        usb_keys,
        mouse,
        menu_down: false,
        restore_down: false,
        window: None,
        surface: None,
        error: None,
        deadline: deadline.map(|d| started + d),
        next_poll: Instant::now(),
        drawn_ms: None,
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
    /// Size of the last rendered image: the ratio the window is held to.
    image: (u32, u32),
    /// Window size before the latest resize, so `aspect_snap` can tell which edge was dragged.
    last_size: (u32, u32),
    held: HeldKeys,
    /// With `--usb-keyboard`, host keys go to the USB keyboard instead of the matrix.
    usb_keys: Option<UsbKeys>,
    /// With `--usb-mouse`, the host mouse drives the USB mouse while the window has captured it (S32).
    mouse: Option<MouseCapture>,
    /// F12 state, released on focus loss like the matrix keys.
    menu_down: bool,
    /// `keymap::RESTORE_KEY` state, released on focus loss too.
    restore_down: bool,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    error: Option<anyhow::Error>,
    deadline: Option<Instant>,
    next_poll: Instant,
    /// `now_ms` of the snapshot last drawn: a snapshot with another one is a new picture.
    drawn_ms: Option<u64>,
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
        self.drawn_ms = Some(snap.now_ms);
        let (sw, sh) = Renderer::canvas(&snap);
        let image = (sw as u32, sh as u32);
        if image != self.image && sw != 0 && sh != 0 {
            // A new output mode brings a new shape: the minimum and the window follow it.
            self.image = image;
            window.set_min_inner_size(Some(LogicalSize::new(BASE_H * image.0 / image.1, BASE_H)));
            if let Some(want) = aspect_snap(size.width, size.height, self.last_size, image, min_size(window, image))
            {
                let _ = window.request_inner_size(PhysicalSize::new(want.0, want.1));
            }
        }
        if surface.resize(w, h).is_err() {
            return;
        }
        // Rendered straight at the size it fills, so each C64 pixel is rounded once (as trx64-cli does).
        let (x0, y0, pw, ph) = place(w.get(), h.get(), image.0, image.1);
        self.renderer.render_scaled(&snap, &mut self.pixels, (pw as usize, ph as usize));
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        put(&self.pixels, (pw, ph), &mut buffer, (w.get(), h.get()), (x0, y0));
        let _ = buffer.present();
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            let mouse = match &self.mouse {
                Some(m) if m.captured => " — mouse captured, PageDown releases it",
                Some(_) => " — click to use the mouse",
                None => "",
            };
            window.set_title(&format!(
                "ue2emu — {:.1} s — {} % — {} MIPS{mouse}",
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
        if code == MOUSE_RELEASE_KEY && self.mouse.as_ref().is_some_and(|m| m.captured) {
            if down {
                self.uncapture();
            }
            return;
        }
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

    /// S32: hide the cursor and lock it to the window, so every host motion goes to the USB mouse.
    fn capture(&mut self) {
        let (Some(window), Some(mouse)) = (&self.window, &mut self.mouse) else { return };
        let locked = window.set_cursor_grab(CursorGrabMode::Locked);
        if locked.is_err() && window.set_cursor_grab(CursorGrabMode::Confined).is_err() {
            return;
        }
        window.set_cursor_visible(false);
        mouse.captured = true;
        self.update_title();
    }

    /// Let the host mouse go, with the USB mouse's buttons released.
    fn uncapture(&mut self) {
        let Some(mouse) = &mut self.mouse else { return };
        if !mouse.captured {
            return;
        }
        mouse.captured = false;
        let release = std::mem::take(&mut mouse.buttons) != 0;
        if let Some(window) = &self.window {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
        }
        if release {
            self.send(vec![HostInput::UsbMouse { dx: 0, dy: 0, wheel: 0, buttons: 0 }]);
        }
        self.update_title();
    }

    /// A click, button or wheel on the window: the first click captures, then everything reaches the USB mouse.
    fn mouse_event(&mut self, event: &WindowEvent) {
        let Some(mouse) = &mut self.mouse else { return };
        let input = match *event {
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if !mouse.captured {
                    if pressed && button == MouseButton::Left {
                        self.capture();
                    }
                    return;
                }
                let bit = match button {
                    MouseButton::Left => ue2_core::devices::usb::BUTTON_LEFT,
                    MouseButton::Right => ue2_core::devices::usb::BUTTON_RIGHT,
                    MouseButton::Middle => ue2_core::devices::usb::BUTTON_MIDDLE,
                    _ => return,
                };
                mouse.buttons = if pressed { mouse.buttons | bit } else { mouse.buttons & !bit };
                HostInput::UsbMouse { dx: 0, dy: 0, wheel: 0, buttons: mouse.buttons }
            }
            WindowEvent::MouseWheel { delta, .. } if mouse.captured => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(p) => p.y / 16.0,
                };
                let wheel = mouse.wheel.take(steps);
                if wheel == 0 {
                    return;
                }
                HostInput::UsbMouse { dx: 0, dy: 0, wheel, buttons: mouse.buttons }
            }
            _ => return,
        };
        self.send(vec![input]);
    }

    /// macOS delivers no key releases for a window that lost focus; lift everything held.
    fn release_all(&mut self) {
        self.uncapture();
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
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => self.mouse_event(&event),
            WindowEvent::Resized(size) => {
                if let Some(window) = &self.window {
                    // winit has no aspect constraint, so the size is corrected after the drag (TRX64 trx64-cli).
                    let min = min_size(window, self.image);
                    if let Some(want) = aspect_snap(size.width, size.height, self.last_size, self.image, min) {
                        let _ = window.request_inner_size(PhysicalSize::new(want.0, want.1));
                    }
                    self.last_size = (size.width, size.height);
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    /// S32: raw motion of a captured mouse, unaffected by the cursor being locked in place.
    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let DeviceEvent::MouseMotion { delta: (dx, dy) } = event else { return };
        let Some(mouse) = self.mouse.as_mut().filter(|m| m.captured) else { return };
        let (dx, dy) = (mouse.x.take(dx), mouse.y.take(dy));
        if dx != 0 || dy != 0 {
            let buttons = mouse.buttons;
            self.send(vec![HostInput::UsbMouse { dx, dy, wheel: 0, buttons }]);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if !self.ctl.running.load(Ordering::Relaxed) || self.deadline.is_some_and(|d| now >= d) {
            el.exit();
            return;
        }
        if now >= self.next_poll {
            self.next_poll = now + POLL;
            let published = self.ctl.display.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).now_ms;
            if let Some(window) = self.window.as_ref().filter(|_| self.drawn_ms != Some(published)) {
                window.request_redraw();
            }
        }
        if now >= self.next_title {
            self.next_title = now + TITLE_EVERY;
            self.update_title();
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_poll.min(self.next_title)));
    }
}

/// S32: the host mouse as the USB mouse sees it: captured or not, the buttons held, and the fractions of motion and
/// wheel not sent yet.
#[derive(Default)]
struct MouseCapture {
    captured: bool,
    buttons: u8,
    x: Fraction,
    y: Fraction,
    wheel: Fraction,
}

/// Host motion in fractional units, handed on in whole ones.
#[derive(Default)]
struct Fraction(f64);

impl Fraction {
    fn take(&mut self, add: f64) -> i32 {
        self.0 += add;
        let whole = self.0.trunc();
        self.0 -= whole;
        whole as i32
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

/// Where an `img_w`×`img_h` frame goes in a `win_w`×`win_h` surface, as (x, y, w, h): as large as fits with
/// its aspect ratio kept, centred. `aspect_snap` keeps the window on that ratio, so it normally fills it.
fn place(win_w: u32, win_h: u32, img_w: u32, img_h: u32) -> (u32, u32, u32, u32) {
    let (ww, wh, iw, ih) = (win_w as u64, win_h as u64, img_w.max(1) as u64, img_h.max(1) as u64);
    let (w, h) = if iw * wh >= ih * ww { (ww, ih * ww / iw) } else { (iw * wh / ih, wh) };
    (((ww - w) / 2) as u32, ((wh - h) / 2) as u32, w as u32, h as u32)
}

/// The smallest window for `image`, in physical pixels: `BASE_H` logical pixels high, as wide as the ratio says.
fn min_size(window: &Window, image: (u32, u32)) -> (u32, u32) {
    if image.0 == 0 || image.1 == 0 {
        return (0, 0);
    }
    let h = (f64::from(BASE_H) * window.scale_factor()).round() as u32;
    (h * image.0 / image.1, h)
}

/// The size a `w`×`h` window should snap to so it keeps the ratio of `image`, or `None` when it already does
/// (after TRX64 trx64-cli `aspect_snap`).
///
/// `prev` is the size before this resize: the axis that changed more is the one being dragged, so it stays and the
/// other follows; correcting the dragged axis would move the corner under the mouse. One pixel of slack, because
/// integer division cannot always land exactly and a snap never satisfied would resize on every frame. Below `min`
/// the window snaps to `min` whole, since clamping one axis would break the ratio.
fn aspect_snap(w: u32, h: u32, prev: (u32, u32), image: (u32, u32), min: (u32, u32)) -> Option<(u32, u32)> {
    let (iw, ih) = image;
    if w == 0 || h == 0 || iw == 0 || ih == 0 {
        return None;
    }
    let want_h = (u64::from(w) * u64::from(ih) / u64::from(iw)) as u32;
    if want_h.abs_diff(h) <= 1 {
        return None;
    }
    let (nw, nh) = if w.abs_diff(prev.0) >= h.abs_diff(prev.1) {
        (w, want_h)
    } else {
        ((u64::from(h) * u64::from(iw) / u64::from(ih)) as u32, h)
    };
    if nw < min.0 || nh < min.1 {
        return Some(min);
    }
    Some((nw, nh))
}

/// Copy the `size` image `src` into the `dst_size` surface at `at`, black around it.
fn put(src: &[u32], size: (u32, u32), dst: &mut [u32], dst_size: (u32, u32), at: (u32, u32)) {
    dst.fill(0);
    let ((w, h), (dw, dh), (x0, y0)) = (size, dst_size, at);
    let (w, h, dw, x0, y0) = (w as usize, h as usize, dw as usize, x0 as usize, y0 as usize);
    if src.len() < w * h || dst.len() < dw * dh as usize || x0 + w > dw || y0 + h > dh as usize {
        return;
    }
    for (y, row) in src.chunks_exact(w.max(1)).take(h).enumerate() {
        dst[(y0 + y) * dw + x0..][..w].copy_from_slice(row);
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
    fn placement_fills_the_window_with_the_aspect_kept() {
        // A window on the image's ratio is filled, at any scale.
        assert_eq!(place(1280, 960, 640, 480), (0, 0, 1280, 960));
        assert_eq!(place(1000, 750, 640, 480), (0, 0, 1000, 750));
        // Off the ratio (before the snap lands): bars on the long side.
        assert_eq!(place(1280, 720, 640, 480), (160, 0, 960, 720));
        assert_eq!(place(640, 480, 1920, 1080), (0, 60, 640, 360));
    }

    #[test]
    fn resizing_holds_the_aspect_ratio() {
        let (vga, hd) = ((640, 480), (1920, 1080));
        let min = (384, 288);
        assert_eq!(aspect_snap(768, 576, (768, 576), vga, min), None, "already 4:3");
        assert_eq!(aspect_snap(1000, 576, (768, 576), vga, min), Some((1000, 750)), "dragged wider: height follows");
        assert_eq!(aspect_snap(768, 700, (768, 576), vga, min), Some((933, 700)), "dragged taller: width follows");
        assert_eq!(aspect_snap(384, 40, min, vga, min), Some(min), "never below the minimum, snapped whole");
        assert_eq!(aspect_snap(768, 576, (768, 576), hd, (512, 288)), Some((768, 432)), "1080p: 16:9");
        assert_eq!(aspect_snap(0, 0, (768, 576), vga, min), None, "minimised");
        assert_eq!(aspect_snap(768, 576, (768, 576), (0, 0), min), None, "no image yet");
    }

    #[test]
    fn put_places_the_image_and_blacks_the_rest() {
        let mut dst = vec![0xFFu32; 4 * 3];
        put(&[0x11, 0x22, 0x33, 0x44], (2, 2), &mut dst, (4, 3), (1, 1));
        #[rustfmt::skip]
        let want = [
            0, 0,    0,    0,
            0, 0x11, 0x22, 0,
            0, 0x33, 0x44, 0,
        ];
        assert_eq!(dst, want);
        put(&[], (0, 0), &mut dst, (4, 3), (0, 0));
        assert!(dst.iter().all(|&p| p == 0), "no frame yet: black");
    }
}
