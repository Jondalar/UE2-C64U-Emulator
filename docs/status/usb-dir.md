# `--usb-dir`: a host directory as a USB stick

Follow-up of S13 (`docs/specs/S11-S14-later.md`, `docs/status/usb.md`). **Done.** The unmodified firmware (`ultimate.elf`
V1.01 3.15) lists a host directory as `USB0`, writes to it (a new D64, a deletion), and the changes reach the host
directory. A file added on the host reaches the firmware through an automatic unplug and replug.

The host directory holds the user's files, so data safety comes before features: nothing on the host is ever deleted
or overwritten without a copy, an image that might be half written is never synced, and guest data that could not be
written to the host keeps its image until a later sync writes it.

## Usage

```sh
target/release/ue2emu run --flash run/flash.bin --usb-dir /path/to/share                 # read-write, default size
target/release/ue2emu run --flash run/flash.bin --usb-dir /path/to/share,size=2G,ro \
    --usb-dir-work run/usb-dir                                                             # write-protected stick
```

- **`--usb-dir PATH[,size=SIZE][,ro]`**, repeatable.
  - Ports: `--usb` images take hub ports 1.. first, then the `--usb-dir` sticks in order, then `--usb-keyboard`; at
    most 3 devices. Port n is `USB<n-1>` in the file browser.
  - `size=`: volume size, `64M` to `2T` (binary units). Default: max(256 MiB, 2 × file content + 64 MiB). A
    directory that does not fit is refused at start.
  - `ro`: the stick is write protected and nothing is ever written back. The firmware marks the stick `FAILED` after
    a write attempt, as it does for a read-only `--usb` image. Host changes still reach the guest.
  - Options are read from the end, so a path may contain commas.
- **`--usb-dir-work DIR`** (default `run/usb-dir`): images, manifests and snapshots, one subdirectory
  `<name>-<hash>` per shared directory. It must not be inside the shared directory.
- **Refused** (`flock`s; nothing is written into the shared directory for them):
  - `/`, the home directory or a directory that contains it, a path that is not a directory;
  - a directory another `--usb-dir` of this or another ue2emu shares (exclusive lock for read-write, shared for `ro`);
  - a read-write stick inside or around another read-write one (a read-write stick also takes a shared lock on every
    ancestor);
  - a second stick on the same work directory (two `ro` sticks of one directory need different `--usb-dir-work`).
- **Control commands** (script or TCP):
  - `usb-sync [--force] [port]`: write the guest's changes back now, for one stick or all.
  - `usb-replug [--discard] [port]`: unplug, sync, rebuild from the host, plug in. `--discard` skips the sync and
    keeps the old image as `discarded-<timestamp>.img` in the work directory. On a port without `--usb-dir` it only
    unplugs and plugs in the device.
  - Both print one line per action and fail on a refused, failed or incomplete sync.
  - Both first wait until the guest has not written for 2 s emulated, so a file the firmware is writing is not taken
    half written. After 10 s emulated they go ahead with a WARNING line.
- **MCP:** `emu_start {usb_dirs: ["PATH[,size=…][,ro]"]}` and `emu_usb_sync {id, port?, force?, replug?, discard?}`
  (`docs/status/mcp.md`).

## How it works

### At start (emulation thread, before the machine runs)

1. `DirVolume::open` checks the path, takes the lock and creates the work subdirectory.
2. `DirVolume::prepare` either resumes or builds:
   - **Resume:** if the previous run left the `unsynced` marker, its image is attached again and synced like any image
     with guest writes.
   - **Build:** otherwise the old (synced) images are removed and `image::build` makes a new one:
     - a sparse `image-<n>.img` with an MBR and one FAT32 partition (type 0x0C at sector 2048, like
       `scripts/make-sd-image.sh`);
     - formatted with the fatfs crate 0.3.6: label from the directory name, cluster size with at least 70 000 clusters;
     - the directory tree copied in with long file names and file modification and creation times;
     - `manifest.json` records every imported path (size, SHA-256, host size, mtime and inode) and every skipped one
       with its reason.
3. `Machine::usb_attach_storage` puts a `CountingBackend` on the port; it counts guest writes. A `Worker` thread owns
   the volume and an FSEvents watcher (`notify` 8.2) on the directory.

### While running

- **Emulation thread, per run slice** (`crates/ue2emu/src/usbdir.rs`):
  - Tracks the write count and the emulated and wall time of the last change.
  - Copies the image to `snapshot.img` between slices, so no block write is half done. `std::fs::copy` clones on
    APFS and costs nothing.
  - Swaps media and answers control requests.
