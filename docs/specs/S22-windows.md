# S22 — Windows port

**Status:** built (2026-09-19).

UE2 builds on macOS and Linux only. Four crates use Unix APIs, networking needs libslirp, and the release is a source
tarball that Homebrew compiles. S22 makes UE2 build, test and run on Windows 11 x86-64 with MSVC and ships a zip on
the GitHub release, the way TRX64 does (`release-binaries.yml`). Scoop, winget and an installer are out of scope.

TRX64 is ready: `trx64-core` builds with MSVC at the pinned rev `4ab20e5` (reSID prelude for MSVC in its `build.rs`,
TRX64 `ad031da`), and TRX64's CI builds Windows zips on `windows-latest`.

## 1. Stages

| Stage | Result |
|---|---|
| W1 | Windows without network: everything except `--net` builds, tests and runs. The zip holds the two exes and the VC runtime. |
| W2 | `--net user` on Windows: libslirp from vcpkg, its DLLs in the zip. |
| — | Bridged networking on Windows: not built (§6). |

W1 is useful on its own: menu, C64, drives, carts, audio, `--usb-dir`, `--settings`, the control port and MCP.

## 2. Build setup

- Target `x86_64-pc-windows-msvc`, Visual Studio 2022 C++ tools (reSID and libslirp need them).
- `ue2-net` gets a cargo feature `slirp` (default on) around `UserNet` and the libslirp link; `ue2emu` a feature `net`
  (default on) that enables it. Without it `--net user` answers that this build has no libslirp; the other modes and
  `--hostfwd` parsing stay. W1 and W2 were built together, so CI and the release build the default features.
- Code that stays Unix-only is gated with `cfg(unix)` (or `cfg(target_os = "macos")` where it already is), and its
  CLI option prints a clear error on Windows instead of disappearing.
- `std::env::home_dir()` (the user profile on Windows since Rust 1.85, no longer deprecated) replaces every `HOME`
  read: `ue2emu/src/config.rs`, `ue2-mcp/src/config.rs`, `ue2-vfat/src/volume.rs`.

## 3. Changes per crate (W1)

### ue2emu

| Where | Today | Windows |
|---|---|---|
| `c64roms.rs:24` | `FileExt::write_all_at` | `Seek` + `write_all` (both platforms) |
| `net.rs:9,210` | `OsStrExt::as_bytes` for the MAC seed | `as_encoded_bytes()`: same bytes on Unix, so existing MACs stay |
| `window.rs` key events | — | drop synthetic key presses on focus, keep synthetic releases (winit replays held keys on Windows; TRX64 `trx64-cli` `window.rs` `accepts`) |
| `audio.rs` device config | the device's rate if 44.1/48 kHz, else 48/44.1 kHz | then the device's own rate, whatever it is: WASAPI shared mode takes only the mix rate, and the reSID engines and the sampler render at any rate, so no resampler is needed |

### ue2-mcp

| Where | Today | Windows |
|---|---|---|
| `main.rs:14` | `tokio::signal::unix` | `tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_break}` |
| `instance.rs:3` | `ExitStatusExt::signal` | none; report the exit code (`proc.rs` holds the platform code) |
| `instance.rs:161` | `process_group(0)` | `creation_flags(CREATE_NEW_PROCESS_GROUP \| CREATE_NO_WINDOW)` |
| `instance.rs:220-257,355-360` | `libc::kill` SIGTERM/SIGKILL | the clean stop stays the control port's `quit`; both harder stops are `TerminateProcess` by pid |
| `instance.rs:465-466` | `pid_alive` with `kill(pid, 0)` | `OpenProcess` + `GetExitCodeProcess` |
| `instance.rs:435-456` | watchdog: `/bin/sh` with `nc` | `ue2-mcp --watchdog PID PORT`, a Rust helper on every platform, so `nc` is no longer needed on macOS and Linux either |
| `instance.rs:94-99` | symlink of the ROMs directory for `UE2_FIRMWARE` | removed on every platform: `png` takes its font from `--roms` (`runner.rs`, `control.rs`), and ue2emu reads `UE2_FIRMWARE` only in tests |
| `config.rs:52,57` | the sibling `ue2emu` | `ue2emu` + `std::env::consts::EXE_SUFFIX` |

### ue2-vfat (`--usb-dir`)

