//! Emulated time: a 100 MHz clock advanced per executed instruction.

pub const CLOCK_HZ: u64 = 100_000_000;
pub const CLOCKS_PER_MS: u64 = CLOCK_HZ / 1000;
/// 25 MIPS emulated CPU speed.
pub const DEFAULT_CLOCKS_PER_INSN: u64 = 4;

#[inline]
pub fn ms_to_clocks(ms: u64) -> u64 {
    ms * CLOCKS_PER_MS
}

#[inline]
pub fn clocks_to_ms(clocks: u64) -> u64 {
    clocks / CLOCKS_PER_MS
}
