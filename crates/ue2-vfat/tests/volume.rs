//! Build, guest edits and sync on scratch directories: the safety rules of docs/status/usb-dir.md end to end.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::fs::Permissions;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
#[cfg(unix)]
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fatfs::{FileSystem, FsOptions};
use tempfile::TempDir;
use ue2_vfat::fatread::read_image;
use ue2_vfat::image::{self, Partition};
use ue2_vfat::volume::SyncError;
#[cfg(unix)]
use ue2_vfat::worker::{Reply, Request, Worker};
use ue2_vfat::{DirSpec, DirVolume};
/// Positional reads and writes on every platform (the tests only).
trait At {
    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<()>;
    fn write_all_at(&self, buf: &[u8], at: u64) -> std::io::Result<()>;
}

impl At for File {
    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<()> {
        let mut f = self;
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(at))?;
        f.read_exact(buf)
    }

    fn write_all_at(&self, buf: &[u8], at: u64) -> std::io::Result<()> {
        let mut f = self;
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(at))?;
        f.write_all(buf)
    }
}

const D64: usize = 174_848;

struct Setup {
    _tmp: TempDir,
    host: PathBuf,
    work: PathBuf,
}

/// A shared directory with nested directories, a long file name, a PRG, a D64 and more, and a separate work root.
fn setup(extra_files: usize) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("C64 Share");
    let work = tmp.path().join("work");
    fs::create_dir_all(host.join("games/old")).unwrap();
    fs::create_dir_all(host.join("empty")).unwrap();
    fs::write(host.join("hello.prg"), b"\x01\x08\x0b\x08\x0a\x00\x99\x22HI\x22\x00\x00\x00").unwrap();
    fs::write(host.join("A Long File Name For The Emulated Stick.txt"), b"long name").unwrap();
    fs::write(host.join("readme.txt"), b"readme v1\n").unwrap();
    fs::write(host.join("games/demo.d64"), vec![0xAA; D64]).unwrap();
    fs::write(host.join("games/old/old.prg"), b"old").unwrap();
    fs::write(host.join("zero.bin"), b"").unwrap();
    for i in 0..extra_files {
        fs::write(host.join(format!("games/extra{i}.prg")), format!("extra {i}")).unwrap();
    }
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    File::options().write(true).open(host.join("readme.txt")).unwrap().set_modified(mtime).unwrap();
    Setup { _tmp: tmp, host, work }
}

fn spec(path: &Path) -> DirSpec {
    DirSpec { path: path.to_path_buf(), size: None, read_only: false }
}

/// The live image mounted read-write, as the guest would change it.
fn guest(image: &Path, edit: impl FnOnce(&fatfs::Dir<Partition>)) {
    let file = OpenOptions::new().read(true).write(true).open(image).unwrap();
    let (start, len) = image::read_partition(&file).unwrap();
    let fs = FileSystem::new(Partition::new(file, start, len), FsOptions::new()).unwrap();
    edit(&fs.root_dir());
    fs.unmount().unwrap();
}

fn write_guest_file(dir: &fatfs::Dir<Partition>, path: &str, data: &[u8]) {
    let mut f = dir.create_file(path).unwrap();
    f.truncate().unwrap();
    f.write_all(data).unwrap();
    f.flush().unwrap();
}

fn snapshot(image: &Path, dir: &Path) -> PathBuf {
    let snap = dir.join("snap.img");
    let _ = fs::remove_file(&snap);
    fs::copy(image, &snap).unwrap();
    snap
}

fn trash_files(host: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    let trash = host.join(".ue2-trash");
    if trash.exists() {
        for stamp in fs::read_dir(&trash).unwrap().flatten() {
            walk(&stamp.path(), &stamp.path(), &mut out);
        }
    }
    out.sort();
    out
}

