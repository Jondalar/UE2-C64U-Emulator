//! IO bus decode for 0x10000000-0x10FFFFFF at 256-byte grain. Spec: docs/specs/S02-core.md

use std::any::Any;

use crate::irq::IrqState;

pub const IO_BASE: u32 = 0x1000_0000;
pub const IO_SIZE: u32 = 0x0100_0000;
pub const IO_GRAIN: u32 = 0x100;

/// Context handed to a device on every access and tick.
pub struct IoCtx<'a> {
    /// Emulated clock in 100 MHz ticks.
    pub now: u64,
    /// PC of the instruction performing the access (0 for ticks).
    pub pc: u32,
    /// Guest DDR (64 MB). DMA masters index with `addr & bus::RAM_MASK`.
    pub ram: &'a mut [u8],
    /// ITU interrupt core; devices drive their source bits here.
    pub irq: &'a mut IrqState,
    /// UART TX bytes, drained by the machine.
    pub console: &'a mut Vec<u8>,
}

pub trait IoDevice: Any {
    fn name(&self) -> &'static str;
    /// `off` is relative to the base of the window the access came through.
    fn read8(&mut self, off: u32, ctx: &mut IoCtx) -> u8;
    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx);
    /// Side-effect-free read for debuggers.
    fn peek8(&self, _off: u32) -> u8 {
        0
    }
    /// Absolute clock at which `tick` must run next, if any.
    fn next_event(&self) -> Option<u64> {
        None
    }
    /// Called once `now >= next_event()`.
    fn tick(&mut self, _ctx: &mut IoCtx) {}
    fn reset(&mut self) {}
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Implements `as_any` / `as_any_mut` inside an `impl IoDevice for T` block.
#[macro_export]
macro_rules! impl_as_any {
    () => {
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
    };
}

struct Window {
    base: u32,
    dev: usize,
}

pub struct IoMap {
    /// Window index + 1 per 256-byte grain; 0 = unmapped.
    grains: Vec<u16>,
    windows: Vec<Window>,
    pub devices: Vec<Box<dyn IoDevice>>,
}

impl Default for IoMap {
    fn default() -> Self {
        Self::new()
    }
}

impl IoMap {
    pub fn new() -> Self {
        IoMap { grains: vec![0; (IO_SIZE / IO_GRAIN) as usize], windows: Vec::new(), devices: Vec::new() }
    }

    /// Add a device and map its primary window. Returns the device index.
    pub fn add(&mut self, base: u32, size: u32, dev: Box<dyn IoDevice>) -> usize {
        self.devices.push(dev);
        let idx = self.devices.len() - 1;
        self.map(base, size, idx);
        idx
    }

    /// Map an additional (alias) window onto an existing device. Offsets are relative to `base`.
    pub fn map(&mut self, base: u32, size: u32, dev: usize) {
        self.map_origin(base, size, dev, base);
    }

    /// Map `base..base+size` onto an existing device, handing it offsets relative to `origin` instead of `base`.
    /// A device mapped at several windows with one origin sees absolute offsets and can tell the windows apart
    /// (docs/specs/S14-c64-trx64.md §2).
    pub fn map_origin(&mut self, base: u32, size: u32, dev: usize, origin: u32) {
        assert!(origin <= base, "IO window {base:#010x} below its origin {origin:#010x}");
        assert!(
            base >= IO_BASE && size > 0 && base.checked_add(size).is_some_and(|end| end <= IO_BASE + IO_SIZE),
            "IO window {base:#010x}+{size:#x} outside IO space"
        );
        assert!(base % IO_GRAIN == 0 && size % IO_GRAIN == 0, "IO window {base:#010x}+{size:#x} not grain aligned");
        self.windows.push(Window { base: origin, dev });
        let w = self.windows.len() as u16;
        for g in ((base - IO_BASE) / IO_GRAIN)..((base + size - IO_BASE) / IO_GRAIN) {
            let slot = &mut self.grains[g as usize];
            assert!(
                *slot == 0,
                "IO window overlap at {:#010x}: {} vs {}",
                IO_BASE + g * IO_GRAIN,
                self.devices[dev].name(),
                self.devices[self.windows[*slot as usize - 1].dev].name()
            );
            *slot = w;
        }
    }

    /// Absolute IO address → (device index, offset within the window).
    #[inline]
    pub fn resolve(&self, addr: u32) -> Option<(usize, u32)> {
        let rel = addr.wrapping_sub(IO_BASE);
        if rel >= IO_SIZE {
            return None;
        }
        match self.grains[(rel / IO_GRAIN) as usize] {
            0 => None,
            w => {
                let win = &self.windows[w as usize - 1];
                Some((win.dev, addr - win.base))
            }
        }
    }

    pub fn get<T: IoDevice>(&self) -> Option<&T> {
        self.devices.iter().find_map(|d| d.as_any().downcast_ref::<T>())
    }

    pub fn get_mut<T: IoDevice>(&mut self) -> Option<&mut T> {
        self.devices.iter_mut().find_map(|d| d.as_any_mut().downcast_mut::<T>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sink;

    impl IoDevice for Sink {
        fn name(&self) -> &'static str {
            "sink"
        }

        fn read8(&mut self, _off: u32, _ctx: &mut IoCtx) -> u8 {
            0
        }

        fn write8(&mut self, _off: u32, _val: u8, _ctx: &mut IoCtx) {}

        crate::impl_as_any!();
    }

    #[test]
    fn map_origin_hands_offsets_from_the_origin() {
        let mut map = IoMap::new();
        let dev = map.add(0x1000_0100, 0x100, Box::new(Sink));
        map.map_origin(0x1005_0000, 0x1_0000, dev, IO_BASE);
        map.map_origin(0x1018_8000, 0x2000, dev, IO_BASE);
        map.map(0x1000_0300, 0x100, dev);
        assert_eq!(map.resolve(0x1000_0142), Some((dev, 0x42)), "add: window-relative");
        assert_eq!(map.resolve(0x1000_0342), Some((dev, 0x42)), "map: window-relative");
        assert_eq!(map.resolve(0x1005_D012), Some((dev, 0x5_D012)));
        assert_eq!(map.resolve(0x1018_9FFF), Some((dev, 0x18_9FFF)));
        assert_eq!(map.resolve(0x1018_A000), None);
    }

    #[test]
    #[should_panic(expected = "below its origin")]
    fn map_origin_rejects_an_origin_above_the_window() {
        let mut map = IoMap::new();
        let dev = map.add(0x1000_0000, 0x100, Box::new(Sink));
        map.map_origin(0x1000_0100, 0x100, dev, 0x1000_0200);
    }
}
