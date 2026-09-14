//! `--usb` / `--usb-dir` / `--usb-keyboard`: devices on the USB hub, and host keys as HID keyboard usages.
//! Spec: docs/specs/S11-S14-later.md §S13, docs/status/usb-dir.md.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Args;
use ue2_core::devices::usb::{UsbConfig, CAPAB_USB_HOST2, HUB_PORTS};
use ue2_core::host::HostInput;
use ue2_core::machine::MachineConfig;
use ue2_vfat::DirSpec;
use winit::keyboard::KeyCode;

#[derive(Args)]
pub struct UsbArgs {
    /// Raw disk image attached as a USB mass-storage device; repeatable, images take the hub ports first
    #[arg(long = "usb", value_name = "IMAGE")]
    images: Vec<PathBuf>,
    /// Host directory as a USB stick: a FAT32 volume built from it, the guest's changes written back
    /// (docs/status/usb-dir.md); repeatable, on the hub ports after the images
    #[arg(long = "usb-dir", value_name = "PATH[,size=SIZE][,ro]", value_parser = parse_dir_spec)]
    dirs: Vec<DirSpec>,
    /// Where --usb-dir keeps its volume images, manifests and snapshots
    #[arg(long = "usb-dir-work", value_name = "DIR", default_value = "run/usb-dir")]
    dir_work: PathBuf,
    /// Attach a USB keyboard; the window sends host keys to it instead of the C64 matrix (F12 stays the menu button)
    #[arg(long)]
    usb_keyboard: bool,
}

fn parse_dir_spec(s: &str) -> Result<DirSpec, String> {
    s.parse()
}

/// Put the devices on the hub and, when there are any, advertise the USB host to the firmware (docs/hw/09 H3).
/// Returns the `--usb-dir` sticks and their work directory; the runner attaches them to the ports left free.
pub fn configure(args: UsbArgs, cfg: &mut MachineConfig) -> Result<(Vec<DirSpec>, PathBuf)> {
    let usb = UsbConfig { images: args.images, storage_slots: args.dirs.len(), keyboard: args.usb_keyboard };
    let devices = usb.devices();
    if devices > HUB_PORTS {
        bail!("--usb/--usb-dir/--usb-keyboard: {devices} devices, but the USB hub has {HUB_PORTS} ports");
    }
    if devices > 0 {
        cfg.capabilities |= CAPAB_USB_HOST2;
    }
    cfg.usb = usb;
    Ok((args.dirs, args.dir_work))
}

