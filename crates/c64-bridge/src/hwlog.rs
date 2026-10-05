//! S38 §4.1: UE2's write log of the memories around the C64 that TRX64's anchors do not hold
//! (docs/specs/S38-trx64-ring.md).
//!
//! On the U64 these memories are the FPGA's and the firmware's DDR: the cartridge ROM that EasyFlash programs, the
//! cartridge RAM, GeoRAM and the REU at `REU_MEMORY_BASE`, all in guest DDR; the GMOD2 EEPROM in the FPGA; and, on the
//! expansion port, a physical cartridge's own flash, EEPROM and RAM. A per-anchor copy would be megabytes a frame, so
//! the model is TRX64-Ultimate T20 §4.2 stream B: every write the C64 side makes into one of them is logged with the
//! byte it overwrote and the byte it wrote. Going back writes the old bytes in reverse; going forward writes the new
//! bytes in order. Each anchor names the log position it was taken at (`Mark`).
//!
//! Only writes from the C64 side are logged: the 6510, the REU's DMA and the cartridge logic, which all reach these
//! memories through the bridge. The firmware's own CPU writes guest DDR directly (a REU preload, the CRT loader, the
//! EEPROM load and save) and the log never sees them: they are the outside world, and going back does not undo them.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Which memory a record is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Space {
    /// Guest DDR, by its offset: cart ROM written in place (EasyFlash), cart RAM, GeoRAM, the REU.
    Ddr = 0,
    /// The internal GMOD2 EEPROM (2 K).
    Eeprom = 1,
    /// A physical cartridge's own memory served by the U64 cartridge logic (`slot::LogicCart`).
    PhysMem = 2,
    /// A physical cartridge's writable image (flash, then EEPROM), as `CartMapper::writable_image` lays it out.
    PhysImage = 3,
}

impl Space {
    fn from_bits(bits: u32) -> Space {
        match bits {
            0 => Space::Ddr,
            1 => Space::Eeprom,
            2 => Space::PhysMem,
            _ => Space::PhysImage,
        }
    }
}

/// Address bits a record carries; the space is in the four above them (T20's extended record does the same).
const ADDR_BITS: u32 = 28;
const ADDR_MASK: u32 = (1 << ADDR_BITS) - 1;

/// The records the log keeps at most: 16 Mi records of 8 bytes, 128 MiB. Past it the oldest go, and the anchors
/// before the new start lose their memories (S38 §8).
pub const DEFAULT_CAP: usize = 16 << 20;

/// One write: where, the byte it overwrote and the byte it wrote. 8 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rec {
    at: u32,
    old: u8,
    new: u8,
}

impl Rec {
    pub fn space(&self) -> Space {
        Space::from_bits(self.at >> ADDR_BITS)
    }

    pub fn addr(&self) -> usize {
        (self.at & ADDR_MASK) as usize
    }
}

/// Where an anchor stands in the log: the log's identity, the record count at the anchor, and how many cuts the log
/// had seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mark {
    pub id: u64,
    pub seq: u64,
    pub cuts: usize,
}

/// A write the log asks its owner to make: memories other than guest DDR, which the log writes itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Poke {
    pub space: Space,
    pub addr: usize,
    pub value: u8,
}

struct State {
    recs: VecDeque<Rec>,
    /// The sequence number of `recs[0]`.
    start: u64,
    /// Where the memories stand: the head while the C64 runs, an anchor's `seq` while the transport holds it.
    pos: u64,
    /// This log's identity: a reset, `--ring=off` or a new log gives a new one, so an anchor of another log (a
    /// `.c64rering` of another session) never matches.
    id: u64,
    /// The position of every cut, oldest first. An anchor after a cut (a pinned mark) describes a future that the
    /// records after the cut no longer lead to.
    cuts: Vec<u64>,
    /// Records are taken: the rings are on and the C64 is not held on an anchor.
    on: bool,
    cap: usize,
    /// Guest DDR, lent with the cartridge logic's lease (`C64Backend::lend_ddr`).
    ddr: Option<(*mut u8, usize)>,
    /// S40: every write into guest DDR for the board's engine taps (cartridge RAM, the REU's memory), while it
    /// listens: (offset, old, new), unchanged bytes too.
    engine: Option<Vec<(usize, u8, u8)>>,
}

impl State {
    fn head(&self) -> u64 {
        self.start + self.recs.len() as u64
    }
}

