//! Volume images: an MBR with one FAT32 partition at sector 2048, like `scripts/make-sd-image.sh` makes (the
//! firmware walks the MBR table in `Disk::Init`, filesystem/disk.cc:100-117), built from a host tree with the fatfs
//! crate into a sparse file.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use chrono::{Datelike, TimeZone, Timelike};
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

use crate::manifest::{Entry, Skipped};
use crate::scan::{self, Scan};
use crate::spec::default_size;
use crate::Hasher;

pub const SECTOR: u64 = 512;
/// First sector of the partition.
pub const PARTITION_START: u64 = 2048;
/// MBR partition types the reader accepts: FAT32 CHS (0x0B) and FAT32 LBA (0x0C).
const FAT32_TYPES: [u8; 2] = [0x0B, 0x0C];

/// A byte range of an image file as a stream, which is how fatfs wants a volume.
pub struct Partition {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Partition {
    pub fn new(file: File, start: u64, len: u64) -> Partition {
        Partition { file, start, len, pos: 0 }
    }
}

impl Read for Partition {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        let got = crate::os::read_at(&self.file, &mut buf[..n], self.start + self.pos)?;
        if got == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "image file shorter than its partition"));
        }
        self.pos += got as u64;
        Ok(got)
    }
}

impl Write for Partition {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::new(ErrorKind::WriteZero, "write past the end of the partition"));
        }
        let put = crate::os::write_at(&self.file, &buf[..n], self.start + self.pos)?;
        self.pos += put as u64;
        Ok(put)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Partition {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match target.filter(|&t| t <= self.len) {
            Some(t) => {
                self.pos = t;
                Ok(t)
            }
            None => Err(io::Error::new(ErrorKind::InvalidInput, "seek outside the partition")),
        }
    }
}

/// Start and length in bytes of partition 1, checked against the file length.
pub fn read_partition(file: &File) -> std::result::Result<(u64, u64), String> {
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut mbr = [0; 512];
    crate::os::read_exact_at(file, &mut mbr, 0).map_err(|e| format!("reading the MBR: {e}"))?;
    if mbr[510..] != [0x55, 0xAA] {
        return Err("no MBR signature".into());
    }
    let entry = &mbr[0x1BE..0x1CE];
    if !FAT32_TYPES.contains(&entry[4]) {
        return Err(format!("partition 1 has type {:#04x}, not FAT32", entry[4]));
    }
    let start = u64::from(u32::from_le_bytes(entry[8..12].try_into().unwrap())) * SECTOR;
    let len = u64::from(u32::from_le_bytes(entry[12..16].try_into().unwrap())) * SECTOR;
    if start == 0 || len == 0 || start + len > file_len {
        return Err(format!("partition 1 ({start}+{len} bytes) does not fit the {file_len}-byte image"));
    }
    Ok((start, len))
}

/// fatfs on partition 1 of the image at `path`, read-only, short names decoded as code page 437 like the firmware
/// writes them ([`crate::cp437`]).
pub fn open_fs(path: &Path) -> std::result::Result<FileSystem<Partition>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (start, len) = read_partition(&file)?;
    let options = FsOptions::new().oem_cp_converter(&crate::cp437::CP437);
    FileSystem::new(Partition::new(file, start, len), options).map_err(|e| format!("mounting the volume: {e}"))
}

/// FAT volume label from a directory name: upper-case letters, digits, `-` and `_` (everything else becomes `_`),
/// at most 11 characters; `UE2DIR` for a name with nothing usable.
pub fn volume_label(name: &str) -> [u8; 11] {
    let mut label = [b' '; 11];
    for (len, c) in name.chars().take(11).enumerate() {
        let c = c.to_ascii_uppercase();
        label[len] = if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_' { c as u8 } else { b'_' };
    }
    if label.iter().all(|&b| b == b'_' || b == b' ') {
        return *b"UE2DIR     ";
    }
    label
}

/// Cluster size: the largest power of two from 512 bytes that still gives at least 70 000 clusters (FAT32 needs
/// more than 65 525, ff.c `MAX_FAT16`), capped at the usual FAT32 sizes (4 K up to 8 GiB, then 8 K, 16 K, 32 K).
pub fn cluster_size(partition_bytes: u64) -> u32 {
    let usual: u64 = match partition_bytes {
        b if b <= 8 << 30 => 4096,
        b if b <= 16 << 30 => 8192,
        b if b <= 32 << 30 => 16384,
        _ => 32768,
    };
    let mut size = 512;
    while size * 2 <= usual && partition_bytes / (size * 2) >= 70_000 {
        size *= 2;
    }
    size as u32
}

