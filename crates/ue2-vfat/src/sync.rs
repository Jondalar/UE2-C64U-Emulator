//! Guest → host: apply what the guest changed on the volume since the last build or sync to the shared directory.
//!
//! The change set is the image tree against the manifest's image side, never against the host. Rules
//! (docs/status/usb-dir.md §Safety):
//! - new and modified files are written to a temporary file next to the target, flushed, and linked (new) or
//!   renamed (modified) into place, with the FAT modification time; a replaced host file's permissions and extended
//!   attributes are kept;
//! - a modified file whose host copy still has the content of the last sync (hashed, not only stat) is first
//!   hard-linked into `.ue2-trash/<timestamp>/`, so the previous version survives;
//! - if the host copy changed since the last sync (or a new guest file meets a host file of the same name), the host
//!   file stays and the guest version is written as `<name> (ue2 conflict <timestamp>).<ext>`; the path stays
//!   diverged, so later guest changes there are conflict copies too until the next build;
//! - a guest directory whose host counterpart is gone is created again; one whose name the host gave to a file or
//!   a symlink is written as a conflict directory. Symlinks are never followed on the way to a target;
//! - a deleted file or emptied directory is moved into `.ue2-trash/<timestamp>/`, never unlinked; one the host
//!   changed since the last sync is kept;
//! - a guest name the host side never imports (`.DS_Store`, `._*`, `.ue2-trash`, `.ue2-tmp-*`) is written as
//!   `ue2-renamed-<name>`;
//! - [`guard`]: a sync that would delete or overwrite more than 25 % of the files, or more than 50, is refused
//!   unless forced.
//!
//! A guest change that cannot be written anywhere (permissions, a full disk, ...) counts in [`Report::failed`]; the
//! caller must then keep the image ([`crate::volume::SyncError::Incomplete`]).
//!
//! Applying is idempotent: a host file that already has the guest content counts as written, a missing one as
//! deleted, so a sync interrupted by a crash can run again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fatread::{ImageItem, ImageTree};
use crate::image;
use crate::manifest::{Entry, HostStat, Manifest};
use crate::scan::{self, MAX_DEPTH, RENAMED_PREFIX, TEMP_PREFIX, TRASH};
use crate::{hash_reader, Hasher};

/// Guard limits: more than this share (1/4) of the files, or more than this many.
pub const GUARD_SHARE_DIVISOR: usize = 4;
pub const GUARD_MAX: usize = 50;

/// The guest's changes, each list sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub deleted_files: Vec<String>,
    pub deleted_dirs: Vec<String>,
    pub added_dirs: Vec<String>,
    pub added_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.deleted_files.is_empty()
            && self.deleted_dirs.is_empty()
            && self.added_dirs.is_empty()
            && self.added_files.is_empty()
            && self.modified_files.is_empty()
    }

    /// Host files this plan would delete or overwrite.
    pub fn destructive(&self) -> usize {
        self.deleted_files.len() + self.modified_files.len()
    }
}

/// Diff the image tree against the manifest's image side.
pub fn plan(manifest: &Manifest, tree: &ImageTree) -> Plan {
    let mut plan = Plan::default();
    for (rel, entry) in &manifest.entries {
        match tree.get(rel) {
            Some(item) if item.dir == entry.dir => {
                if !item.dir && item.hash != entry.hash {
                    plan.modified_files.push(rel.clone());
                }
            }
            other => {
                if entry.dir {
                    plan.deleted_dirs.push(rel.clone());
                } else {
                    plan.deleted_files.push(rel.clone());
                }
                if let Some(item) = other {
                    if item.dir {
                        plan.added_dirs.push(rel.clone());
                    } else {
                        plan.added_files.push(rel.clone());
                    }
                }
            }
        }
    }
    for (rel, item) in tree {
        if !manifest.entries.contains_key(rel) {
            if item.dir {
                plan.added_dirs.push(rel.clone());
            } else {
                plan.added_files.push(rel.clone());
            }
        }
    }
    for list in [&mut plan.deleted_files, &mut plan.deleted_dirs, &mut plan.added_dirs, &mut plan.added_files, &mut plan.modified_files] {
        list.sort();
    }
    plan
}