#[test]
fn build_imports_the_tree_with_long_names_and_timestamps() {
    let s = setup(0);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let prepared = vol.prepare().unwrap();
    assert!(!prepared.resumed);
    assert!(prepared.lines[0].contains("volume C64_SHARE, 256 MiB, 6 files"), "{:?}", prepared.lines);
    assert_eq!(fs::metadata(&prepared.image).unwrap().len(), 256 << 20);

    let tree = read_image(&prepared.image).unwrap();
    let paths: Vec<_> = tree.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "A Long File Name For The Emulated Stick.txt",
            "empty",
            "games",
            "games/demo.d64",
            "games/old",
            "games/old/old.prg",
            "hello.prg",
            "readme.txt",
            "zero.bin"
        ]
    );
    assert_eq!(tree["games/demo.d64"].size, D64 as u64);
    let manifest = vol.manifest().unwrap();
    for (rel, item) in &tree {
        assert_eq!(item.hash, manifest.entries[rel].hash, "{rel}");
    }
    let modified = tree["readme.txt"].modified.unwrap();
    let want = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert!(want.duration_since(modified).unwrap() < Duration::from_secs(2), "FAT mtime within 2 s");
    assert_eq!(vol.host_changed().unwrap(), None);
}

#[test]
fn guest_changes_reach_the_host_and_deletions_go_to_the_trash() {
    let s = setup(4);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| {
        write_guest_file(root, "new disk.d64", &vec![0x55; D64]);
        write_guest_file(root, "readme.txt", b"readme v2 from the guest\n");
        root.remove("games/old/old.prg").unwrap();
        root.remove("games/old").unwrap();
        root.remove("empty").unwrap();
        root.create_dir("made by guest").unwrap();
        write_guest_file(root, "made by guest/inner.prg", b"inner");
    });
    let report = vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap();
    assert_eq!((report.written, report.dirs_created, report.trashed, report.conflicts, report.kept), (3, 1, 3, 0, 0), "{:#?}", report.lines);

    assert_eq!(fs::read(s.host.join("new disk.d64")).unwrap(), vec![0x55; D64]);
    assert_eq!(fs::read(s.host.join("readme.txt")).unwrap(), b"readme v2 from the guest\n");
    assert_eq!(fs::read(s.host.join("made by guest/inner.prg")).unwrap(), b"inner");
    assert!(!s.host.join("games/old").exists() && !s.host.join("empty").exists());
    assert_eq!(trash_files(&s.host), ["games/old/old.prg", "readme.txt"], "deleted file and previous version");
    let trash = report.trash_dir.clone().unwrap();
    assert_eq!(fs::read(trash.join("readme.txt")).unwrap(), b"readme v1\n");
    assert!(trash.join("empty").is_dir());
    let leftovers: Vec<_> = fs::read_dir(&s.host).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".ue2-tmp")).collect();
    assert!(leftovers.is_empty(), "no temporary files left");

    // The sync's own writes are not host changes, and a second sync finds nothing.
    assert_eq!(vol.host_changed().unwrap(), None);
    let again = vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap();
    assert_eq!(again.lines, ["no guest changes"]);
}

#[test]
fn host_changes_win_and_the_guest_version_becomes_a_conflict_copy() {
    // 3 destructive changes: 14 files keep them under the guard's 25 %.
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    fs::write(s.host.join("readme.txt"), b"host edit\n").unwrap();
    fs::write(s.host.join("same.txt"), b"host version").unwrap();
    fs::remove_file(s.host.join("hello.prg")).unwrap();
    fs::write(s.host.join("games/old/old.prg"), b"host kept this").unwrap();
    guest(&image, |root| {
        write_guest_file(root, "readme.txt", b"guest edit\n");
        write_guest_file(root, "same.txt", b"guest version");
        write_guest_file(root, "hello.prg", b"guest hello");
        root.remove("games/old/old.prg").unwrap();
    });
    let report = vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap();
    assert_eq!((report.conflicts, report.kept, report.trashed), (2, 1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("readme.txt")).unwrap(), b"host edit\n");
    assert_eq!(fs::read(s.host.join("same.txt")).unwrap(), b"host version");
    assert_eq!(fs::read(s.host.join("hello.prg")).unwrap(), b"guest hello", "host deleted it: nothing to keep");
    assert_eq!(fs::read(s.host.join("games/old/old.prg")).unwrap(), b"host kept this");
    let copies: Vec<String> = fs::read_dir(&s.host)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("(ue2 conflict "))
        .collect();
    assert_eq!(copies.len(), 2, "{copies:?}");
    let readme_copy = copies.iter().find(|n| n.starts_with("readme (ue2 conflict ") && n.ends_with(").txt")).unwrap();
    assert_eq!(fs::read(s.host.join(readme_copy)).unwrap(), b"guest edit\n");
    assert!(trash_files(&s.host).is_empty());
    // The conflict copies and the kept file are host changes the next rebuild brings to the guest.
    assert!(vol.host_changed().unwrap().is_some());
    assert_eq!(vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap().lines, ["no guest changes"]);
}

