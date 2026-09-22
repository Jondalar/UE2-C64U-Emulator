//! Emulated time: a 100 MHz clock advanced per executed instruction.

pub const CLOCK_HZ: u64 = 100_000_000;
pub const CLOCKS_PER_MS: u64 = CLOCK_HZ / 1000;
/// 25 MIPS emulated CPU speed.
pub const DEFAULT_CLOCKS_PER_INSN: u64 = 4;

/// The wait state one byte through the C64 memory window adds, in [`CLOCK_HZ`] ticks.
///
/// The window is a DMA master on the C64's bus: a byte steals a bus cycle and the RISC-V waits for the handshake
/// around it. Measured on a real C64 Ultimate (firmware 3.15) over `GET /v1/machine:readmem`, which stops the
/// machine and memcpy's from the aperture (c64_subsys.cc:579): between 256 and 32768 bytes the request time grows
/// by **3.26 us per byte**, about three C64 cycles. The same measurement against this emulator showed 1.61 us
/// before this constant existed -- the firmware's own instructions around each byte -- so the wait state is the
/// remainder, and the total lands on the device's figure.
///
/// It matters beyond bookkeeping: the cartridge flash times an erase in C64 cycles and finishes it on the first
/// access at or after the due one, so a poll loop that costs nothing never gets there. The upstream cart tool's
/// chip erase polls 6 000 000 times, which is 20 s on the device and was under 3 s here (docs/status/cart-slot.md).
pub const DMA_BYTE_CLOCKS: u64 = 166;

#[inline]
pub fn ms_to_clocks(ms: u64) -> u64 {
    ms * CLOCKS_PER_MS
}

#[inline]
pub fn clocks_to_ms(clocks: u64) -> u64 {
    clocks / CLOCKS_PER_MS
}