| Where | Today | Windows |
|---|---|---|
| `check.rs:10`, `image.rs:8` | `FileExt` positional read/write | `os.rs`: `read_at`/`write_at` on Unix, `seek_read`/`seek_write` on Windows, with exact/all loops |
| `manifest.rs:8`, `volume.rs:8` | `MetadataExt` (mtime, ctime, inode, mode) | `os.rs`: Unix unchanged; Windows takes `modified()` with inode 0 (std has no stable file index), and the fingerprint from attributes, size, write and creation time |
| `volume.rs:61-104` | `flock` on the shared directories | `os::lock_dir`: `flock` stays on Unix; Windows cannot lock a directory, so a lock file per directory in `%TEMP%\ue2emu-locks`, named after the path, with `File::try_lock` / `try_lock_shared` |
| `volume.rs:163-172` | `canonicalize` | unchanged: the `\\?\` prefix on Windows is the same on both sides of every comparison and only shows in messages |
| `sync.rs:311` | `libc::ENOTDIR` | `ErrorKind::NotADirectory` (the component walk already sees a file before it descends) |
| `sync.rs:495` | `PermissionsExt` | `cfg(unix)` |
| `sync.rs` hard links | `hard_link` | fall back to a copy that keeps the modification time where the file system has no hard links (FAT, exFAT, some shares), on every platform |
| names the guest writes | — | a name Windows refuses (`CON`, `NUL`, `COM1`, ...) fails like any refused write: the sync reports it and keeps the image (§6) |
| `scan.rs` tests, `tests/volume.rs` | symlinks, `mkfifo` | `cfg(unix)` |

### ue2-net (W1 part)

- `socket_vmnet.rs` and `examples/mock_socket_vmnet.rs` (Unix domain socket): `cfg(unix)`. The daemon exists on macOS
  only; `--net socket-vmnet` errors on Windows.
- `vmnet.rs` is already `cfg(target_os = "macos")` (`lib.rs:18`).

ue2-core, rv32 and c64-bridge have no platform code.

## 4. `--net user` on Windows (W2)

**libslirp.** vcpkg port `libslirp`, triplet `x64-windows` (4.9.x; it pulls in glib, gettext/libintl, libiconv,
pcre2, libffi, zlib). dosbox-staging ships it this way on `windows-2022`: `slirp-0.dll`, `glib-2.0-0.dll`,
`iconv-2.dll`, `intl-8.dll`, `pcre2-8.dll`. A cold build takes 20-30 min for glib, so CI keeps the installed tree in
the GitHub Actions cache (or vcpkg's binary cache). `ue2-net/build.rs` takes `SLIRP_LIB_DIR` as today; the workflow
points it at `installed\x64-windows\lib`.

**Sockets are 64 bits.** On Windows a socket is a `SOCKET`, 64 bits on Win64, and libslirp 4.9 marks the `int`
poll API deprecated there. On Windows only (`cfg(windows)`):

- `SlirpConfig.version` 6, `slirp_pollfds_fill_socket`, an add-poll callback taking a `SOCKET` (`usize`), and the
  `register_poll_socket` / `unregister_poll_socket` fields of `SlirpCb` (`ffi.rs:31,36-47`);
- Windows' `IN6_ADDR` is 2-byte aligned where the Unix `in6_addr` is 4, which shifts the offsets in `SlirpConfig`
  (below: own types and a layout test);
- Unix stays on version 4, so Debian 12 and Ubuntu 24.04 (libslirp 4.7) keep linking.

**The poll.** `lib.rs:263-279,358-375` calls `libc::poll` with a zero timeout after every run slice. On Windows it
becomes `WSAPoll` with the same zero timeout:

- never request `POLLPRI` (WSAPoll fails with WSAEINVAL), request only `POLLRDNORM`/`POLLWRNORM`, and map
  `POLLERR`/`POLLHUP` from the results;
- skip the call when libslirp hands no socket (WSAPoll needs at least one);
- `WSAStartup` once before the first `slirp_new` (Rust's std starts Winsock only on its own first socket use).

Own `InAddr`/`In6Addr` types replace `libc`'s in `ffi.rs`, with the alignment per platform. `build.rs` compiles
`layout.c` against the real header (from `SLIRP_INCLUDE_DIR`, the `include` next to `SLIRP_LIB_DIR`, or Homebrew's)
and the `layout_matches_the_header` test compares 20 offsets and sizes, and on Windows the version 5/6 fields; without
a compilable header the test is skipped with a build warning.

If WSAPoll misbehaves, the fallback is QEMU's: `select()` with a zero timeout over fd sets built from the same list
(`util/main-loop.c`).

**Unchanged:** the host port forwards, the web proxy (`web_proxy.rs`), the control port and the GDB stub are plain
std/tokio TCP. Windows Firewall asks once when a port is bound to an address other than loopback.

## 5. CI and release

**CI** (`.github/workflows/ci.yml`): a third matrix entry `windows-x86_64` on `windows-latest`. W1 builds and tests
with `--no-default-features --features trx64`; W2 adds the cached vcpkg step, `SLIRP_LIB_DIR`, and vcpkg's `bin` on
`PATH` for the tests.

**Release** (new `release-binaries.yml`, on a `v*` tag, modelled on TRX64's): build `ue2emu` and `ue2-mcp` in release
mode, check `--version`, and pack `ue2emu-X.Y.Z-windows-x86_64.zip` with a `.sha256` next to it:

- `ue2emu.exe`, `ue2-mcp.exe`;
- from W2 the five libslirp DLLs;
- the VC runtime DLLs the exes and DLLs import (`vcruntime140.dll`, and what else the check lists), copied app-local
  from the Visual Studio redist directory, so nothing has to be installed;
- `LICENSE` and the notices of glib (LGPL-2.1), libslirp (BSD-3) and reSID (GPL-2).

The job lists the imports of every shipped exe and DLL (`dumpbin /dependents` after `msvc-dev-cmd`, or
`llvm-readobj --coff-imports`) and fails on a DLL that is neither in the zip nor part of Windows. TRX64's release job
skips this check on Windows ("dumpbin unavailable"); UE2's must run it.

## 6. Not built

- Bridged networking on Windows. Npcap on the network card needs a separate Npcap install (the free edition may not be
  bundled) and usually does not carry a second MAC over Wi-Fi; TAP-Windows6 with a Windows network bridge needs an
  admin driver install; Wintun carries IP packets, not Ethernet frames, so no DHCP.
- Windows on arm64 (`windows-11-arm`): after W2, once vcpkg's arm64 glib is known to build.
- A static glib (`x64-windows-static`) to drop the DLLs.
- Conflict names for guest file names Windows refuses.
- A Job Object around ue2-mcp's instances as a backstop to the watchdog.
- Scoop, winget, an installer.

## 7. Code

| File | Change |
|---|---|
| `crates/ue2emu/Cargo.toml`, `src/main.rs`, `src/net.rs` | feature `net`; `as_encoded_bytes` |
| `crates/ue2emu/src/c64roms.rs`, `config.rs`, `window.rs`, `audio.rs` | §3 |
| `crates/ue2-mcp/src/main.rs`, `instance.rs`, `config.rs` | signals, process control, watchdog helper, `EXE_SUFFIX` |
| `crates/ue2-vfat/src/*` | positional I/O helper, metadata, lock files, `dunce`, error kinds |
| `crates/ue2-net/src/ffi.rs`, `lib.rs`, `socket_vmnet.rs`, `build.rs`, `Cargo.toml` | `cfg(windows)` FFI v6 and WSAPoll; `windows-sys` |
| `.github/workflows/ci.yml`, `release-binaries.yml` (new) | Windows job; release zips |
| `README.md`, `docs/status/install.md`, `docs/ARCHITECTURE.md` | platforms: Windows from W1, `--net user` from W2 |

## 8. Acceptance

W1:

1. The Windows CI job builds and tests the workspace (without `net`); macOS and Linux unchanged.
2. The release zip installs by unzipping; `ue2emu --version` and `ue2-mcp --version` run on a clean Windows 11 (VM
   is fine) with nothing else installed; the import check passes.
3. On Windows 11 with the upstream ELF and ROMs: the overlay menu, TRX64 to BASIC `READY.`, a PRG from `--usb-dir`
   (and a change the guest writes lands in the host directory), a CRT, SID audio, `--settings`, the control port
   (`--script`), and ue2-mcp starting and stopping an instance from Claude Code.

W2:

4. The CI job also builds and tests `ue2-net`; the `SlirpConfig` layout test passes on Windows.
5. On Windows 11: `--net user` gets its DHCP lease, the web UI answers on `http://127.0.0.1:8080`, FTP and Telnet
   through the default forwards work, and a multi-megabyte FTP upload to the guest completes.
