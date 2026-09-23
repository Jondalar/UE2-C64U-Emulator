//! ITU (interrupt controller, timers, ms timer, button, capabilities) + console UART.
//! Window 0x10000000-0x100000FF, decoded by `off & 0x3F` (00-memory-map Q-A4): `addr[5:4]` selects IRQ/timer,
//! UART, ms/LED/high-IRQ or the dummy port that reads 0 (itu.vhd:277-293, io_bus_splitter.vhd:51-53).
//! Register behaviour: docs/hw/02-itu-uart.md T0 + T1 IRQ core. Spec: docs/specs/S03-itu-uart.md

use crate::io::{IoCtx, IoDevice, IoMap, IO_BASE};
use crate::irq::IrqState;
use crate::machine::MachineConfig;
use crate::time::{clocks_to_ms, CLOCK_HZ};

/// ITU_BASE (iomap.h:10).
const ITU_BASE: u32 = IO_BASE;
const ITU_SIZE: u32 = 0x100;

// IRQ/timer sub-block (itu_pkg.vhd:7-22).
const GLOBAL: u32 = 0x00;
const ENABLE: u32 = 0x01;
const DISABLE: u32 = 0x02;
const EDGE: u32 = 0x03;
const CLEAR: u32 = 0x04;
const ACTIVE: u32 = 0x05;
const TIMER: u32 = 0x06;
const IRQ_TIMER_EN: u32 = 0x07;
const IRQ_TIMER_HI: u32 = 0x08;
const IRQ_TIMER_LO: u32 = 0x09;
const BUTTONS: u32 = 0x0A;
const FPGA_VERSION_REG: u32 = 0x0B;
const CAPABILITIES_0: u32 = 0x0C;
const CAPABILITIES_3: u32 = 0x0F;
// UART sub-block, only `addr[1:0]` decoded (uart_peripheral_io.vhd:170,219).
const UART_FIRST: u32 = 0x10;
const UART_LAST: u32 = 0x1F;
const UART_DATA: u32 = 0;
const UART_FLAGS: u32 = 2;
/// UART_ICTRL alias written by `__crt0_dummy_trap_handler` (crt0.S:307-309, 02 H14, 00 A2).
const EARLY_TRAP: u32 = 0x1F;
const EARLY_TRAP_MARKER: u8 = 0x49;
// ms/LED/high-IRQ sub-block (itu_pkg.vhd:25-32).
const MS_TIMER_HI: u32 = 0x22;
const MS_TIMER_LO: u32 = 0x23;
const USB_BUSY: u32 = 0x24;
const SD_BUSY: u32 = 0x25;
const MISC_IO: u32 = 0x26;
const IRQ_HIGH_EN: u32 = 0x27;
const IRQ_HIGH_ACT: u32 = 0x28;
const PRINTER_BUSY: u32 = 0x29;

/// ITU_FPGA_VERSION. The U64-II `g_version` is unknown and cosmetic (02 Q4); this is the open U2+ top value
/// (ultimate_logic_32.vhd:13).
const FPGA_VERSION: u8 = 0x25;
/// UART_FLAGS: TX done/idle, TX FIFO never full, no RX data (02 H1, 00 B1, uart_peripheral_io.vhd:208-215).
const UART_FLAGS_IDLE: u8 = 0x40;
/// ITU_TIMER step: `c_timer_div` = 5 ticks of the 1 µs tick (itu.vhd:50,92-105) = 500 clocks.
const TIMER_STEP: u64 = CLOCK_HZ / 1_000_000 * 5;
/// ITU_IRQ_TIMER_HI/LO reset value (itu.vhd:264).
const IRQ_TIMER_VAL_RESET: u16 = 0x8000;

pub struct Itu {
    pub capabilities: u32,
    pub menu_button: bool,
    /// ITU_TIMER value written last and the clock of that write (02 §Functional model).
    timer: u8,
    timer_written: u64,
    irq_timer_en: bool,
    irq_timer_select: bool,
    irq_timer_val: u16,
    /// `irq_timer_cnt` while the counter stands still; while it runs it is derived from `next_pulse`.
    irq_timer_frozen: u32,
    /// Clock of the next pulse on low source 0; `Some` exactly while the counter runs.
    next_pulse: Option<u64>,
    /// Clock of the last `sync`, i.e. of the last access or tick: the time base of `peek8`.
    synced: u64,
}

