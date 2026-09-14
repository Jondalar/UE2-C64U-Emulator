//! ELF symbolizer. Spec: docs/specs/S02-core.md

use std::path::Path;

use anyhow::{Context, Result};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind, SymbolSection};

#[derive(Clone, Debug)]
pub struct Symbol {
    pub addr: u32,
    pub size: u32,
    /// Demangled name.
    pub name: String,
    pub mangled: String,
}

#[derive(Clone, Debug, Default)]
pub struct Symbols {
    /// Sorted by address.
    pub syms: Vec<Symbol>,
}

impl Symbols {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Function and object symbols plus code labels, C++ demangled.
    ///
    /// Assembler entry points have no `.type` and are STT_NOTYPE labels in executable sections, e.g. `_start`,
    /// `__crt0_dummy_trap_handler` (crt0.S:280) and `freertos_risc_v_trap_handler` (port_asm.S:123). They
    /// are kept because the fault hooks and trap PCs need them. RISC-V mapping symbols (`$x`, `$d`) and
    /// `.L` locals are dropped.
    pub fn from_elf(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file = object::File::parse(&*data).with_context(|| format!("{}: not an object file", path.display()))?;
        let mut syms = Vec::new();
        for sym in file.symbols() {
            let SymbolSection::Section(index) = sym.section() else { continue };
            let wanted = match sym.kind() {
                SymbolKind::Text | SymbolKind::Data => true,
                SymbolKind::Unknown | SymbolKind::Label => {
                    file.section_by_index(index).is_ok_and(|s| s.kind() == SectionKind::Text)
                }
                _ => false,
            };
            let Ok(mangled) = sym.name() else { continue };
            if !wanted || mangled.is_empty() || mangled.starts_with('$') || mangled.starts_with(".L") {
                continue;
            }
            syms.push(Symbol {
                addr: sym.address() as u32,
                size: sym.size() as u32,
                name: demangle(mangled),
                mangled: mangled.to_owned(),
            });
        }
        Ok(Self::from_symbols(syms))
    }

    /// Table from unsorted symbols. At equal addresses sized symbols sort after labels, so `lookup` prefers them.
    pub fn from_symbols(mut syms: Vec<Symbol>) -> Self {
        syms.sort_by_key(|s| (s.addr, s.size != 0));
        Symbols { syms }
    }

    /// Enclosing symbol and offset into it.
    ///
    /// A sized symbol covers `addr..addr+size`; a label (size 0) covers everything up to the next symbol.
    pub fn lookup(&self, addr: u32) -> Option<(&str, u32)> {
        let sym = &self.syms[self.syms.partition_point(|s| s.addr <= addr).checked_sub(1)?];
        let off = addr - sym.addr;
        (sym.size == 0 || off < sym.size).then_some((sym.name.as_str(), off))
    }

    /// Address of a symbol by mangled or demangled name.
    pub fn addr_of(&self, name: &str) -> Option<u32> {
        self.syms.iter().find(|s| s.name == name || s.mangled == name).map(|s| s.addr)
    }

    /// `name+0x12` (`name` at offset 0), or the raw address.
    pub fn format(&self, addr: u32) -> String {
        match self.lookup(addr) {
            Some((name, 0)) => name.to_owned(),
            Some((name, off)) => format!("{name}+{off:#x}"),
            None => format!("{addr:#010x}"),
        }
    }
}

/// Demangled C++ name; C names and names `cpp_demangle` cannot parse are returned unchanged.
fn demangle(mangled: &str) -> String {
    if mangled.starts_with("_Z") {
        let options = cpp_demangle::DemangleOptions::default();
        if let Some(name) = cpp_demangle::Symbol::new(mangled).ok().and_then(|s| s.demangle(&options).ok()) {
            return name;
        }
    }
    mangled.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::tests::{firmware_root, FIRMWARE_ELF};

    fn sym(addr: u32, size: u32, mangled: &str) -> Symbol {
        Symbol { addr, size, name: demangle(mangled), mangled: mangled.to_owned() }
    }

    #[test]
    fn lookup_format_and_addr_of() {
        let table = Symbols::from_symbols(vec![
            sym(0x3000, 0, "label"),
            sym(0x1000, 0x10, "_ZN7Overlay4pollEv"),
            sym(0x2000, 0x20, "sized"),
            sym(0x2000, 0, "alias_label"),
        ]);
        assert_eq!(table.syms.iter().map(|s| s.addr).collect::<Vec<_>>(), [0x1000, 0x2000, 0x2000, 0x3000]);
        assert_eq!(table.lookup(0x0FFF), None);
        assert_eq!(table.lookup(0x100F), Some(("Overlay::poll()", 0xF)));
        assert_eq!(table.lookup(0x1010), None, "past the end of a sized symbol");
        assert_eq!(table.lookup(0x2004), Some(("sized", 4)), "sized symbol preferred over a label at its address");
        assert_eq!(table.lookup(0x3FFF_0000), Some(("label", 0x3FFE_D000)));
        assert_eq!(table.format(0x1000), "Overlay::poll()");
        assert_eq!(table.format(0x2012), "sized+0x12");
        assert_eq!(table.format(0x1800), "0x00001800");
        assert_eq!(table.addr_of("_ZN7Overlay4pollEv"), Some(0x1000));
        assert_eq!(table.addr_of("Overlay::poll()"), Some(0x1000));
        assert_eq!(table.addr_of("missing"), None);
        assert_eq!(Symbols::empty().format(0x30000), "0x00030000");
    }

    #[test]
    fn firmware_symbols() {
        let Some(root) = firmware_root() else { return };
        let table = Symbols::from_elf(&root.join(FIRMWARE_ELF)).unwrap();
        assert!(table.syms.windows(2).all(|w| w[0].addr <= w[1].addr));
        assert!(table.addr_of("ultimate_main").is_some());
        assert_eq!(table.lookup(0x33700), Some(("freertos_risc_v_trap_handler", 0)));
        assert_eq!(table.addr_of("__crt0_dummy_trap_handler"), Some(0x30178));
        let assert_fn = table.addr_of("vAssertCalled").unwrap();
        assert_eq!(table.format(assert_fn + 4), "vAssertCalled+0x4");
        assert!(table.addr_of("C_exception_handler").is_some());
        assert!(table.syms.iter().all(|s| !s.mangled.starts_with('$')));
        let cpp = table.syms.iter().find(|s| s.mangled.starts_with("_ZN") && s.name != s.mangled).unwrap();
        assert_eq!(table.addr_of(&cpp.name), table.addr_of(&cpp.mangled));
    }
}