- **Worker thread:** parses, syncs and rebuilds.

| Trigger | What happens |
|---|---|
| The guest wrote, then stayed quiet for 2 s emulated **and** 2 s wall clock | Snapshot, sync |
| `usb-sync` | Snapshot, sync (`--force` overrides the guard) |
| Host change: watcher event, 500 ms quiet, host differs from the manifest | Once the guest is quiet: unplug, snapshot, sync, rebuild, plug in (see "No loops") |
| `usb-replug` | Unplug, snapshot, sync, rebuild, plug in |
| `quit` (clean stop, also window close and `emu_stop`) | While a guest write is less than 2 s emulated old, the machine keeps running (at most 10 s emulated); then the last sync of every stick with guest writes |
| Firmware halt, SIGKILL, Ctrl-C | Nothing is synced. The image and the `unsynced` marker stay; the next run resumes it |

### Guest → host sync (`crates/ue2-vfat/src/sync.rs`)

1. **Parse.** `fatread::read_image` parses the snapshot:
   - `check::check_fat32` reads the on-disk structures: FAT32 with 512-byte sectors, every chain inside the volume,
     no free or bad cluster in a chain, no cluster in two chains, every file chain exactly as long as its size needs;
   - fatfs then walks the tree, reads and hashes every file, and must agree with the check's counts.

   Any error: nothing is synced, the image is kept, the error is logged. An automatic retry waits for new guest
   writes. Short names are decoded as code page 437, like the firmware's FatFs writes them; a long name that is not
   valid UTF-16 is a parse error.
2. **Plan.** `sync::plan` diffs the image tree against the manifest's **image** side, not against the host: new,
   modified (SHA-256 differs), deleted.
3. **Guard.** `sync::guard` refuses a plan that **deletes** more than 25 % of the manifest's files, or more than 50.
   Nothing is synced, the image is kept, and automatic syncs and replugs stop until an explicit `usb-sync`
   (`--force`) succeeds. Overwrites do not count: writing back a file the guest changed is the purpose of the sync,
   and counting it meant a stick with fewer than four files could never sync a single edit.
4. **Apply.** `sync::apply` writes each change to the host with the rules below, then saves the manifest (image side
   from the snapshot, host side from the host after writing).
5. **Incomplete.** A guest file or directory that could not be written anywhere (permissions, a full disk, a name the
   host refuses) is a `NOT WRITTEN` line. The sync is then `NOT SYNCED` (`SyncError::Incomplete`):
   - what was written is saved in the manifest, so nothing is written twice;
   - the image and the `unsynced` marker stay, nothing is rebuilt, and quit reports "guest changes are NOT synced";
   - the next run resumes the image; a sync after the cause is fixed writes the rest.

### Host → guest

- The worker compares the host tree with the manifest's **host** side:
  - a path the manifest does not have, or one it has that is gone;
  - a file whose size, mtime or inode differs and whose SHA-256 differs too.
- A real change is announced once. The emulation thread waits until the guest is quiet, unplugs the stick, syncs it,
  builds a new image and swaps it in while the stick is out, then plugs in.
- If that sync is refused or fails, nothing is rebuilt and the old image is plugged back in.

**No loops:**
- The sync's own writes are recorded in the manifest's host side, so the following watcher events find no difference.
- Rebuilds write only into the work directory, which the watcher does not see.
- The watcher also drops events on `.ue2-trash`, `.ue2-tmp-*` and Finder metadata.
- A replug that did not rebuild (sync refused, failed or incomplete; rebuild failed) runs once. The worker then holds
  a fingerprint of the host tree (names, sizes, times, inodes, modes, ctimes) and announces nothing until the host
  differs from it, a sync succeeds, or `usb-replug` runs.
- A host change that no longer fits the volume size is reported once ("the host changes cannot reach the guest"),
  without a replug, and held the same way.
- While the image at its current write count does not parse, no automatic replug starts at all; ue2emu reports once
  that host changes wait, and `usb-replug --discard` keeps the image aside and rebuilds from the host.

### Hot-plug in the USB model (`crates/ue2-core/src/devices/usb/`)

- **Unplug:** each hub port has a plug state. Unplugging clears the port's connection and enable bits and sets
  C_PORT_CONNECTION. The status endpoint reports the change, and `UsbHubDriver::handle_irqdata` removes the child and
  clears the change (usb_hub.cc:299-316). Transactions to an unplugged device get no answer (ERROR).
- **Plug-in:** it waits until the driver has cleared C_PORT_CONNECTION (at most 5 s), then 500 ms more. A connect seen
  while the old child still exists would end in "Device already present!" (usb_hub.cc:355-379). The driver then
  resets the port and enumerates the device as before.
