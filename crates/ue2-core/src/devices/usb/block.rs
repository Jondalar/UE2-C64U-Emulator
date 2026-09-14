//! Block storage behind a USB mass-storage device (docs/hw/09-usb.md §F6, T1 step 9).
//!
//! [`BlockBackend`] is all the SCSI layer (`storage.rs`) needs: a fixed number of 512-byte blocks that can be read
//! and written. The trait knows nothing about host files. [`ImageFile`], a raw disk image, is the implementation
//! `--usb IMAGE` uses; frontends plug in others through `Machine::usb_attach_storage` (the `ue2-vfat` crate builds
//! a FAT32 volume from a host directory, docs/status/usb-dir.md).

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Logical block size reported by READ CAPACITY; the driver accepts up to 4096 (usb_scsi.cc:562).
pub const BLOCK_SIZE: u64 = 512;

/// Storage medium of a USB stick: `blocks()` blocks of [`BLOCK_SIZE`] bytes.
///
/// `Send`, so a frontend can build a backend on one thread and hand it to the emulation thread.
pub trait BlockBackend: Send {
    /// Capacity in blocks. Fixed while the backend is attached.
    fn blocks(&self) -> u64;
    /// A read-only medium: WRITE(10) fails with DATA PROTECT.
    fn read_only(&self) -> bool;
    /// Fill `buf` (a whole number of blocks) from block `lba` on.
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Store `data` (a whole number of blocks) from block `lba` on.
    fn write_blocks(&mut self, lba: u64, data: &[u8]) -> io::Result<()>;
}

/// A raw disk image file; its length rounded down to [`BLOCK_SIZE`] is the capacity.
pub struct ImageFile {
    file: File,
    blocks: u64,
    read_only: bool,
}

impl ImageFile {
    /// Opens the image read-write, or read-only (write protected) when writing is not permitted.
    pub fn open(path: &Path) -> io::Result<ImageFile> {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => Self::new(file, false),
            Err(e) if matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem) => {
                Self::new(File::open(path)?, true)
            }
            Err(e) => Err(e),
        }
    }

    /// Opens the image read-only: the stick is write protected.
    pub fn open_read_only(path: &Path) -> io::Result<ImageFile> {
        Self::new(File::open(path)?, true)
    }

    fn new(file: File, read_only: bool) -> io::Result<ImageFile> {
        let blocks = file.metadata()?.len() / BLOCK_SIZE;
        if blocks == 0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "USB image is smaller than one block"));
        }
        Ok(ImageFile { file, blocks, read_only })
    }
}

impl BlockBackend for ImageFile {
    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(lba * BLOCK_SIZE))?;
        self.file.read_exact(buf)
    }

    fn write_blocks(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "read-only image"));
        }
        self.file.seek(SeekFrom::Start(lba * BLOCK_SIZE))?;
        self.file.write_all(data)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn image_file_reads_and_writes_whole_blocks() {
        let img = NamedTempFile::new().unwrap();
        fs::write(img.path(), vec![0u8; 3 * 512 + 100]).unwrap();
        let mut dev = ImageFile::open(img.path()).unwrap();
        assert_eq!((dev.blocks(), dev.read_only()), (3, false), "a partial last block does not count");
        dev.write_blocks(2, &[0xA5; 512]).unwrap();
        let mut buf = [0; 1024];
        dev.read_blocks(1, &mut buf).unwrap();
        assert_eq!((buf[511], buf[512], buf[1023]), (0, 0xA5, 0xA5));
        assert!(dev.read_blocks(3, &mut [0; 512]).is_err(), "past the end");

        let mut ro = ImageFile::open_read_only(img.path()).unwrap();
        assert!(ro.read_only());
        assert!(ro.write_blocks(0, &[1; 512]).is_err());
        assert_eq!(fs::read(img.path()).unwrap()[0], 0);
        assert!(ImageFile::open(NamedTempFile::new().unwrap().path()).is_err(), "empty image");
    }
}
