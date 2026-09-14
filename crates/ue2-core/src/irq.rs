//! ITU interrupt core (docs/hw/02-itu-uart.md §Interrupts, docs/hw/00-memory-map.md §Interrupts), checked
//! against `fpga/io/itu/vhdl_source/itu.vhd:111-126,130-145,236-253,275`.
//! The ITU device owns the register view; every other device only drives source bits.
//! Internals owned by spec S03.

/// Low bits that are edge-latched: timer (0), USB (2), C64 reset (7). 00-memory-map §3 C2.
pub const EDGE_MASK_DEFAULT: u8 = 0x85;

#[derive(Clone, Debug)]
pub struct IrqState {
    /// ITU_IRQ_GLOBAL bit 0, reset 1 (itu.vhd:256).
    pub global_en: bool,
    /// Low-byte enable mask (ENABLE sets bits, DISABLE clears bits), reset 0 (itu.vhd:136-139,257).
    pub mask: u8,
    /// Low bits that are edge-latched; the others are level. Fixed by `g_edge_write => false`
    /// (ultimate_logic_32.vhd:507-508), so the ITU never writes it.
    pub edge_mask: u8,
    /// Latched edge events, cleared by ITU_IRQ_CLEAR. They latch whatever the mask (itu.vhd:236-243, 02 H7).
    pub flags: u8,
    /// Current level of the low-byte sources driven with `set_level` (`irq_c`, itu.vhd:111).
    pub level: u8,
    /// ITU_IRQ_HIGH_EN.
    pub high_en: u8,
    /// Current level of the high-byte sources.
    pub high_src: u8,
}

impl Default for IrqState {
    fn default() -> Self {
        Self::new()
    }
}

impl IrqState {
    pub fn new() -> Self {
        IrqState { global_en: true, mask: 0, edge_mask: EDGE_MASK_DEFAULT, flags: 0, level: 0, high_en: 0, high_src: 0 }
    }

    /// One-clock pulse on low source `bit` (0..=7), e.g. the IRQ timer (itu.vhd:113-116). On an edge bit it
    /// latches the flag unless the source is already held high (no rise); on a level bit it is too short to see.
    #[inline]
    pub fn pulse(&mut self, bit: u8) {
        self.flags |= (1 << bit) & self.edge_mask & !self.level;
    }

    /// Level of low source `bit` (0..=7). A rise on an edge bit latches its flag (itu.vhd:236-243), e.g. a held
    /// C64 reset on bit 7; a level bit is active while high (itu.vhd:275).
    #[inline]
    pub fn set_level(&mut self, bit: u8, on: bool) {
        let b = 1 << bit;
        if on {
            self.flags |= b & self.edge_mask & !self.level;
            self.level |= b;
        } else {
            self.level &= !b;
        }
    }

    /// Level of high source `bit` (0..=7).
    #[inline]
    pub fn set_high(&mut self, bit: u8, on: bool) {
        if on {
            self.high_src |= 1 << bit;
        } else {
            self.high_src &= !(1 << bit);
        }
    }

    /// ITU_IRQ_ACTIVE = `(flag | level & ~edge) & mask` (itu.vhd:171-172,275).
    #[inline]
    pub fn active(&self) -> u8 {
        (self.flags | (self.level & !self.edge_mask)) & self.mask
    }

    /// ITU_IRQ_HIGH_ACT = `src & HIGH_EN`, level, no latch (itu.vhd:227-228).
    #[inline]
    pub fn high_active(&self) -> u8 {
        self.high_src & self.high_en
    }

    /// ITU_IRQ_CLEAR: drops latched flags only (itu.vhd:144-145). A level source stays active while high; an edge
    /// source held high latches again only after it falls and rises. In RTL a set in the same clock as the clear
    /// wins (itu.vhd:145,236-243); the emulator applies sources before the instruction that writes CLEAR.
    #[inline]
    pub fn clear(&mut self, bits: u8) {
        self.flags &= !bits;
    }

    /// CPU external interrupt line (mip.MEIP): `irq_en & (active ≠ 0 | high_active ≠ 0)` (itu.vhd:245-253).
    #[inline]
    pub fn line(&self) -> bool {
        self.global_en && (self.active() != 0 || self.high_active() != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_state_matches_rtl() {
        let irq = IrqState::new();
        assert!(irq.global_en);
        assert_eq!((irq.mask, irq.edge_mask, irq.flags, irq.high_en), (0, 0x85, 0, 0));
        assert!(!irq.line());
    }

    #[test]
    fn edge_flag_latches_while_masked() {
        let mut irq = IrqState::new();
        irq.pulse(0);
        assert_eq!(irq.active(), 0);
        assert!(!irq.line());
        irq.mask |= 0x01;
        assert_eq!(irq.active(), 0x01);
        assert!(irq.line());
        irq.clear(0x01);
        assert_eq!(irq.active(), 0);
        assert!(!irq.line());
    }

    #[test]
    fn level_source_follows_its_line_and_ignores_clear_and_pulses() {
        let mut irq = IrqState::new();
        irq.mask = 0x38;
        irq.pulse(3);
        assert_eq!(irq.active(), 0);
        irq.set_level(4, true);
        assert_eq!(irq.active(), 0x10);
        irq.clear(0xFF);
        assert_eq!(irq.active(), 0x10);
        irq.set_level(4, false);
        assert_eq!(irq.active(), 0);
        assert_eq!(irq.flags, 0);
    }

    #[test]
    fn edge_source_held_high_latches_once_per_rise() {
        let mut irq = IrqState::new();
        irq.mask = 0x80;
        irq.set_level(7, true);
        assert_eq!(irq.active(), 0x80);
        irq.clear(0x80);
        assert_eq!(irq.active(), 0, "held edge source must not stay active after CLEAR");
        irq.set_level(7, true);
        irq.pulse(7);
        assert_eq!(irq.active(), 0, "no rise while already high");
        irq.set_level(7, false);
        irq.set_level(7, true);
        assert_eq!(irq.active(), 0x80);
    }

    #[test]
    fn line_follows_global_mask_and_high_irqs() {
        let mut irq = IrqState::new();
        irq.set_high(5, true);
        assert_eq!(irq.high_active(), 0);
        assert!(!irq.line());
        irq.high_en = 0x20;
        assert_eq!(irq.high_active(), 0x20);
        assert!(irq.line());
        irq.global_en = false;
        assert!(!irq.line());
        irq.global_en = true;
        irq.set_high(5, false);
        assert!(!irq.line());
        irq.pulse(2);
        irq.mask = 0x04;
        assert!(irq.line());
        irq.global_en = false;
        assert!(!irq.line());
    }
}