#[test]
fn a_one_file_stick_syncs_its_only_file_back() {
    // The guard counts deletions, not overwrites. It used to count both, so one edit on a stick with fewer than
    // four files was always more than a quarter of them and could never be written back (docs/status/usb-dir.md).
    let tmp = tempfile::tempdir().unwrap();
    let (host, work) = (tmp.path().join("One File"), tmp.path().join("work"));
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("slots.cfg"), b"v1").unwrap();
    let mut vol = DirVolume::open(&spec(&host), &work).unwrap();
    let image = vol.prepare().unwrap().image;
    assert_eq!(vol.manifest().unwrap().files(), 1);

    guest(&image, |root| write_guest_file(root, "slots.cfg", b"v2 from the guest"));
    let snap = snapshot(&image, &work);
    let report = vol.sync(&snap, false).unwrap();

    assert_eq!(report.written, 1);
    assert_eq!(report.trashed, 0);
    assert_eq!(fs::read(host.join("slots.cfg")).unwrap(), b"v2 from the guest");

    // Deleting that one file is still a deletion of 100 %, so the guard stops it.
    guest(&image, |root| root.remove("slots.cfg").unwrap());
    let snap = snapshot(&image, &work);
    assert_eq!(vol.sync(&snap, false).unwrap_err(), SyncError::Guard { destructive: 1, files: 1 });
    assert!(host.join("slots.cfg").exists(), "nothing touched");
}

#[test]
fn the_mass_deletion_guard_refuses_until_forced() {
    let s = setup(2);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    assert_eq!(vol.manifest().unwrap().files(), 8);
    guest(&image, |root| {
        for f in ["hello.prg", "readme.txt", "zero.bin"] {
            root.remove(f).unwrap();
        }
    });
    let snap = snapshot(&image, s.work.as_path());
    let err = vol.sync(&snap, false).unwrap_err();
    assert_eq!(err, SyncError::Guard { destructive: 3, files: 8 });
    assert!(err.to_string().contains("usb-sync --force"));
    assert!(s.host.join("hello.prg").exists() && !s.host.join(".ue2-trash").exists(), "nothing touched");
    let report = vol.sync(&snap, true).unwrap();
    assert_eq!(report.trashed, 3);
    assert_eq!(trash_files(&s.host), ["hello.prg", "readme.txt", "zero.bin"]);
}

#[test]
fn a_corrupt_image_is_never_synced() {
    let s = setup(0);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| root.remove("readme.txt").unwrap());
    let snap = snapshot(&image, s.work.as_path());
    // Cut the chain of games/demo.d64 (86 clusters of 2048 bytes): mark its first cluster free in FAT 1.
    let file = OpenOptions::new().read(true).write(true).open(&snap).unwrap();
    let (start, _) = image::read_partition(&file).unwrap();
    let mut boot = [0u8; 512];
    At::read_exact_at(&file, &mut boot, start).unwrap();
    let reserved = u64::from(u16::from_le_bytes([boot[14], boot[15]]));
    let fat = start + reserved * 512;
    let mut entries = vec![0u8; 4096];
    At::read_exact_at(&file, &mut entries, fat).unwrap();
    let long_chain = (3..1024).find(|&c| {
        let next = u32::from_le_bytes(entries[c * 4..c * 4 + 4].try_into().unwrap()) & 0x0FFF_FFFF;
        next == c as u32 + 1
    });
    let c = long_chain.expect("a multi-cluster chain");
    At::write_all_at(&file, &[0, 0, 0, 0], fat + c as u64 * 4).unwrap();
    drop(file);
    match vol.sync(&snap, true) {
        Err(SyncError::Parse(msg)) => assert!(msg.contains("free cluster"), "{msg}"),
        other => panic!("expected a parse error, got {other:?}"),
    }
    assert!(s.host.join("readme.txt").exists() && !s.host.join(".ue2-trash").exists());
}

