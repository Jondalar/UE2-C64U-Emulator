//! Emulator clock (100 MHz) → C64 cycles at the model's clock (docs/specs/S14-c64-trx64.md §4, S25 §3).

use ue2_core::time::CLOCK_HZ;

/// C64 cycle count as a function of the emulator clock, from an anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    /// Emulator clock at the anchor.
    now: u64,
    /// C64 cycle count at the anchor.
    cycles: u64,
    /// C64 cycles per second: the machine's row, `Timing::cpu_hz` (985 248 PAL, 1 022 730 NTSC).
    hz: u64,
}

impl Clock {
    pub fn new(now: u64, cycles: u64, hz: u64) -> Self {
        Clock { now, cycles, hz }
    }

    /// C64 cycle count at emulator clock `now` (not before the anchor), rounded down. Computed from the anchor, so
    /// rounding never accumulates.
    pub fn cycles(&self, now: u64) -> u64 {
        let clocks = u128::from(now - self.now);
        self.cycles + (clocks * u128::from(self.hz) / u128::from(CLOCK_HZ)) as u64
    }

    /// Emulator clock at C64 cycle `cycles` (not before the anchor), rounded down: the inverse of [`Self::cycles`].
    pub fn time_of(&self, cycles: u64) -> u64 {
        let n = u128::from(cycles.saturating_sub(self.cycles));
        self.now + (n * u128::from(CLOCK_HZ) / u128::from(self.hz)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PAL C64 master clock: 17 734 475 Hz / 18 = 985 248.6 Hz, the 0.6 ppm ignored.
    const C64_HZ: u64 = 985_248;

    #[test]
    fn clocks_to_pal_cycles() {
        let clock = Clock::new(0, 0, C64_HZ);
        assert_eq!(clock.cycles(CLOCK_HZ), C64_HZ, "1 s");
        assert_eq!((clock.cycles(101), clock.cycles(102)), (0, 1), "one cycle is 101.497 clocks");
        assert_eq!(clock.cycles(20 * CLOCK_HZ / 1000), 19_704, "20 ms, about one PAL frame of 19 656 cycles");
        assert_eq!(clock.cycles(3600 * CLOCK_HZ), 3600 * C64_HZ, "no drift over an hour");
        let anchored = Clock::new(5 * CLOCK_HZ, 1234, C64_HZ);
        assert_eq!(anchored.cycles(5 * CLOCK_HZ), 1234);
        assert_eq!(anchored.cycles(6 * CLOCK_HZ), 1234 + C64_HZ);
    }

    #[test]
    fn clocks_to_ntsc_cycles() {
        let clock = Clock::new(0, 0, 1_022_730);
        assert_eq!(clock.cycles(CLOCK_HZ), 1_022_730, "1 s");
        assert_eq!(clock.cycles(CLOCK_HZ / 60), 17_045, "one 60 Hz frame, about 263 x 65 = 17 095 cycles");
    }
}
