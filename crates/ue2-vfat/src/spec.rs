//! `--usb-dir PATH[,size=SIZE][,ro]` and the volume size policy.

use std::path::PathBuf;
use std::str::FromStr;

const MIB: u64 = 1 << 20;
/// Smallest volume: FAT32 needs more than 65525 clusters (ff.c `MAX_FAT16`), 512-byte clusters from here on.
pub const MIN_SIZE: u64 = 64 * MIB;
/// Default lower bound of the size policy.
pub const DEFAULT_MIN_SIZE: u64 = 256 * MIB;
/// Free space the default adds on top of twice the content.
pub const DEFAULT_SLACK: u64 = 64 * MIB;
/// Largest volume: READ(10)/WRITE(10) address 32-bit LBAs of 512-byte blocks.
pub const MAX_SIZE: u64 = 2 << 40;

/// One `--usb-dir` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirSpec {
    pub path: PathBuf,
    /// Volume size in bytes; None applies [`default_size`].
    pub size: Option<u64>,
    /// Write-protected stick: nothing is ever written back.
    pub read_only: bool,
}

impl FromStr for DirSpec {
    type Err = String;

    /// Options are taken from the end (`,ro`, `,size=512M`), so a path may itself contain commas.
    fn from_str(s: &str) -> Result<DirSpec, String> {
        let mut rest = s;
        let (mut size, mut read_only) = (None, false);
        while let Some((head, option)) = rest.rsplit_once(',') {
            if option == "ro" {
                read_only = true;
            } else if let Some(value) = option.strip_prefix("size=") {
                size = Some(parse_size(value)?);
            } else if option.contains('=') || option.is_empty() {
                return Err(format!("unknown --usb-dir option '{option}' (expected size=SIZE or ro)"));
            } else {
                break;
            }
            rest = head;
        }
        if rest.is_empty() {
            return Err("--usb-dir needs a directory path".into());
        }
        Ok(DirSpec { path: PathBuf::from(rest), size, read_only })
    }
}

/// `512M`, `2G`, `1048576K`, `300MiB`, or plain bytes. Binary units. Within [`MIN_SIZE`]..=[`MAX_SIZE`].
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(digits);
    let n: u64 = number.parse().map_err(|_| format!("size '{s}': expected a number with K, M, G or T"))?;
    let shift = match unit.to_ascii_uppercase().trim_end_matches("IB").trim_end_matches('B') {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("size '{s}': unknown unit '{unit}' (K, M, G, T)")),
    };
    let bytes = n.checked_mul(1 << shift).ok_or_else(|| format!("size '{s}' is too large"))?;
    if !(MIN_SIZE..=MAX_SIZE).contains(&bytes) {
        return Err(format!("size '{s}': a volume holds 64M to 2T"));
    }
    Ok(bytes)
}

/// The default volume size for `content` bytes of files: max(256 MiB, 2 × content + 64 MiB), whole MiB, at most
/// [`MAX_SIZE`].
pub fn default_size(content: u64) -> u64 {
    let size = DEFAULT_MIN_SIZE.max(content.saturating_mul(2).saturating_add(DEFAULT_SLACK));
    size.div_ceil(MIB).saturating_mul(MIB).min(MAX_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_take_options_from_the_end() {
        let spec = |s: &str| s.parse::<DirSpec>();
        assert_eq!(spec("games"), Ok(DirSpec { path: "games".into(), size: None, read_only: false }));
        assert_eq!(spec("a,b/c,ro,size=512M"), Ok(DirSpec { path: "a,b/c".into(), size: Some(512 * MIB), read_only: true }));
        assert_eq!(spec("d,size=1G,ro").unwrap().size, Some(1 << 30));
        assert!(spec("d,size=12M").unwrap_err().contains("64M to 2T"));
        assert!(spec("d,rw=1").unwrap_err().contains("unknown --usb-dir option 'rw=1'"));
        assert!(spec(",ro").unwrap_err().contains("needs a directory"));
        assert!(spec("d,").unwrap_err().contains("unknown --usb-dir option ''"));
    }

    #[test]
    fn sizes_and_the_default_policy() {
        assert_eq!(parse_size("300MiB"), Ok(300 * MIB));
        assert_eq!(parse_size("2g"), Ok(2 << 30));
        assert_eq!(parse_size("67108864"), Ok(64 * MIB));
        assert!(parse_size("3X").is_err() && parse_size("M").is_err() && parse_size("3T").is_err());
        assert_eq!(default_size(0), 256 * MIB);
        assert_eq!(default_size(100 * MIB), 264 * MIB);
        assert_eq!(default_size(100 * MIB + 1), 265 * MIB, "whole MiB");
        assert_eq!(default_size(u64::MAX), MAX_SIZE);
    }
}