#[test]
fn an_interrupted_sync_can_run_again_without_duplicates() {
    let s = setup(4);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    let manifest_path = vol.work().join("manifest.json");
    let before = fs::read(&manifest_path).unwrap();
    guest(&image, |root| {
        write_guest_file(root, "fresh.prg", b"fresh");
        write_guest_file(root, "readme.txt", b"v2");
    });
    vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap();
    // A crash before the manifest was saved: the old manifest again, then the same sync.
    drop(vol);
    fs::write(&manifest_path, before).unwrap();
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    fs::write(vol.work().join("unsynced"), b"").unwrap();
    assert!(vol.prepare().unwrap().resumed);
    let report = vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap();
    assert_eq!((report.written, report.conflicts), (0, 0), "{:#?}", report.lines);
    let names: Vec<_> = fs::read_dir(&s.host).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("conflict")), "{names:?}");
    assert_eq!(fs::read(s.host.join("readme.txt")).unwrap(), b"v2");
}

#[test]
fn locks_resume_read_only_and_refused_paths() {
    let s = setup(0);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    assert!(DirVolume::open(&spec(&s.host), &s.work).unwrap_err().to_string().contains("in use by another --usb-dir"));
    let image = vol.prepare().unwrap().image;
    vol.set_unsynced(true).unwrap();
    drop(vol);

    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let prepared = vol.prepare().unwrap();
    assert!(prepared.resumed && prepared.image == image, "{prepared:?}");
    vol.set_unsynced(false).unwrap();
    fs::write(s.host.join("added.prg"), b"added").unwrap();
    assert_eq!(vol.host_changed().unwrap().as_deref(), Some("new on the host: added.prg"));
    let rebuilt = vol.build().unwrap();
    assert_eq!(vol.host_changed().unwrap(), None);
    vol.remove_old_image(&image);
    assert!(!image.exists() && rebuilt.image.exists());
    drop(vol);

    let ro = DirSpec { read_only: true, ..spec(&s.host) };
    let mut vol = DirVolume::open(&ro, &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    assert_eq!(vol.sync(&snapshot(&image, s.work.as_path()), false).unwrap().lines, ["read-only stick: nothing to sync"]);
    assert!(DirVolume::open(&ro, &s.work.join("other")).is_ok(), "read-only sticks share the directory lock");
    drop(vol);

    assert!(DirVolume::open(&spec(Path::new("/")), &s.work).is_err());
    assert!(DirVolume::open(&spec(&s.host), &s.host.join("games/work")).unwrap_err().to_string().contains("must not be inside"));
    assert!(DirVolume::open(&spec(&s.host.join("readme.txt")), &s.work).is_err());
}

#[test]
fn a_directory_too_large_for_the_given_size_is_refused() {
    let s = setup(0);
    fs::write(s.host.join("big.bin"), vec![0u8; 70 << 20]).unwrap();
    let small = DirSpec { size: Some(64 << 20), ..spec(&s.host) };
    let mut vol = DirVolume::open(&small, &s.work).unwrap();
    let err = vol.prepare().unwrap_err().to_string();
    assert!(err.contains("too small"), "{err}");
    let mut image_files = fs::read_dir(vol.work()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".img"));
    assert!(image_files.next().is_none(), "no partial image left");
    let mut read = Vec::new();
    File::open(s.host.join("readme.txt")).unwrap().read_to_end(&mut read).unwrap();
    assert_eq!(read, b"readme v1\n");
}

