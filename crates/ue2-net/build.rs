//! Links the system libslirp. `SLIRP_LIB_DIR` names its directory; without it Homebrew's /opt/homebrew/lib is searched
//! when it exists, else only the linker's default paths (where a Linux distribution's libslirp lives).

use std::path::Path;

/// Homebrew's library directory on Apple silicon.
const HOMEBREW_LIB: &str = "/opt/homebrew/lib";

fn main() {
    println!("cargo:rerun-if-env-changed=SLIRP_LIB_DIR");
    match std::env::var("SLIRP_LIB_DIR") {
        Ok(dir) => println!("cargo:rustc-link-search=native={dir}"),
        Err(_) if Path::new(HOMEBREW_LIB).is_dir() => println!("cargo:rustc-link-search=native={HOMEBREW_LIB}"),
        Err(_) => {}
    }
    println!("cargo:rustc-link-lib=dylib=slirp");
}
