//! The host directory tree as the volume sees it: what gets imported, and what is skipped and why.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::manifest::{HostStat, Skipped};

/// Where deleted files go, in the shared directory's root. Never imported, never synced.
pub const TRASH: &str = ".ue2-trash";
/// Prefix of the temporary files a sync writes before renaming them into place.
pub const TEMP_PREFIX: &str = ".ue2-tmp-";
/// FAT32 file size limit: 4 GiB - 1.
pub const MAX_FILE: u64 = 0xFFFF_FFFF;
/// Directory nesting that is followed.
pub const MAX_DEPTH: usize = 32;

/// Names that are neither imported nor reported: ue2's trash and temporary files, and macOS Finder metadata
/// (`.DS_Store`, AppleDouble `._*`). A guest file or directory with such a name is written back as
/// [`RENAMED_PREFIX`]`<name>`.
pub fn ignored(name: &str) -> bool {
    name == TRASH || name.starts_with(TEMP_PREFIX) || name == ".DS_Store" || name.starts_with("._")
}

/// Prefix of the host name of a guest file or directory whose own name is [`ignored`].
pub const RENAMED_PREFIX: &str = "ue2-renamed-";

/// A temporary file name a sync makes: `.ue2-tmp-<pid>-<counter>`.
pub fn is_temp_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix(TEMP_PREFIX).and_then(|rest| rest.split_once('-')).is_some_and(|(pid, n)| digits(pid) && digits(n))
}

/// Why a FAT long file name cannot be `name`, if it cannot: the character set of the fatfs crate
/// (`validate_long_name`), which is also what ChaN's FatFs in the firmware accepts, minus names FatFs would change
/// (leading spaces, trailing dots and spaces).
pub fn fat_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name.len() > 255 {
        return Some("name longer than 255 bytes");
    }
    if name.starts_with(' ') || name.ends_with(' ') || name.ends_with('.') {
        return Some("leading space or trailing dot or space");
    }
    if name.contains('\u{FFFD}') {
        // The snapshot reader refuses U+FFFD: on the volume it marks a damaged long name.
        return Some("U+FFFD (replacement character) in the name");
    }
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric()
            || ('\u{80}'..='\u{FFFF}').contains(&c)
            || "$%'-_@~`!(){}. +,;=[]^#&".contains(c);
        if !ok {
            return Some("character a FAT file name cannot hold");
        }
    }
    None
}

/// A file or directory to import.
#[derive(Clone, Debug)]
pub struct HostItem {
    /// `/`-separated path relative to the shared directory.
    pub rel: String,
    pub path: PathBuf,
    pub dir: bool,
    /// Of the symlink target for `link`.
    pub stat: HostStat,
    pub modified: SystemTime,
    /// A symlink to a file inside the shared directory.
    pub link: bool,
}

#[derive(Debug, Default)]
pub struct Scan {
    /// Parents before children, names in byte order.
    pub items: Vec<HostItem>,
    pub skipped: Vec<Skipped>,
}

/// Walk `root` (canonical). Unreadable entries below the root are skipped, not errors.
pub fn scan(root: &Path) -> io::Result<Scan> {
    let mut scan = Scan::default();
    fs::read_dir(root)?;
    walk(root, root, "", 0, &mut scan);
    Ok(scan)
}

