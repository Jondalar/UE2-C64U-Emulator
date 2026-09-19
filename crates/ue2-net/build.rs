//! With the `slirp` feature: links libslirp and compiles `layout.c` for the FFI layout test.
//!
//! - `SLIRP_LIB_DIR` names the library directory; without it Homebrew's /opt/homebrew/lib is searched when it exists,
//!   else only the linker's default paths (where a Linux distribution's libslirp lives). On Windows it is vcpkg's
//!   `installed\x64-windows\lib` (docs/specs/S22-windows.md §4).
//! - `layout.c` includes `<slirp/libslirp.h>` from `SLIRP_INCLUDE_DIR`, else the `include` next to `SLIRP_LIB_DIR`,
//!   else Homebrew's, else the compiler's defaults. When it does not compile, the layout test is skipped.

use std::path::{Path, PathBuf};

/// Homebrew's prefix on Apple silicon.
const HOMEBREW: &str = "/opt/homebrew";

fn main() {
    println!("cargo:rerun-if-env-changed=SLIRP_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SLIRP_INCLUDE_DIR");
    println!("cargo:rerun-if-changed=layout.c");
    println!("cargo::rustc-check-cfg=cfg(ue2_slirp_layout)");
    if std::env::var_os("CARGO_FEATURE_SLIRP").is_none() {
        return;
    }
    let lib_dir = std::env::var_os("SLIRP_LIB_DIR").map(PathBuf::from);
    match &lib_dir {
        Some(dir) => println!("cargo:rustc-link-search=native={}", dir.display()),
        None if Path::new(HOMEBREW).join("lib").is_dir() => println!("cargo:rustc-link-search=native={HOMEBREW}/lib"),
        None => {}
    }
    println!("cargo:rustc-link-lib=dylib=slirp");

    let include = std::env::var_os("SLIRP_INCLUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| lib_dir.as_ref().and_then(|d| d.parent()).map(|p| p.join("include")))
        .or_else(|| Some(Path::new(HOMEBREW).join("include")).filter(|p| p.is_dir()));
    let mut build = cc::Build::new();
    build.file("layout.c").warnings(false).cargo_warnings(false);
    if let Some(dir) = include {
        build.include(dir);
    }
    match build.try_compile("ue2_slirp_layout") {
        Ok(()) => println!("cargo:rustc-cfg=ue2_slirp_layout"),
        Err(e) => println!("cargo:warning=libslirp.h not compiled, FFI layout test skipped: {e}"),
    }
}