impl Itu {
    pub fn new(capabilities: u32) -> Self {
        Itu {
            capabilities,
            menu_button: false,
            timer: 0,
            timer_written: 0,
            irq_timer_en: false,
            irq_timer_select: false,
            irq_timer_val: IRQ_TIMER_VAL_RESET,
            irq_timer_frozen: 0,
            next_pulse: None,
            synced: 0,
        }
    }

    /// Host menu button (ITU_BUTTON_REG bit 6).
    pub fn set_menu_button(&mut self, pressed: bool) {
        self.menu_button = pressed;
    }

    /// ITU_TIMER: −1 per 5 µs down to 0, then holds (itu.vhd:101-105, 02 H2/H3, 00 B4).
    fn timer_value(&self, now: u64) -> u8 {
        let steps = now.saturating_sub(self.timer_written) / TIMER_STEP;
        self.timer.saturating_sub(u8::try_from(steps).unwrap_or(u8::MAX))
    }

    /// Counter load value `val & 0xFF` (itu.vhd:120,152).
    fn irq_timer_reload(&self) -> u32 {
        u32::from(self.irq_timer_val) << 8 | 0xFF
    }

    /// `irq_timer_cnt` after the clock edge `now`. It runs down to 0, pulses on the next clock and reloads, so the
    /// period is `(val + 1) * 256` clocks (itu.vhd:114-125); valid after `sync(now)`.
    fn irq_timer_cnt(&self, now: u64) -> u32 {
        match self.next_pulse {
            Some(t) => (t - 1 - now) as u32,
            None => self.irq_timer_frozen,
        }
    }

    /// ITU_IRQ_TIMER_EN write (itu.vhd:148-153). The counter loads only if the timer was disabled, so enabling
    /// restarts the phase (first pulse one full period later) and a rewrite while enabled keeps it. With
    /// select = 1 the counter waits for `irq_timer_tick`, which is unconnected (itu.vhd:34), so it stands still;
    /// the firmware never selects it (02 §Functional model).
    fn write_irq_timer_en(&mut self, val: u8, now: u64) {
        let cnt = if self.irq_timer_en { self.irq_timer_cnt(now) } else { self.irq_timer_reload() };
        self.irq_timer_en = val & 0x01 != 0;
        self.irq_timer_select = val & 0x02 != 0;
        self.irq_timer_frozen = cnt;
        self.next_pulse = (self.irq_timer_en && !self.irq_timer_select).then(|| now + u64::from(cnt) + 1);
    }

    /// Delivers a due IRQ-timer pulse to low source 0 (02 H6, 00 B9). Pulses a late caller missed merge into one
    /// edge flag, like ticks during a masked section on hardware (02 §Functional model "Tick collapse"); the phase
    /// is kept. A reload value changed while running applies from this reload on, as in RTL.
    fn sync(&mut self, now: u64, irq: &mut IrqState) {
        self.synced = now;
        if let Some(t) = self.next_pulse.filter(|&t| now >= t) {
            irq.pulse(0);
            let period = u64::from(self.irq_timer_reload()) + 1;
            self.next_pulse = Some(now + period - (now - t) % period);
        }
    }

    /// The registers that do not read the IRQ core, at clock `now`; valid after `sync(now)`. Other offsets read 0.
    fn get(&self, off: u32, now: u64) -> u8 {
        match off & 0x3F {
            TIMER => self.timer_value(now),
            IRQ_TIMER_EN => u8::from(self.irq_timer_en) | u8::from(self.irq_timer_select) << 1,
            // Readback is the counter, not the reload value (itu.vhd:178-181).
            IRQ_TIMER_HI => (self.irq_timer_cnt(now) >> 8) as u8,
            IRQ_TIMER_LO => self.irq_timer_cnt(now) as u8,
            // `buttons[2:0]` are not modelled; bit 6 = menu, idle 0 (itu.vhd:192-196, 02 H12).
            BUTTONS => u8::from(self.menu_button) << 6,
            FPGA_VERSION_REG => FPGA_VERSION,
            // Big-endian capability word (itu.vhd:184-191, itu.c:23-26, 02 H9-H11).
            o @ CAPABILITIES_0..=CAPABILITIES_3 => self.capabilities.to_be_bytes()[(o - CAPABILITIES_0) as usize],
            // Only FLAGS reads non-zero: RX FIFO empty, ICTRL without `g_impl_irq` (uart_peripheral_io.vhd:219-223).
            o @ UART_FIRST..=UART_LAST if o & 0x03 == UART_FLAGS => UART_FLAGS_IDLE,
            // Derived from emulated time, so stable between reads (02 H4/H5, itu.c:83-86).
            MS_TIMER_HI => (clocks_to_ms(now) >> 8) as u8,
            MS_TIMER_LO => clocks_to_ms(now) as u8,
            _ => 0,
        }
    }
}

