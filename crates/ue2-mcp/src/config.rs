//! Paths and defaults. Every path can be overridden from the environment (see `main.rs` usage).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Firmware ELF inside a built 1541ultimate checkout.
pub const ELF_REL: &str = "target/u64ii/riscv/ultimate/result/ultimate.elf";

#[derive(Clone, Debug)]
pub struct Config {
    /// UE2-C64U-Emulator checkout: default firmware and run directories.
    pub repo: PathBuf,
    /// The `ue2emu` binary.
    pub emulator: PathBuf,
    /// Default 1541ultimate checkout (firmware ELF, roms).
    pub firmware_tree: PathBuf,
    /// Instance directories (`<run_base>/<id>/`).
    pub run_base: PathBuf,
    /// This server's pid (instance watchdogs stop the emulator when it disappears).
    pub server_pid: u32,
}

impl Config {
    pub fn from_env() -> Config {
        let repo = env_path("UE2_REPO").unwrap_or_else(default_repo);
        Config {
            emulator: env_path("UE2EMU_BIN").unwrap_or_else(|| repo.join("target/release/ue2emu")),
            firmware_tree: env_path("UE2_FIRMWARE_TREE").unwrap_or_else(|| repo.join("firmware/1541ultimate")),
            run_base: env_path("UE2_MCP_RUN").unwrap_or_else(|| repo.join("run/mcp")),
            server_pid: std::process::id(),
            repo,
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(|v| resolve(&v.to_string_lossy()))
}

/// The repo that contains this binary (`<repo>/target/release/ue2-mcp`), else the source checkout it was built from.
fn default_repo() -> PathBuf {
    let is_repo = |d: &Path| d.join("crates/ue2emu/Cargo.toml").is_file();
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.canonicalize().unwrap_or(exe);
        if let Some(dir) = exe.ancestors().skip(1).find(|d| is_repo(d)) {
            return dir.to_path_buf();
        }
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    dir.canonicalize().unwrap_or(dir)
}

/// A path argument: `~/` expands to $HOME, relative paths are relative to the server's working directory
/// (the directory the MCP client started it in).
pub fn resolve(p: &str) -> PathBuf {
    let expanded = match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map(|c| c.join(&expanded)).unwrap_or(expanded)
    }
}

/// Firmware image to boot, and the 1541ultimate tree it lives in (for the default roms directory).
///
/// `arg` may be an image file (ELF, .app, .ue2) or a checkout directory (its built ELF is used).
pub fn resolve_firmware(cfg: &Config, arg: Option<&str>) -> Result<(PathBuf, Option<PathBuf>)> {
    let path = arg.map(resolve).unwrap_or_else(|| cfg.firmware_tree.join(ELF_REL));
    if path.is_dir() {
        let elf = path.join(ELF_REL);
        if !elf.is_file() {
            bail!(
                "{} has no built firmware ({} is missing); build that checkout first",
                path.display(),
                elf.display()
            );
        }
        return Ok((elf, Some(path)));
    }
    if !path.is_file() {
        bail!("firmware image {} not found", path.display());
    }
    let tree = path.ancestors().skip(1).find(|d| d.join("roms/chars.bin").is_file()).map(Path::to_path_buf);
    Ok((path, tree))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_expands_home_and_relative_paths() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve("~/x/y"), PathBuf::from(home).join("x/y"));
        assert_eq!(resolve("/abs/p"), PathBuf::from("/abs/p"));
        assert_eq!(resolve("rel/p"), std::env::current_dir().unwrap().join("rel/p"));
    }

    #[test]
    fn firmware_tree_directory_needs_a_built_elf() {
        let dir = std::env::temp_dir().join(format!("ue2-mcp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("roms")).unwrap();
        std::fs::write(dir.join("roms/chars.bin"), [0u8; 8]).unwrap();
        let cfg = Config {
            repo: dir.clone(),
            emulator: dir.join("ue2emu"),
            firmware_tree: dir.clone(),
            run_base: dir.join("run"),
            server_pid: 1,
        };
        let err = resolve_firmware(&cfg, Some(dir.to_str().unwrap())).unwrap_err().to_string();
        assert!(err.contains("has no built firmware"), "{err}");
        let elf = dir.join(ELF_REL);
        std::fs::create_dir_all(elf.parent().unwrap()).unwrap();
        std::fs::write(&elf, b"\x7fELF").unwrap();
        assert_eq!(resolve_firmware(&cfg, Some(dir.to_str().unwrap())).unwrap(), (elf.clone(), Some(dir.clone())));
        assert_eq!(resolve_firmware(&cfg, None).unwrap(), (elf.clone(), Some(dir.clone())), "file inside a tree");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
