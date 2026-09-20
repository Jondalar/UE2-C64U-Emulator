//! The VICE binary monitor protocol, so third-party debuggers can attach (S23 §8, M4).
//!
//! A compatibility surface, nothing more. It exists so that C64 Studio, IceBro Lite, VS64, VS65 and the .NET
//! bridge attach to UE2 as they attach to VICE — and to Denise, which serves the same protocol. It is not our API
//! and it does not shape the rest of the monitor.
//!
//! **The C64 core only.** Memspace 0 is the 6510 and 1-4 are the drives; the protocol's memspace byte names
//! machines, not CPUs, and every register value on the wire is 16 bits. The firmware's RISC-V is not reachable
//! here at all and stays on our own control protocol.
//!
//! Ported from VICE 3.10, `src/monitor/monitor_binary.c` — the layouts below are that file's, byte for byte:
//! `monitor_binary_response` (the 12-byte header), `monitor_binary_response_checkpoint_info` (23 bytes),
//! `write_registers` (item size 3), `monitor_binary_process_banks_available` and `..._registers_available` (the
//! length-prefixed item lists), and `monitor_binary_process_command` for the 11-byte request header. Where VICE's
//! own parser is careless we are not: it drops one byte on a bad magic and desyncs on a short body; we bound-check
//! every field and resynchronise on the magic.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use anyhow::{Context, Result};
use ue2_core::machine::Machine;

use crate::monitor;

/// `ASC_STX` and `MON_BINARY_API_VERSION` (monitor_binary.c:290).
const STX: u8 = 0x02;
const API_VERSION: u8 = 0x02;
/// `MON_EVENT_ID` (monitor_binary.c:292): the request id every unsolicited response carries.
const EVENT_ID: u32 = 0xFFFF_FFFF;
/// The request header is 11 bytes, the response header 12.
const REQUEST_HEADER: usize = 11;
/// A body longer than this is a client that lost the framing; we drop the connection rather than allocate.
const BODY_MAX: u32 = 1 << 20;

/// Commands (monitor_binary.c:83-125). Only the ones we answer are named.
mod cmd {
    pub const MEM_GET: u8 = 0x01;
    pub const MEM_SET: u8 = 0x02;
    pub const CHECKPOINT_GET: u8 = 0x11;
    pub const CHECKPOINT_SET: u8 = 0x12;
    pub const CHECKPOINT_DELETE: u8 = 0x13;
    pub const CHECKPOINT_LIST: u8 = 0x14;
    pub const CHECKPOINT_TOGGLE: u8 = 0x15;
    pub const REGISTERS_GET: u8 = 0x31;
    pub const REGISTERS_SET: u8 = 0x32;
    pub const ADVANCE_INSTRUCTIONS: u8 = 0x71;
    pub const KEYBOARD_FEED: u8 = 0x72;
    pub const EXECUTE_UNTIL_RETURN: u8 = 0x73;
    pub const PING: u8 = 0x81;
    pub const BANKS_AVAILABLE: u8 = 0x82;
    pub const REGISTERS_AVAILABLE: u8 = 0x83;
    pub const VICE_INFO: u8 = 0x85;
    pub const EXIT: u8 = 0xAA;
    pub const QUIT: u8 = 0xBB;
    pub const RESET: u8 = 0xCC;
}

/// Responses and events (monitor_binary.c:130-174).
mod resp {
    pub const MEM_GET: u8 = 0x01;
    pub const MEM_SET: u8 = 0x02;
    pub const CHECKPOINT_INFO: u8 = 0x11;
    pub const CHECKPOINT_DELETE: u8 = 0x13;
    pub const CHECKPOINT_LIST: u8 = 0x14;
    pub const CHECKPOINT_TOGGLE: u8 = 0x15;
    pub const REGISTER_INFO: u8 = 0x31;
    pub const STOPPED: u8 = 0x62;
    pub const RESUMED: u8 = 0x63;
    pub const ADVANCE_INSTRUCTIONS: u8 = 0x71;
    pub const KEYBOARD_FEED: u8 = 0x72;
    pub const EXECUTE_UNTIL_RETURN: u8 = 0x73;
    pub const PING: u8 = 0x81;
    pub const BANKS_AVAILABLE: u8 = 0x82;
    pub const REGISTERS_AVAILABLE: u8 = 0x83;
    pub const VICE_INFO: u8 = 0x85;
    pub const EXIT: u8 = 0xAA;
    pub const RESET: u8 = 0xCC;
}