/// Every file below `dir` with its content, symlinks not followed.
fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let t = e.file_type().unwrap();
            let rel = e.path().strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if t.is_dir() {
                walk(&e.path(), base, out);
            } else if t.is_file() {
                out.insert(rel, fs::read(e.path()).unwrap());
            } else {
                out.insert(rel, b"<symlink>".to_vec());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The one entry of `dir` whose name starts with `prefix`.
fn only_named(dir: &Path, prefix: &str) -> PathBuf {
    let found: Vec<PathBuf> =
        fs::read_dir(dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(prefix)).map(|e| e.path()).collect();
    assert_eq!(found.len(), 1, "{prefix}: {found:?}");
    found[0].clone()
}

/// Gives a directory its permissions back when the test ends, also on a failure, so the temporary tree can go.
#[cfg(unix)]
struct Writable(PathBuf);

#[cfg(unix)]
impl Drop for Writable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, Permissions::from_mode(0o755));
    }
}

#[test]
fn a_directory_the_host_renamed_under_unsynced_guest_writes_is_created_again() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| {
        write_guest_file(root, "games/lostdisk.d64", &vec![0x42; D64]);
        write_guest_file(root, "games/extra0.prg", b"guest edit of extra0");
    });
    fs::rename(s.host.join("games"), s.host.join("games2")).unwrap();
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.written, report.dirs_created, report.failed), (2, 1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("games/lostdisk.d64")).unwrap(), vec![0x42; D64]);
    assert_eq!(fs::read(s.host.join("games/extra0.prg")).unwrap(), b"guest edit of extra0");
    assert_eq!(fs::read(s.host.join("games2/extra0.prg")).unwrap(), b"extra 0", "the host's renamed tree is untouched");
    assert_eq!(fs::read(s.host.join("games2/demo.d64")).unwrap(), vec![0xAA; D64]);
    // The rebuild that brings the rename to the guest holds both.
    assert!(vol.host_changed().unwrap().is_some());
    let tree = read_image(&vol.build().unwrap().image).unwrap();
    assert!(tree.contains_key("games/lostdisk.d64") && tree.contains_key("games2/demo.d64"), "{:?}", tree.keys());
}

#[cfg(unix)]
#[test]
fn guest_data_the_host_cannot_take_keeps_the_image_until_a_sync_writes_it() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| {
        write_guest_file(root, "games/fdisk.d64", &vec![0x33; D64]);
        write_guest_file(root, "hello2.prg", b"written");
    });
    let games = s.host.join("games");
    let restore = Writable(games.clone());
    fs::set_permissions(&games, Permissions::from_mode(0o555)).unwrap();

    // A replug (as for a host change): the sync cannot write everything, so nothing is rebuilt and no image removed.
    let worker = Worker::spawn(vol, Arc::new(AtomicU64::new(0)), false).unwrap();
    worker.send(Request::Replug { snapshot: Some(snapshot(&image, &s.work)), force: false, discard: false });
    let Some(Reply::Replugged(r)) = worker.wait_reply() else { panic!("no replug reply") };
    drop(restore);
    match &r.sync_error {
        Some(e @ SyncError::Incomplete(report)) => {
            assert_eq!(report.failed, 1, "{:#?}", report.lines);
            assert!(report.lines.iter().any(|l| l.starts_with("NOT WRITTEN games/fdisk.d64: ")), "{:#?}", report.lines);
            assert!(e.to_string().starts_with("NOT SYNCED: 1 guest change(s)"));
        }
        other => panic!("expected an incomplete sync, got {other:?}"),
    }
    assert!(r.backend.is_none() && r.error.is_none());
    assert!(image.exists() && !image.with_file_name("image-2.img").exists(), "the image is kept, nothing rebuilt");
    assert_eq!(fs::read(s.host.join("hello2.prg")).unwrap(), b"written", "what could be written was");

    // The cause fixed on the host: the next sync writes the rest, and nothing twice.
    worker.send(Request::Sync { snapshot: snapshot(&image, &s.work), force: false });
    let Some(Reply::Synced(result)) = worker.wait_reply() else { panic!("no sync reply") };
    let report = result.unwrap();
    assert_eq!((report.written, report.failed), (1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(games.join("fdisk.d64")).unwrap(), vec![0x33; D64]);
    worker.shutdown();
}

