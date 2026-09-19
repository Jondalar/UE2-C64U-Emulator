//! One shared directory and its volume: safety checks and locks, the work directory in the run directory, build,
//! sync and host-change detection.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

use crate::fatread;
use crate::os;
use crate::image;
use crate::manifest::{self, Manifest, Skipped};
use crate::scan;
use crate::spec::DirSpec;
use crate::sync::{self, Report};
use crate::{hash_reader, timestamp, Hasher};

/// Marker in the work directory: the image holds guest writes that are not synced yet.
pub const UNSYNCED: &str = "unsynced";
/// Snapshot the frontend copies the image to before a sync.
pub const SNAPSHOT: &str = "snapshot.img";

/// Why a sync did not happen, or did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The image does not parse cleanly (the guest may have crashed or been unplugged mid-write).
    Parse(String),
    /// The mass-deletion guard refused it: host files the sync would delete, files in the manifest. Overwrites are
    /// not counted (`sync::Plan::destructive`).
    Guard { destructive: usize, files: usize },
    Io(String),
    /// Applied in part: some guest changes could not be written to the host ([`Report::failed`]). What was written
    /// is recorded; the image still holds the rest and must be kept, unsynced, until a later sync writes it.
    Incomplete(Report),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SyncError::Parse(e) => write!(f, "the image does not parse cleanly, nothing synced ({e}); the image is kept"),
            SyncError::Guard { destructive, files } => write!(
                f,
                "REFUSED: this sync would delete {destructive} of {files} files on the host (limit: 25 % or 50); \
                 nothing synced, the image is kept; check the stick, then run usb-sync --force"
            ),
            SyncError::Io(e) => write!(f, "sync failed: {e}"),
            SyncError::Incomplete(report) => write!(
                f,
                "NOT SYNCED: {} guest change(s) could not be written to the host (the NOT WRITTEN lines); the image keeps \
                 them and stays unsynced, and host changes wait; fix the cause on the host, then run usb-sync",
                report.failed
            ),
        }
    }
}

/// Directory locks that keep two sticks from syncing into the same files (`os::lock_dir`). Nothing is written into
/// the shared directory.
struct DirLock(#[allow(dead_code, reason = "held for the lock")] Vec<File>);

impl DirLock {
    /// Lock the shared directory `dir`: exclusively for a read-write stick, shared for a read-only one. A read-write
    /// stick also takes a shared lock on every ancestor it can open, so no read-write stick of this or another
    /// ue2emu shares a directory inside or around it (flock conflicts between descriptors of one process too).
    fn acquire(dir: &Path, read_only: bool) -> Result<DirLock> {
        let lock = os::lock_dir(dir, !read_only).with_context(|| format!("locking {}", dir.display()))?;
        let Some(file) = lock else {
            bail!(
                "{} is in use by another --usb-dir of this or another ue2emu (the same directory, or a read-write one \
                 inside it)",
                dir.display()
            );
        };
        let mut files = vec![file];
        if !read_only {
            for ancestor in dir.ancestors().skip(1) {
                let Ok(lock) = os::lock_dir(ancestor, false) else { continue };
                let Some(file) = lock else {
                    bail!(
                        "{} is inside {}, which a read-write --usb-dir of this or another ue2emu shares",
                        dir.display(),
                        ancestor.display()
                    );
                };
                files.push(file);
            }
        }
        Ok(DirLock(files))
    }

    /// Lock the work subdirectory exclusively: one stick per work directory.
    fn work(work: &Path) -> Result<DirLock> {
        let lock = os::lock_dir(work, true).with_context(|| format!("locking {}", work.display()))?;
        let Some(file) = lock else {
            bail!("the work directory {} is in use by another --usb-dir (the same directory shared twice?)", work.display());
        };
        Ok(DirLock(vec![file]))
    }
}

/// Why `root` (canonical) must not be shared: the file system root, or the home directory `home` or a directory
/// that contains it.
pub fn refused_root(root: &Path, home: Option<&Path>) -> Option<String> {
    if root.parent().is_none() {
        return Some("refusing to share the file system root".into());
    }
    if home.is_some_and(|home| home.starts_with(root)) {
        return Some(format!("refusing to share {}: it is or contains the home directory; share a directory inside it", root.display()));
    }
    None
}

pub struct DirVolume {
    spec: DirSpec,
    root: PathBuf,
    work: PathBuf,
    label: [u8; 11],
    manifest: Option<Manifest>,
    _lock: DirLock,
    _work_lock: DirLock,
}

impl std::fmt::Debug for DirVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("DirVolume").field("root", &self.root).field("work", &self.work).field("read_only", &self.spec.read_only).finish()
    }
}