/// Errors (monitor_binary.c:178-186).
mod err {
    pub const OK: u8 = 0x00;
    pub const OBJECT_MISSING: u8 = 0x01;
    pub const INVALID_MEMSPACE: u8 = 0x02;
    pub const CMD_INVALID_LENGTH: u8 = 0x80;
    pub const INVALID_PARAMETER: u8 = 0x81;
    pub const CMD_INVALID_API_VERSION: u8 = 0x82;
    pub const CMD_INVALID_TYPE: u8 = 0x83;
    pub const CMD_FAILURE: u8 = 0x8F;
}

/// Register ids (montypes.h:52-107) and the 6502 list VICE reports (mon_register6502.c:73-82), in its order. The
/// `NV-BDIZC` entry is `MON_REGISTER_IS_FLAGS` and VICE leaves it out of the wire, so it is not here either.
const REGISTERS: [(u8, u8, &str); 8] = [
    (0x03, 16, "PC"),
    (0x00, 8, "A"),
    (0x01, 8, "X"),
    (0x02, 8, "Y"),
    (0x04, 8, "SP"),
    (0x05, 8, "FL"),
    (0x35, 16, "LIN"),
    (0x36, 16, "CYC"),
];

/// `banknames`/`banknums` of a C64 (c64mem.c:1239-1250), and the lens each one is in our machine. Keeping the
/// names is the point: a client looks them up by name.
const BANKS: [(u16, &str, &str); 6] = [
    (0, "default", "cpu"),
    (0, "cpu", "cpu"),
    (1, "ram", "ram"),
    (2, "rom", "rom"),
    (3, "io", "io"),
    (4, "cart", "cart"),
];

/// The version this server reports for itself (`VICE_INFO`). We answer as the VICE whose protocol this is, because
/// clients gate features on it; the fourth field is ours, so a client that looks can tell.
const VICE_VERSION: [u8; 4] = [3, 10, 0, 0];

/// One checkpoint, as VICE keeps them (`mon_checkpoint_t`, the fields the protocol exposes).
#[derive(Clone, Debug)]
struct Checkpoint {
    number: u32,
    start: u16,
    end: u16,
    stop_when_hit: bool,
    enabled: bool,
    /// Bit 0 load, bit 1 store, bit 2 exec (`MEMORY_OP`).
    operation: u8,
    temporary: bool,
    hit_count: u32,
    ignore_count: u32,
}

/// The server: a listener, at most one client, and the checkpoints that client set.
pub struct ViceServer {
    listener: TcpListener,
    client: Option<TcpStream>,
    inbox: Vec<u8>,
    checkpoints: Vec<Checkpoint>,
    next_number: u32,
    /// The C64 was stopped for the client and has to be told when it runs again.
    announced_stop: bool,
    /// The machine carries the current checkpoint set; cleared whenever it changes.
    armed: bool,
}