fn new_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (NEXT.fetch_add(1, Ordering::Relaxed) << 48)
}

impl Default for State {
    fn default() -> Self {
        State {
            recs: VecDeque::new(),
            start: 0,
            pos: 0,
            id: new_id(),
            cuts: Vec::new(),
            on: false,
            cap: DEFAULT_CAP,
            ddr: None,
            engine: None,
        }
    }
}

/// The log, shared by the backend, the cartridge logic, the EEPROM and the REU's store, like `cart::CartHandle`.
#[derive(Clone, Default)]
pub struct HwLog(Arc<Cell>);

#[derive(Default)]
struct Cell(UnsafeCell<State>);

// SAFETY: as for `cart::SharedCell` — the backend, TRX64's `Machine` and every copy of the handle live on the
// emulation thread; `Send` is only required because the devices that hold a copy are `Send`.
unsafe impl Send for Cell {}
unsafe impl Sync for Cell {}

impl HwLog {
    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        // SAFETY: single thread, and no call below re-enters the log.
        f(unsafe { &mut *self.0 .0.get() })
    }

    /// A write into `space` at `addr`, `old` → `new`. Kept only while the log takes records and the byte changed.
    #[inline]
    pub fn note(&self, space: Space, addr: usize, old: u8, new: u8) {
        self.with(|s| {
            if let Some(e) = s.engine.as_mut().filter(|_| space == Space::Ddr) {
                e.push((addr, old, new));
            }
        });
        if old == new {
            return;
        }
        self.with(|s| {
            if !s.on {
                return;
            }
            s.recs.push_back(Rec { at: ((space as u32) << ADDR_BITS) | (addr as u32 & ADDR_MASK), old, new });
            while s.recs.len() > s.cap {
                s.recs.pop_front();
                s.start += 1;
            }
            s.pos = s.head();
        })
    }

    /// S40: the engine's taps listen to guest DDR writes (or stop).
    pub fn set_engine(&self, on: bool) {
        self.with(|s| s.engine = on.then(Vec::new));
    }

    /// S40: the guest DDR writes since the last call.
    pub fn take_engine(&self) -> Vec<(usize, u8, u8)> {
        self.with(|s| s.engine.as_mut().map(std::mem::take).unwrap_or_default())
    }

    pub fn set_on(&self, on: bool) {
        self.with(|s| s.on = on);
    }

    /// Guest DDR for the pokes into [`Space::Ddr`], with the lease the cartridge logic gets.
    pub fn set_ddr(&self, ddr: Option<(*mut u8, usize)>) {
        self.with(|s| s.ddr = ddr);
    }

    /// The position an anchor taken now stands at.
    pub fn mark(&self) -> Mark {
        self.with(|s| Mark { id: s.id, seq: s.head(), cuts: s.cuts.len() })
    }

    /// Why the log cannot bring the memories to `m`, or `None` when it can.
    pub fn uncovered(&self, m: &Mark) -> Option<&'static str> {
        self.with(|s| {
            if m.id != s.id {
                Some("the anchor is from another log (a reset, or a file)")
            } else if m.seq < s.start {
                Some("the anchor is older than the log (it holds at most 16 Mi writes)")
            } else if m.seq > s.head() || s.cuts.get(m.cuts..).is_some_and(|c| c.iter().any(|&cut| m.seq > cut)) {
                Some("the anchor is in a future a release cut")
            } else {
                None
            }
        })
    }

    /// Bring the memories to anchor `m`: the old bytes back to it, or the new ones forward to it. Guest DDR is written
    /// here; the other spaces come back as pokes, in order, for the owner. `None` when `m` is not covered or guest DDR
    /// is not lent; nothing moves then.
    pub fn seek(&self, m: &Mark) -> Option<Vec<Poke>> {
        if self.uncovered(m).is_some() {
            return None;
        }
        self.with(|s| {
            let (ptr, len) = s.ddr?;
            if s.pos < s.start {
                return None;
            }
            let (from, to) = ((s.pos - s.start) as usize, (m.seq - s.start) as usize);
            let mut pokes = Vec::new();
            let mut put = |r: &Rec, value: u8| match r.space() {
                Space::Ddr if r.addr() < len => {
                    // SAFETY: `ptr`/`len` is guest DDR lent for the access in progress (`C64Backend::lend_ddr`), and
                    // the address is inside it.
                    unsafe { *ptr.add(r.addr()) = value };
                }
                Space::Ddr => {}
                space => pokes.push(Poke { space, addr: r.addr(), value }),
            };
            if to < from {
                for r in s.recs.range(to..from).rev() {
                    put(r, r.old);
                }
            } else {
                for r in s.recs.range(from..to) {
                    put(r, r.new);
                }
            }
            s.pos = m.seq;
            Some(pokes)
        })
    }

    /// The release: the records after where the memories stand are a future that will not happen now. They go, and
    /// the cut is noted for the anchors that outlive it.
    pub fn cut(&self) {
        self.with(|s| {
            let keep = (s.pos - s.start) as usize;
            s.recs.truncate(keep);
            s.cuts.push(s.pos);
        })
    }

    /// Drop the records before `seq`: no anchor reaches back to them.
    pub fn trim_to(&self, seq: u64) {
        self.with(|s| {
            let n = seq.saturating_sub(s.start).min(s.recs.len() as u64);
            s.recs.drain(..n as usize);
            s.start += n;
        })
    }

    /// A new log: no records, a new identity.
    pub fn clear(&self) {
        self.with(|s| {
            let (on, cap, ddr, engine) = (s.on, s.cap, s.ddr, s.engine.take());
            *s = State { on, cap, ddr, engine, ..State::default() };
        })
    }

    /// Records held, and their bytes.
    pub fn len(&self) -> usize {
        self.with(|s| s.recs.len())
    }

    pub fn bytes(&self) -> usize {
        self.len() * std::mem::size_of::<Rec>()
    }

    #[cfg(test)]
    pub fn set_cap(&self, cap: usize) {
        self.with(|s| s.cap = cap.max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lent(log: &HwLog, ddr: &mut [u8]) {
        log.set_ddr(Some((ddr.as_mut_ptr(), ddr.len())));
    }

    /// Back and forth over the records: DDR is written here, the other spaces come back as pokes in order.
    #[test]
    fn seek_undoes_and_redoes() {
        let (log, mut ddr) = (HwLog::default(), vec![0u8; 16]);
        lent(&log, &mut ddr);
        log.set_on(true);
        let a = log.mark();
        for (i, v) in [(3usize, 1u8), (3, 2), (5, 9)] {
            log.note(Space::Ddr, i, ddr[i], v);
            ddr[i] = v;
        }
        log.note(Space::Eeprom, 7, 0xFF, 0x12);
        log.note(Space::Ddr, 1, 4, 4);
        assert_eq!(std::mem::size_of::<Rec>(), 8);
        assert_eq!(log.len(), 4, "an unchanged byte is no record");
        let b = log.mark();
        log.set_on(false);
        assert_eq!(log.seek(&a), Some(vec![Poke { space: Space::Eeprom, addr: 7, value: 0xFF }]));
        assert_eq!((ddr[3], ddr[5]), (0, 0));
        assert_eq!(log.seek(&b), Some(vec![Poke { space: Space::Eeprom, addr: 7, value: 0x12 }]));
        assert_eq!((ddr[3], ddr[5]), (2, 9));
        log.set_ddr(None);
        assert_eq!(log.seek(&a), None, "no DDR lent: nothing moves");
        assert_eq!(ddr[3], 2);
    }

    /// A cut drops the future, and an anchor beyond it (a pinned mark) is no longer covered; a cap and a new log
    /// uncover the anchors they leave behind.
    #[test]
    fn cuts_caps_and_identity() {
        let (log, mut ddr) = (HwLog::default(), vec![0u8; 16]);
        lent(&log, &mut ddr);
        log.set_on(true);
        let a = log.mark();
        log.note(Space::Ddr, 0, 0, 1);
        let b = log.mark();
        log.note(Space::Ddr, 0, 1, 2);
        let c = log.mark();
        log.set_on(false);
        log.seek(&b).unwrap();
        log.cut();
        log.set_on(true);
        log.note(Space::Ddr, 0, 1, 3);
        log.note(Space::Ddr, 0, 3, 4);
        assert!(log.uncovered(&c).is_some(), "past the cut");
        assert_eq!(log.uncovered(&b), None);
        log.set_cap(2);
        log.note(Space::Ddr, 0, 4, 5);
        assert!(log.uncovered(&a).is_some(), "older than the log");
        log.clear();
        assert!(log.uncovered(&b).is_some(), "another log");
    }
}
