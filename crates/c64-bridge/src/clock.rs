//! Emulator clock (100 MHz) → PAL C64 cycles (docs/specs/S14-c64-trx64.md §4).

use ue2_core::time::CLOCK_HZ;

/// PAL C64 master clock: 17 734 475 Hz / 18 = 985 248.6 Hz, the 0.6 ppm ignored.
pub const C64_HZ: u64 = 985_248;

/// C64 cycle count as a function of the emulator clock, from an anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    /// Emulator clock at the anchor.
    now: u64,
    /// C64 cycle count at the anchor.
    cycles: u64,
}

impl Clock {
    pub fn new(now: u64, cycles: u64) -> Self {
        Clock { now, cycles }
    }

    /// C64 cycle count at emulator clock `now` (not before the anchor), rounded down. Computed from the anchor, so
    /// rounding never accumulates.
    pub fn cycles(&self, now: u64) -> u64 {
        let clocks = u128::from(now - self.now);
        self.cycles + (clocks * u128::from(C64_HZ) / u128::from(CLOCK_HZ)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_to_pal_cycles() {
        let clock = Clock::new(0, 0);
        assert_eq!(clock.cycles(CLOCK_HZ), C64_HZ, "1 s");
        assert_eq!((clock.cycles(101), clock.cycles(102)), (0, 1), "one cycle is 101.497 clocks");
        assert_eq!(clock.cycles(20 * CLOCK_HZ / 1000), 19_704, "20 ms, about one PAL frame of 19 656 cycles");
        assert_eq!(clock.cycles(3600 * CLOCK_HZ), 3600 * C64_HZ, "no drift over an hour");
        let anchored = Clock::new(5 * CLOCK_HZ, 1234);
        assert_eq!(anchored.cycles(5 * CLOCK_HZ), 1234);
        assert_eq!(anchored.cycles(6 * CLOCK_HZ), 1234 + C64_HZ);
    }
}
