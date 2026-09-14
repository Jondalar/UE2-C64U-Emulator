//! What a volume holds and what the host held, as of the last build or sync (`manifest.json` in the work
//! directory). Sync diffs the image against the image side of this record, never against the host, so a host
//! change is never mistaken for a guest change; the host side detects host changes and conflicts.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const FILE_NAME: &str = "manifest.json";

/// Host file identity: size, modification time and inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStat {
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_ns: u32,
    pub ino: u64,
}

impl HostStat {
    pub fn of(meta: &Metadata) -> HostStat {
        HostStat { size: meta.len(), mtime_s: meta.mtime(), mtime_ns: meta.mtime_nsec() as u32, ino: meta.ino() }
    }
}

/// One file or directory, keyed by its `/`-separated path relative to the shared directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub dir: bool,
    /// Image side: file size on the volume.
    #[serde(default)]
    pub size: u64,
    /// Image side: SHA-256 of the file on the volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Host side: the host file or directory as it was when it last matched the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStat>,
    /// Host side: SHA-256 of the host file then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_hash: Option<String>,
    /// Imported through a symlink inside the directory: the symlink is never replaced, a guest change becomes a
    /// conflict copy next to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
    /// The last sync wrote the guest version as a conflict copy and left the host file alone: the image and the host
    /// differ at this path until the next build. A later guest change here is a conflict copy again, a guest deletion
    /// is not written back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub diverged: bool,
}

/// A host path the volume does not hold, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Canonical shared directory.
    pub host_root: PathBuf,
    /// Image file name in the work directory.
    pub image: String,
    /// FAT volume label.
    pub label: String,
    /// Volume (image) size in bytes.
    pub size: u64,
    /// Unix time of the build.
    pub built_unix: u64,
    pub entries: BTreeMap<String, Entry>,
    pub skipped: Vec<Skipped>,
    /// Image directory → host directory a sync writes it to instead, until the next build: a conflict directory
    /// (the host has a file or symlink under the guest directory's name).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remap: BTreeMap<String, String>,
}

impl Manifest {
    pub fn load(path: &Path) -> io::Result<Manifest> {
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if manifest.version != VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("manifest version {}", manifest.version)));
        }
        Ok(manifest)
    }

    /// Write through a temporary file and a rename, so a crash leaves the old or the new manifest.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
        }
        fs::rename(tmp, path)
    }

    /// Number of files (not directories).
    pub fn files(&self) -> usize {
        self.entries.values().filter(|e| !e.dir).count()
    }
}
