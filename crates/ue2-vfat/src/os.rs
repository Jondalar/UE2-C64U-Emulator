//! What differs between Unix and Windows (docs/specs/S22-windows.md §3): positional file I/O, the metadata that
//! identifies a host file, and the locks that keep two sticks off one directory.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Read at `at` without the file position (Unix); on Windows the position moves, which nothing here relies on.
pub fn read_at(file: &File, buf: &mut [u8], at: u64) -> io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_at(file, buf, at);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(file, buf, at);
}

pub fn write_at(file: &File, buf: &[u8], at: u64) -> io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_at(file, buf, at);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_write(file, buf, at);
}

pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut at: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, at) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                at += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn write_all_at(file: &File, mut buf: &[u8], mut at: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match write_at(file, buf, at) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                at += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Modification time (seconds, nanoseconds) and file identity: the inode on Unix; 0 on Windows, where std has no
/// stable file index, so size, time and content hash decide there.
pub fn mtime_ino(meta: &Metadata) -> (i64, u32, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.mtime(), meta.mtime_nsec() as u32, meta.ino())
    }
    #[cfg(windows)]
    {
        let since = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).unwrap_or_default();
        (since.as_secs() as i64, since.subsec_nanos(), 0)
    }
}

/// Everything a host change alters, including a permission change, but not reading.
pub fn fingerprint(meta: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = meta;
        format!("{:?}", (m.mode(), m.size(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        format!("{:?}", (meta.file_attributes(), meta.file_size(), meta.last_write_time(), meta.creation_time()))
    }
}

/// A lock on a directory, held while the value lives; `None` when another holder conflicts.
///
/// - Unix: `flock` on the directory itself, which conflicts between descriptors of one process too.
/// - Windows cannot lock a directory: a lock file per directory, named after its path, in `%TEMP%\ue2emu-locks`, with
///   `File::try_lock`, which also conflicts between handles of one process. Nothing is written into the directory.
pub fn lock_dir(dir: &Path, exclusive: bool) -> io::Result<Option<File>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = File::open(dir)?;
        let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        // SAFETY: flock(2) on a descriptor this function owns.
        let locked = unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) == 0 };
        Ok(locked.then_some(file))
    }
    #[cfg(windows)]
    {
        let locks = std::env::temp_dir().join("ue2emu-locks");
        std::fs::create_dir_all(&locks)?;
        let key = dir.as_os_str().to_string_lossy().to_lowercase();
        // FNV-1a, as `instance_unique_id` in ue2emu: stable across Rust releases.
        let hash =
            key.bytes().fold(0xCBF2_9CE4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(locks.join(format!("{hash:016x}.lock")))?;
        let locked = if exclusive { file.try_lock() } else { file.try_lock_shared() };
        match locked {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Whether an error says a path component is a file, not a directory.
pub fn not_a_directory(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::ENOTDIR);
    #[cfg(windows)]
    return e.kind() == io::ErrorKind::NotADirectory;
}

/// Give `out` the permission bits of `meta` (Unix); Windows files keep the defaults of their directory.
pub fn copy_permissions(meta: &Metadata, out: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        out.set_permissions(std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777))
    }
    #[cfg(windows)]
    {
        let _ = (meta, out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_io_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
        write_all_at(&file, b"hello", 3).unwrap();
        let mut buf = [0; 5];
        read_exact_at(&file, &mut buf, 3).unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(read_exact_at(&file, &mut [0; 4], 6).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_directory_lock_conflicts_inside_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let shared = lock_dir(dir.path(), false).unwrap().expect("shared");
        assert!(lock_dir(dir.path(), false).unwrap().is_some(), "two shared locks");
        assert!(lock_dir(dir.path(), true).unwrap().is_none(), "exclusive while shared");
        drop(shared);
    }
}