fn walk(root: &Path, dir: &Path, rel_dir: &str, depth: usize, scan: &mut Scan) {
    let skip = |scan: &mut Scan, rel: &str, reason: String| scan.skipped.push(Skipped { path: rel.into(), reason });
    let mut names: Vec<OsString> = match fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.file_name())).collect(),
        Err(e) => return skip(scan, rel_dir, format!("cannot read the directory: {e}")),
    };
    names.sort();
    let mut upper_names = HashSet::new();
    for os_name in names {
        let path = dir.join(&os_name);
        let Some(name) = os_name.to_str() else {
            skip(scan, &format!("{rel_dir}/{}", os_name.to_string_lossy()), "name is not UTF-8".into());
            continue;
        };
        let rel = if rel_dir.is_empty() { name.to_string() } else { format!("{rel_dir}/{name}") };
        if ignored(name) {
            continue;
        }
        if let Some(problem) = fat_name_problem(name) {
            skip(scan, &rel, problem.into());
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                skip(scan, &rel, format!("cannot read metadata: {e}"));
                continue;
            }
        };
        let (meta, link) = if meta.file_type().is_symlink() {
            match fs::canonicalize(&path) {
                Err(_) => {
                    skip(scan, &rel, "broken symlink".into());
                    continue;
                }
                Ok(target) if !target.starts_with(root) => {
                    skip(scan, &rel, "symlink leaves the shared directory".into());
                    continue;
                }
                Ok(target) => match fs::metadata(&target) {
                    Ok(m) if m.is_dir() => {
                        skip(scan, &rel, "symlink to a directory".into());
                        continue;
                    }
                    Ok(m) => (m, true),
                    Err(e) => {
                        skip(scan, &rel, format!("cannot read the symlink target: {e}"));
                        continue;
                    }
                },
            }
        } else {
            (meta, false)
        };
        if !meta.is_dir() && !meta.is_file() {
            skip(scan, &rel, "special file (FIFO, socket or device)".into());
            continue;
        }
        if !upper_names.insert(name.to_uppercase()) {
            skip(scan, &rel, "another name in this directory differs only in letter case".into());
            continue;
        }
        if meta.is_file() && meta.len() > MAX_FILE {
            skip(scan, &rel, "4 GiB or larger".into());
            continue;
        }
        if meta.is_dir() {
            if depth + 1 >= MAX_DEPTH {
                skip(scan, &rel, format!("nested deeper than {MAX_DEPTH} directories"));
                continue;
            }
            if let Err(e) = fs::read_dir(&path) {
                skip(scan, &rel, format!("cannot read the directory: {e}"));
                continue;
            }
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let dir = meta.is_dir();
        scan.items.push(HostItem { rel: rel.clone(), path: path.clone(), dir, stat: HostStat::of(&meta), modified, link });
        if dir {
            walk(root, &path, &rel, depth + 1, scan);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn names_fat_cannot_hold_are_reported() {
        for good in ["hello.prg", "A Long File Name With Spaces.d64", "Grüße (1).txt", ".hidden", "a+b=c,d;[e]"] {
            assert_eq!(fat_name_problem(good), None, "{good}");
        }
        for bad in ["a:b", "q?", "star*", "pipe|", "back\\slash", "tab\t", "trailing.", "trailing ", " lead", "😀"] {
            assert!(fat_name_problem(bad).is_some(), "{bad:?}");
        }
        assert!(fat_name_problem(&"x".repeat(256)).is_some());
        assert!(ignored(".ue2-trash") && ignored(".ue2-tmp-12") && ignored(".DS_Store") && ignored("._x"));
        assert_eq!(fat_name_problem("odd\u{FFFD}name.txt"), Some("U+FFFD (replacement character) in the name"));
        assert!(is_temp_name(".ue2-tmp-47779-0") && !is_temp_name(".ue2-tmp-12") && !is_temp_name(".ue2-tmp-1-x"));
    }

    #[test]
    fn scan_imports_the_tree_and_skips_what_fat_cannot_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("games/sub")).unwrap();
        fs::write(root.join("games/sub/deep.prg"), b"x").unwrap();
        fs::write(root.join("Readme.txt"), b"a").unwrap();
        fs::write(root.join("bad:name"), b"a").unwrap();
        fs::create_dir(root.join(TRASH)).unwrap();
        fs::write(root.join(".DS_Store"), b"a").unwrap();
        symlink(root.join("Readme.txt"), root.join("link.txt")).unwrap();
        symlink("/etc/hosts", root.join("outside")).unwrap();
        symlink(root.join("games"), root.join("dirlink")).unwrap();
        symlink(root.join("missing"), root.join("broken")).unwrap();
        let fifo = root.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

        let scan = scan(&root).unwrap();
        let rels: Vec<_> = scan.items.iter().map(|i| (i.rel.as_str(), i.dir, i.link)).collect();
        assert_eq!(
            rels,
            [
                ("Readme.txt", false, false),
                ("games", true, false),
                ("games/sub", true, false),
                ("games/sub/deep.prg", false, false),
                ("link.txt", false, true),
            ]
        );
        let mut skipped: Vec<_> = scan.skipped.iter().map(|s| (s.path.as_str(), s.reason.as_str())).collect();
        skipped.sort();
        assert_eq!(
            skipped,
            [
                ("bad:name", "character a FAT file name cannot hold"),
                ("broken", "broken symlink"),
                ("dirlink", "symlink to a directory"),
                ("fifo", "special file (FIFO, socket or device)"),
                ("outside", "symlink leaves the shared directory"),
            ]
        );
    }
}