- **Host side:** `HostInput::UsbPlug { port, connected }`, `Machine::usb_attach_storage`, `Machine::usb_replace_backend`
  and `Machine::usb_port`.
- **Medium:** the storage device serves any `BlockBackend` (`block.rs`). `ImageFile` is the raw image of `--usb`.

## Safety rules

- **New files:** a temporary `.ue2-tmp-*` next to the target is written, checked against the snapshot's SHA-256,
  given the FAT modification time, `fsync`ed and hard-linked into place. The link fails rather than replace a file that
  appeared meanwhile.
- **Modified files:** if the host copy still has the content of the last sync (its SHA-256 is compared, not only size,
  mtime and inode), the old version is first hard-linked into `.ue2-trash/<timestamp>/`, then the temporary file is
  renamed over it. The temporary file first gets the old file's permission bits, extended attributes and ACL.
- **Conflicts:** if the host copy changed since the last sync, or a new guest file meets a different host file of that
  name, the host file stays and the guest version is written as `<name> (ue2 conflict <timestamp>).<ext>`. A guest
  change to a file the host deleted is written to the original name.
  - The path then stays diverged until the next build: a later guest write there is another conflict copy, and a
    guest deletion is not written back. The host version is never replaced by a guest version the guest wrote without
    having seen it.
- **Directories:** every directory above a path is checked component by component without following symlinks.
  - One the host removed or renamed is created again, so guest files written into it land at their original path.
  - One whose name the host gave to a file or a symlink becomes `<name> (ue2 conflict <timestamp>)/`; the manifest's
    `remap` sends later syncs there too until the next build.
  - A guest deletion below a directory that is no longer a real directory is not written back.
- **Deletions:** a deleted file, or a deleted directory that holds nothing but empty directories on the host, is moved
  into `.ue2-trash/<timestamp>/`, never unlinked. A file the host changed since the last sync is kept, and so is a
  directory with files.
- **Trash:** `.ue2-trash` is never imported, never synced, and invisible to the guest. Emptying it is up to the user.
- **Symlinks:** never followed on the way to a target. A file imported through a symlink inside the directory is never
  replaced, moved or deleted from the guest side; a guest change to it is written as a conflict copy next to it.