impl ViceServer {
    /// Listen on `addr`. The socket is non-blocking: the run loop polls it between slices.
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(addr).with_context(|| format!("vice monitor on {addr}"))?;
        listener.set_nonblocking(true).context("vice monitor: non-blocking listener")?;
        eprintln!("vice monitor: listening on {addr} (one client, VICE binary protocol)");
        Ok(ViceServer {
            listener,
            client: None,
            inbox: Vec::new(),
            checkpoints: Vec::new(),
            next_number: 1,
            announced_stop: false,
            armed: false,
        })
    }

    /// Serve whatever the client has sent. Called between run slices, so nothing here blocks.
    pub fn poll(&mut self, machine: &mut Machine, state: &mut monitor::State) {
        self.accept();
        if self.client.is_none() {
            if !self.checkpoints.is_empty() {
                // Nobody is listening: take the gate off the machine so it runs as it did.
                self.checkpoints.clear();
                monitor::set_c64_checkpoints(machine, &[]);
            }
            return;
        }
        if !self.armed {
            self.arm(machine);
            self.armed = true;
        }
        if let Some(pc) = monitor::take_c64_checkpoint_hit(machine) {
            self.hit(machine, state, pc);
        }
        if !self.fill() {
            self.drop_client("closed the connection");
            return;
        }
        while let Some((request_id, api, command, body)) = self.take_request() {
            self.dispatch(machine, state, request_id, api, command, &body);
        }
    }

    /// A checkpoint hit, seen by the run loop while the machine ran (S23 §8: the info goes out *before* the stop).
    pub fn hit(&mut self, machine: &mut Machine, state: &mut monitor::State, pc: u16) {
        let Some(index) = self.checkpoints.iter().position(|c| c.enabled && c.start <= pc && pc <= c.end) else {
            return;
        };
        self.checkpoints[index].hit_count += 1;
        let checkpoint = self.checkpoints[index].clone();
        self.send(EVENT_ID, resp::CHECKPOINT_INFO, err::OK, &checkpoint_body(&checkpoint, true));
        if checkpoint.stop_when_hit {
            monitor::halt_c64(machine, state, true);
            self.announce_stop(machine, state);
        }
        if checkpoint.temporary {
            self.checkpoints.remove(index);
        }
    }

    /// Hand the enabled checkpoints to the machine, so its own runs stop on them instead of the run loop noticing
    /// a slice too late.
    fn arm(&self, machine: &mut Machine) {
        let ranges: Vec<(u16, u16, u8)> =
            self.checkpoints.iter().filter(|c| c.enabled).map(|c| (c.start, c.end, c.operation)).collect();
        monitor::set_c64_checkpoints(machine, &ranges);
    }

    fn accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, from)) => {
                if stream.set_nonblocking(true).is_err() {
                    return;
                }
                if self.client.is_some() {
                    // VICE takes one client; a second is told by being closed at once.
                    eprintln!("vice monitor: refused {from}, a client is already attached");
                    return;
                }
                eprintln!("vice monitor: {from} attached");
                self.client = Some(stream);
                self.inbox.clear();
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("vice monitor: accept failed: {e}"),
        }
    }

    /// Read what is there. False when the client is gone.
    fn fill(&mut self) -> bool {
        let Some(stream) = self.client.as_mut() else { return true };
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => self.inbox.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("vice monitor: read failed: {e}");
                    return false;
                }
            }
        }
    }

    /// One complete request, or None. A byte that is not the magic is dropped until one is — VICE drops exactly
    /// one and desyncs; resynchronising is strictly better and costs nothing.
    fn take_request(&mut self) -> Option<(u32, u8, u8, Vec<u8>)> {
        loop {
            let start = self.inbox.iter().position(|&b| b == STX)?;
            if start > 0 {
                self.inbox.drain(..start);
            }
            if self.inbox.len() < REQUEST_HEADER {
                return None;
            }
            let api = self.inbox[1];
            let length = u32::from_le_bytes([self.inbox[2], self.inbox[3], self.inbox[4], self.inbox[5]]);
            let request_id = u32::from_le_bytes([self.inbox[6], self.inbox[7], self.inbox[8], self.inbox[9]]);
            let command = self.inbox[10];
            if length > BODY_MAX {
                // Not a header we wrote down correctly, or a client that lost its place: start over at the next
                // magic rather than wait for a megabyte that is not coming.
                self.inbox.drain(..1);
                continue;
            }
            let total = REQUEST_HEADER + length as usize;
            if self.inbox.len() < total {
                return None;
            }
            let body = self.inbox[REQUEST_HEADER..total].to_vec();
            self.inbox.drain(..total);
            return Some((request_id, api, command, body));
        }
    }

    fn dispatch(
        &mut self,
        machine: &mut Machine,
        state: &mut monitor::State,
        id: u32,
        api: u8,
        command: u8,
        body: &[u8],
    ) {
        // monitor_binary.c:1768 — the version is checked before anything else is read.
        if !(0x01..=API_VERSION).contains(&api) {
            self.error(id, err::CMD_INVALID_API_VERSION);
            return;
        }
        match command {
            cmd::PING => self.send(id, resp::PING, err::OK, &[]),
            cmd::VICE_INFO => {
                let mut out = vec![4];
                out.extend_from_slice(&VICE_VERSION);
                out.extend_from_slice(&[4, 0, 0, 0, 0]);
                self.send(id, resp::VICE_INFO, err::OK, &out);
            }
            cmd::BANKS_AVAILABLE => self.banks(id),
            cmd::REGISTERS_AVAILABLE => match memspace(body.first().copied().unwrap_or(0)) {
                Some(_) => self.registers_available(id),
                None => self.error(id, err::INVALID_MEMSPACE),
            },
            cmd::REGISTERS_GET => self.registers_get(machine, state, id, body),
            cmd::REGISTERS_SET => self.registers_set(machine, state, id, body),
            cmd::MEM_GET => self.mem_get(machine, state, id, body),
            cmd::MEM_SET => self.mem_set(machine, state, id, body),
            cmd::CHECKPOINT_GET => self.checkpoint_get(id, body),
            cmd::CHECKPOINT_SET => self.checkpoint_set(id, body),
            cmd::CHECKPOINT_DELETE => self.checkpoint_delete(id, body),
            cmd::CHECKPOINT_LIST => self.checkpoint_list(id),
            cmd::CHECKPOINT_TOGGLE => self.checkpoint_toggle(id, body),
            cmd::ADVANCE_INSTRUCTIONS => self.advance(machine, state, id, body),
            cmd::EXECUTE_UNTIL_RETURN => self.until_return(machine, state, id),
            cmd::KEYBOARD_FEED => self.keyboard(machine, id, body),
            cmd::RESET => self.reset(machine, state, id, body),
            cmd::EXIT => {
                monitor::halt_c64(machine, state, false);
                self.send(id, resp::EXIT, err::OK, &[]);
                self.announce_resume(machine, state);
            }
            // S23 §8: a debugger does not get to end the emulator. We detach instead and say so.
            cmd::QUIT => {
                monitor::halt_c64(machine, state, false);
                self.error(id, err::CMD_FAILURE);
                self.drop_client("asked the emulator to quit, which this server does not do");
            }
            _ => self.error(id, err::CMD_INVALID_TYPE),
        }
    }

    // ── the C64's registers ──────────────────────────────────────────────────────────

    fn registers_available(&mut self, id: u32) {
        let mut out = (REGISTERS.len() as u16).to_le_bytes().to_vec();
        for (reg, bits, name) in REGISTERS {
            out.push(name.len() as u8 + 3);
            out.push(reg);
            out.push(bits);
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        self.send(id, resp::REGISTERS_AVAILABLE, err::OK, &out);
    }

    fn registers_get(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        let Some(&space) = body.first() else {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        };
        if memspace(space).is_none() {
            self.error(id, err::INVALID_MEMSPACE);
            return;
        }
        let values = monitor::c64_registers(machine, state);
        self.send(id, resp::REGISTER_INFO, err::OK, &register_body(&values));
    }

    fn registers_set(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        if body.len() < 3 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        if memspace(body[0]).is_none() {
            self.error(id, err::INVALID_MEMSPACE);
            return;
        }
        let count = u16::from_le_bytes([body[1], body[2]]) as usize;
        if body.len() < 3 + count * 4 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        for item in body[3..].chunks_exact(4).take(count) {
            let (reg, value) = (item[1], u16::from_le_bytes([item[2], item[3]]));
            monitor::set_c64_register(machine, state, reg, value);
        }
        let values = monitor::c64_registers(machine, state);
        self.send(id, resp::REGISTER_INFO, err::OK, &register_body(&values));
    }

    // ── memory ───────────────────────────────────────────────────────────────────────

    fn mem_get(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        if body.len() < 8 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        let (start, end) = (u16::from_le_bytes([body[1], body[2]]), u16::from_le_bytes([body[3], body[4]]));
        if start > end {
            self.error(id, err::INVALID_PARAMETER);
            return;
        }
        if memspace(body[5]).is_none() {
            self.error(id, err::INVALID_MEMSPACE);
            return;
        }
        let Some(lens) = bank_lens(u16::from_le_bytes([body[6], body[7]])) else {
            self.error(id, err::INVALID_PARAMETER);
            return;
        };
        let length = end as usize - start as usize + 1;
        let mut out = (length as u16).to_le_bytes().to_vec();
        out.extend(monitor::c64_read(machine, state, start, length, lens));
        self.send(id, resp::MEM_GET, err::OK, &out);
    }

    fn mem_set(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        if body.len() < 8 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        let (start, end) = (u16::from_le_bytes([body[1], body[2]]), u16::from_le_bytes([body[3], body[4]]));
        if start > end {
            self.error(id, err::INVALID_PARAMETER);
            return;
        }
        if memspace(body[5]).is_none() {
            self.error(id, err::INVALID_MEMSPACE);
            return;
        }
        let Some(lens) = bank_lens(u16::from_le_bytes([body[6], body[7]])) else {
            self.error(id, err::INVALID_PARAMETER);
            return;
        };
        let length = end as usize - start as usize + 1;
        if body.len() < 8 + length {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        monitor::c64_write(machine, state, start, &body[8..8 + length], lens);
        self.send(id, resp::MEM_SET, err::OK, &[]);
    }

    // ── checkpoints ──────────────────────────────────────────────────────────────────

    fn checkpoint_get(&mut self, id: u32, body: &[u8]) {
        let Some(number) = le32(body) else {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        };
        match self.checkpoints.iter().find(|c| c.number == number) {
            Some(c) => {
                let body = checkpoint_body(c, false);
                self.send(id, resp::CHECKPOINT_INFO, err::OK, &body);
            }
            None => self.error(id, err::OBJECT_MISSING),
        }
    }

    fn checkpoint_set(&mut self, id: u32, body: &[u8]) {
        if body.len() < 8 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        if body.len() >= 9 && memspace(body[8]).is_none() {
            self.error(id, err::INVALID_MEMSPACE);
            return;
        }
        let checkpoint = Checkpoint {
            number: self.next_number,
            start: u16::from_le_bytes([body[0], body[1]]),
            end: u16::from_le_bytes([body[2], body[3]]),
            stop_when_hit: body[4] != 0,
            enabled: body[5] != 0,
            operation: body[6],
            temporary: body[7] != 0,
            hit_count: 0,
            ignore_count: 0,
        };
        self.next_number += 1;
        self.checkpoints.push(checkpoint.clone());
        self.send(id, resp::CHECKPOINT_INFO, err::OK, &checkpoint_body(&checkpoint, false));
        self.armed = false;
    }

    fn checkpoint_delete(&mut self, id: u32, body: &[u8]) {
        let Some(number) = le32(body) else {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        };
        match self.checkpoints.iter().position(|c| c.number == number) {
            Some(index) => {
                self.checkpoints.remove(index);
                self.send(id, resp::CHECKPOINT_DELETE, err::OK, &[]);
                self.armed = false;
            }
            None => self.error(id, err::OBJECT_MISSING),
        }
    }

    /// Every checkpoint as its own `CHECKPOINT_INFO`, then the count — VICE's own shape.
    fn checkpoint_list(&mut self, id: u32) {
        for checkpoint in self.checkpoints.clone() {
            self.send(id, resp::CHECKPOINT_INFO, err::OK, &checkpoint_body(&checkpoint, false));
        }
        let count = (self.checkpoints.len() as u32).to_le_bytes();
        self.send(id, resp::CHECKPOINT_LIST, err::OK, &count);
    }

    fn checkpoint_toggle(&mut self, id: u32, body: &[u8]) {
        if body.len() < 5 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        let number = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        match self.checkpoints.iter_mut().find(|c| c.number == number) {
            Some(checkpoint) => {
                checkpoint.enabled = body[4] != 0;
                self.send(id, resp::CHECKPOINT_TOGGLE, err::OK, &[]);
                self.armed = false;
            }
            None => self.error(id, err::OBJECT_MISSING),
        }
    }

    // ── run control ──────────────────────────────────────────────────────────────────

    fn advance(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        if body.len() < 3 {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        let over = body[0] != 0;
        let count = u16::from_le_bytes([body[1], body[2]]);
        match monitor::step_c64(machine, state, u64::from(count.max(1)), over) {
            Ok(()) => {
                self.send(id, resp::ADVANCE_INSTRUCTIONS, err::OK, &[]);
                self.announce_stop(machine, state);
            }
            Err(_) => self.error(id, err::CMD_FAILURE),
        }
    }

    fn until_return(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32) {
        match monitor::c64_until_return(machine, state) {
            Ok(()) => {
                self.send(id, resp::EXECUTE_UNTIL_RETURN, err::OK, &[]);
                self.announce_stop(machine, state);
            }
            Err(_) => self.error(id, err::CMD_FAILURE),
        }
    }

    fn keyboard(&mut self, machine: &mut Machine, id: u32, body: &[u8]) {
        let Some(&length) = body.first() else {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        };
        if body.len() < 1 + length as usize {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        let text: String = body[1..1 + length as usize].iter().map(|&b| char::from(b)).collect();
        monitor::feed_keyboard(machine, &text);
        self.send(id, resp::KEYBOARD_FEED, err::OK, &[]);
    }

    /// Reset goes through the firmware, as everything that changes the machine does (S23 §3).
    fn reset(&mut self, machine: &mut Machine, state: &mut monitor::State, id: u32, body: &[u8]) {
        if body.is_empty() {
            self.error(id, err::CMD_INVALID_LENGTH);
            return;
        }
        match monitor::reset_c64(machine, state, body[0] != 0) {
            Ok(()) => self.send(id, resp::RESET, err::OK, &[]),
            Err(_) => self.error(id, err::CMD_FAILURE),
        }
    }

    // ── events ───────────────────────────────────────────────────────────────────────

    /// The first-contact order clients wait for: `REGISTER_INFO`, then `STOPPED` (monitor_binary.c:489-493).
    fn announce_stop(&mut self, machine: &mut Machine, state: &mut monitor::State) {
        let values = monitor::c64_registers(machine, state);
        let pc = values.iter().find(|(reg, _)| *reg == 0x03).map_or(0, |(_, v)| *v);
        self.send(EVENT_ID, resp::REGISTER_INFO, err::OK, &register_body(&values));
        self.send(EVENT_ID, resp::STOPPED, err::OK, &pc.to_le_bytes());
        self.announced_stop = true;
    }

    fn announce_resume(&mut self, machine: &mut Machine, state: &mut monitor::State) {
        if !self.announced_stop {
            return;
        }
        let values = monitor::c64_registers(machine, state);
        let pc = values.iter().find(|(reg, _)| *reg == 0x03).map_or(0, |(_, v)| *v);
        self.send(EVENT_ID, resp::RESUMED, err::OK, &pc.to_le_bytes());
        self.announced_stop = false;
    }

    // ── the wire ─────────────────────────────────────────────────────────────────────

    fn error(&mut self, id: u32, code: u8) {
        self.send(id, 0, code, &[]);
    }

    fn send(&mut self, id: u32, response: u8, error: u8, body: &[u8]) {
        let Some(stream) = self.client.as_mut() else { return };
        let mut out = Vec::with_capacity(12 + body.len());
        out.push(STX);
        out.push(API_VERSION);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.push(response);
        out.push(error);
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(body);
        if let Err(e) = stream.write_all(&out) {
            eprintln!("vice monitor: write failed: {e}");
            self.client = None;
            self.inbox.clear();
        }
    }

    fn drop_client(&mut self, why: &str) {
        eprintln!("vice monitor: client detached: {why}");
        self.client = None;
        self.inbox.clear();
        self.announced_stop = false;
    }
}

/// `monitor_binary_response_checkpoint_info` (monitor_binary.c:508-531): 23 bytes.
fn checkpoint_body(checkpoint: &Checkpoint, hit: bool) -> Vec<u8> {
    let mut out = checkpoint.number.to_le_bytes().to_vec();
    out.push(u8::from(hit));
    out.extend_from_slice(&checkpoint.start.to_le_bytes());
    out.extend_from_slice(&checkpoint.end.to_le_bytes());
    out.push(u8::from(checkpoint.stop_when_hit));
    out.push(u8::from(checkpoint.enabled));
    out.push(checkpoint.operation);
    out.push(u8::from(checkpoint.temporary));
    out.extend_from_slice(&checkpoint.hit_count.to_le_bytes());
    out.extend_from_slice(&checkpoint.ignore_count.to_le_bytes());
    // No condition: `CONDITION_SET` is refused (S23 §8), so this is always 0, and memspace 0 is the computer.
    out.push(0);
    out.push(0);
    out
}

/// `write_registers` (monitor_binary.c:445-463): count, then item size 3, id, value.
fn register_body(values: &[(u8, u16)]) -> Vec<u8> {
    let mut out = (values.len() as u16).to_le_bytes().to_vec();
    for (reg, value) in values {
        out.push(3);
        out.push(*reg);
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

impl ViceServer {
    fn banks(&mut self, id: u32) {
        let mut out = (BANKS.len() as u16).to_le_bytes().to_vec();
        for (number, name, _) in BANKS {
            out.push(name.len() as u8 + 3);
            out.extend_from_slice(&number.to_le_bytes());
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        self.send(id, resp::BANKS_AVAILABLE, err::OK, &out);
    }
}

/// `get_requested_memspace` (monitor_binary.c:399-413). We have the computer and drive 8; 2-4 are drives this
/// machine does not have, and a client is told so rather than lied to.
fn memspace(byte: u8) -> Option<u8> {
    matches!(byte, 0 | 1).then_some(byte)
}

/// The bank number a client asked for, as one of our lens names.
fn bank_lens(number: u16) -> Option<&'static str> {
    BANKS.iter().find(|(id, _, _)| *id == number).map(|(_, _, lens)| *lens)
}

fn le32(body: &[u8]) -> Option<u32> {
    (body.len() >= 4).then(|| u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S23 §8: the layouts are VICE's, byte for byte, because a client counts them.
    #[test]
    fn the_bodies_have_vices_own_shape() {
        let checkpoint = Checkpoint {
            number: 1,
            start: 0xc000,
            end: 0xc0ff,
            stop_when_hit: true,
            enabled: true,
            operation: 4,
            temporary: false,
            hit_count: 7,
            ignore_count: 0,
        };
        let body = checkpoint_body(&checkpoint, true);
        assert_eq!(body.len(), 23, "monitor_binary.c:509 declares 23 bytes");
        assert_eq!(&body[0..4], &1u32.to_le_bytes(), "the number");
        assert_eq!(body[4], 1, "hit");
        assert_eq!(&body[5..7], &0xc000u16.to_le_bytes());
        assert_eq!(&body[7..9], &0xc0ffu16.to_le_bytes());
        assert_eq!((body[9], body[10], body[11], body[12]), (1, 1, 4, 0));
        assert_eq!(&body[13..17], &7u32.to_le_bytes(), "hit count");
        assert_eq!((body[21], body[22]), (0, 0), "no condition, memspace 0");

        let registers = register_body(&[(0x03, 0xe5cd), (0x00, 0x12)]);
        assert_eq!(registers.len(), 2 + 2 * 4, "count plus one item of size 3 each, with its length byte");
        assert_eq!(&registers[0..2], &2u16.to_le_bytes());
        assert_eq!(&registers[2..6], &[3, 0x03, 0xcd, 0xe5], "item size, id, value little-endian");
    }

    /// The banks are the C64's own names (c64mem.c:1239-1250): a client looks them up by name.
    #[test]
    fn the_banks_are_the_c64s() {
        assert_eq!(bank_lens(0), Some("cpu"), "default and cpu are both bank 0");
        assert_eq!(bank_lens(1), Some("ram"));
        assert_eq!(bank_lens(4), Some("cart"));
        assert_eq!(bank_lens(9), None);
        assert_eq!(memspace(0), Some(0), "the computer");
        assert_eq!(memspace(1), Some(1), "drive 8");
        assert_eq!(memspace(2), None, "this machine has no drive 9");
    }

    /// The framing: a header that lies is not followed, and a stray byte does not desync the stream.
    #[test]
    fn the_framing_resynchronises_instead_of_desyncing() {
        let mut server = ViceServer {
            listener: TcpListener::bind("127.0.0.1:0").expect("a port"),
            client: None,
            inbox: Vec::new(),
            checkpoints: Vec::new(),
            next_number: 1,
            announced_stop: false,
            armed: false,
        };
        let ping = |id: u32| {
            let mut out = vec![STX, API_VERSION];
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&id.to_le_bytes());
            out.push(cmd::PING);
            out
        };

        server.inbox = ping(7);
        assert_eq!(server.take_request().map(|r| (r.0, r.2)), Some((7, cmd::PING)));
        assert!(server.inbox.is_empty(), "a whole request is consumed");

        // Noise before the magic, and a second request behind the first.
        server.inbox = vec![0xff, 0x00];
        server.inbox.extend(ping(8));
        server.inbox.extend(ping(9));
        assert_eq!(server.take_request().map(|r| r.0), Some(8), "the noise is dropped, not the request");
        assert_eq!(server.take_request().map(|r| r.0), Some(9));
        assert_eq!(server.take_request().map(|r| r.0), None);

        // A body that has not arrived yet waits; it does not consume the header.
        let mut partial = ping(10);
        partial[2] = 4;
        server.inbox = partial.clone();
        assert!(server.take_request().is_none(), "the four body bytes are still missing");
        server.inbox.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(server.take_request().map(|r| (r.0, r.3.len())), Some((10, 4)));

        // A length nobody could mean is not waited for.
        let mut huge = ping(11);
        huge[2..6].copy_from_slice(&(BODY_MAX + 1).to_le_bytes());
        server.inbox = huge;
        server.inbox.extend(ping(12));
        assert_eq!(server.take_request().map(|r| r.0), Some(12), "it resynchronises on the next request");
    }
}
