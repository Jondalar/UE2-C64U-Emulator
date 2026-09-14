//! Read a volume image (a snapshot, never the live image) into the tree sync compares with the manifest.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::time::SystemTime;

use crate::check::{self, Totals};
use crate::image::{self, Partition};
use crate::Hasher;

/// A file or directory on the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageItem {
    pub dir: bool,
    pub size: u64,
    /// SHA-256 of a file.
    pub hash: Option<String>,
    pub modified: Option<SystemTime>,
}

/// Keyed by `/`-separated path. Every name is kept, also the ones the host side never imports (`.DS_Store`,
/// `.ue2-trash`, ...): sync writes those back under another name, so no guest data is dropped.
pub type ImageTree = BTreeMap<String, ImageItem>;

type Dir<'a> = fatfs::Dir<'a, Partition>;

/// Parse the image at `path`: [`check::check_fat32`] first, then a walk with fatfs that reads and hashes every
/// file and must agree with the check's counts. Any disagreement or read error is an error: such an image is not
/// synced.
pub fn read_image(path: &std::path::Path) -> Result<ImageTree, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (start, len) = image::read_partition(&file)?;
    let totals = check::check_fat32(&file, start, len)?;
    let fs = image::open_fs(path)?;
    let mut tree = ImageTree::new();
    let mut seen = Totals::default();
    walk(&fs.root_dir(), "", 0, &mut tree, &mut seen)?;
    if seen != totals {
        return Err(format!("the directory walk ({seen:?}) disagrees with the structure check ({totals:?})"));
    }
    Ok(tree)
}

fn walk(dir: &Dir, rel_dir: &str, depth: usize, tree: &mut ImageTree, seen: &mut Totals) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{rel_dir}: nested too deep"));
    }
    let mut upper_names = HashSet::new();
    for entry in dir.iter() {
        let entry = entry.map_err(|e| format!("reading directory /{rel_dir}: {e}"))?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(format!("/{rel}: a name the host cannot take"));
        }
        if name.contains('\u{FFFD}') {
            // Short names are decoded as code page 437, so this is a long name that is not valid UTF-16.
            return Err(format!("/{rel}: a damaged long file name (not valid UTF-16)"));
        }
        if !upper_names.insert(name.to_uppercase()) {
            return Err(format!("/{rel}: two names in one directory differ only in letter case"));
        }
        let modified = image::host_time(entry.modified());
        if entry.is_dir() {
            seen.dirs += 1;
            tree.insert(rel.clone(), ImageItem { dir: true, size: 0, hash: None, modified });
            walk(&entry.to_dir(), &rel, depth + 1, tree, seen)?;
            continue;
        }
        seen.files += 1;
        seen.bytes += entry.len();
        let mut file = entry.to_file();
        let mut hasher = Hasher::default();
        let mut buf = vec![0; 1 << 16];
        let mut len = 0;
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    len += n as u64;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("reading /{rel}: {e}")),
            }
        }
        if len != entry.len() {
            return Err(format!("/{rel}: read {len} of {} bytes", entry.len()));
        }
        tree.insert(rel, ImageItem { dir: false, size: len, hash: Some(hasher.hex()), modified });
    }
    Ok(())
}
