//! The REU's RAM is the firmware's DDR (TRX64 Spec 854, docs/status/reu.md).
//!
//! On the U64 the REU does not own its memory: `REU_MEMORY_BASE 0x1000000`, `REU_MAX_SIZE 0x1000000` (c64.h:14-15) is
//! guest DDR, and the firmware preloads an image by writing there with its own CPU (`reu_preloader.cc:104`). A device
//! that allocated sixteen megabytes of its own would give two copies and every preload would land in the one the C64
//! never reads.
//!
//! So [`ReuRam`] is TRX64's `ExpansionRam` over the DDR that `C64Port` lends per access (`C64Backend::lend_ddr`), the
//! same lease the cartridge logic runs on (cart.rs). The shapes differ, and that is the whole point of this module:
//! TRX64 **holds** the store for the life of the device, while the DDR is only **lent** for one access. The borrow
//! cannot be stored, so the store is a second shared pointer cell of `CartLogic::set_ddr`'s shape, updated on the same
//! lend, and a REU access outside a lease finds nothing there.
//!
//! Nothing lent is not an error (854 D3): the bridge lends only around the accesses that can reach the C64 bus, so
//! between them "not lent" is the normal state. A read then returns [`UNLENT`] and a write is dropped.

use std::cell::UnsafeCell;
use std::sync::Arc;

use trx64_core::expansion::ExpansionRam;

/// `REU_MEMORY_BASE` (c64.h:14): where the REU's RAM lives in guest DDR. The same region the U64 cart logic serves as
/// GeoRAM (`cart::GEORAM_BASE`), which is why the firmware has one setting for both (c64.cc:90).
pub const REU_BASE: usize = 0x0100_0000;

/// `REU_MAX_SIZE` (c64.h:15): 16 MB, the largest `C64_REU_SIZE` (`128 << 7`).
pub const REU_MAX_SIZE: u32 = 0x0100_0000;

/// The size an REU starts at. C64_REU_SIZE resets to "111" (c64.rs `CART_REGS`), which is 16 MB; the register→KiB
/// mapping itself belongs where the register is decoded (`devices::c64::reu_size_kb`), and the firmware writes the
/// size before the enable (c64.cc:315-317), so this stands only until it does.
pub const DEFAULT_SIZE_KB: u32 = REU_MAX_SIZE / 1024;

/// What a read sees with no DDR lent. TRX64 has a floating-bus value of its own for an address with no DRAM behind it,
/// but `ExpansionRam::read` returns a plain `u8` and cannot say "nothing here", so the store has to invent one; 0xFF is
/// the REU's own `floating_bus` default (reu.rs), which keeps the two answers equal. See docs/status/reu.md §Findings.
pub const UNLENT: u8 = 0xFF;

/// Guest DDR at [`REU_BASE`] as the REU's store, shared between the backend (which lends) and the `Reu` TRX64 holds.
#[derive(Clone, Default)]
pub struct ReuRam(Arc<Cell>);

#[derive(Default)]
struct Cell(UnsafeCell<State>);

// SAFETY: as for `cart::SharedCell` — the backend, TRX64's `Machine` and every copy of the handle live on the emulation
// thread; `Send` is only required because `ExpansionRam: Send`.
unsafe impl Send for Cell {}
unsafe impl Sync for Cell {}

/// `State` is `Copy` and is read and written whole, so no two `&mut` to it ever overlap.
#[derive(Clone, Copy, Default)]
struct State {
    /// Guest DDR lent by `C64Port` for the access in progress (pointer and length), or None between accesses.
    ddr: Option<(*mut u8, usize)>,
    /// Bytes of DDR the REU addresses: `C64_REU_SIZE`, never past [`REU_MAX_SIZE`].
    size: u32,
}

impl ReuRam {
    fn get(&self) -> State {
        // SAFETY: `State` is `Copy`; this reads it out without holding a reference across anything.
        unsafe { *self.0 .0.get() }
    }

    fn set(&self, s: State) {
        // SAFETY: as in `get`. Every caller is on the emulation thread and neither reads nor writes re-enter.
        unsafe { *self.0 .0.get() = s };
    }

    /// Guest DDR for the accesses that follow, or `None` when `C64Port` takes it back. Called from
    /// `C64Backend::lend_ddr` with the very lease the cartridge logic gets.
    pub fn set_ddr(&self, ddr: Option<(*mut u8, usize)>) {
        self.set(State { ddr, ..self.get() });
    }

    /// How much DDR the REU addresses, from `C64_REU_SIZE` in KiB.
    pub fn set_size_kb(&self, kb: u32) {
        self.set(State { size: (kb.saturating_mul(1024)).min(REU_MAX_SIZE), ..self.get() });
    }