impl IoDevice for Itu {
    fn name(&self) -> &'static str {
        "itu"
    }

    fn read8(&mut self, off: u32, ctx: &mut IoCtx) -> u8 {
        self.sync(ctx.now, ctx.irq);
        let irq = &*ctx.irq;
        match off & 0x3F {
            GLOBAL => u8::from(irq.global_en),
            ENABLE => irq.mask,
            EDGE => irq.edge_mask,
            // 02 H7, 00 C1.
            ACTIVE => irq.active(),
            IRQ_HIGH_EN => irq.high_en,
            // 02 H8.
            IRQ_HIGH_ACT => irq.high_active(),
            o => self.get(o, ctx.now),
        }
    }

    /// [`Itu::get`] as of the last access or tick, without delivering a due pulse. The IRQ core registers (GLOBAL,
    /// ENABLE, EDGE, ACTIVE, IRQ_HIGH_EN/ACT) read 0: they live in `IrqState`, which a peek cannot reach.
    fn peek8(&self, off: u32) -> u8 {
        self.get(off, self.synced)
    }

    fn write8(&mut self, off: u32, val: u8, ctx: &mut IoCtx) {
        self.sync(ctx.now, ctx.irq);
        let irq = &mut *ctx.irq;
        match off & 0x3F {
            GLOBAL => irq.global_en = val & 0x01 != 0,
            ENABLE => irq.mask |= val,
            DISABLE => irq.mask &= !val,
            CLEAR => irq.clear(val),
            TIMER => {
                self.timer = val;
                self.timer_written = ctx.now;
            }
            IRQ_TIMER_EN => self.write_irq_timer_en(val, ctx.now),
            IRQ_TIMER_HI => self.irq_timer_val = self.irq_timer_val & 0x00FF | u16::from(val) << 8,
            IRQ_TIMER_LO => self.irq_timer_val = self.irq_timer_val & 0xFF00 | u16::from(val),
            EARLY_TRAP if val == EARLY_TRAP_MARKER => ctx.console.extend_from_slice(b"\n[early trap]\n"),
            // TX FIFO push; `\r` is stripped by the host console (02 §Console UART, 00 B1).
            o @ UART_FIRST..=UART_LAST if o & 0x03 == UART_DATA => ctx.console.push(val),
            IRQ_HIGH_EN => irq.high_en = val,
            // Accepted without a modelled effect: EDGE (`g_edge_write => false`, ultimate_logic_32.vhd:508), UART
            // GET/FLAGS/ICTRL (no RX, no overflow, no `g_impl_irq`), busy LEDs and misc_io (itu.vhd:206-213).
            EDGE | USB_BUSY | SD_BUSY | MISC_IO | PRINTER_BUSY => {}
            _ => {}
        }
    }

    fn next_event(&self) -> Option<u64> {
        self.next_pulse
    }

    fn tick(&mut self, ctx: &mut IoCtx) {
        self.sync(ctx.now, ctx.irq);
    }

    crate::impl_as_any!();
}

