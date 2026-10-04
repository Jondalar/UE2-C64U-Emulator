//! S38 §7: `.c64rering` files of the TRX64-Ultimate board (`trx_c64re.cc` on the app's `trx64/main`, 029da8ef).
//!
//! The board writes a `.c64rering` from its frame ring and loads one back, by its own rules:
//! - its writer: stored-block gzip, at most 64 entries, every payload with a `trx64Hw` node (the FPGA's chip block),
//!   empty delta and CPU rings, no labels;
//! - its loader takes only the entry `checkpoint_ring.current_id` names, and refuses a compressed body and a payload
//!   without `trx64Hw` (layout 1). It reads the keys it needs by name and passes over the rest.
//!
//! UE2 loads such a file as TRX64 does, with the board's `ram` node corrected ([`adopt`]). It writes the board's
//! format ([`write_board_ring`]) for the anchors that carry `trx64Hw`: those the board wrote. Its own anchors have no
//! chip block, and the block is the FPGA's flip-flops (`c64_core.v`'s state-bus map: the 6502's 209 bits, each CIA's
//! 363), which TRX64's state does not give.

use serde_json::Value;
use trx64_core::checkpoint_ring::{CheckpointAnchorDump, RuntimeCheckpointRingDump};
use trx64_core::cpu_history::CpuHistoryRingDump;
use trx64_core::delta_ring::DeltaRingDump;
use trx64_core::native_snapshot::{ta_u8, ta_u8_decode};
use trx64_core::ring_dump::{RingBufferDump, RING_DUMP_FORMAT_VERSION, RING_DUMP_MAGIC};

/// The board's node in a payload.
pub const BOARD_NODE: &str = "trx64Hw";

/// The board writes at most this many entries (`MAX_RING_ENTRIES`).
pub const BOARD_MAX_ENTRIES: usize = 64;

/// The `trx64Hw` layout the board loads.
const LAYOUT: i64 = 1;

/// The chip block: 58 state-bus words, little endian (`TRX_BLOCK_WORDS`).
const BLOCK_BYTES: usize = 58 * 4;

/// The snap positions the loader accepts (`x`, in pixels: cycle 12 to 63 of line 0).
const SNAP_X: std::ops::RangeInclusive<i64> = 96..=504;

/// The board's stored blocks (`GZ_BLOCK`); the loader takes any length a stored block can have.
const STORED_BLOCK: usize = 16 * 1024;

/// Whether `payload` is a board anchor.
pub fn is_board(payload: &Value) -> bool {
    payload.get(BOARD_NODE).is_some_and(|n| !n.is_null())
}

/// A board anchor's payload as TRX64 reads it. The board writes the colour nibbles at `$D800-$DBFF` of its `ram`
/// node and the RAM under the I/O into `trx64Hw.ramUnderIo`; TRX64's `ram` is the RAM and its colour RAM is the VIC
/// node's. So the RAM under the I/O goes back into `ram`, which the board's own loader also does. Returns whether the
/// payload is a board anchor.
pub(crate) fn adopt(payload: &mut Value) -> bool {
    if !is_board(payload) {
        return false;
    }
    let under = payload[BOARD_NODE].get("ramUnderIo").and_then(ta_u8_decode);
    let ram = payload.get("ram").and_then(ta_u8_decode);
    if let (Some(under), Some(mut ram)) = (under, ram) {
        if under.len() == 0x400 && ram.len() == 0x10000 {
            ram[0xD800..0xDC00].copy_from_slice(&under);
            payload["ram"] = ta_u8(&ram);
        }
    }
    true
}

/// What a board anchor does not carry, for the notes of a restore.
pub(crate) fn notes(payload: &Value) -> Vec<String> {
    let mut notes = vec![
        "a board anchor (TRX64-Ultimate): the file has no SID, IEC, interrupt-line, keyboard or expansion-port node; \
         those stay live"
            .to_string(),
        "a board anchor: its VIC node has the registers and colour RAM only: raster compare line 0, the sprite and \
         draw pipelines cold"
            .to_string(),
    ];
    if payload.get("turbo").is_some_and(|t| !t.is_null()) {
        notes.push(
            "a board anchor: its turbo node is the board's constant (its core has no turbo): $D030/$D031 = $00".into(),
        );
    }
    notes
}