/// The mass-deletion guard: `Err((destructive, files))` when the plan deletes or overwrites more than a quarter of
/// the manifest's `files`, or more than [`GUARD_MAX`].
pub fn guard(plan: &Plan, files: usize) -> Result<(), (usize, usize)> {
    let destructive = plan.destructive();
    if destructive > GUARD_MAX || destructive * GUARD_SHARE_DIVISOR > files {
        return Err((destructive, files));
    }
    Ok(())
}

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files written to the host, including conflict copies.
    pub written: usize,
    /// Directories created on the host, including conflict directories.
    pub dirs_created: usize,
    /// Files and directories moved into the trash (deleted by the guest).
    pub trashed: usize,
    /// Guest versions written under a conflict name (files and directories).
    pub conflicts: usize,
    /// Guest deletions not written back (host changed, symlinked, diverged). No guest data is lost by these.
    pub kept: usize,
    /// Guest files and directories that could not be written anywhere on the host. Their data exists only in the
    /// image, which must be kept.
    pub failed: usize,
    /// The trash directory of this sync, if it was used.
    pub trash_dir: Option<PathBuf>,
    /// One line per action.
    pub lines: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} written, {} directories created, {} moved to the trash, {} conflict copies, {} kept",
            self.written, self.dirs_created, self.trashed, self.conflicts, self.kept
        );
        if self.failed > 0 {
            s.push_str(&format!(", {} NOT WRITTEN", self.failed));
        }
        if let Some(dir) = &self.trash_dir {
            s.push_str(&format!(" (trash: {})", dir.display()));
        }
        s
    }
}

/// The host at a manifest path compared with the manifest's host side.
#[derive(Debug, PartialEq, Eq)]
enum Host {
    Missing,
    /// Unchanged since the last build or sync.
    Same,
    /// A file with other content, or something of another type.
    Changed,
}

/// The host at `path` against the manifest `entry` (None for a path the manifest does not have). A file is Same only
/// when its content still hashes to the entry's host side: an in-place edit can keep size, mtime and inode. The
/// caller has checked that every directory above `path` is a real directory.
fn host_state(path: &Path, entry: Option<&Entry>) -> io::Result<Host> {
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Host::Missing),
        other => other?,
    };
    let Some(entry) = entry else { return Ok(Host::Changed) };
    if entry.dir {
        return Ok(if meta.is_dir() { Host::Same } else { Host::Changed });
    }
    if !meta.is_file() || entry.host_hash.is_none() {
        return Ok(Host::Changed);
    }
    Ok(if entry.host_hash == Some(hash_reader(File::open(path)?)?.0) { Host::Same } else { Host::Changed })
}

/// SHA-256 of a regular host file, None for anything else.
fn host_hash(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    meta.is_file().then(|| hash_reader(File::open(path).ok()?).ok().map(|h| h.0)).flatten()
}

fn depth(rel: &str) -> usize {
    rel.matches('/').count()
}

/// `dir/` and `name` of a `/`-separated path; `dir` is empty or ends with `/`.
fn split(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..=i], &rel[i + 1..]),
        None => ("", rel),
    }
}

/// The parent directory of a `/`-separated path, "" for the root.
fn parent(rel: &str) -> &str {
    rel.rfind('/').map_or("", |i| &rel[..i])
}

/// `name (ue2 conflict <stamp>).ext`, with ` <n>` after the stamp for n > 0.
pub fn conflict_name(name: &str, stamp: &str, n: usize) -> String {
    let tag = if n == 0 { format!("ue2 conflict {stamp}") } else { format!("ue2 conflict {stamp} {n}") };
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({tag}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({tag})"),
    }
}

/// `name (ue2 conflict <stamp>)` for a directory: the extension is not split off.
fn conflict_dir_name(name: &str, stamp: &str, n: usize) -> String {
    if n == 0 {
        format!("{name} (ue2 conflict {stamp})")
    } else {
        format!("{name} (ue2 conflict {stamp} {n})")
    }
}

/// Where the directories above a host path stand.
#[derive(Debug, PartialEq, Eq)]
enum Parents {
    /// Every one is a real directory (no symlink).
    Real,
    /// One is gone.
    Missing,
    /// One is a file, a symlink or something else.
    NotDir,
}

