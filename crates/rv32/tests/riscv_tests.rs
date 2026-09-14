//! Official riscv-tests `rv32ui-p-*` and `rv32um-p-*` on the rvlite CPU (docs/specs/S01-cpu-rv32.md "Tests").
//!
//! The ELFs come from `scripts/build-riscv-tests.sh`. They are looked up in `$RISCV_TESTS_DIR`, falling back to
//! `<repo>/target/riscv-tests`; when none are there the test is skipped with a message.
//!
//! No test is skipped: every `p` test of both suites runs in M-mode and uses only RV32IM + Zicsr. The environment's
//! setup CSRs (pmpaddr0, pmpcfg0, satp, medeleg, mideleg, mnstatus; env/p/riscv_test.h) are RAZ/WI on rvlite, and
//! `mhartid` reads 0 (docs/hw/01-cpu-boot-memory.md CSR table).

use object::{Object, ObjectSegment, ObjectSymbol};
use rv32::{Bus, Cpu, Exit};
use std::path::{Path, PathBuf};

/// The `p` environment links every test at 0x80000000 (env/p/link.ld).
const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: usize = 1 << 20;
/// The longest test finishes in well under 100 000 instructions.
const MAX_INSNS: u64 = 1_000_000;

/// RAM at [`RAM_BASE`]; other addresses read 0 and ignore writes. Records writes to the `tohost` word.
struct TestBus {
    ram: Vec<u8>,
    tohost: u32,
    tohost_written: bool,
}

impl Bus for TestBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.ram.get(addr.wrapping_sub(RAM_BASE) as usize).copied().unwrap_or(0)
    }
    fn write8(&mut self, addr: u32, val: u8) {
        self.tohost_written |= addr.wrapping_sub(self.tohost) < 4;
        if let Some(b) = self.ram.get_mut(addr.wrapping_sub(RAM_BASE) as usize) {
            *b = val;
        }
    }
}

fn tests_dir() -> PathBuf {
    std::env::var_os("RISCV_TESTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/riscv-tests"))
}

/// Runs one test ELF until it writes `tohost`. Pass is the value 1 (RVTEST_PASS); a failure writes
/// `TESTNUM << 1 | 1` (RVTEST_FAIL). Returns the instruction count.
fn run(path: &Path) -> Result<u64, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let elf = object::File::parse(&*data).map_err(|e| e.to_string())?;
    let tohost = elf
        .symbols()
        .find(|s| s.name() == Ok("tohost"))
        .ok_or("no tohost symbol")?
        .address() as u32;
    let mut bus = TestBus { ram: vec![0; RAM_SIZE], tohost, tohost_written: false };
    for seg in elf.segments() {
        let bytes = seg.data().map_err(|e| e.to_string())?;
        let off = (seg.address() as u32).wrapping_sub(RAM_BASE) as usize;
        bus.ram
            .get_mut(off..off + bytes.len())
            .ok_or_else(|| format!("segment {:#x} outside test RAM", seg.address()))?
            .copy_from_slice(bytes);
    }

    let mut cpu = Cpu::new(elf.entry() as u32);
    while cpu.insns < MAX_INSNS {
        let exit = cpu.step(&mut bus);
        if bus.tohost_written {
            return match bus.read32(tohost) {
                1 => Ok(cpu.insns),
                v => Err(format!("tohost = {v:#x}: test case {} failed", v >> 1)),
            };
        }
        match exit {
            Exit::Illegal(w) => return Err(format!("illegal instruction {w:#010x} at {:#x}", cpu.csr.mepc)),
            Exit::Ebreak => return Err(format!("ebreak at {:#x}", cpu.csr.mepc)),
            Exit::Stepped | Exit::Interrupt | Exit::Wfi => {}
        }
    }
    Err(format!("no tohost write within {MAX_INSNS} instructions (pc {:#x})", cpu.pc))
}

#[test]
fn rv32ui_rv32um_p() {
    let dir = tests_dir();
    let is_test = |name: &str| (name.starts_with("rv32ui-p-") || name.starts_with("rv32um-p-")) && !name.contains('.');
    let mut elfs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_test))
                .collect()
        })
        .unwrap_or_default();
    if elfs.is_empty() {
        eprintln!(
            "skipping riscv-tests: no rv32ui-p-*/rv32um-p-* ELFs in {} (run scripts/build-riscv-tests.sh or set RISCV_TESTS_DIR)",
            dir.display()
        );
        return;
    }
    elfs.sort();
    let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
    for suite in ["rv32ui-p-", "rv32um-p-"] {
        assert!(elfs.iter().any(|p| name(p).starts_with(suite)), "no {suite}* ELFs in {}", dir.display());
    }

    let mut failed = Vec::new();
    for elf in &elfs {
        match run(elf) {
            Ok(insns) => eprintln!("PASS {} ({insns} instructions)", name(elf)),
            Err(e) => {
                eprintln!("FAIL {}: {e}", name(elf));
                failed.push(name(elf));
            }
        }
    }
    assert!(failed.is_empty(), "{} of {} riscv-tests failed: {failed:?}", failed.len(), elfs.len());
    eprintln!("{} riscv-tests passed, 0 skipped", elfs.len());
}