/// The `.c64rering` the board loads: `entries` (at most [`BOARD_MAX_ENTRIES`], oldest first, each a board anchor)
/// with `current` as the one it restores. Stored-block gzip, empty delta and CPU rings, as the board writes.
pub fn write_board_ring(entries: Vec<CheckpointAnchorDump>, current: String, seq: u64) -> Result<Vec<u8>, String> {
    if entries.is_empty() || entries.len() > BOARD_MAX_ENTRIES {
        return Err(format!("{} entries: the board writes 1 to {BOARD_MAX_ENTRIES}", entries.len()));
    }
    if !entries.iter().any(|e| e.id == current) {
        return Err(format!("the current entry {current} is not among them"));
    }
    if let Some(e) = entries.iter().find(|e| !is_board(&e.payload)) {
        return Err(format!("{} has no {BOARD_NODE} node", e.id));
    }
    let n = entries.len() as u64;
    let dump = RingBufferDump {
        checkpoint_ring: RuntimeCheckpointRingDump {
            budget_bytes: n * 256 * 1024,
            max_entries: n,
            seq: seq.max(n),
            current_id: Some(current),
            entries,
        },
        delta_ring: DeltaRingDump { entry_cap: 1, writes_cap: 1, enabled: false, entries: Vec::new(), writes: Vec::new() },
        cpu_history: CpuHistoryRingDump { capacity: 1, enabled: false, entries: Vec::new() },
    };
    let json = serde_json::to_vec(&dump).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(9 + json.len() + json.len() / STORED_BLOCK * 5 + 32);
    out.extend_from_slice(RING_DUMP_MAGIC);
    out.push(RING_DUMP_FORMAT_VERSION);
    out.extend_from_slice(&gzip_stored(&json));
    Ok(out)
}