#[test]
fn a_guest_directory_whose_name_the_host_gave_to_a_file_becomes_a_conflict_directory() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| {
        root.create_dir("D").unwrap();
        write_guest_file(root, "D/guest.prg", b"guest in D");
    });
    fs::write(s.host.join("D"), b"host file D").unwrap();
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.written, report.conflicts, report.failed), (1, 1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("D")).unwrap(), b"host file D");
    let dir = only_named(&s.host, "D (ue2 conflict ");
    assert_eq!(fs::read(dir.join("guest.prg")).unwrap(), b"guest in D");

    // Later guest writes in D, before a rebuild, go to the same conflict directory.
    guest(&image, |root| write_guest_file(root, "D/second.prg", b"second"));
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.written, report.failed), (1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(only_named(&s.host, "D (ue2 conflict ").join("second.prg")).unwrap(), b"second");
}

#[test]
fn after_a_conflict_later_guest_changes_never_replace_the_host_version() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    fs::write(s.host.join("readme.txt"), b"HOST EDIT (valuable)").unwrap();
    fs::write(s.host.join("N.txt"), b"host new").unwrap();
    guest(&image, |root| {
        write_guest_file(root, "readme.txt", b"guest edit 1");
        write_guest_file(root, "N.txt", b"guest new");
    });
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!(report.conflicts, 2, "{:#?}", report.lines);

    // The guest writes both again before the replug has shown it the host versions.
    guest(&image, |root| {
        write_guest_file(root, "readme.txt", b"guest edit 2 (longer)");
        write_guest_file(root, "N.txt", b"guest new, second write");
    });
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.conflicts, report.trashed, report.failed), (2, 0, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("readme.txt")).unwrap(), b"HOST EDIT (valuable)");
    assert_eq!(fs::read(s.host.join("N.txt")).unwrap(), b"host new");

    // And deletes them.
    guest(&image, |root| {
        root.remove("readme.txt").unwrap();
        root.remove("N.txt").unwrap();
    });
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.kept, report.trashed), (2, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("readme.txt")).unwrap(), b"HOST EDIT (valuable)");
    assert_eq!(fs::read(s.host.join("N.txt")).unwrap(), b"host new");
    assert!(trash_files(&s.host).is_empty());
    let mut copies: Vec<Vec<u8>> =
        contents(&s.host).into_iter().filter(|(rel, _)| rel.contains("(ue2 conflict ")).map(|(_, data)| data).collect();
    copies.sort();
    let want: [&[u8]; 4] = [b"guest edit 1", b"guest edit 2 (longer)", b"guest new", b"guest new, second write"];
    assert_eq!(copies, want, "every guest version is kept as a conflict copy");
}

#[test]
fn a_host_name_with_u_fffd_is_skipped_and_short_names_decode_as_code_page_437() {
    let s = setup(8);
    fs::write(s.host.join("odd\u{FFFD}name.txt"), b"odd").unwrap();
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let prepared = vol.prepare().unwrap();
    assert!(prepared.lines.iter().any(|l| l == "  skipped odd\u{FFFD}name.txt: U+FFFD (replacement character) in the name"), "{:?}", prepared.lines);
    let image = prepared.image;
    guest(&image, |root| write_guest_file(root, "GRXN.PRG", b"gruen"));

    // The firmware's FatFs stores an upper-case 8.3 name as a short name only, in code page 437. Patch the short
    // entry to GR\x9aN (the stale long name then fails its checksum and is ignored, as for such an entry).
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let (start, _) = image::read_partition(&file).unwrap();
    let mut boot = [0u8; 512];
    file.read_exact_at(&mut boot, start).unwrap();
    let cluster = u64::from(boot[13]) * 512;
    let reserved = u64::from(u16::from_le_bytes([boot[14], boot[15]]));
    let fat_sectors = u64::from(u32::from_le_bytes(boot[36..40].try_into().unwrap()));
    let root_cluster = u64::from(u32::from_le_bytes(boot[44..48].try_into().unwrap()));
    let root_at = start + (reserved + u64::from(boot[16]) * fat_sectors) * 512 + (root_cluster - 2) * cluster;
    let mut dir = vec![0u8; cluster as usize];
    file.read_exact_at(&mut dir, root_at).unwrap();
    let at = dir.chunks_exact(32).position(|e| &e[..11] == b"GRXN    PRG").expect("the short entry") * 32;
    file.write_all_at(&[0x9A], root_at + at as u64 + 2).unwrap();
    drop(file);

    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.written, report.failed), (1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("GR\u{DC}N.PRG")).unwrap(), b"gruen");
    assert_eq!(fs::read(s.host.join("odd\u{FFFD}name.txt")).unwrap(), b"odd");
}