/// FAT timestamp of a host time, clamped to 1980-2107. FAT has no time zone, so the volume uses the firmware's:
/// the emulated RTC counts host UTC seconds (ue2-core devices/misc.rs `RtcTimer`) and the firmware's `get_fattime`
/// applies no zone (rtc_dummy.cc:119-123), so the files the guest writes carry UTC. Imported files do too, and a
/// synced file gets its host time back exactly (to FAT's 2 s).
pub fn fat_time(t: SystemTime) -> fatfs::DateTime {
    let first = chrono::Utc.with_ymd_and_hms(1980, 1, 1, 0, 0, 0).unwrap();
    let last = chrono::Utc.with_ymd_and_hms(2107, 12, 31, 23, 59, 58).unwrap();
    let utc = chrono::DateTime::<chrono::Utc>::from(t).clamp(first, last);
    fatfs::DateTime {
        date: fatfs::Date { year: utc.year() as u16, month: utc.month() as u16, day: utc.day() as u16 },
        time: fatfs::Time {
            hour: utc.hour() as u16,
            min: utc.minute() as u16,
            sec: utc.second() as u16,
            millis: (utc.nanosecond() / 1_000_000).min(999) as u16,
        },
    }
}

/// Host time of a FAT timestamp (UTC, see [`fat_time`]); None for an invalid one.
pub fn host_time(dt: fatfs::DateTime) -> Option<SystemTime> {
    let (d, t) = (dt.date, dt.time);
    let utc = chrono::Utc
        .with_ymd_and_hms(i32::from(d.year), u32::from(d.month), u32::from(d.day), t.hour.into(), t.min.into(), t.sec.into())
        .single()?;
    Some(SystemTime::from(utc) + Duration::from_millis(t.millis.into()))
}

/// What [`build`] put on the volume.
pub struct Built {
    pub entries: BTreeMap<String, Entry>,
    pub skipped: Vec<Skipped>,
    pub size: u64,
    pub files: usize,
    pub dirs: usize,
    pub content: u64,
}

/// The sectors of a volume for `items` (`size` None applies [`default_size`]); an error when they do not fit.
pub fn volume_sectors(root: &Path, items: &[scan::HostItem], size: Option<u64>) -> Result<u64> {
    let content: u64 = items.iter().filter(|i| !i.dir).map(|i| i.stat.size).sum();
    let size = size.unwrap_or_else(|| default_size(content));
    let sectors = (size / SECTOR).min(u64::from(u32::MAX));
    let part_bytes = (sectors - PARTITION_START) * SECTOR;
    let cluster = u64::from(cluster_size(part_bytes));
    let needed: u64 = items.iter().map(|i| if i.dir { cluster } else { i.stat.size.div_ceil(cluster) * cluster }).sum();
    // FAT copies, reserved sectors and the root directory take about 2 % at most.
    if needed > part_bytes / 100 * 97 {
        bail!(
            "{} holds {} MiB of files; a {} MiB volume is too small (give a larger size=, e.g. size={}M)",
            root.display(),
            content >> 20,
            size >> 20,
            ((needed * 2) >> 20) + 64
        );
    }
    Ok(sectors)
}