struct Applier<'a> {
    root: &'a Path,
    stamp: &'a str,
    /// The manifest's remap, updated here.
    remap: BTreeMap<String, String>,
    report: Report,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Applier<'_> {
    /// The host path, relative to the root, of the image path `rel`: remapped directories, and names the host side
    /// never imports renamed to `ue2-renamed-<name>`.
    fn host_rel(&self, rel: &str) -> String {
        let (mut image, mut host) = (String::new(), String::new());
        for component in rel.split('/') {
            if !image.is_empty() {
                image.push('/');
                host.push('/');
            }
            image.push_str(component);
            if let Some(mapped) = self.remap.get(&image) {
                host.clone_from(mapped);
                continue;
            }
            if scan::ignored(component) {
                host.push_str(RENAMED_PREFIX);
            }
            host.push_str(component);
        }
        host
    }

    fn host(&self, rel: &str) -> PathBuf {
        self.root.join(self.host_rel(rel))
    }

    /// `rel`, and its host path when that differs.
    fn shown(&self, rel: &str) -> String {
        let host = self.host_rel(rel);
        if host == rel {
            rel.to_string()
        } else {
            format!("{rel} (on the host: {host})")
        }
    }

    fn line(&mut self, line: String) {
        self.report.lines.push(line);
    }

    /// A guest change that could not be written: its data is only in the image.
    fn fail(&mut self, what: &str, why: impl std::fmt::Display) {
        self.report.failed += 1;
        self.line(format!("NOT WRITTEN {what}: {why}"));
    }

    /// The directories above the image path `rel` on the host, walked from the root without following symlinks.
    fn parents(&self, rel: &str) -> io::Result<Parents> {
        let dir = parent(rel);
        if dir.is_empty() {
            return Ok(Parents::Real);
        }
        let mut path = self.root.to_path_buf();
        for component in self.host_rel(dir).split('/') {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(Parents::NotDir),
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Parents::Missing),
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Ok(Parents::NotDir),
                Err(e) => return Err(e),
            }
        }
        Ok(Parents::Real)
    }

    /// Make the host directory of the image directory `rel_dir` ("" is the root) a real directory, component by
    /// component without following symlinks: a missing one is created, one whose name the host gave to a file, a
    /// symlink or anything else becomes a conflict directory next to it, recorded in the remap so later syncs
    /// write there too.
    fn ensure_dir(&mut self, rel_dir: &str) -> io::Result<()> {
        if rel_dir.is_empty() {
            return Ok(());
        }
        let mut image = String::new();
        for component in rel_dir.split('/') {
            if !image.is_empty() {
                image.push('/');
            }
            image.push_str(component);
            let host_rel = self.host_rel(&image);
            let path = self.root.join(&host_rel);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&path)?;
                    self.report.dirs_created += 1;
                    self.line(format!("created {host_rel}/"));
                }
                Ok(_) => {
                    let (dir, name) = split(&host_rel);
                    let mut made = None;
                    for n in 0..1000 {
                        let candidate = format!("{dir}{}", conflict_dir_name(name, self.stamp, n));
                        match fs::create_dir(self.root.join(&candidate)) {
                            Ok(()) => {
                                made = Some(candidate);
                                break;
                            }
                            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    let candidate = made.ok_or_else(|| io::Error::new(ErrorKind::AlreadyExists, "no free conflict name"))?;
                    self.report.dirs_created += 1;
                    self.report.conflicts += 1;
                    self.line(format!(
                        "conflict {host_rel}/: the host has a file or symlink of that name; the guest's directory is {candidate}/"
                    ));
                    self.remap.insert(image.clone(), candidate);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `.ue2-trash/<stamp>` of this sync, created on first use. A `.ue2-trash` that is not a real directory (a
    /// symlink) is refused.
    fn trash_dir(&mut self) -> io::Result<PathBuf> {
        if let Some(dir) = &self.report.trash_dir {
            return Ok(dir.clone());
        }
        let base = self.root.join(TRASH);
        match fs::symlink_metadata(&base) {
            Ok(meta) if !meta.is_dir() => {
                return Err(io::Error::new(ErrorKind::AlreadyExists, format!("{} is not a directory", base.display())))
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir(&base)?,
            Err(e) => return Err(e),
        }
        let mut dir = base.join(self.stamp);
        let mut n = 1;
        while fs::symlink_metadata(&dir).is_ok() {
            n += 1;
            dir = base.join(format!("{}-{n}", self.stamp));
        }
        fs::create_dir(&dir)?;
        self.report.trash_dir = Some(dir.clone());
        Ok(dir)
    }

    /// A free path in the trash for the image path `rel` (laid out like the host), parents created.
    fn trash_path(&mut self, rel: &str) -> io::Result<PathBuf> {
        let host_rel = self.host_rel(rel);
        let mut dst = self.trash_dir()?.join(&host_rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut n = 1;
        while fs::symlink_metadata(&dst).is_ok() {
            n += 1;
            dst = self.trash_dir()?.join(format!("{host_rel}.{n}"));
        }
        Ok(dst)
    }

    fn move_to_trash(&mut self, rel: &str) -> io::Result<PathBuf> {
        let dst = self.trash_path(rel)?;
        fs::rename(self.host(rel), &dst)?;
        Ok(dst)
    }

    /// Copy `rel` from the snapshot into a temporary file next to its host path (whose directory must exist),
    /// checked against the snapshot's hash, with the FAT modification time. `like`: the host file this copy
    /// replaces, whose permissions and extended attributes it takes. Returns the temporary path and a note when
    /// that metadata could not be copied.
    fn temp_copy(
        &mut self,
        fs_image: &fatfs::FileSystem<image::Partition>,
        rel: &str,
        item: &ImageItem,
        like: Option<&Path>,
    ) -> io::Result<(PathBuf, Option<String>)> {
        let target = self.host(rel);
        let parent = target.parent().unwrap_or(self.root);
        let tmp = parent.join(format!("{TEMP_PREFIX}{}-{}", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let result = (|| {
            let mut src = fs_image.root_dir().open_file(rel)?;
            let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            let mut hasher = Hasher::default();
            let mut buf = vec![0; 1 << 16];
            loop {
                let n = match io::Read::read(&mut src, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                out.write_all(&buf[..n])?;
                hasher.update(&buf[..n]);
            }
            if Some(hasher.hex()) != item.hash {
                return Err(io::Error::new(ErrorKind::InvalidData, "the snapshot changed while it was synced"));
            }
            let note = like.and_then(|like| copy_metadata(like, &out).err()).map(|e| format!("permissions or extended attributes not copied: {e}"));
            if let Some(modified) = item.modified {
                out.set_modified(modified)?;
            }
            out.sync_all()?;
            Ok(note)
        })();
        match result {
            Ok(note) => Ok((tmp, note)),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Link `tmp` to `dst` only if `dst` does not exist, then drop the temporary name.
    fn link_new(tmp: &Path, dst: &Path) -> io::Result<()> {
        fs::hard_link(tmp, dst)?;
        fs::remove_file(tmp)
    }

    /// Write the guest version under a free conflict name next to the host path of `rel`; returns that host path.
    fn conflict_copy(&mut self, tmp: &Path, rel: &str) -> io::Result<String> {
        let host_rel = self.host_rel(rel);
        let (dir, name) = split(&host_rel);
        for n in 0..1000 {
            let candidate = format!("{dir}{}", conflict_name(name, self.stamp, n));
            match Self::link_new(tmp, &self.root.join(&candidate)) {
                Ok(()) => return Ok(candidate),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(ErrorKind::AlreadyExists, "no free conflict name"))
    }

    fn entry_from(&self, rel: &str, item: &ImageItem) -> Entry {
        let path = self.host(rel);
        let host = fs::symlink_metadata(&path).ok().map(|m| HostStat::of(&m));
        let host_hash = if item.dir { None } else { host_hash(&path) };
        Entry { dir: item.dir, size: item.size, hash: item.hash.clone(), host, host_hash, link: false, diverged: false }
    }
}

/// Give `out` the permission bits and (on macOS) the extended attributes and ACL of the host file `like`.
fn copy_metadata(like: &Path, out: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(like)?;
    out.set_permissions(fs::Permissions::from_mode(meta.permissions().mode() & 0o777))?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let src = File::open(like)?;
        // SAFETY: fcopyfile(3) on two descriptors this function holds open, without a state object.
        let rc = unsafe {
            libc::fcopyfile(src.as_raw_fd(), out.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_XATTR | libc::COPYFILE_ACL)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The nearest directory of `rel` (itself excluded) in `failed`.
fn failed_ancestor<'s>(failed: &'s BTreeSet<String>, rel: &str) -> Option<&'s str> {
    let mut dir = parent(rel);
    while !dir.is_empty() {
        if let Some(d) = failed.get(dir) {
            return Some(d);
        }
        dir = parent(dir);
    }
    None
}

/// Apply `plan` from the snapshot image `snapshot` (the same image `tree` was read from) to `root`, updating
/// `manifest` (entries and remap) with what was done. Per-path problems are reported and leave that path alone:
/// guest deletions not written back count as kept, guest data not written as failed. Only a snapshot that cannot be
/// opened is an error.
pub fn apply(root: &Path, snapshot: &Path, manifest: &mut Manifest, tree: &ImageTree, plan: &Plan, stamp: &str) -> Result<Report, String> {
    let fs_image = image::open_fs(snapshot)?;
    let mut a = Applier { root, stamp, remap: std::mem::take(&mut manifest.remap), report: Report::default() };
    apply_plan(&mut a, &fs_image, &mut manifest.entries, tree, plan);
    manifest.remap = std::mem::take(&mut a.remap);
    Ok(a.report)
}

fn apply_plan(a: &mut Applier, fs_image: &fatfs::FileSystem<image::Partition>, entries: &mut BTreeMap<String, Entry>, tree: &ImageTree, plan: &Plan) {
    for rel in &plan.deleted_files {
        let entry = entries.remove(rel);
        let shown = a.shown(rel);
        if entry.as_ref().is_some_and(|e| e.link) {
            a.report.kept += 1;
            a.line(format!("kept {shown}: imported through a symlink, the guest's deletion is not written back"));
            continue;
        }
        match a.parents(rel) {
            Ok(Parents::Real) => {}
            Ok(Parents::Missing) => {
                a.line(format!("deleted {shown}: already gone on the host"));
                continue;
            }
            Ok(Parents::NotDir) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}: a directory above it is no longer a directory on the host; nothing deleted"));
                continue;
            }
            Err(e) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}: {e}"));
                continue;
            }
        }
        let diverged = entry.as_ref().is_some_and(|e| e.diverged);
        match host_state(&a.host(rel), entry.as_ref()) {
            Ok(Host::Missing) => a.line(format!("deleted {shown}: already gone on the host")),
            Ok(Host::Same) if !diverged => match a.move_to_trash(rel) {
                Ok(dst) => {
                    a.report.trashed += 1;
                    a.line(format!("deleted {shown}: moved to {}", dst.display()));
                }
                Err(e) => {
                    a.report.kept += 1;
                    a.line(format!("kept {shown}: moving it to the trash failed: {e}"));
                }
            },
            Ok(Host::Same) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}: deleted by the guest, but the host version was never on the stick (conflict)"));
            }
            Ok(Host::Changed) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}: deleted by the guest but changed on the host since the last sync"));
            }
            Err(e) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}: {e}"));
            }
        }
    }

    let mut deleted_dirs = plan.deleted_dirs.clone();
    deleted_dirs.sort_by_key(|rel| std::cmp::Reverse(depth(rel)));
    for rel in &deleted_dirs {
        entries.remove(rel);
        let shown = a.shown(rel);
        match a.parents(rel) {
            Ok(Parents::Real) => {}
            Ok(Parents::Missing) => {
                a.line(format!("deleted {shown}/: already gone on the host"));
                continue;
            }
            Ok(Parents::NotDir) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}/: a directory above it is no longer a directory on the host; nothing deleted"));
                continue;
            }
            Err(e) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}/: {e}"));
                continue;
            }
        }
        let path = a.host(rel);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => a.line(format!("deleted {shown}/: already gone on the host")),
            Err(e) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}/: {e}"));
            }
            Ok(meta) if meta.is_dir() && only_empty_dirs(&path) => match a.move_to_trash(rel) {
                Ok(dst) => {
                    a.report.trashed += 1;
                    a.line(format!("deleted {shown}/: moved to {}", dst.display()));
                }
                Err(e) => {
                    a.report.kept += 1;
                    a.line(format!("kept {shown}/: moving it to the trash failed: {e}"));
                }
            },
            Ok(meta) if meta.is_dir() => {
                a.report.kept += 1;
                a.line(format!("kept {shown}/: deleted by the guest but it still holds files on the host"));
            }
            Ok(_) => {
                a.report.kept += 1;
                a.line(format!("kept {shown}/: deleted by the guest, and the host has something else of that name"));
            }
        }
    }

    // Image directories that could not be made on the host; everything below them fails too.
    let mut failed_dirs: BTreeSet<String> = BTreeSet::new();
    for rel in &plan.added_dirs {
        let shown = a.shown(rel);
        if let Some(dir) = failed_ancestor(&failed_dirs, rel) {
            let why = format!("its directory {dir}/ could not be created");
            a.fail(&format!("{shown}/"), why);
            failed_dirs.insert(rel.clone());
            continue;
        }
        match a.ensure_dir(rel) {
            Ok(()) => {
                let entry = a.entry_from(rel, &tree[rel]);
                entries.insert(rel.clone(), entry);
            }
            Err(e) => {
                a.fail(&format!("{shown}/"), e);
                failed_dirs.insert(rel.clone());
            }
        }
    }

    let mut files: Vec<(&String, bool)> = plan.added_files.iter().map(|r| (r, false)).collect();
    files.extend(plan.modified_files.iter().map(|r| (r, true)));
    files.sort();
    for (rel, modified) in files {
        let item = &tree[rel];
        if let Some(dir) = failed_ancestor(&failed_dirs, rel) {
            let why = format!("its directory {dir}/ could not be created");
            let shown = a.shown(rel);
            a.fail(&shown, why);
            continue;
        }
        if let Err(e) = a.ensure_dir(parent(rel)) {
            let shown = a.shown(rel);
            a.fail(&shown, format!("creating its directory: {e}"));
            failed_dirs.insert(parent(rel).to_string());
            continue;
        }
        let shown = a.shown(rel);
        let old = entries.get(rel).cloned();
        let (link, diverged) = old.as_ref().map_or((false, false), |e| (e.link, e.diverged));
        let path = a.host(rel);
        let state = match host_state(&path, if modified { old.as_ref() } else { None }) {
            Ok(state) => state,
            Err(e) => {
                a.fail(&shown, e);
                continue;
            }
        };
        // Already applied (an earlier, interrupted sync, or the host made the same change): adopt it.
        if state != Host::Missing && item.hash.is_some() && host_hash(&path) == item.hash {
            let entry = a.entry_from(rel, item);
            entries.insert(rel.clone(), entry);
            a.line(format!("unchanged {shown}: the host already has the guest's content"));
            continue;
        }
        // Replace the host file only when it is still the version the guest saw.
        let replace = state == Host::Same && modified && !diverged && !link;
        let (tmp, note) = match a.temp_copy(fs_image, rel, item, replace.then_some(path.as_path())) {
            Ok(copy) => copy,
            Err(e) => {
                a.fail(&shown, e);
                continue;
            }
        };
        let outcome: io::Result<(String, bool)> = match state {
            Host::Missing => Applier::link_new(&tmp, &path).map(|()| (format!("wrote {shown}"), false)),
            _ if replace => (|| {
                let backup = a.trash_path(rel)?;
                fs::hard_link(&path, &backup)?;
                fs::rename(&tmp, &path)?;
                Ok((format!("wrote {shown} (previous version: {})", backup.display()), false))
            })(),
            _ => a.conflict_copy(&tmp, rel).map(|name| {
                let why = if link {
                    "imported through a symlink, which is not replaced"
                } else if diverged {
                    "the host version differs since an earlier conflict"
                } else if modified {
                    "changed on the host since the last sync"
                } else {
                    "the host has a different file of that name"
                };
                (format!("conflict {shown}: {why}; the guest version is {name}"), true)
            }),
        };
        let _ = fs::remove_file(&tmp);
        match outcome {
            Ok((line, conflict)) => {
                a.report.written += 1;
                a.line(line);
                if let Some(note) = note {
                    a.line(format!("  {shown}: {note}"));
                }
                let mut entry = a.entry_from(rel, item);
                if conflict {
                    a.report.conflicts += 1;
                    // The host keeps its version: the host side stays what the guest last saw, and later guest changes
                    // here are conflicts too until the next build.
                    if let Some(old) = &old {
                        entry.host = old.host;
                        entry.host_hash.clone_from(&old.host_hash);
                    }
                    entry.link = link;
                    entry.diverged = true;
                }
                entries.insert(rel.clone(), entry);
            }
            Err(e) => a.fail(&shown, e),
        }
    }
}