pub fn install(map: &mut IoMap, cfg: &MachineConfig) {
    map.add(ITU_BASE, ITU_SIZE, Box::new(Itu::new(cfg.capabilities)));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Tick period programmed by `vPortSetupTimerInterrupt`: (0x07A0 + 1) * 256 (riscv_main.c:173-176).
    const TICK: u64 = 499_968;

    /// The ITU with a local IRQ core and console; `run_to` ticks like an ideal machine loop.
    struct Rig {
        itu: Itu,
        irq: IrqState,
        console: Vec<u8>,
        now: u64,
    }

    impl Rig {
        fn new() -> Self {
            Rig { itu: Itu::new(0x3400_0222), irq: IrqState::new(), console: Vec::new(), now: 0 }
        }

        fn rd(&mut self, off: u32) -> u8 {
            let mut ctx = IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut [], irq: &mut self.irq, console: &mut self.console };
            self.itu.read8(off, &mut ctx)
        }

        fn wr(&mut self, off: u32, val: u8) {
            let mut ctx = IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut [], irq: &mut self.irq, console: &mut self.console };
            self.itu.write8(off, val, &mut ctx);
        }

        fn run_to(&mut self, t: u64) {
            while let Some(e) = self.itu.next_event().filter(|&e| e <= t) {
                self.now = e;
                let mut ctx =
                    IoCtx { stall: 0, now: self.now, pc: 0, ram: &mut [], irq: &mut self.irq, console: &mut self.console };
                self.itu.tick(&mut ctx);
            }
            self.now = t;
        }

        /// riscv_main.c:178-186, write order as in ELF @0x3DFC4 (00 B9).
        fn setup_timer_interrupt(&mut self) {
            for (off, val) in [
                (0x27, 0x00),
                (0x07, 0x00),
                (0x02, 0xFF),
                (0x04, 0xFF),
                (0x08, 0x07),
                (0x09, 0xA0),
                (0x07, 0x01),
                (0x01, 0x01),
                (0x00, 0x01),
            ] {
                self.wr(off, val);
            }
        }

        /// ISR head: R ACTIVE, W CLEAR ← pending (riscv_main.c:86-87).
        fn isr_ack(&mut self) -> u8 {
            let pending = self.rd(0x05);
            self.wr(0x04, pending);
            pending
        }
    }

    #[test]
    fn firmware_tick_program_fires_every_499968_clocks() {
        let mut r = Rig::new();
        r.now = 1_000;
        r.setup_timer_interrupt();
        assert_eq!(r.itu.next_event(), Some(1_000 + TICK));
        r.run_to(1_000 + TICK - 1);
        assert_eq!(r.rd(0x05), 0);
        assert!(!r.irq.line());
        for n in 1..=5 {
            r.run_to(1_000 + n * TICK);
            assert_eq!(r.rd(0x05), 0x01, "tick {n}");
            assert!(r.irq.line());
            assert_eq!(r.isr_ack(), 0x01);
            assert_eq!(r.rd(0x05), 0);
            assert_eq!(r.rd(0x28), 0);
            assert!(!r.irq.line());
            assert_eq!(r.itu.next_event(), Some(1_000 + (n + 1) * TICK));
        }
    }

    #[test]
    fn tick_latches_while_masked_and_line_follows_global() {
        let mut r = Rig::new();
        r.setup_timer_interrupt();
        r.wr(0x02, 0x01);
        r.run_to(TICK);
        assert_eq!(r.rd(0x05), 0);
        assert!(!r.irq.line());
        r.wr(0x01, 0x01);
        assert_eq!(r.rd(0x01), 0x01);
        assert_eq!(r.rd(0x05), 0x01);
        assert!(r.irq.line());
        r.wr(0x00, 0x00);
        assert_eq!(r.rd(0x00), 0);
        assert!(!r.irq.line());
        r.wr(0x00, 0xFF);
        assert_eq!(r.rd(0x00), 0x01);
        assert!(r.irq.line());
    }

    #[test]
    fn late_access_collapses_missed_ticks_and_keeps_phase() {
        let mut r = Rig::new();
        r.setup_timer_interrupt();
        r.now = 3 * TICK + 10;
        assert_eq!(r.isr_ack(), 0x01);
        assert_eq!(r.rd(0x05), 0);
        assert_eq!(r.itu.next_event(), Some(4 * TICK));
    }

    #[test]
    fn irq_timer_phase_resets_only_on_enable() {
        let mut r = Rig::new();
        r.setup_timer_interrupt();
        r.run_to(100_000);
        r.wr(0x07, 0x01);
        assert_eq!(r.itu.next_event(), Some(TICK));
        r.wr(0x07, 0x00);
        assert_eq!(r.rd(0x07), 0);
        assert_eq!(r.itu.next_event(), None);
        r.run_to(5 * TICK);
        assert_eq!(r.rd(0x05), 0);
        r.wr(0x07, 0x01);
        assert_eq!(r.itu.next_event(), Some(5 * TICK + TICK));
        r.wr(0x07, 0x03);
        assert_eq!(r.rd(0x07), 0x03);
        assert_eq!(r.itu.next_event(), None, "select = 1 counts an unconnected tick");
    }

    #[test]
    fn irq_timer_readback_is_the_counter() {
        let mut r = Rig::new();
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0x00, 0x00));
        r.setup_timer_interrupt();
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0xA0, 0xFF));
        r.run_to(0x100);
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0x9F, 0xFF));
        r.run_to(TICK - 1);
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0x00, 0x00));
        r.run_to(TICK);
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0xA0, 0xFF));
        r.run_to(TICK + 0x2345);
        r.wr(0x07, 0x00);
        r.run_to(3 * TICK);
        // 0x7A0FF - 0x2345 = 0x77DBA, frozen while disabled.
        assert_eq!((r.rd(0x08), r.rd(0x09)), (0x7D, 0xBA));
    }

    #[test]
    fn itu_timer_counts_down_per_5us_and_holds() {
        let mut r = Rig::new();
        r.now = 12_345;
        r.wr(0x06, 200);
        assert_eq!(r.rd(0x06), 200);
        r.now += 500;
        assert_eq!(r.rd(0x06), 199);
        r.now = 12_345 + 99_999;
        assert_eq!(r.rd(0x06), 1);
        r.now = 12_345 + 100_000;
        assert_eq!(r.rd(0x06), 0);
        r.now += 10_000_000_000;
        assert_eq!(r.rd(0x06), 0);
    }

    #[test]
    fn wait_ms_2_polls_to_zero_without_interrupts() {
        let mut r = Rig::new();
        r.now = 7;
        // itu.c:63-71 with 4 clocks per poll instruction.
        for _ in 0..2 {
            r.wr(0x06, 200);
            while r.rd(0x06) != 0 {
                r.now += 4;
            }
        }
        assert_eq!(r.now - 7, 2 * 200 * 500);
        assert!(!r.irq.line());
    }

    #[test]
    fn ms_timer_hi_lo_from_emulated_time() {
        let mut r = Rig::new();
        r.now = 0x1234 * 100_000 + 99_999;
        // getMsTimer: LO, HI, LO, HI until equal (itu.c:83-86).
        let a = u16::from(r.rd(0x23)) | u16::from(r.rd(0x22)) << 8;
        let b = u16::from(r.rd(0x23)) | u16::from(r.rd(0x22)) << 8;
        assert_eq!((a, b), (0x1234, 0x1234));
        r.now += 1;
        assert_eq!((r.rd(0x22), r.rd(0x23)), (0x12, 0x35));
        r.now = 0x1_0005 * 100_000;
        assert_eq!((r.rd(0x22), r.rd(0x23)), (0x00, 0x05));
    }

    #[test]
    fn capabilities_big_endian_and_fpga_version() {
        let mut r = Rig::new();
        assert_eq!([r.rd(0x0C), r.rd(0x0D), r.rd(0x0E), r.rd(0x0F)], [0x34, 0x00, 0x02, 0x22]);
        assert_eq!(r.rd(0x0B), FPGA_VERSION);
    }

    #[test]
    fn uart_flags_data_to_console_and_early_trap_marker() {
        let mut r = Rig::new();
        for off in [0x12, 0x16, 0x1A, 0x1E] {
            assert_eq!(r.rd(off), 0x40);
        }
        for off in [0x10, 0x11, 0x13, 0x1F] {
            assert_eq!(r.rd(off), 0);
        }
        r.wr(0x10, b'O');
        r.wr(0x10, b'K');
        r.wr(0x14, b'!');
        r.wr(0x11, 0x00);
        r.wr(0x12, 0x01);
        r.wr(0x13, 0x03);
        assert_eq!(r.console, b"OK!");
        r.wr(0x1F, 0x49);
        assert_eq!(r.console, b"OK!\n[early trap]\n");
    }

    #[test]
    fn high_irq_enable_read_modify_write_and_active() {
        let mut r = Rig::new();
        r.wr(0x27, 0x00);
        r.irq.set_high(5, true);
        r.irq.set_high(3, true);
        assert_eq!(r.rd(0x28), 0);
        assert!(!r.irq.line());
        // install_high_irq(5) / (6) (riscv_main.c:43, u64_config.cc:971,974).
        for bit in [5, 6] {
            let en = r.rd(0x27);
            r.wr(0x27, en | 1 << bit);
        }
        assert_eq!(r.rd(0x27), 0x60);
        assert_eq!(r.rd(0x28), 0x20);
        assert!(r.irq.line());
        // No-handler path of the ISR (riscv_main.c:124-127).
        let en = r.rd(0x27);
        r.wr(0x27, en & !(1 << 5));
        assert_eq!(r.rd(0x28), 0);
        assert!(!r.irq.line());
    }

    #[test]
    fn menu_button_is_bit_6() {
        let mut r = Rig::new();
        assert_eq!(r.rd(0x0A), 0x00);
        r.itu.set_menu_button(true);
        assert_eq!(r.rd(0x0A), 0x40);
        r.itu.set_menu_button(false);
        assert_eq!(r.rd(0x0A), 0x00);
    }

    #[test]
    fn unlisted_offsets_read_zero_edge_is_fixed_and_block_mirrors() {
        let mut r = Rig::new();
        for off in [0x24, 0x25, 0x26, 0x29] {
            r.wr(off, 0xFF);
        }
        r.wr(0x03, 0x00);
        assert_eq!(r.rd(0x03), 0x85);
        let zero: Vec<u32> = [0x02, 0x04].into_iter().chain(0x20..=0x21).chain(0x24..=0x26).chain(0x29..=0x3F).collect();
        for off in zero {
            assert_eq!(r.rd(off), 0, "offset {off:#04x}");
        }
        assert_eq!(r.rd(0x4C), 0x34);
        assert_eq!(r.rd(0xCF), 0x22);
        r.wr(0x90, b'x');
        assert_eq!(r.console, b"x");
    }

    #[test]
    fn peek_shows_plain_registers_as_of_the_last_access() {
        let mut r = Rig::new();
        let start = 0x1234 * 100_000;
        r.now = start;
        r.itu.set_menu_button(true);
        r.setup_timer_interrupt();
        r.wr(0x06, 200);
        r.now = start + TICK;
        assert_eq!([0x0C, 0x0D, 0x0E, 0x0F].map(|o| r.itu.peek8(o)), [0x34, 0x00, 0x02, 0x22]);
        assert_eq!([0x0B, 0x0A, 0x12, 0x1E].map(|o| r.itu.peek8(o)), [FPGA_VERSION, 0x40, 0x40, 0x40]);
        assert_eq!([0x22, 0x23, 0x06].map(|o| r.itu.peek8(o)), [0x12, 0x34, 200], "timers as of the last write");
        assert_eq!([0x07, 0x08, 0x09].map(|o| r.itu.peek8(o)), [0x01, 0xA0, 0xFF]);
        assert_eq!([0x00, 0x01, 0x03, 0x05].map(|o| r.itu.peek8(o)), [0; 4], "IRQ core not reachable");
        assert_eq!((r.irq.active(), r.itu.next_event()), (0, Some(start + TICK)), "a peek delivers no pulse");
        assert_eq!(r.rd(0x05), 0x01);
        assert_eq!([0x22, 0x23, 0x06].map(|o| r.itu.peek8(o)), [0x12, 0x38, 0], "a read moves the time base");
    }

    #[test]
    fn install_maps_the_itu_window() {
        let mut map = IoMap::new();
        install(&mut map, &MachineConfig::new(PathBuf::new(), PathBuf::new()));
        assert_eq!(map.resolve(0x1000_000C), Some((0, 0x0C)));
        assert_eq!(map.resolve(0x1000_00FF), Some((0, 0xFF)));
        assert_eq!(map.resolve(0x1000_0100), None);
        assert_eq!(map.get::<Itu>().map(|i| i.capabilities), Some(0x3400_0226));
    }
}
