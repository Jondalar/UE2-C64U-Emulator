//! A host directory as a USB stick for ue2emu (`--usb-dir`, docs/status/usb-dir.md).
//!
//! - [`volume::DirVolume`] builds a FAT32 volume (MBR, long file names, file timestamps) from the directory tree
//!   into a sparse image file in the run directory ([`image`]), and records what it imported in a [`manifest`].
//! - [`backend::CountingBackend`] serves that image to the emulated stick and counts guest writes.
//! - [`volume::DirVolume::sync`] writes what the guest changed back to the host: it parses a snapshot of the image
//!   ([`fatread`], checked structurally by [`check`]), diffs it against the manifest and applies the changes with
//!   the safety rules of [`sync`] (atomic writes, conflict copies, `.ue2-trash` instead of deleting, a
//!   mass-deletion guard).
//! - [`worker::Worker`] runs one volume on its own thread with a file watcher ([`watch`]) that reports host changes.
//!
//! Nothing here touches the emulator; ue2emu drives it (crates/ue2emu/src/usbdir.rs).

pub mod backend;
pub mod check;
pub mod cp437;
pub mod fatread;
pub mod image;
pub mod manifest;
pub mod scan;
pub mod spec;
pub mod sync;
pub mod volume;
pub mod watch;
pub mod worker;

pub use spec::DirSpec;
pub use volume::DirVolume;

use sha2::{Digest, Sha256};

/// Local time as `YYYYMMDD-HHMMSS`, for trash directories and conflict names.
pub fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Incremental SHA-256 with a lower-case hex result.
#[derive(Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn hex(self) -> String {
        self.0.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// SHA-256 of everything `r` yields, and its length.
pub fn hash_reader(mut r: impl std::io::Read) -> std::io::Result<(String, u64)> {
    let mut hasher = Hasher::default();
    let mut buf = vec![0; 1 << 16];
    let mut len = 0;
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok((hasher.hex(), len)),
            Ok(n) => {
                hasher.update(&buf[..n]);
                len += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