- **Reserved names:** a guest file or directory named `.DS_Store`, `._*`, `.ue2-trash` or `.ue2-tmp-*` is written as
  `ue2-renamed-<name>` (the host side never imports those names, so the guest's data would be lost otherwise). The
  rebuild shows it to the guest under the new name.
- **Guard:** more than 25 % of the files deleted, or more than 50 → refused until forced. Overwrites are not counted.
  It applies to automatic syncs, `usb-replug` and the sync at quit; only `usb-sync --force` overrides it.
- **Parse check:** an image that fails it is never synced.
- **Idempotence:** a file that already has the guest content counts as written, a missing one as deleted. A sync
  interrupted by a crash (manifest not saved) can run again without duplicates. The `unsynced` marker makes the next
  run resume and repeat it.
- **Unsynced marker:** the emulation thread creates it right after the run slice with the first guest write since the
  last complete sync, and removes it only after a sync that wrote everything.
- **Temporary files:** `.ue2-tmp-<pid>-<n>` files a killed sync left behind are removed when a read-write stick starts
  (only regular files with exactly that name pattern, outside `.ue2-trash`).
- **Kept images:** images with unsynced writes that cannot be resumed (read-only now, manifest lost) are kept as
  `unsynced-<timestamp>-image-<n>.img`, never deleted.

## Limits

- **Import:**
  - Skipped with a line in the manifest and on stderr: files over 4 GiB, symlinks that leave the directory or point
    to a directory, broken symlinks, FIFOs, sockets and devices.
  - Also skipped: names FAT cannot hold (`\ / : * ? " < > |`, control characters, characters outside the BMP,
    leading spaces, trailing dots or spaces, more than 255 bytes), names with U+FFFD (the reader takes it for a
    damaged name), names that differ only in letter case from a sibling, and directories nested deeper than 32.
  - Not imported and never reported: `.DS_Store`, AppleDouble `._*`, `.ue2-trash`, `.ue2-tmp-*`.
- **Timestamps:**
  - File times only (fatfs has no directory timestamps), 2 s resolution.
  - UTC: the emulated RTC counts host UTC seconds and the firmware applies no time zone (rtc_dummy.cc), so the
    firmware's own files carry UTC, and imported files use the same base.
- **Names in the firmware:** the firmware's FatFs converts long names to code page 437, so non-ASCII names may show
  as `?` and may not survive a rename in the firmware.
- **A rename in the guest** syncs as a deletion (to the trash) plus a new file.
- **Only FAT32** with 512-byte sectors is synced. A guest reformat deletes everything, which the guard refuses.
- **Import time:** the volume is built before the machine starts. A large directory takes as long as copying it; the
  image is sparse.
- **Host edits during a sync:** a host change between the conflict check and the rename of one file, or a directory
  swapped for a symlink between the directory check and the write, is not detected (a window of microseconds per
  file).
- **Host change detection** (host to guest) compares size, mtime and inode, and hashes only when they differ. A host
  edit that keeps all three (for example `rsync --inplace -t`) does not trigger a replug; it reaches the guest at the
  next rebuild. A guest change to such a file still becomes a conflict copy, because the sync compares hashes.
- **A replug hold** ends with the next change of the host fingerprint. A host change made during a failed replug, before
  its fingerprint is taken, waits for the next host change, guest sync or `usb-replug`.
- **Replug latency:** FSEvents, 500 ms debounce, guest quiet time (2 s emulated and wall), rebuild, 500 ms replug gap,
  then enumeration. The whole smoke run, with its automatic replug, takes about 5 s wall.
- **The watcher** cannot watch a directory FSEvents does not cover (some network mounts). ue2emu then logs it, and host
  changes need `usb-replug`.
- **`ue2-mcp`:** `emu_stop` waits up to 120 s (default) for an instance with `usb_dirs`. A server killed without
  cleanup gives the emulator about 3 s before SIGTERM; an interrupted last sync is repeated at the next start.

## Verification

- **Driver:** `scripts/smoke-usb-dir.sh` drives `scripts/smoke-usb-dir.ctl` over TCP and runs its `#!` lines on the
  host. Everything is in a new scratch directory under `run/`, removed after a pass. The run:

  1. Prepares the share: `hello.prg`, `readme.txt`, a long file name, `games/demo.d64`, `games/sub/deep.prg`,
     `notes1..6.txt`.
  2. Checks the root and `/USB0/` listings: the long name `A Long File Name For The Emul`, `games` as DIR,
     `/USB0/games/` with `demo.d64` and `sub`, no `.ue2-trash`.
  3. The firmware writes: F5 → Create → D64 Image → `newdisk` (`newdisk.d64 D64 171K`); `readme` quick seek, RETURN
     → Delete → `y`.
  4. Runs `usb-sync 1`:

     ```
     port 1: deleted readme.txt: moved to …/share/.ue2-trash/20260913-195753/readme.txt
     port 1: wrote newdisk.d64
     port 1: 1 written, 0 directories created, 1 moved to the trash, 0 conflict copies, 0 kept (trash: …)
     ```

     Host checks: `newdisk.d64` has 174848 bytes, `readme.txt` is gone, and the trash copy equals the original.
  5. Host: writes `added.prg`. ue2emu logs `new on the host: added.prg` and "bringing host changes to the guest". The
     console shows `-> Disconnect done (#1:1)`, `DeInstalling SCSI Lun 0`, `Installing USB0`. After `usb` quick seek
     and RIGHT, `/USB0/` lists `added.prg PRG 4` and `newdisk.d64`, without `readme.txt`.
  6. The firmware creates `lastdisk.d64`, then `quit`. Stderr shows "syncing guest changes before exit", then "wrote
     lastdisk.d64".
  7. Host checks: `lastdisk.d64` has 174848 bytes, no `unsynced` marker, no `.ue2-tmp-*` left.

  8. The last D64 is created with `quit` sent right after RETURN, without waiting for it: stderr shows "waiting for
     the guest to finish writing", and the host file has all 174848 bytes.

  Result: `PASS smoke-usb-dir (24.820 s emulated, 5 s wall)`.
- **Unit and integration tests:** `cargo test -p ue2-vfat`, on temporary directories.
  - Scan rules.
  - Build and read-back with long names and timestamps.
  - Guest changes to the host with trash and previous versions.
  - Conflicts (host edit, same new name, host deletion, host-kept deletion).
  - Guard refusal and `force`.
  - A cut FAT chain refused.
  - An interrupted sync repeated without duplicates.
  - Lock, resume, read-only, refused paths, too-small size.
  - Review findings (USBDIR-FIX), one test each: a directory the host renamed under unsynced guest writes; guest data
    in a directory the host made read-only (a replug keeps the image, the next sync writes the rest); a guest directory
    whose name is a host file (conflict directory, later writes follow); later guest writes and deletions after a
    conflict; U+FFFD host names and code page 437 short names; a directory replaced by a symlink to outside; stale
    temporary files; an in-place host edit that keeps size, mtime and inode; reserved guest names; nested and duplicate
    shares; permissions and extended attributes of a replaced file.
  - Unit: the worker's replug hold (`worker.rs`), the host path mapping (`sync.rs`), code page 437, refused roots.
- **USB model tests:**
  - `hub.rs`: unplug and plug reports, no reset of an unplugged port.
  - `mod.rs`: a replug waits for the driver's C_PORT_CONNECTION clear or the 5 s timeout; stable port numbers;
    attach and swap.
  - `block.rs`: `ImageFile`.
- **Control tests:** parsing and execution of `usb-sync` / `usb-replug`.
- **Other real runs (scratch directories):**
  - `usb-replug 1` on a `--usb` image: Disconnect done, `port 1: plugged back in`, Installing USB0.
  - `,ro`: D64 creation fails in the firmware (`USB0 … FAILED`), the share is unchanged, and `usb-sync 1` prints
    `read-only stick: nothing to sync`.
- **Review reproductions** with the real firmware (scratch directories, driver with SIGKILL and restart):
  - SIGKILL after the firmware created `games/lostdisk.d64`, host `mv games games2`, restart: the resumed sync prints
    `created games/`, `wrote games/lostdisk.d64`; after the replug `games/lostdisk.d64` (174848 bytes) and `games2/`
    are both on the host.
  - `chmod 555 games`, the firmware creates `games/fdisk.d64`: `NOT WRITTEN games/fdisk.d64: Permission denied`,
    `NOT SYNCED`; a host change gives at most one replug in 30 s; quit reports NOT synced, the marker and `image-1.img`
    stay over two restarts; after `chmod 755` the resumed sync writes it and removes the marker.
  - Corrupt FAT (size field bumped offline) plus a host change: 0 `Disconnect done` in 30 s emulated plus 20 s wall,
    one "host changes wait" line; `usb-replug --discard 1` keeps `discarded-*.img` and rebuilds.
  - `size=64M` plus a 100 MiB host file: 0 `Disconnect done`, one "cannot reach the guest" line per host change; an
    explicit `usb-replug` fails once with one disconnect.
  - `quit` right after RETURN on a new D64: the quit waits, the host file is complete. A stale `.ue2-tmp-47779-0` is
    removed at start.
  - `--usb-dir share --usb-dir share/games`, the reverse order, and `share,ro` twice: refused at start.
- **Regressions:** `scripts/smoke-all.sh` and the S13 smoke (`docs/status/usb.md`) pass unchanged.

## Code map

| File | Content |
|---|---|
| `crates/ue2-core/src/devices/usb/block.rs` | `BlockBackend` trait (no host file system), `ImageFile` |
| `crates/ue2-core/src/devices/usb/{hub,mod}.rs` | Port plug state, deferred plug-in, `attach_storage`, `replace_backend`, `port_info` |
| `crates/ue2-core/src/{host,machine}.rs` | `HostInput::UsbPlug`, `Machine::usb_attach_storage` / `usb_replace_backend` / `usb_port` |
| `crates/ue2-vfat/src/spec.rs` | `DirSpec` parsing, size policy |
| `crates/ue2-vfat/src/scan.rs` | Host tree walk and skip rules |
| `crates/ue2-vfat/src/image.rs` | MBR, partition stream, cluster size, label, FAT32 build, UTC timestamps |
| `crates/ue2-vfat/src/check.rs` | Raw FAT32 structure check |
| `crates/ue2-vfat/src/cp437.rs` | Code page 437 for short names |
| `crates/ue2-vfat/src/fatread.rs` | Snapshot reader (check + fatfs walk + hashes) |
| `crates/ue2-vfat/src/manifest.rs` | Manifest (image side and host side per path, diverged paths, directory remap) |
| `crates/ue2-vfat/src/sync.rs` | Plan, guard, apply (directory checks, conflict directories, reserved names, metadata copy), stale temporary files |
| `crates/ue2-vfat/src/volume.rs` | `DirVolume`: lock, work directory, prepare/resume, build, sync, host change |
| `crates/ue2-vfat/src/{backend,watch,worker}.rs` | Counting backend, FSEvents watcher, worker thread |
| `crates/ue2emu/src/usbdir.rs` | Emulation-thread controller: quiet tracking, snapshots, automatic sync and replug, requests, last sync |
| `scripts/smoke-usb-dir.{sh,ctl}` | Smoke test |