/// Control-language name, host key and HID usage (HID Usage Tables, Keyboard/Keypad page 0x07) of every key the
/// USB keyboard offers. The firmware maps usages to menu keys and C64 matrix positions (keyboard_usb.cc:57-75).
const KEYS: &[(&str, KeyCode, u8)] = &[
    ("a", KeyCode::KeyA, 0x04),
    ("b", KeyCode::KeyB, 0x05),
    ("c", KeyCode::KeyC, 0x06),
    ("d", KeyCode::KeyD, 0x07),
    ("e", KeyCode::KeyE, 0x08),
    ("f", KeyCode::KeyF, 0x09),
    ("g", KeyCode::KeyG, 0x0A),
    ("h", KeyCode::KeyH, 0x0B),
    ("i", KeyCode::KeyI, 0x0C),
    ("j", KeyCode::KeyJ, 0x0D),
    ("k", KeyCode::KeyK, 0x0E),
    ("l", KeyCode::KeyL, 0x0F),
    ("m", KeyCode::KeyM, 0x10),
    ("n", KeyCode::KeyN, 0x11),
    ("o", KeyCode::KeyO, 0x12),
    ("p", KeyCode::KeyP, 0x13),
    ("q", KeyCode::KeyQ, 0x14),
    ("r", KeyCode::KeyR, 0x15),
    ("s", KeyCode::KeyS, 0x16),
    ("t", KeyCode::KeyT, 0x17),
    ("u", KeyCode::KeyU, 0x18),
    ("v", KeyCode::KeyV, 0x19),
    ("w", KeyCode::KeyW, 0x1A),
    ("x", KeyCode::KeyX, 0x1B),
    ("y", KeyCode::KeyY, 0x1C),
    ("z", KeyCode::KeyZ, 0x1D),
    ("1", KeyCode::Digit1, 0x1E),
    ("2", KeyCode::Digit2, 0x1F),
    ("3", KeyCode::Digit3, 0x20),
    ("4", KeyCode::Digit4, 0x21),
    ("5", KeyCode::Digit5, 0x22),
    ("6", KeyCode::Digit6, 0x23),
    ("7", KeyCode::Digit7, 0x24),
    ("8", KeyCode::Digit8, 0x25),
    ("9", KeyCode::Digit9, 0x26),
    ("0", KeyCode::Digit0, 0x27),
    ("return", KeyCode::Enter, 0x28),
    ("escape", KeyCode::Escape, 0x29),
    ("backspace", KeyCode::Backspace, 0x2A),
    ("tab", KeyCode::Tab, 0x2B),
    ("space", KeyCode::Space, 0x2C),
    ("-", KeyCode::Minus, 0x2D),
    ("=", KeyCode::Equal, 0x2E),
    ("[", KeyCode::BracketLeft, 0x2F),
    ("]", KeyCode::BracketRight, 0x30),
    ("\\", KeyCode::Backslash, 0x31),
    (";", KeyCode::Semicolon, 0x33),
    ("'", KeyCode::Quote, 0x34),
    ("`", KeyCode::Backquote, 0x35),
    (",", KeyCode::Comma, 0x36),
    (".", KeyCode::Period, 0x37),
    ("/", KeyCode::Slash, 0x38),
    ("capslock", KeyCode::CapsLock, 0x39),
    ("f1", KeyCode::F1, 0x3A),
    ("f2", KeyCode::F2, 0x3B),
    ("f3", KeyCode::F3, 0x3C),
    ("f4", KeyCode::F4, 0x3D),
    ("f5", KeyCode::F5, 0x3E),
    ("f6", KeyCode::F6, 0x3F),
    ("f7", KeyCode::F7, 0x40),
    ("f8", KeyCode::F8, 0x41),
    ("f9", KeyCode::F9, 0x42),
    ("f10", KeyCode::F10, 0x43),
    ("f11", KeyCode::F11, 0x44),
    ("f12", KeyCode::F12, 0x45),
    ("printscreen", KeyCode::PrintScreen, 0x46),
    ("scrolllock", KeyCode::ScrollLock, 0x47),
    ("pause", KeyCode::Pause, 0x48),
    ("insert", KeyCode::Insert, 0x49),
    ("home", KeyCode::Home, 0x4A),
    ("pageup", KeyCode::PageUp, 0x4B),
    ("delete", KeyCode::Delete, 0x4C),
    ("end", KeyCode::End, 0x4D),
    ("pagedown", KeyCode::PageDown, 0x4E),
    ("right", KeyCode::ArrowRight, 0x4F),
    ("left", KeyCode::ArrowLeft, 0x50),
    ("down", KeyCode::ArrowDown, 0x51),
    ("up", KeyCode::ArrowUp, 0x52),
    ("lctrl", KeyCode::ControlLeft, 0xE0),
    ("lshift", KeyCode::ShiftLeft, 0xE1),
    ("lalt", KeyCode::AltLeft, 0xE2),
    ("lgui", KeyCode::SuperLeft, 0xE3),
    ("rctrl", KeyCode::ControlRight, 0xE4),
    ("rshift", KeyCode::ShiftRight, 0xE5),
    ("ralt", KeyCode::AltRight, 0xE6),
    ("rgui", KeyCode::SuperRight, 0xE7),
];

/// Control-language USB key name (`a`, `return`, `f10`, `lshift`, …), case-insensitive → HID usage.
pub fn usage_by_name(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    KEYS.iter().find(|(n, _, _)| *n == lower).map(|&(_, _, usage)| usage)
}

/// Host key (physical position) → HID usage.
fn host_usage(code: KeyCode) -> Option<u8> {
    KEYS.iter().find(|(_, c, _)| *c == code).map(|&(_, _, usage)| usage)
}

/// Host keys held in the window, pressed on the USB keyboard. A key pressed twice without a release presses once.
#[derive(Default)]
pub struct UsbKeys {
    held: HashSet<u8>,
}