/// The image to attach after [`DirVolume::prepare`] or [`DirVolume::build`].
#[derive(Debug)]
pub struct Prepared {
    pub image: PathBuf,
    /// The image of an earlier run with unsynced guest writes, attached again instead of a new build.
    pub resumed: bool,
    pub lines: Vec<String>,
}

/// Create (`unsynced`) or remove the `unsynced` marker at `marker`.
pub fn set_marker(marker: &Path, unsynced: bool) -> io::Result<()> {
    if unsynced {
        fs::write(marker, b"the image holds guest writes that are not synced to the host yet\n")
    } else {
        match fs::remove_file(marker) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

impl DirVolume {
    /// Check `spec.path`, lock it and create and lock its work directory under `work_root`. Refused: a missing path
    /// or not a directory, `/`, the home directory or a directory containing it, a work directory inside the shared
    /// one (or the reverse), and a directory another stick shares ([`DirLock::acquire`]).
    pub fn open(spec: &DirSpec, work_root: &Path) -> Result<DirVolume> {
        let root = fs::canonicalize(&spec.path).with_context(|| format!("--usb-dir {}", spec.path.display()))?;
        if !root.is_dir() {
            bail!("--usb-dir {}: not a directory", spec.path.display());
        }
        let home = std::env::home_dir().and_then(|h| fs::canonicalize(h).ok());
        if let Some(why) = refused_root(&root, home.as_deref()) {
            bail!("--usb-dir: {why}");
        }
        fs::create_dir_all(work_root).with_context(|| format!("creating {}", work_root.display()))?;
        let work_root = fs::canonicalize(work_root)?;
        if work_root.starts_with(&root) || root.starts_with(&work_root) {
            bail!("--usb-dir {}: the work directory {} must not be inside it or contain it", root.display(), work_root.display());
        }
        let lock = DirLock::acquire(&root, spec.read_only)?;
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut hasher = Hasher::default();
        hasher.update(root.as_os_str().as_encoded_bytes());
        let tag: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(32).collect();
        let work = work_root.join(format!("{tag}-{}", &hasher.hex()[..10]));
        fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
        let work_lock = DirLock::work(&work)?;
        let manifest = Manifest::load(&work.join(manifest::FILE_NAME)).ok().filter(|m| m.host_root == root);
        Ok(DirVolume { spec: spec.clone(), label: image::volume_label(&name), root, work, manifest, _lock: lock, _work_lock: work_lock })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn work(&self) -> &Path {
        &self.work
    }

    pub fn read_only(&self) -> bool {
        self.spec.read_only
    }

    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// The image the manifest describes.
    pub fn image_path(&self) -> Option<PathBuf> {
        self.manifest.as_ref().map(|m| self.work.join(&m.image))
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.work.join(SNAPSHOT)
    }

    /// The `unsynced` marker ([`set_marker`]).
    pub fn marker_path(&self) -> PathBuf {
        self.work.join(UNSYNCED)
    }

    /// The image for this run. A read-write stick first removes temporary files an interrupted sync left in the
    /// shared directory. An earlier run that ended with unsynced guest writes left the `unsynced` marker: its image
    /// is resumed (and synced by the frontend like any dirty image). If it cannot be resumed (read-only now, or its
    /// manifest is gone) every image file is kept as `unsynced-<timestamp>-<name>`, never deleted. Without the marker
    /// the old images are all synced: they are removed and a new one is built.
    pub fn prepare(&mut self) -> Result<Prepared> {
        let mut lines = if self.spec.read_only { Vec::new() } else { sync::remove_stale_temps(&self.root) };
        let marker = self.marker_path();
        let images = |work: &Path| -> io::Result<Vec<PathBuf>> {
            Ok(fs::read_dir(work)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("image-") && n.to_string_lossy().ends_with(".img")))
                .collect())
        };
        if marker.exists() {
            if let Some(image) = self.image_path().filter(|p| p.is_file() && !self.spec.read_only) {
                lines.push(format!(
                    "resuming {}: an earlier run ended with guest writes that were not synced to {}",
                    image.display(),
                    self.root.display()
                ));
                return Ok(Prepared { image, resumed: true, lines });
            }
            let stamp = timestamp();
            for image in images(&self.work)? {
                let kept = self.work.join(format!("unsynced-{stamp}-{}", image.file_name().unwrap().to_string_lossy()));
                fs::rename(&image, &kept)?;
                eprintln!(
                    "usb-dir: {}: an image with unsynced guest writes cannot be resumed (read-only stick or no manifest); kept as {}",
                    self.root.display(),
                    kept.display()
                );
            }
            fs::remove_file(&marker)?;
        }
        for image in images(&self.work)? {
            fs::remove_file(image)?;
        }
        let _ = fs::remove_file(self.work.join(SNAPSHOT));
        let mut prepared = self.build()?;
        lines.append(&mut prepared.lines);
        prepared.lines = lines;
        Ok(prepared)
    }

    /// Build a new image from the host into `image-<n>.img` and make it current. The previous image file is left
    /// for the caller to remove once nothing uses it.
    pub fn build(&mut self) -> Result<Prepared> {
        let mut n = self.manifest.as_ref().and_then(|m| m.image.strip_prefix("image-")?.strip_suffix(".img")?.parse::<u64>().ok()).unwrap_or(0);
        let path = loop {
            n += 1;
            let path = self.work.join(format!("image-{n}.img"));
            if !path.exists() {
                break path;
            }
        };
        let built = match image::build(&self.root, &path, self.spec.size, self.label) {
            Ok(built) => built,
            Err(e) => {
                let _ = fs::remove_file(&path);
                return Err(e);
            }
        };
        let label = String::from_utf8_lossy(&self.label).trim_end().to_string();
        let mut lines = vec![format!(
            "volume {label}, {} MiB, {} files ({} MiB) and {} directories imported, {} skipped",
            built.size >> 20,
            built.files,
            built.content >> 20,
            built.dirs,
            built.skipped.len()
        )];
        lines.extend(built.skipped.iter().map(|Skipped { path, reason }| format!("  skipped {path}: {reason}")));
        let manifest = Manifest {
            version: manifest::VERSION,
            host_root: self.root.clone(),
            image: path.file_name().unwrap().to_string_lossy().into_owned(),
            label,
            size: built.size,
            built_unix: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            entries: built.entries,
            skipped: built.skipped,
            remap: BTreeMap::new(),
        };
        manifest.save(&self.work.join(manifest::FILE_NAME))?;
        self.manifest = Some(manifest);
        Ok(Prepared { image: path, resumed: false, lines })
    }

    /// Rename the current image to `discarded-<timestamp>.img` (never deleted) and return the new name.
    pub fn discard_image(&mut self) -> io::Result<Option<PathBuf>> {
        let Some(image) = self.image_path().filter(|p| p.exists()) else { return Ok(None) };
        let kept = self.work.join(format!("discarded-{}.img", timestamp()));
        fs::rename(&image, &kept)?;
        Ok(Some(kept))
    }

    /// Remove a superseded `image-<n>.img` of this work directory.
    pub fn remove_old_image(&self, path: &Path) {
        let ours = path.parent() == Some(self.work.as_path())
            && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("image-"))
            && self.image_path().as_deref() != Some(path);
        if ours {
            let _ = fs::remove_file(path);
        }
    }

    /// Create or remove the `unsynced` marker.
    pub fn set_unsynced(&self, unsynced: bool) -> io::Result<()> {
        set_marker(&self.marker_path(), unsynced)
    }

    /// Write the guest's changes in the snapshot image `snapshot` back to the host (module `sync`). A read-only
    /// stick has nothing to sync. What was written is saved in the manifest even when some changes could not be
    /// written ([`SyncError::Incomplete`]).
    pub fn sync(&mut self, snapshot: &Path, force: bool) -> Result<Report, SyncError> {
        if self.spec.read_only {
            return Ok(Report { lines: vec!["read-only stick: nothing to sync".into()], ..Report::default() });
        }
        let manifest = self.manifest.as_mut().ok_or_else(|| SyncError::Io("no manifest".into()))?;
        let tree = fatread::read_image(snapshot).map_err(SyncError::Parse)?;
        let plan = sync::plan(manifest, &tree);
        if plan.is_empty() {
            return Ok(Report { lines: vec!["no guest changes".into()], ..Report::default() });
        }
        if !force {
            sync::guard(&plan, manifest.files()).map_err(|(destructive, files)| SyncError::Guard { destructive, files })?;
        }
        let report = sync::apply(&self.root, snapshot, manifest, &tree, &plan, &timestamp()).map_err(SyncError::Io)?;
        manifest.save(&self.work.join(manifest::FILE_NAME)).map_err(|e| SyncError::Io(format!("saving the manifest: {e}")))?;
        if report.failed > 0 {
            return Err(SyncError::Incomplete(report));
        }
        Ok(report)
    }

    /// A change on the host since the last build or sync, described; None when the host still matches the manifest.
    pub fn host_changed(&self) -> io::Result<Option<String>> {
        let Some(manifest) = &self.manifest else { return Ok(Some("no manifest yet".into())) };
        let scan = scan::scan(&self.root)?;
        let mut seen = HashSet::new();
        for item in &scan.items {
            seen.insert(item.rel.as_str());
            let Some(entry) = manifest.entries.get(&item.rel) else {
                return Ok(Some(format!("new on the host: {}", item.rel)));
            };
            if entry.dir != item.dir {
                return Ok(Some(format!("changed on the host: {}", item.rel)));
            }
            if !item.dir && entry.host != Some(item.stat) {
                let unchanged = entry.host_hash.is_some()
                    && File::open(&item.path).and_then(hash_reader).ok().map(|h| h.0) == entry.host_hash;
                if !unchanged {
                    return Ok(Some(format!("changed on the host: {}", item.rel)));
                }
            }
        }
        for (rel, entry) in &manifest.entries {
            if entry.host.is_some() && !seen.contains(rel.as_str()) {
                return Ok(Some(format!("gone from the host: {rel}")));
            }
        }
        Ok(None)
    }

    /// A digest of the host tree's metadata (names, types, sizes, times, inodes, modes, ctimes) and the skipped
    /// names: it changes with any host change, including a chmod, but not by reading.
    pub fn host_fingerprint(&self) -> io::Result<String> {
        let scan = scan::scan(&self.root)?;
        let mut hasher = Hasher::default();
        let mut add = |rel: &str, meta: Option<fs::Metadata>| {
            let m = meta.map(|m| os::fingerprint(&m));
            hasher.update(format!("{rel}\0{m:?}\n").as_bytes());
        };
        add("", fs::symlink_metadata(&self.root).ok());
        for item in &scan.items {
            add(&item.rel, fs::symlink_metadata(&item.path).ok());
        }
        for skipped in &scan.skipped {
            add(&skipped.path, None);
        }
        Ok(hasher.hex())
    }

    /// Why a rebuild from the host would fail now (the files no longer fit the volume size), if it would.
    pub fn rebuild_problem(&self) -> Option<String> {
        let scan = match scan::scan(&self.root) {
            Ok(scan) => scan,
            Err(e) => return Some(format!("reading {}: {e}", self.root.display())),
        };
        image::volume_sectors(&self.root, &scan.items, self.spec.size).err().map(|e| format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_and_directories_holding_home_are_refused() {
        let home = Path::new("/Users/someone");
        assert!(refused_root(Path::new("/"), Some(home)).is_some());
        assert!(refused_root(home, Some(home)).is_some());
        assert!(refused_root(Path::new("/Users"), Some(home)).unwrap().contains("contains the home directory"));
        assert_eq!(refused_root(Path::new("/Users/someone/c64"), Some(home)), None);
        assert_eq!(refused_root(Path::new("/Users/someonex"), Some(home)), None, "a component, not a string prefix");
        assert_eq!(refused_root(Path::new("/Volumes/stick"), None), None);
    }
}