/// True when `dir` holds nothing but directories and names that are never imported (`.DS_Store`), recursively.
fn only_empty_dirs(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    entries.flatten().all(|e| {
        let name = e.file_name();
        let ignorable = name.to_str().is_some_and(scan::ignored);
        match e.file_type() {
            Ok(t) if t.is_dir() && !t.is_symlink() => only_empty_dirs(&e.path()),
            Ok(_) => ignorable,
            Err(_) => false,
        }
    })
}

/// Remove the temporary files (`.ue2-tmp-<pid>-<n>`, regular files only) a sync left in the shared directory when
/// its process was killed. The trash is skipped and symlinks are not followed. Only call it while holding the
/// directory lock, so no sync is running. Returns one line per file.
pub fn remove_stale_temps(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, depth: usize, lines: &mut Vec<String>) {
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(file_type) = e.file_type() else { continue };
            let name = e.file_name();
            let name = name.to_str();
            if depth == 0 && name == Some(TRASH) {
                continue;
            }
            if file_type.is_file() && name.is_some_and(scan::is_temp_name) {
                let path = e.path();
                match fs::remove_file(&path) {
                    Ok(()) => lines.push(format!("removed {}, a temporary file of an interrupted sync", path.display())),
                    Err(err) => lines.push(format!("could not remove {}, a temporary file of an interrupted sync: {err}", path.display())),
                }
            } else if file_type.is_dir() {
                walk(&e.path(), depth + 1, lines);
            }
        }
    }
    let mut lines = Vec::new();
    walk(root, 0, &mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names_keep_the_extension() {
        assert_eq!(conflict_name("game.d64", "20260913-101500", 0), "game (ue2 conflict 20260913-101500).d64");
        assert_eq!(conflict_name("README", "S", 2), "README (ue2 conflict S 2)");
        assert_eq!(conflict_name(".hidden", "S", 0), ".hidden (ue2 conflict S)");
        assert_eq!(conflict_name("a.b.prg", "S", 0), "a.b (ue2 conflict S).prg");
        assert_eq!(conflict_dir_name("v1.2", "S", 0), "v1.2 (ue2 conflict S)");
    }

    #[test]
    fn guard_limits() {
        let plan = |n| Plan { deleted_files: (0..n).map(|i| i.to_string()).collect(), ..Plan::default() };
        assert_eq!(guard(&plan(1), 4), Ok(()), "25 % is allowed");
        assert_eq!(guard(&plan(2), 7), Err((2, 7)), "more than 25 %");
        assert_eq!(guard(&plan(50), 1000), Ok(()));
        assert_eq!(guard(&plan(51), 1000), Err((51, 1000)), "more than 50");
        assert_eq!(guard(&Plan::default(), 0), Ok(()));
    }

    #[test]
    fn host_paths_rename_reserved_names_and_follow_the_remap() {
        let root = Path::new("/share");
        let mut a = Applier { root, stamp: "S", remap: BTreeMap::new(), report: Report::default() };
        assert_eq!(a.host_rel("games/a.prg"), "games/a.prg");
        assert_eq!(a.host_rel("._GAME.PRG"), "ue2-renamed-._GAME.PRG");
        assert_eq!(a.host_rel(".ue2-trash/x/.DS_Store"), "ue2-renamed-.ue2-trash/x/ue2-renamed-.DS_Store");
        a.remap.insert("D".into(), "D (ue2 conflict S)".into());
        a.remap.insert("D/sub".into(), "D (ue2 conflict S)/sub (ue2 conflict S)".into());
        assert_eq!(a.host_rel("D/x.prg"), "D (ue2 conflict S)/x.prg");
        assert_eq!(a.host_rel("D/sub/y"), "D (ue2 conflict S)/sub (ue2 conflict S)/y");
        assert_eq!(a.shown("D"), "D (on the host: D (ue2 conflict S))");
        assert_eq!((parent("a/b/c"), parent("a"), split("a/b/c")), ("a/b", "", ("a/b/", "c")));
    }
}