    /// The DDR byte REU address `off` names, if it is fitted, lent and inside the lease.
    fn at(&self, off: u32) -> Option<*mut u8> {
        let State { ddr, size } = self.get();
        let (ptr, len) = ddr?;
        let addr = REU_BASE.checked_add(usize::try_from(off).ok()?)?;
        (off < size && addr < len).then(|| {
            // SAFETY: `ptr`/`len` is `IoCtx::ram`, lent by `C64Port` for the access in progress and taken back before
            // the access returns (`C64Backend::lend_ddr`); `addr < len` keeps the offset inside it, and nothing else
            // touches guest DDR while the C64 runs inside that access.
            unsafe { ptr.add(addr) }
        })
    }
}

impl ExpansionRam for ReuRam {
    fn len(&self) -> u32 {
        self.get().size
    }

    fn read(&self, off: u32) -> u8 {
        // SAFETY: `at` returns a pointer only inside the current lease.
        self.at(off).map_or(UNLENT, |p| unsafe { *p })
    }

    fn write(&mut self, off: u32, value: u8) {
        if let Some(p) = self.at(off) {
            // SAFETY: as in `read`.
            unsafe { *p = value };
        }
    }

    // `clone_ram` and `is_owned` keep their defaults: the bytes are the host's, so a cloned machine gets no copy and a
    // snapshot writes `ram: null` (854 D7).
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guest DDR, large enough to hold a 16 MB REU at `REU_BASE`.
    fn ddr() -> Vec<u8> {
        vec![0; 0x0400_0000]
    }

    fn lent(ram: &ReuRam, ddr: &mut [u8]) {
        ram.set_ddr(Some((ddr.as_mut_ptr(), ddr.len())));
    }

    #[test]
    fn nothing_lent_reads_a_harmless_byte_and_drops_writes() {
        let mut ram = ReuRam::default();
        ram.set_size_kb(512);
        assert_eq!(ram.read(0), UNLENT);
        assert_eq!(ram.read(0x00FF_FFFF), UNLENT, "the top of a 24-bit REU address");
        ram.write(0, 0x5A);
        ram.write(0x00FF_FFFF, 0x5A);
        assert_eq!(ram.len(), 512 * 1024, "the size stands without a lease");
    }

    #[test]
    fn a_lent_store_is_ddr_at_the_reu_base() {
        let (mut ddr, mut ram) = (ddr(), ReuRam::default());
        ram.set_size_kb(512);
        lent(&ram, &mut ddr);
        ddr[REU_BASE] = 0x11;
        ddr[REU_BASE + 0x7FFFF] = 0x22;
        assert_eq!(ram.read(0), 0x11);
        assert_eq!(ram.read(0x7FFFF), 0x22);
        ram.write(1, 0x33);
        assert_eq!(ddr[REU_BASE + 1], 0x33, "the write lands in the firmware's DDR");
    }

    #[test]
    fn above_the_fitted_size_is_not_ddr() {
        let (mut ddr, mut ram) = (ddr(), ReuRam::default());
        ram.set_size_kb(128);
        lent(&ram, &mut ddr);
        ddr[REU_BASE + 0x20000] = 0x44;
        assert_eq!(ram.read(0x20000), UNLENT, "128 KiB fitted, so 0x20000 has no DRAM");
        ram.write(0x20000, 0x55);
        assert_eq!(ddr[REU_BASE + 0x20000], 0x44, "and a write there is dropped, not wrapped");
        // Growing it makes the same DDR reachable, contents intact (854 D4).
        ram.set_size_kb(16384);
        assert_eq!(ram.read(0x20000), 0x44);
    }

    #[test]
    fn a_short_lease_is_not_read_past() {
        let (mut short, mut ram) = (vec![0u8; REU_BASE + 4], ReuRam::default());
        ram.set_size_kb(16384);
        lent(&ram, &mut short);
        short[REU_BASE + 3] = 0x66;
        assert_eq!(ram.read(3), 0x66);
        assert_eq!(ram.read(4), UNLENT, "one past the lease");
        ram.write(4, 0x77);
        assert_eq!(ram.read(0x00FF_FFFF), UNLENT);
    }

    #[test]
    fn the_lease_ends_and_the_store_goes_quiet() {
        let (mut ddr, mut ram) = (ddr(), ReuRam::default());
        ram.set_size_kb(512);
        lent(&ram, &mut ddr);
        ram.write(0, 0x88);
        ram.set_ddr(None);
        assert_eq!(ram.read(0), UNLENT, "the byte is still in DDR, but not reachable between accesses");
        ram.write(0, 0x99);
        lent(&ram, &mut ddr);
        assert_eq!(ram.read(0), 0x88, "and the write made without a lease changed nothing");
    }

    /// The store TRX64 holds and the one the backend lends through are the same cell.
    #[test]
    fn a_clone_shares_the_lease() {
        let (mut ddr, ram) = (ddr(), ReuRam::default());
        let mut held = ram.clone();
        ram.set_size_kb(512);
        lent(&ram, &mut ddr);
        ddr[REU_BASE] = 0xAB;
        assert_eq!(held.read(0), 0xAB);
        held.write(1, 0xCD);
        assert_eq!(ddr[REU_BASE + 1], 0xCD);
        ram.set_ddr(None);
        assert_eq!(held.read(0), UNLENT);
    }
}