#[cfg(unix)]
#[test]
fn a_directory_replaced_by_a_symlink_to_outside_the_share_is_never_followed() {
    let s = setup(8);
    let outside = s._tmp.path().join("OUTSIDE");
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    fs::rename(s.host.join("games"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, s.host.join("games")).unwrap();
    let before = contents(&outside);
    guest(&image, |root| {
        write_guest_file(root, "games/extra0.prg", b"guest version of extra0");
        write_guest_file(root, "games/new.prg", b"new");
        root.remove("games/extra1.prg").unwrap();
    });
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.failed, report.kept, report.trashed), (0, 1, 0), "{:#?}", report.lines);
    assert_eq!(contents(&outside), before, "nothing outside the shared directory changed");
    let dir = only_named(&s.host, "games (ue2 conflict ");
    assert_eq!(fs::read(dir.join("extra0.prg")).unwrap(), b"guest version of extra0");
    assert_eq!(fs::read(dir.join("new.prg")).unwrap(), b"new");
    assert!(fs::symlink_metadata(s.host.join("games")).unwrap().file_type().is_symlink());
}

#[test]
fn temporary_files_of_an_interrupted_sync_are_removed_at_start() {
    let s = setup(0);
    fs::write(s.host.join("games/.ue2-tmp-47779-0"), vec![0u8; 4096]).unwrap();
    fs::write(s.host.join(".ue2-tmp-notes"), b"not a ue2 temporary name").unwrap();
    fs::create_dir_all(s.host.join(".ue2-trash/20260101-000000")).unwrap();
    fs::write(s.host.join(".ue2-trash/20260101-000000/.ue2-tmp-1-1"), b"in the trash").unwrap();
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let prepared = vol.prepare().unwrap();
    assert!(prepared.lines[0].starts_with("removed ") && prepared.lines[0].contains("games/.ue2-tmp-47779-0"), "{:?}", prepared.lines);
    assert!(!s.host.join("games/.ue2-tmp-47779-0").exists());
    assert!(s.host.join(".ue2-tmp-notes").exists());
    assert!(s.host.join(".ue2-trash/20260101-000000/.ue2-tmp-1-1").exists());
}

#[cfg(unix)]
#[test]
fn an_in_place_host_edit_that_keeps_size_mtime_and_inode_is_a_conflict() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    let path = s.host.join("readme.txt");
    let meta = fs::metadata(&path).unwrap();
    {
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"README v1!").unwrap();
        f.set_modified(meta.modified().unwrap()).unwrap();
    }
    let after = fs::metadata(&path).unwrap();
    assert_eq!((after.len(), after.modified().unwrap(), after.ino()), (meta.len(), meta.modified().unwrap(), meta.ino()));
    guest(&image, |root| write_guest_file(root, "readme.txt", b"guest version"));
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.conflicts, report.trashed), (1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(&path).unwrap(), b"README v1!");
}