/// Build the image file `image` (must not exist) from the tree at `root` (canonical). `size` None applies
/// [`default_size`]. The host is only read.
pub fn build(root: &Path, image: &Path, size: Option<u64>, label: [u8; 11]) -> Result<Built> {
    let Scan { items, mut skipped } = scan::scan(root).with_context(|| format!("reading {}", root.display()))?;
    let content: u64 = items.iter().filter(|i| !i.dir).map(|i| i.stat.size).sum();
    let sectors = volume_sectors(root, &items, size)?;
    let part_bytes = (sectors - PARTITION_START) * SECTOR;
    let cluster = u64::from(cluster_size(part_bytes));

    let file = OpenOptions::new().read(true).write(true).create_new(true).open(image)
        .with_context(|| format!("creating {}", image.display()))?;
    file.set_len(sectors * SECTOR)?;
    let mut mbr = [0u8; 512];
    mbr[0x1BE..0x1C6].copy_from_slice(&[0x00, 0xFE, 0xFF, 0xFF, 0x0C, 0xFE, 0xFF, 0xFF]);
    mbr[0x1C6..0x1CA].copy_from_slice(&(PARTITION_START as u32).to_le_bytes());
    mbr[0x1CA..0x1CE].copy_from_slice(&((sectors - PARTITION_START) as u32).to_le_bytes());
    mbr[510..].copy_from_slice(&[0x55, 0xAA]);
    crate::os::write_all_at(&file, &mbr, 0)?;

    let start = PARTITION_START * SECTOR;
    let volume_id = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
    let options = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .bytes_per_cluster(cluster as u32)
        .volume_label(label)
        .volume_id(volume_id);
    fatfs::format_volume(Partition::new(file.try_clone()?, start, part_bytes), options).context("formatting FAT32")?;
    let fs = FileSystem::new(Partition::new(file.try_clone()?, start, part_bytes), FsOptions::new())
        .context("mounting the new volume")?;

    let mut entries = BTreeMap::new();
    let (mut files, mut dirs) = (0, 0);
    let mut failed_dirs: Vec<String> = Vec::new();
    {
        let root_dir = fs.root_dir();
        for item in &items {
            if failed_dirs.iter().any(|d| item.rel.starts_with(&format!("{d}/"))) {
                continue;
            }
            if item.dir {
                match root_dir.create_dir(&item.rel) {
                    Ok(_) => {
                        dirs += 1;
                        let entry = Entry { dir: true, size: 0, hash: None, host: Some(item.stat), host_hash: None, link: false, diverged: false };
                        entries.insert(item.rel.clone(), entry);
                    }
                    Err(e) => {
                        skipped.push(Skipped { path: item.rel.clone(), reason: format!("the volume refused the directory: {e}") });
                        failed_dirs.push(item.rel.clone());
                    }
                }
                continue;
            }
            let mut src = match File::open(&item.path) {
                Ok(src) => src,
                Err(e) => {
                    skipped.push(Skipped { path: item.rel.clone(), reason: format!("cannot open: {e}") });
                    continue;
                }
            };
            let mut dst = match root_dir.create_file(&item.rel) {
                Ok(dst) => dst,
                Err(e) => {
                    skipped.push(Skipped { path: item.rel.clone(), reason: format!("the volume refused the file: {e}") });
                    continue;
                }
            };
            let mut hasher = Hasher::default();
            let mut buf = vec![0; 1 << 16];
            let mut len = 0u64;
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e).with_context(|| format!("reading {}", item.path.display())),
                };
                if len + n as u64 > scan::MAX_FILE {
                    bail!("{} grew past 4 GiB while it was imported", item.path.display());
                }
                dst.write_all(&buf[..n]).with_context(|| format!("writing {} to the volume (volume full? give size=)", item.rel))?;
                hasher.update(&buf[..n]);
                len += n as u64;
            }
            let time = fat_time(item.modified);
            // Deprecated in favour of a custom TimeProvider, which would be global state per file system. The setters
            // still work after the last write: only a later write or read overwrites them.
            #[allow(deprecated)]
            {
                dst.set_modified(time);
                dst.set_created(time);
            }
            dst.flush().with_context(|| format!("finishing {} on the volume", item.rel))?;
            drop(dst);
            files += 1;
            let hash = hasher.hex();
            // The host stat is the one from before reading: a host change during the copy shows up as a host change.
            let entry = Entry {
                dir: false,
                size: len,
                hash: Some(hash.clone()),
                host: Some(item.stat),
                host_hash: Some(hash),
                link: item.link,
                diverged: false,
            };
            entries.insert(item.rel.clone(), entry);
        }
    }
    fs.unmount().context("unmounting the new volume")?;
    Ok(Built { entries, skipped, size: sectors * SECTOR, files, dirs, content })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_clusters_and_times() {
        assert_eq!(&volume_label("my games"), b"MY_GAMES   ");
        assert_eq!(&volume_label("c64-stuff.2024 extra"), b"C64-STUFF_2");
        assert_eq!(&volume_label("..."), b"UE2DIR     ");
        assert_eq!(cluster_size(64 << 20), 512);
        assert_eq!(cluster_size(256 << 20), 2048);
        assert_eq!(cluster_size(4 << 30), 4096);
        assert_eq!(cluster_size(20 << 30), 16384);

        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000);
        let fat = fat_time(t);
        assert_eq!((fat.date.year, fat.date.month, fat.date.day, fat.time.hour, fat.time.min, fat.time.sec), (2025, 6, 15, 15, 6, 40), "UTC");
        let back = host_time(fat).unwrap();
        assert!(back <= t && t.duration_since(back).unwrap() < Duration::from_secs(2), "2-second FAT resolution");
        assert_eq!(fat_time(SystemTime::UNIX_EPOCH).date.year, 1980, "clamped");
        let zero = fatfs::DateTime { date: fatfs::Date { year: 1980, month: 0, day: 0 }, time: fatfs::Time { hour: 0, min: 0, sec: 0, millis: 0 } };
        assert_eq!(host_time(zero), None);
    }
}
