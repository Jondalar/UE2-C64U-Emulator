//! The firmware's command interface, driven from the host (S23 §6).
//!
//! The UCI block (docs/specs/S15-uci.md) is the cartridge software's door into the running firmware: a client
//! writes a command into the block, the firmware's "UCI Server" task answers with reply data and a status string.
//! The monitor knocks on the same door from outside the machine — `AccessKind::Host`, so it is not a C64 bus cycle
//! and the unlock detector and the `$FF00` trigger stay untouched (trx64 `uci.rs`).
//!
//! Firmware paths are under firmware/1541ultimate/software.
//!
//! Two things a caller must know. The firmware answers on its own task, so [`command`] runs the emulator until the
//! block has the reply — display, USB and network are not serviced in that stretch, which is why the commands used
//! here are the short ones. And the block belongs to whoever is using it: a command is refused while the C64 has
//! one in flight.

use trx64_core::{Access, AccessKind, ExpansionDevice, UciStatus};
use ue2_core::machine::{Machine, RunExit};

/// `ControlTarget ct1(4)` (control_target.cc:25): the target with the settings, reset and freeze commands.
pub const TARGET_CONTROL: u8 = 4;

/// `CTRL_CMD_LOAD_CONFIG` (control_target.h:28): read a `.cfg` and effectuate every store it touched. Added to the
/// firmware in `Add UCI control command to load config file`; an older build answers "UNKNOWN COMMAND".
pub const CTRL_CMD_LOAD_CONFIG: u8 = 0x50;

/// The C64 side's four registers inside the eight-byte window (command_if_pkg.vhd:27-31).
const SLOT_CONTROL: u16 = 4;
const SLOT_COMMAND: u16 = 5;
const SLOT_RESPONSE: u16 = 6;
const SLOT_STATUS: u16 = 7;

/// The control writes a client makes: push what it wrote, and take the data the firmware validated
/// (command_intf.h:51-53). Bit 2 is the abort a client uses to get out of a transaction.
const CMD_NEW_COMMAND: u8 = 0x01;
const CMD_DATA_ACCEPTED: u8 = 0x02;
const CMD_ABORT: u8 = 0x04;

/// `slot_status(5 downto 4)`: bit 1 says the data is valid, bit 0 that more follows (command_protocol.vhd).
const STATE_VALID: u8 = 0b10;
const STATE_MORE: u8 = 0b01;

/// The block's own buffers, so a length the firmware got wrong cannot spin us (command_if_pkg.vhd:33-41).
const RESPONSE_MAX: usize = 896;
const STATUS_MAX: usize = 256;

/// How long the firmware may take over one command, in emulated milliseconds. Opening a `.cfg` and effectuating a
/// store is milliseconds of hardware time; seconds are a hang.
const TIMEOUT_MS: u64 = 5_000;
/// Firmware instructions between two looks at the block — about a tenth of a millisecond.
const STEP_INSNS: u64 = 5_000;

/// What the firmware answered: the reply data (`CTRL_CMD_LOAD_CONFIG` puts its parse log there) and the status
/// string, which is the firmware's own `code,TEXT` (command_intf.cc:236-241).
pub struct Reply {
    pub data: Vec<u8>,
    pub status: String,
}

impl Reply {
    /// `00,OK` is the only success the firmware spells.
    pub fn ok(&self) -> bool {
        self.status.starts_with("00,")
    }

    /// The reply data as a reader sees it: the firmware writes plain text there, one message per line.
    pub fn text(&self) -> String {
        self.data.iter().map(|&b| char::from(b)).filter(|c| *c != '\r').collect()
    }
}

/// Push one command into the block and run the firmware until it has answered.
///
/// `bytes` is the message as the target sees it: the target id, the command byte, then the command's own arguments
/// (command_intf.cc:157-183).
pub fn command(m: &mut Machine, bytes: &[u8]) -> Result<Reply, String> {
    let before = status(m)?;
    if !before.enabled {
        return Err("the firmware's command interface is off ([C64 and Cartridge Settings] Command Interface)".into());
    }
    if before.state != 0 || before.new_command || before.data_accepted || before.abort {
        return Err("the command interface is busy: the C64 has a command in flight".into());
    }
    let window = before.window;
    for &b in bytes {
        write(m, window + SLOT_COMMAND, b)?;
    }
    write(m, window + SLOT_CONTROL, CMD_NEW_COMMAND)?;

    let deadline = m.now_ms() + TIMEOUT_MS;
    let (mut data, mut text) = (Vec::new(), Vec::new());
    loop {
        let mut s = status(m)?;
        while s.state & STATE_VALID == 0 {
            if m.now_ms() >= deadline {
                // Leave the block as a client that gave up leaves it: the firmware clears the abort itself.
                write(m, window + SLOT_CONTROL, CMD_ABORT)?;
                return Err(format!("the firmware did not answer within {TIMEOUT_MS} emulated ms"));
            }
            match m.run(STEP_INSNS) {
                RunExit::Budget => {}
                RunExit::Breakpoint(pc) => {
                    return Err(format!("a breakpoint at {pc:#010x} stopped the firmware with a command in flight"))
                }
                RunExit::Halted(why) => {
                    return Err(format!("the firmware stopped with a command in flight: {why}"))
                }
            }
            s = status(m)?;
        }
        // Each read takes one byte and advances the block's pointer, exactly as a client's `LDA` does.
        while status(m)?.response_valid && data.len() < RESPONSE_MAX {
            data.push(read(m, window + SLOT_RESPONSE)?);
        }
        while status(m)?.status_valid && text.len() < STATUS_MAX {
            text.push(read(m, window + SLOT_STATUS)?);
        }
        let more = s.state & STATE_MORE != 0;
        write(m, window + SLOT_CONTROL, CMD_DATA_ACCEPTED)?;
        if !more {
            break;
        }
    }
    let status: String = text.iter().map(|&b| char::from(b)).collect();
    Ok(Reply { data, status: status.trim_end_matches(['\0', ' ']).to_string() })
}

/// The block, or the sentence that says why there is none.
fn status(m: &mut Machine) -> Result<UciStatus, String> {
    c64(m)?.uci_status().ok_or_else(|| "this C64 carries no command interface block".to_string())
}

fn c64(m: &mut Machine) -> Result<&mut trx64_core::Machine, String> {
    Ok(super::trx64(m).ok_or("the monitor needs a C64: start with --c64 trx64")?.trx64())
}

/// One host access to a register of the block. `stalled` is 0: nothing stole the bus, there was no bus cycle.
fn access(clk: u64, addr: u16) -> Access {
    Access { addr, clk, kind: AccessKind::Host, stalled: 0, stalled_on_bus: 0 }
}

fn write(m: &mut Machine, addr: u16, value: u8) -> Result<(), String> {
    let c64 = c64(m)?;
    let clk = c64.c64_core.clk;
    let block = c64.uci_mut().ok_or("this C64 carries no command interface block")?;
    block.write(access(clk, addr), value);
    Ok(())
}

fn read(m: &mut Machine, addr: u16) -> Result<u8, String> {
    let c64 = c64(m)?;
    let clk = c64.c64_core.clk;
    let block = c64.uci_mut().ok_or("this C64 carries no command interface block")?;
    block
        .read(access(clk, addr), None)
        .ok_or_else(|| format!("{addr:#06x} is not the block's window; the firmware moved it"))
}