impl UsbKeys {
    /// The event for a host key change, if the key exists on the USB keyboard and its state changed.
    pub fn key(&mut self, code: KeyCode, down: bool) -> Option<HostInput> {
        let usage = host_usage(code)?;
        let changed = if down { self.held.insert(usage) } else { self.held.remove(&usage) };
        changed.then_some(HostInput::UsbKey { usage, down })
    }

    /// Releases for every held key (the window lost focus).
    pub fn release_all(&mut self) -> Vec<HostInput> {
        self.held.drain().map(|usage| HostInput::UsbKey { usage, down: false }).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn args(images: &[&str], usb_keyboard: bool) -> UsbArgs {
        UsbArgs {
            images: images.iter().map(PathBuf::from).collect(),
            dirs: Vec::new(),
            dir_work: PathBuf::from("run/usb-dir"),
            usb_keyboard,
        }
    }

    #[test]
    fn configure_sets_the_capability_only_with_devices() {
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        let caps = cfg.capabilities;
        configure(args(&[], false), &mut cfg).unwrap();
        assert_eq!((cfg.capabilities, cfg.usb.devices()), (caps, 0));
        configure(args(&["a.img", "b.img"], true), &mut cfg).unwrap();
        assert_eq!(cfg.capabilities, caps | CAPAB_USB_HOST2);
        assert_eq!((cfg.usb.images[1].as_path(), cfg.usb.keyboard), (Path::new("b.img"), true));
        assert!(configure(args(&["a.img", "b.img", "c.img"], true), &mut cfg).is_err(), "4 devices, 3 ports");
    }

    #[test]
    fn usb_dirs_take_free_ports_after_the_images() {
        let mut cfg = MachineConfig::new(PathBuf::new(), PathBuf::new());
        let dirs = UsbArgs { dirs: vec!["share".parse().unwrap(), "other,ro".parse().unwrap()], ..args(&["a.img"], false) };
        let (specs, work) = configure(dirs, &mut cfg).unwrap();
        assert_eq!((cfg.usb.images.len(), cfg.usb.storage_slots, cfg.usb.devices()), (1, 2, 3));
        assert_eq!((specs[1].read_only, work.as_path()), (true, Path::new("run/usb-dir")));
        assert_ne!(cfg.capabilities & CAPAB_USB_HOST2, 0);
        let too_many = UsbArgs { dirs: vec!["share".parse().unwrap(); 2], ..args(&["a.img"], true) };
        assert!(configure(too_many, &mut cfg).unwrap_err().to_string().contains("4 devices"));
        assert!(parse_dir_spec("x,size=1").is_err());
    }

    #[test]
    fn names_and_host_keys_map_to_usages() {
        assert_eq!(usage_by_name("a"), Some(0x04));
        assert_eq!(usage_by_name("F10"), Some(0x43));
        assert_eq!(usage_by_name("Return"), Some(0x28));
        assert_eq!(usage_by_name("nope"), None);
        assert_eq!(host_usage(KeyCode::ArrowDown), Some(0x51));
        assert_eq!(host_usage(KeyCode::ShiftRight), Some(0xE5));
        assert_eq!(host_usage(KeyCode::NumpadEnter), None);
        for (i, (name, code, usage)) in KEYS.iter().enumerate() {
            assert!(KEYS[i + 1..].iter().all(|(n, c, u)| n != name && c != code && u != usage), "{name} twice");
        }
    }

    #[test]
    fn held_keys_press_once_and_release_all() {
        let mut keys = UsbKeys::default();
        assert_eq!(keys.key(KeyCode::KeyA, true), Some(HostInput::UsbKey { usage: 0x04, down: true }));
        assert_eq!(keys.key(KeyCode::KeyA, true), None);
        assert_eq!(keys.key(KeyCode::NumpadEnter, true), None, "not on the keyboard");
        assert_eq!(keys.key(KeyCode::KeyB, false), None, "not held");
        keys.key(KeyCode::ShiftLeft, true);
        let released = keys.release_all();
        assert_eq!(released.len(), 2);
        for usage in [0x04, 0xE1] {
            assert!(released.contains(&HostInput::UsbKey { usage, down: false }), "{usage:#x} released");
        }
        assert!(keys.release_all().is_empty());
    }
}
