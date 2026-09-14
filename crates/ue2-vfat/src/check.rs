//! Structural check of a FAT32 volume before anything is synced from it. A guest that crashed or was unplugged in
//! the middle of a write leaves chains and directory entries that disagree; such an image must never reach the
//! host (docs/status/usb-dir.md §Safety).
//!
//! Checked, straight from the on-disk structures: the boot sector is FAT32 with 512-byte sectors and fits its
//! partition; every directory and file chain stays inside the cluster range, never meets a free or bad cluster,
//! ends, and shares no cluster with another chain; every file's chain has exactly the clusters its size needs.

use std::fs::File;
use std::os::unix::fs::FileExt;

/// Counts of what the directory tree holds, for comparison with the fatfs walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

const EOC: u32 = 0x0FFF_FFF8;
const BAD: u32 = 0x0FFF_FFF7;
const MAX_DEPTH: usize = 64;

struct Volume<'a> {
    file: &'a File,
    data_start: u64,
    cluster_bytes: u64,
    max_cluster: u32,
    fat: Vec<u8>,
    used: Vec<u64>,
}

impl Volume<'_> {
    fn next(&self, cluster: u32) -> u32 {
        let at = cluster as usize * 4;
        u32::from_le_bytes(self.fat[at..at + 4].try_into().unwrap()) & 0x0FFF_FFFF
    }

    /// The clusters of the chain from `first`, marked used.
    fn chain(&mut self, first: u32, what: &str) -> Result<Vec<u32>, String> {
        let mut chain = Vec::new();
        let mut cluster = first;
        loop {
            if cluster < 2 || cluster > self.max_cluster {
                return Err(format!("{what}: cluster {cluster} is outside the volume"));
            }
            let (word, bit) = (cluster as usize / 64, 1u64 << (cluster % 64));
            if self.used[word] & bit != 0 {
                return Err(format!("{what}: cluster {cluster} is used twice (cross-linked or looping chain)"));
            }
            self.used[word] |= bit;
            chain.push(cluster);
            match self.next(cluster) {
                next if next >= EOC => return Ok(chain),
                BAD => return Err(format!("{what}: chain runs into a bad cluster after {cluster}")),
                0 => return Err(format!("{what}: chain runs into a free cluster after {cluster}")),
                next => cluster = next,
            }
        }
    }
}

/// Check the FAT32 volume at `start`..`start + len` of `file`.
pub fn check_fat32(file: &File, start: u64, len: u64) -> Result<Totals, String> {
    let io = |e: std::io::Error| format!("reading the image: {e}");
    let mut boot = [0u8; 512];
    file.read_exact_at(&mut boot, start).map_err(io)?;
    let u16_at = |i: usize| u16::from_le_bytes([boot[i], boot[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(boot[i..i + 4].try_into().unwrap());
    if boot[510..] != [0x55, 0xAA] {
        return Err("boot sector signature missing".into());
    }
    let (bytes_per_sector, sectors_per_cluster, reserved, fats) = (u16_at(11), boot[13], u16_at(14), boot[16]);
    let (root_entries, total16, fat16_sectors, total32, fat_sectors, root_cluster) =
        (u16_at(17), u16_at(19), u16_at(22), u32_at(32), u32_at(36), u32_at(44));
    if bytes_per_sector != 512 {
        return Err(format!("sector size {bytes_per_sector}, expected 512"));
    }
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() || reserved == 0 || fats == 0 {
        return Err("boot sector geometry is invalid".into());
    }
    if root_entries != 0 || fat16_sectors != 0 || fat_sectors == 0 {
        return Err("not a FAT32 volume".into());
    }
    let total = u64::from(if total16 != 0 { u32::from(total16) } else { total32 });
    if total * 512 > len {
        return Err("the volume is larger than its partition".into());
    }
    let data_sectors = u64::from(reserved) + u64::from(fats) * u64::from(fat_sectors);
    if data_sectors >= total {
        return Err("the FATs do not fit the volume".into());
    }
    let clusters = (total - data_sectors) / u64::from(sectors_per_cluster);
    if clusters <= 65_525 {
        return Err(format!("{clusters} clusters are too few for FAT32"));
    }
    if u64::from(fat_sectors) * 512 < (clusters + 2) * 4 {
        return Err("the FAT is too small for the cluster count".into());
    }
    let mut fat = vec![0; (clusters as usize + 2) * 4];
    file.read_exact_at(&mut fat, start + u64::from(reserved) * 512).map_err(io)?;
    let mut vol = Volume {
        file,
        data_start: start + data_sectors * 512,
        cluster_bytes: u64::from(sectors_per_cluster) * 512,
        max_cluster: (clusters + 1) as u32,
        used: vec![0; clusters as usize / 64 + 2],
        fat,
    };

    let mut totals = Totals::default();
    let mut stack = vec![(root_cluster, "/".to_string(), 0usize)];
    while let Some((first, path, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(format!("{path}: directories nested deeper than {MAX_DEPTH}"));
        }
        let chain = vol.chain(first, &format!("directory {path}"))?;
        let mut data = vec![0; chain.len() * vol.cluster_bytes as usize];
        for (i, &cluster) in chain.iter().enumerate() {
            let at = vol.data_start + u64::from(cluster - 2) * vol.cluster_bytes;
            let chunk = &mut data[i * vol.cluster_bytes as usize..][..vol.cluster_bytes as usize];
            vol.file.read_exact_at(chunk, at).map_err(io)?;
        }
        for e in data.chunks_exact(32) {
            if e[0] == 0 {
                break;
            }
            let attr = e[11];
            if e[0] == 0xE5 || attr & 0x0F == 0x0F || attr & 0x08 != 0 {
                continue;
            }
            if e[0] == b'.' && (e[1] == b' ' || (e[1] == b'.' && e[2] == b' ')) {
                continue;
            }
            let name = format!("{path}{}", String::from_utf8_lossy(&e[..11]).trim_end());
            let first = u32::from(u16::from_le_bytes([e[20], e[21]])) << 16 | u32::from(u16::from_le_bytes([e[26], e[27]]));
            let size = u64::from(u32::from_le_bytes(e[28..32].try_into().unwrap()));
            if attr & 0x10 != 0 {
                totals.dirs += 1;
                stack.push((first, format!("{name}/"), depth + 1));
                continue;
            }
            totals.files += 1;
            totals.bytes += size;
            if size == 0 && first == 0 {
                continue;
            }
            let length = vol.chain(first, &format!("file {name}"))?.len() as u64;
            let needed = size.div_ceil(vol.cluster_bytes).max(1);
            if length != needed {
                return Err(format!("file {name}: {size} bytes need {needed} clusters, its chain has {length}"));
            }
        }
    }
    Ok(totals)
}
