//! The volume image as the emulated stick's medium, counting guest writes so the frontend can tell when the guest
//! has been quiet.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ue2_core::devices::usb::block::{BlockBackend, ImageFile};

pub struct CountingBackend {
    inner: ImageFile,
    writes: Arc<AtomicU64>,
}

impl CountingBackend {
    /// The image at `path`; each WRITE(10) that reaches it adds 1 to `writes`.
    pub fn open(path: &Path, read_only: bool, writes: Arc<AtomicU64>) -> io::Result<CountingBackend> {
        let inner = if read_only { ImageFile::open_read_only(path)? } else { ImageFile::open(path)? };
        if !read_only && inner.read_only() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not writable", path.display())));
        }
        Ok(CountingBackend { inner, writes })
    }
}

impl BlockBackend for CountingBackend {
    fn blocks(&self) -> u64 {
        self.inner.blocks()
    }

    fn read_only(&self) -> bool {
        self.inner.read_only()
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        // Counted before the write: a failed write may still have changed blocks.
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write_blocks(lba, data)
    }
}