#[test]
fn guest_files_with_names_the_host_side_never_imports_are_written_renamed() {
    let s = setup(8);
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| {
        write_guest_file(root, "._GAME.PRG", b"appledouble");
        write_guest_file(root, ".DS_Store", b"finder");
        root.create_dir(".ue2-trash").unwrap();
        write_guest_file(root, ".ue2-trash/x.prg", b"guest trash");
        write_guest_file(root, "games/.ue2-tmp-5-5", b"guest tmp");
    });
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert_eq!((report.written, report.dirs_created, report.failed), (4, 1, 0), "{:#?}", report.lines);
    assert_eq!(fs::read(s.host.join("ue2-renamed-._GAME.PRG")).unwrap(), b"appledouble");
    assert_eq!(fs::read(s.host.join("ue2-renamed-.DS_Store")).unwrap(), b"finder");
    assert_eq!(fs::read(s.host.join("ue2-renamed-.ue2-trash/x.prg")).unwrap(), b"guest trash");
    assert_eq!(fs::read(s.host.join("games/ue2-renamed-.ue2-tmp-5-5")).unwrap(), b"guest tmp");
    assert!(!s.host.join(".ue2-trash").exists() && !s.host.join(".DS_Store").exists() && !s.host.join("._GAME.PRG").exists());
    // The rebuild shows them to the guest under their new names.
    assert!(vol.host_changed().unwrap().is_some());
    let tree = read_image(&vol.build().unwrap().image).unwrap();
    assert!(tree.contains_key("ue2-renamed-.ue2-trash/x.prg") && tree.contains_key("games/ue2-renamed-.ue2-tmp-5-5"));
}

#[test]
fn nested_and_duplicate_shares_are_refused() {
    let s = setup(0);
    let inner = s.host.join("games");
    let other_work = s._tmp.path().join("work2");
    {
        let _outer = DirVolume::open(&spec(&s.host), &s.work).unwrap();
        let err = DirVolume::open(&spec(&inner), &other_work).unwrap_err().to_string();
        assert!(err.contains("is inside") && err.contains("read-write --usb-dir"), "{err}");
    }
    {
        let _inner = DirVolume::open(&spec(&inner), &s.work).unwrap();
        let err = DirVolume::open(&spec(&s.host), &other_work).unwrap_err().to_string();
        assert!(err.contains("in use by another --usb-dir"), "{err}");
    }
    let ro = DirSpec { read_only: true, ..spec(&s.host) };
    let _first = DirVolume::open(&ro, &s.work).unwrap();
    let err = DirVolume::open(&ro, &s.work).unwrap_err().to_string();
    assert!(err.contains("the work directory") && err.contains("in use"), "{err}");
    assert!(DirVolume::open(&spec(&s.host), &other_work).is_err(), "read-write over a read-only share");
}

#[cfg(target_os = "macos")]
fn xattr(path: &Path, name: &str, value: Option<&[u8]>) -> Option<Vec<u8>> {
    use std::ffi::CString;
    let p = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let n = CString::new(name).unwrap();
    if let Some(value) = value {
        // SAFETY: setxattr(2) with valid C strings and a buffer of the given length.
        let rc = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        assert_eq!(rc, 0, "setxattr: {}", std::io::Error::last_os_error());
        return None;
    }
    let mut buf = vec![0u8; 256];
    // SAFETY: getxattr(2) into a buffer of the given length.
    let len = unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
    (len >= 0).then(|| buf[..len as usize].to_vec())
}

#[cfg(unix)]
#[test]
fn a_replaced_host_file_keeps_its_permissions_and_extended_attributes() {
    let s = setup(8);
    let path = s.host.join("hello.prg");
    fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
    #[cfg(target_os = "macos")]
    xattr(&path, "com.example.tag", Some(b"blue"));
    let mut vol = DirVolume::open(&spec(&s.host), &s.work).unwrap();
    let image = vol.prepare().unwrap().image;
    guest(&image, |root| write_guest_file(root, "hello.prg", b"guest hello"));
    let report = vol.sync(&snapshot(&image, &s.work), false).unwrap();
    assert!(report.lines.iter().any(|l| l.starts_with("wrote hello.prg (previous version: ")), "{:#?}", report.lines);
    assert!(!report.lines.iter().any(|l| l.contains("not copied")), "{:#?}", report.lines);
    assert_eq!(fs::read(&path).unwrap(), b"guest hello");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
    #[cfg(target_os = "macos")]
    assert_eq!(xattr(&path, "com.example.tag", None).as_deref(), Some(&b"blue"[..]));
}