/// A gzip member of stored blocks, as the board's `Gz` writes it: the header without flags, 16 KiB blocks, the last
/// one final, CRC-32 and the length behind.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 0x08, 0, 0, 0, 0, 0, 0, 0x03];
    let mut chunks = data.chunks(STORED_BLOCK).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(c) = chunks.next() {
        let len = c.len() as u16;
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    let mut crc = flate2::Crc::new();
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// What [`check_board_ring`] found in a file the board would load.
#[derive(Debug, PartialEq, Eq)]
pub struct BoardCheck {
    pub entries: usize,
    pub current_id: String,
    pub stored_blocks: usize,
}

/// Check `bytes` against the board's loader (`trx_c64re_load`, `gunzip_stored`) and writer (`MAX_RING_ENTRIES`), rule
/// by rule as its code has them; the error names the rule that fails. The board cannot be run here, so this is the
/// test of a file meant for it.
pub fn check_board_ring(bytes: &[u8]) -> Result<BoardCheck, String> {
    // The container: magic, version 1, a gzip member of at least 18 bytes.
    if bytes.len() < 9 + 18 || &bytes[..8] != RING_DUMP_MAGIC || bytes[8] != 1 {
        return Err("Not a .c64re or .c64rering (format 1)".into());
    }
    let (json, stored_blocks) = gunzip_stored(&bytes[9..])?;
    let doc: Value = serde_json::from_slice(&json).map_err(|_| "Bad JSON".to_string())?;
    let ring = &doc["checkpoint_ring"];
    let (Some(current), Some(entries)) = (ring["current_id"].as_str(), ring["entries"].as_array()) else {
        return Err("No checkpoint_ring entries".into());
    };
    if entries.len() > BOARD_MAX_ENTRIES {
        return Err(format!("{} entries: the board writes at most {BOARD_MAX_ENTRIES}", entries.len()));
    }
    let cp = entries
        .iter()
        .find(|e| e["id"].as_str() == Some(current))
        .and_then(|e| e.get("payload"))
        .ok_or("The current entry is not in the file")?;
    let hw = cp.get(BOARD_NODE).filter(|n| n.is_object()).ok_or("No trx64Hw node (not written by this board)")?;
    if hw["layout"].as_i64() != Some(LAYOUT) {
        return Err("Unknown trx64Hw layout".into());
    }
    let len = |v: &Value| board_b64_len(v);
    if len(&cp["ram"]) != Some(0x10000)
        || len(&hw["block"]) != Some(BLOCK_BYTES)
        || len(&hw["ramUnderIo"]) != Some(0x400)
        || len(&hw["colorRam"]) != Some(0x400)
    {
        return Err("Missing or short arrays".into());
    }
    if !hw["x"].as_i64().is_some_and(|x| SNAP_X.contains(&x)) {
        return Err("Bad snap position".into());
    }
    Ok(BoardCheck { entries: entries.len(), current_id: current.to_string(), stored_blocks })
}

/// The bytes the board's `tok_ta` takes from a `{"$ta":…,"b64":…}` node: it skips every character that is not base64
/// (padding included) and counts whole bytes.
fn board_b64_len(v: &Value) -> Option<usize> {
    let s = v.get("b64")?.as_str()?;
    let bits = s.bytes().filter(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/').count() * 6;
    Some(bits / 8)
}

/// The board's `gunzip_stored`: the body of a gzip member of stored blocks, and how many blocks it had.
fn gunzip_stored(p: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let len = p.len();
    if len < 18 || p[0] != 0x1F || p[1] != 0x8B || p[2] != 8 {
        return Err("Not a gzip body".into());
    }
    let mut i = 10usize;
    let flg = p[3];
    let at = |i: usize| p.get(i).copied().map(usize::from).ok_or_else(|| "Truncated gzip body".to_string());
    if flg & 4 != 0 {
        i += 2 + (at(i)? | at(i + 1)? << 8);
    }
    for bit in [8, 16] {
        if flg & bit != 0 {
            while i < len && p[i] != 0 {
                i += 1;
            }
            i += 1;
        }
    }
    if flg & 2 != 0 {
        i += 2;
    }
    let isize = u32::from_le_bytes([p[len - 4], p[len - 3], p[len - 2], p[len - 1]]) as usize;
    let mut out = Vec::with_capacity(isize);
    let mut blocks = 0;
    loop {
        if i + 5 > len {
            return Err("Truncated gzip body".into());
        }
        let h = p[i];
        if h & 6 != 0 {
            return Err("Compressed .c64re (only the board's own uncompressed files load)".into());
        }
        let n = usize::from(p[i + 1]) | usize::from(p[i + 2]) << 8;
        i += 5;
        if i + n > len || out.len() + n > isize {
            return Err("Bad stored block".into());
        }
        out.extend_from_slice(&p[i..i + n]);
        i += n;
        blocks += 1;
        if h & 1 != 0 {
            break;
        }
    }
    Ok((out, blocks))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read as _;

    use super::*;
    use trx64_core::ring_dump::read_ring_dump;

    /// The board's own file (TRX64-Ultimate `tests/ringdump/board_2026-10-04.c64rering`, written by the app's
    /// `trx64/main`): 9 anchors of a PAL C64 at READY, no marks, no cartridge, no turbo node. Kept gzipped.
    pub(crate) fn board_file() -> Vec<u8> {
        let gz = include_bytes!("../tests/data/board_2026-10-04.c64rering.gz");
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut bytes).expect("the fixture unpacks");
        bytes
    }

    /// The check passes the board's own file and finds what the board wrote.
    #[test]
    fn the_board_file_passes_the_board_rules() {
        let bytes = board_file();
        assert_eq!(bytes.len(), 873_917);
        let check = check_board_ring(&bytes).unwrap();
        assert_eq!(check, BoardCheck { entries: 9, current_id: "cp_97660_97662".into(), stored_blocks: check.stored_blocks });
        assert!(check.stored_blocks > 1);
    }

    /// The check refuses what the board's loader refuses: a compressed body, a payload without the chip block, a wrong
    /// layout, a short array, a snap position outside line 0's cycles 12-63, a current id not in the file.
    #[test]
    fn the_check_refuses_what_the_board_refuses() {
        let dump = read_ring_dump(&board_file()).unwrap();
        let compressed = trx64_core::ring_dump::write_ring_dump(&dump);
        assert!(check_board_ring(&compressed).unwrap_err().starts_with("Compressed"));

        let with = |f: &dyn Fn(&mut Value)| {
            let mut entries = dump.checkpoint_ring.entries.clone();
            f(&mut entries.last_mut().unwrap().payload);
            let current = entries.last().unwrap().id.clone();
            check_board_ring(&write_board_ring_unchecked(entries, current)).map(|_| ())
        };
        assert_eq!(with(&|_| {}), Ok(()));
        assert_eq!(with(&|p| p[BOARD_NODE]["layout"] = 2.into()), Err("Unknown trx64Hw layout".into()));
        assert_eq!(with(&|p| p[BOARD_NODE]["x"] = 95.into()), Err("Bad snap position".into()));
        assert_eq!(with(&|p| p[BOARD_NODE]["x"] = 505.into()), Err("Bad snap position".into()));
        assert_eq!(with(&|p| p[BOARD_NODE]["colorRam"] = ta_u8(&[0; 0x3FF])), Err("Missing or short arrays".into()));
        assert_eq!(with(&|p| p["ram"] = Value::Null), Err("Missing or short arrays".into()));
        assert_eq!(
            with(&|p| {
                p.as_object_mut().unwrap().remove(BOARD_NODE);
            }),
            Err("No trx64Hw node (not written by this board)".into())
        );
        let entries = dump.checkpoint_ring.entries.clone();
        assert_eq!(
            check_board_ring(&write_board_ring_unchecked(entries, "cp_1_1".into())).map(|_| ()),
            Err("The current entry is not in the file".into())
        );
    }

    /// The writer's file: stored blocks the board's reader takes, the TRX64 reader too, the same entries; and its own
    /// limits.
    #[test]
    fn the_board_writer_writes_what_both_readers_take() {
        let dump = read_ring_dump(&board_file()).unwrap();
        let entries = dump.checkpoint_ring.entries.clone();
        let current = entries[4].id.clone();
        let bytes = write_board_ring(entries.clone(), current.clone(), dump.checkpoint_ring.seq).unwrap();
        let check = check_board_ring(&bytes).unwrap();
        assert_eq!((check.entries, check.current_id.as_str()), (9, current.as_str()));
        let back = read_ring_dump(&bytes).unwrap();
        assert_eq!(back.checkpoint_ring.entries.len(), 9);
        assert_eq!(back.checkpoint_ring.entries[4].payload, entries[4].payload);
        assert_eq!((back.delta_ring.entry_count(), back.cpu_history.entry_count()), (0, 0));

        let mut many = Vec::new();
        for k in 0..65 {
            let mut e = entries[0].clone();
            e.id = format!("cp_{k}_{k}");
            many.push(e);
        }
        assert!(write_board_ring(many.clone(), "cp_0_0".into(), 0).is_err(), "65 entries");
        many.truncate(64);
        assert_eq!(check_board_ring(&write_board_ring(many, "cp_63_63".into(), 0).unwrap()).unwrap().entries, 64);
        let mut plain = entries[0].clone();
        plain.payload.as_object_mut().unwrap().remove(BOARD_NODE);
        assert!(write_board_ring(vec![plain], entries[0].id.clone(), 0).unwrap_err().contains("trx64Hw"));
    }

    /// `adopt` puts the RAM under the I/O back where TRX64 reads RAM; the board put colour nibbles there.
    #[test]
    fn adopt_puts_the_ram_under_the_io_back() {
        let dump = read_ring_dump(&board_file()).unwrap();
        let mut p = dump.checkpoint_ring.entries[0].payload.clone();
        let colour = ta_u8_decode(&p["ram"]).unwrap()[0xD800..0xDC00].to_vec();
        let under = ta_u8_decode(&p[BOARD_NODE]["ramUnderIo"]).unwrap();
        assert!(colour.iter().all(|c| *c < 0x10), "the board's ram node holds colour nibbles there");
        assert_ne!(colour, under);
        assert!(adopt(&mut p));
        assert_eq!(ta_u8_decode(&p["ram"]).unwrap()[0xD800..0xDC00], under[..]);
        let mut ue2 = serde_json::json!({ "ram": ta_u8(&[1; 0x10000]) });
        assert!(!adopt(&mut ue2));
    }

    /// The writer without its own checks, for files the board must refuse.
    fn write_board_ring_unchecked(entries: Vec<CheckpointAnchorDump>, current: String) -> Vec<u8> {
        let dump = RingBufferDump {
            checkpoint_ring: RuntimeCheckpointRingDump {
                budget_bytes: 0,
                max_entries: entries.len() as u64,
                seq: 0,
                current_id: Some(current),
                entries,
            },
            delta_ring: DeltaRingDump { entry_cap: 1, writes_cap: 1, enabled: false, entries: Vec::new(), writes: Vec::new() },
            cpu_history: CpuHistoryRingDump { capacity: 1, enabled: false, entries: Vec::new() },
        };
        let mut out = RING_DUMP_MAGIC.to_vec();
        out.push(1);
        out.extend_from_slice(&gzip_stored(&serde_json::to_vec(&dump).unwrap()));
        out
    }
}
