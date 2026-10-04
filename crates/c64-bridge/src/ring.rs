//! S38: TRX64's own time-travel machinery on the C64 behind the firmware (docs/specs/S38-trx64-ring.md).
//!
//! Three rings, all TRX64's: the delta ring and the CPU history, which `Machine` feeds per retired instruction, and
//! the checkpoint ring, which the TRX64 daemon owns and feeds once per frame. Here the bridge owns the checkpoint ring
//! in the daemon's place, because the bridge is what advances the machine.
//!
//! The rings record only while the C64 runs at 1 MHz: never while turbo is engaged, while the 6510 is held (C64_STOP,
//! reset) or the machine pause stands (S37), or while the transport holds the machine on an anchor.
//!
//! A restore puts back the C64's own state from TRX64's anchor, and UE2's own part around the C64 from the anchor's
//! `ue2Hw` node and the write log (hwlog.rs): what on the U64 the FPGA holds for the C64 — the cartridge logic's
//! registers and memories, the REU's memory, the GMOD2 EEPROM, a physical cartridge, the UCI block when no command is
//! in flight. What the firmware owns stays live: the drives and the IEC devices, the turbo enable word and menu speed,
//! the model, whether a REU is fitted and its size, the bus sharing, the sampler and the mixer.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint_with, checkpoint_model, restore_runtime_checkpoint, CaptureOpts};
use trx64_core::checkpoint_ring::{
    checkpoint_ring_budget_for, checkpoint_ring_max_entries, RuntimeCheckpointRef, RuntimeCheckpointRing,
    DEFAULT_CHECKPOINT_RING_SECONDS,
};
use trx64_core::cpu_history::DEFAULT_CAPACITY;
use trx64_core::delta_ring::{DeltaRing, DEFAULT_REVERSE_SECONDS};
use trx64_core::ring_dump::{read_ring_dump, write_ring_dump, RingBufferDump, RingDumpInfo};
use trx64_core::{CpuHistoryRing, Machine, Uci, UciEvents, UciStatus};

use crate::cart::CartRegs;
use crate::clock::Clock;
use crate::hwlog::{Mark, Poke, Space};
use crate::slot::PhysNode;
use crate::Trx64Backend;

/// The daemon's capture cadence: one anchor per frame (TRX64 Spec 808, `checkpoint_capture_every_frames`).
pub const DEFAULT_CADENCE_FRAMES: u64 = 1;

/// Checkpoint nodes that are the firmware's, not the C64's: taken from the live machine at a restore. The expansion
/// port's node is half and half (`restore_c64`).
const FIRMWARE_NODES: [&str; 5] = ["drivePart", "driveB", "iecDevices", "folders", "hostFdc"];

/// The key of UE2's node in an anchor (§4.1). TRX64's restore reads the keys it knows and leaves the rest, so a
/// `.c64rering` with the node still loads in TRX64.
pub const UE2_NODE: &str = "ue2Hw";

/// Captures between two sweeps of the side table (`Ring::sweep`).
const SWEEP_EVERY: u32 = 64;

/// UE2's node in an anchor: where the write log stood, the cartridge logic's registers and the physical cartridge.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Ue2Node {
    log: Mark,
    cart: CartRegs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    physical: Option<PhysNode>,
}

/// What an anchor holds besides its payload, in this session only: its log position, and the UCI block when it was
/// enabled. The block is TRX64's and has no serialized form, so it rides no file.
struct Side {
    id: String,
    seq: u64,
    uci: Option<Arc<Uci>>,
}

/// The UCI block's state as the firmware reads it and its RAM: two captures that agree share one copy.
type UciPrint = (UciStatus, Vec<u8>);

fn uci_print(u: &Uci) -> UciPrint {
    (u.status(), (0x800..0x1000).map(|off| u.fw_read(off)).collect())
}

/// No transaction anywhere in the block: the command state is idle, no handshake bit, no data valid, no event for the
/// firmware. The firmware's half then holds nothing either.
fn uci_idle(s: &UciStatus) -> bool {
    s.state == 0
        && !(s.new_command || s.data_accepted || s.abort || s.error || s.response_valid || s.status_valid)
        && !(s.freeze || s.trigger)
        && s.pending == UciEvents::default()
}

/// What the firmware set up in the block: its enable, place, bus id, routing and interrupt mask.
fn uci_setup(s: &UciStatus) -> (bool, u8, u8, bool, bool, u8) {
    (s.enabled, s.slot_base, s.bus_id, s.routed_io1, s.routed_io2, s.irq_mask)
}

/// What the transport is doing, in the daemon's words (trx64-daemon `transport::TransportMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Live,
    Cut,
    StepBack,
    StepFwd,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Live => "LIVE",
            Mode::Cut => "CUT",
            Mode::StepBack => "STEP \u{25c0}",
            Mode::StepFwd => "STEP \u{25b6}",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Mode::Live | Mode::Cut => "\u{25b6}",
            Mode::StepBack => "\u{25c0}|",
            Mode::StepFwd => "|\u{25b6}",
        }
    }
}

/// The checkpoint ring and the transport over it.
pub struct Ring {
    pub anchors: RuntimeCheckpointRing,
    /// Frames between two anchors.
    pub cadence_frames: u64,
    /// How far back the ring reaches, in seconds; with the cadence it sets the entry cap.
    pub window_seconds: f64,
    /// Frame boundaries recorded since the last anchor.
    frames_since: u64,
    /// The anchor the machine stands on; `None` is the head (the machine runs).
    pub cursor: Option<String>,
    pub mode: Mode,
    /// Anchors the last cut dropped.
    pub last_cut: u64,
    /// What the last restore could not put back (§4.1), for the monitor to say.
    pub notes: Vec<String>,
    /// The anchors' session-only part, in capture order; entries of anchors that are gone go at the next sweep.
    side: VecDeque<Side>,
    since_sweep: u32,
    last_uci: Option<(UciPrint, Arc<Uci>)>,
}

impl Ring {
    fn new(nominal_fps: u32) -> Ring {
        let window_seconds = DEFAULT_CHECKPOINT_RING_SECONDS;
        let max = checkpoint_ring_max_entries(window_seconds, DEFAULT_CADENCE_FRAMES, nominal_fps);
        Ring {
            anchors: RuntimeCheckpointRing::with_budget_and_max_entries(checkpoint_ring_budget_for(max), max),
            cadence_frames: DEFAULT_CADENCE_FRAMES,
            window_seconds,
            frames_since: 0,
            cursor: None,
            mode: Mode::Live,
            last_cut: 0,
            notes: Vec::new(),
            side: VecDeque::new(),
            since_sweep: 0,
            last_uci: None,
        }
    }

    /// Drop the side entries of anchors that are gone; the oldest log position still wanted, or `None` with no
    /// anchor left.
    fn sweep(&mut self) -> Option<u64> {
        self.since_sweep = 0;
        let live: HashSet<String> = self.anchors.list().into_iter().map(|r| r.id).collect();
        self.side.retain(|s| live.contains(&s.id));
        self.side.iter().map(|s| s.seq).min()
    }

    fn side(&self, id: &str) -> Option<&Side> {
        self.side.iter().rev().find(|s| s.id == id)
    }

    /// The anchors, oldest first, as (id, frame, cycles).
    pub fn ids(&self) -> Vec<(String, u64, u64)> {
        self.anchors.list().into_iter().map(|r| (r.id, r.frame, r.cycles)).collect()
    }

    /// Where the cursor is: (index, total), the head when there is no cursor. `None` with no anchors, or when the
    /// cursor's anchor is gone.
    pub fn position(&self) -> Option<(usize, usize)> {
        let ids = self.ids();
        if ids.is_empty() {
            return None;
        }
        match &self.cursor {
            None => Some((ids.len() - 1, ids.len())),
            Some(id) => ids.iter().position(|(i, _, _)| i == id).map(|i| (i, ids.len())),
        }
    }

    /// Retune the entry cap for the cadence and window at `nominal_fps`; returns the cap.
    pub fn retune(&mut self, nominal_fps: u32) -> u64 {
        let want = checkpoint_ring_max_entries(self.window_seconds, self.cadence_frames, nominal_fps);
        self.anchors.set_max_entries(want)
    }
}

/// The capture the daemon's ring takes (trx64-daemon `capture_recorder_anchor_payload`): no framebuffer, no expansion
/// RAM. UE2 passes no disk or cartridge blobs: the drives are the firmware's, and the cartridge is UE2's own, carried
/// by the `ue2Hw` node `ring_frame` adds (§4.1). Drive B's node is left out of the stored anchor, since a restore takes
/// it from the live machine; it would otherwise store B's whole disk every frame.
fn anchor_payload(m: &Machine) -> serde_json::Value {
    let mut cp = capture_runtime_checkpoint_with(
        m,
        "",
        "",
        None,
        None,
        None,
        None,
        CaptureOpts { omit_framebuffer: true, omit_expansion_ram: true },
    );
    if let Some(tree) = cp.as_object_mut() {
        tree.remove("driveB");
    }
    cp
}

impl Trx64Backend {
    /// S38: TRX64's rings on (`--ring`, the default) or off. On: the delta ring at TRX64's default depth
    /// (`TRX64_REVERSE_SECONDS`, else 10 s), the CPU history at its default capacity and the checkpoint ring at the
    /// daemon's defaults (one anchor per frame, 60 s). Off: the rings shrink to one entry and record nothing.
    pub fn set_ring(&mut self, on: bool) {
        if on == self.ring.is_some() {
            return;
        }
        if on {
            let secs = crate::USER_REVERSE_SECONDS
                .get()
                .cloned()
                .flatten()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(DEFAULT_REVERSE_SECONDS)
                .max(1);
            let (entries, writes) = DeltaRing::caps_for_seconds(secs);
            self.m.delta_ring.resize(entries, writes);
            self.m.cpu_history.resize(DEFAULT_CAPACITY);
            self.ring = Some(Box::new(Ring::new(self.m.timing().nominal_fps)));
        } else {
            self.m.cpu_history = CpuHistoryRing::with_capacity(1);
            self.m.delta_ring = DeltaRing::with_capacity(1, 1);
            self.ring = None;
        }
        self.hwlog.clear();
        self.arm_rings();
    }

    /// UE2's write log (§4.1): the writes it holds and their bytes.
    pub fn ue2_log(&self) -> (usize, usize) {
        (self.hwlog.len(), self.hwlog.bytes())
    }

    /// The checkpoint ring and its transport, when the rings are on.
    pub fn ring(&self) -> Option<&Ring> {
        self.ring.as_deref()
    }

    pub fn ring_mut(&mut self) -> Option<&mut Ring> {
        self.ring.as_deref_mut()
    }

    /// Whether the rings record now: on, the 6510 running at 1 MHz, nothing holding the machine.
    pub fn ring_recording(&self) -> bool {
        self.ring.as_ref().is_some_and(|r| r.cursor.is_none())
            && !self.stopped
            && !self.reset_held
            && !self.paused
            && !self.m.turbo_engaged()
    }

    /// Whether the transport holds the C64 on an anchor: no C64 cycle runs until it lets go.
    pub fn ring_holds(&self) -> bool {
        self.ring.as_ref().is_some_and(|r| r.cursor.is_some())
    }

    /// Switch TRX64's per-instruction rings on or off for what the machine is doing now. With the rings off they are
    /// left as `new` built them.
    pub(crate) fn arm_rings(&mut self) {
        // UE2's write log takes every write while the rings are on, turbo and held 6510 included: going back to the
        // last anchor before them has to undo those too. Only while the transport holds the C64 on an anchor does it
        // stand (§4.1).
        self.hwlog.set_on(self.ring.as_ref().is_some_and(|r| r.cursor.is_none()));
        if self.ring.is_none() {
            return;
        }
        let on = self.ring_recording();
        self.m.delta_ring.set_enabled(on);
        self.m.cpu_history.set_enabled(on);
    }

    /// For a run out of band (the monitor's steps): the per-instruction rings record whatever the C64 is held by,
    /// unless they are off or turbo is engaged. The next slice arms them for the machine again.
    pub fn arm_rings_for_step(&mut self) {
        if self.ring.is_none() {
            return;
        }
        let on = !self.m.turbo_engaged();
        self.m.delta_ring.set_enabled(on);
        self.m.cpu_history.set_enabled(on);
    }

    /// Up to cycle `target` while recording, stopping at each frame boundary on the way: the checkpoint ring takes an
    /// anchor there every `cadence_frames` frames. The daemon's pump hands the machine one frame at a time and captures
    /// after it (`stream_maybe_autocapture`); here the slices are the firmware's, so the frame is found in them.
    pub(crate) fn run_recording(&mut self, target: u64) {
        loop {
            let clk = self.m.c64_core.clk;
            let boundary = clk + self.cycles_to_frame_end() + crate::BOUNDARY_SLACK;
            if boundary > target {
                break;
            }
            self.run_to(boundary);
            if self.m.c64_core.clk == clk {
                // The C64 waits for the firmware (UCI): nothing ran, nothing to record.
                return;
            }
            self.ring_frame();
            if !self.ring_recording() {
                // Turbo came on in this frame.
                self.arm_rings();
                break;
            }
        }
        // The last frame's instruction may have run past the slice's end; `run_to` wants a target ahead.
        if self.m.c64_core.clk < target {
            self.run_to(target);
        }
    }

    /// A frame boundary while recording: an anchor when the cadence is due, with UE2's node in it (§4.1). A capture
    /// error is a gap in the ring, as in the daemon.
    fn ring_frame(&mut self) {
        let (frame, clk) = (self.m.vic.frame, self.m.c64_core.clk);
        let Some(ring) = self.ring.as_deref_mut() else { return };
        ring.frames_since += 1;
        if ring.frames_since < ring.cadence_frames.max(1) {
            return;
        }
        ring.frames_since = 0;
        let mut payload = anchor_payload(&self.m);
        let node = self.ue2_node();
        let seq = node.log.seq;
        if let Ok(v) = serde_json::to_value(&node) {
            payload[UE2_NODE] = v;
        }
        let uci = self.m.uci().filter(|u| u.status().enabled).map(|u| {
            let print = uci_print(u);
            let ring = self.ring.as_deref_mut().expect("checked above");
            match &ring.last_uci {
                Some((p, arc)) if *p == print => arc.clone(),
                _ => {
                    let arc = Arc::new(u.clone());
                    ring.last_uci = Some((print, arc.clone()));
                    arc
                }
            }
        });
        let ring = self.ring.as_deref_mut().expect("checked above");
        if let Ok(r) = ring.anchors.capture(payload, frame, clk) {
            ring.side.push_back(Side { id: r.id, seq, uci });
        }
        ring.since_sweep += 1;
        if ring.since_sweep >= SWEEP_EVERY {
            let oldest = ring.sweep().unwrap_or(seq);
            self.hwlog.trim_to(oldest);
        }
        let ring = self.ring.as_deref_mut().expect("checked above");
        if ring.mode == Mode::Cut {
            ring.mode = Mode::Live;
        }
    }

    /// UE2's node for an anchor taken now. A physical cartridge's flash changes reach the log first, so the node's log
    /// position covers them.
    fn ue2_node(&mut self) -> Ue2Node {
        self.flush_physical();
        let physical = self.slot.with(|s| s.physical().map(|p| p.node()));
        Ue2Node { log: self.hwlog.mark(), cart: self.cart.with(|c| c.regs()), physical }
    }

    /// A physical cartridge's flash changes since the last look into the log (`PhysicalCart::log_image`).
    fn flush_physical(&mut self) {
        let (log, clk) = (self.hwlog.clone(), self.m.c64_core.clk);
        self.slot.with(|s| {
            if let Some(p) = s.physical_mut() {
                p.log_image(&log, clk);
            }
        });
    }

    /// A C64 reset replaces the machine: the anchors describe one that no longer exists (the daemon's
    /// `timeline_discard`), and the per-instruction history ends at the reset as TRX64's cold reset ends it.
    pub(crate) fn ring_discard(&mut self) {
        let Some(ring) = self.ring.as_deref_mut() else { return };
        ring.anchors.clear();
        ring.cursor = None;
        ring.mode = Mode::Live;
        ring.frames_since = 0;
        ring.side.clear();
        ring.last_uci = None;
        self.m.delta_ring.clear();
        self.m.cpu_history.clear();
        // UE2's write log describes the same machine: a new one.
        self.hwlog.clear();
        self.arm_rings();
    }

    /// Stand the C64 on anchor `id` (an id or a mark name): its own state from the anchor, the firmware's from the
    /// live machine (§4 of the spec). The transport then holds the machine there until [`Self::ring_release`].
    pub fn ring_goto(&mut self, id: &str, mode: Mode) -> Result<RuntimeCheckpointRef, String> {
        let ring = self.ring.as_deref().ok_or("the rings are off (--ring=off)")?;
        let anchor = ring.anchors.get(id).ok_or_else(|| format!("no anchor {id}"))?;
        let mut cp =
            ring.anchors.restore_snapshot(&anchor.id).ok_or_else(|| format!("anchor {} is gone (evicted)", anchor.id))?;
        let uci = ring.side(&anchor.id).and_then(|s| s.uci.clone());
        // Leaving the head: the log takes what a physical cartridge's flash did since the last anchor, then stands.
        if ring.cursor.is_none() {
            self.flush_physical();
        }
        self.hwlog.set_on(false);
        let restored = self.restore_c64(&mut cp, uci.as_deref());
        let ring = self.ring.as_deref_mut().expect("checked above");
        ring.notes = match restored {
            Ok(notes) => notes,
            Err(e) => {
                self.arm_rings();
                return Err(e);
            }
        };
        ring.cursor = Some(anchor.id.clone());
        ring.mode = mode;
        self.arm_rings();
        Ok(anchor)
    }

    /// Let go of the machine where the transport stands. The anchors after it are a future that will not happen now:
    /// they go, marks excepted (the daemon's `timeline_truncate`). Returns how many went.
    pub fn ring_release(&mut self) -> u64 {
        let Some(ring) = self.ring.as_deref_mut() else { return 0 };
        let Some(id) = ring.cursor.take() else { return 0 };
        let dropped = ring.anchors.truncate_after(&id, true);
        // UE2's memories stand where the anchor had them: the records after it go with the anchors.
        self.hwlog.cut();
        ring.mode = Mode::Cut;
        ring.last_cut = dropped;
        ring.frames_since = 0;
        // The C64 stood while the emulator's clock ran on: it continues from here, as after the machine pause.
        self.clock = Some(Clock::new(self.now, self.m.c64_core.clk, self.cpu_hz()));
        self.arm_rings();
        dropped
    }

    /// The whole reverse-debug buffer as a `.c64rering` (TRX64 `ring_dump`): the checkpoint ring with the newest anchor
    /// as current (the daemon's `ringbuffer_dump_to_path`, wherever the transport stands), the delta ring and the CPU
    /// history.
    pub fn ring_dump(&self) -> Result<(Vec<u8>, RingDumpInfo), String> {
        let ring = self.ring.as_deref().ok_or("the rings are off (--ring=off)")?;
        let current = ring.anchors.list().last().map(|r| r.id.clone());
        let dump = RingBufferDump {
            checkpoint_ring: ring.anchors.to_dump(current),
            delta_ring: self.m.delta_ring.to_dump(),
            cpu_history: self.m.cpu_history.to_dump(),
        };
        let bytes = write_ring_dump(&dump);
        let info = dump.info(bytes.len() as u64);
        Ok((bytes, info))
    }

    /// A `.c64rering` back: the three rings from the file, and the C64 on its current anchor, held there.
    pub fn ring_load(&mut self, bytes: &[u8]) -> Result<RingDumpInfo, String> {
        if self.ring.is_none() {
            return Err("the rings are off (--ring=off)".into());
        }
        let dump = read_ring_dump(bytes)?;
        let anchors = RuntimeCheckpointRing::from_dump(&dump.checkpoint_ring);
        let current = dump.checkpoint_ring.current_id.clone().or_else(|| anchors.list().last().map(|r| r.id.clone()));
        // Check the anchor restores before anything is replaced.
        if let Some(id) = &current {
            let cp = anchors.restore_snapshot(id).ok_or_else(|| format!("the file's current anchor {id} is not in it"))?;
            self.check_model(&cp)?;
        }
        // The file's anchors reach this session's write log only if they were taken from it (§9).
        let log_id = self.hwlog.mark().id;
        let side: VecDeque<Side> = dump
            .checkpoint_ring
            .entries
            .iter()
            .filter_map(|a| {
                let node: Ue2Node = serde_json::from_value(a.payload.get(UE2_NODE)?.clone()).ok()?;
                (node.log.id == log_id).then(|| Side { id: a.id.clone(), seq: node.log.seq, uci: None })
            })
            .collect();
        let ring = self.ring.as_deref_mut().expect("checked above");
        ring.anchors = anchors;
        ring.cursor = None;
        ring.mode = Mode::Live;
        ring.frames_since = 0;
        ring.side = side;
        ring.last_uci = None;
        self.m.delta_ring = DeltaRing::from_dump(&dump.delta_ring);
        self.m.cpu_history = CpuHistoryRing::from_dump(&dump.cpu_history);
        if let Some(id) = current {
            self.ring_goto(&id, Mode::StepBack)?;
        }
        Ok(dump.info(bytes.len() as u64))
    }

    /// The model the checkpoint was taken on must be the one the firmware runs: a restore would otherwise switch the
    /// C64 behind the firmware's back (C64_VIDEOFORMAT is the firmware's).
    fn check_model(&self, cp: &serde_json::Value) -> Result<(), String> {
        let live = self.m.model();
        match checkpoint_model(cp)? {
            Some(model) if !std::ptr::eq(model, live) => {
                Err(format!("the anchor is a {} and the firmware runs a {}", model.name, live.name))
            }
            _ => Ok(()),
        }
    }

    /// Restore the C64's own state from `cp`, UE2's part from its `ue2Hw` node and the write log, and keep the
    /// firmware's live. `uci` is the UCI block as the anchor had it, when this session kept it. What could not be put
    /// back comes back as notes.
    pub(crate) fn restore_c64(&mut self, cp: &mut serde_json::Value, uci: Option<&Uci>) -> Result<Vec<String>, String> {
        self.check_model(cp)?;
        let node = cp.get(UE2_NODE).cloned();
        // The firmware's half, from the live machine: the drives, the IEC devices, and the turbo enable word and menu
        // speed. `$D030`/`$D031` are the C64's and come from the anchor.
        let live = capture_runtime_checkpoint_with(
            &self.m,
            "",
            "",
            None,
            None,
            None,
            None,
            CaptureOpts { omit_framebuffer: true, omit_expansion_ram: true },
        );
        let tree = cp.as_object_mut().ok_or("the anchor is not a checkpoint")?;
        for key in FIRMWARE_NODES {
            match live.get(key) {
                Some(v) => tree.insert(key.to_string(), v.clone()),
                None => tree.remove(key),
            };
        }
        // The expansion port: the REU's registers are the C64's and come from the anchor; whether a REU is fitted and
        // its size are the firmware's (C64_REU_ENABLE, C64_REU_SIZE). With no REU on either side the live port stays as
        // it is. The REU's memory is the write log's.
        let live_exp = live.get("expansion").cloned().unwrap_or(serde_json::Value::Null);
        let kept = match tree.get_mut("expansion") {
            Some(a) if !a.is_null() && !live_exp.is_null() && a.get("kind") == live_exp.get("kind") => {
                a["sizeKb"] = live_exp["sizeKb"].clone();
                false
            }
            _ => true,
        };
        if kept {
            tree.insert("expansion".into(), live_exp);
        }
        if let (Some(t), Some(l)) = (tree.get_mut("turbo").and_then(|t| t.as_object_mut()), live.get("turbo")) {
            for key in ["enable", "menuIndex", "menuStalls"] {
                if let Some(v) = l.get(key) {
                    t.insert(key.to_string(), v.clone());
                }
            }
        }
        // TRX64's restore detaches a cartridge the checkpoint does not carry and resets the UCI block to power-on
        // (c64re_snapshot.rs `restore_runtime_checkpoint`); both are UE2's here and go back afterwards. So do the
        // drives, exactly: the injected nodes only keep TRX64's device map right.
        let live_clk = self.m.c64_core.clk;
        let frame = self.m.vic.frame;
        let cart = self.m.cartridge.take();
        let cart_image = self.m.cartridge_image.take();
        let live_uci = self.m.uci().cloned();
        let uci_busy = self.uci_wait.is_some();
        let (drive_a, drive_b) = (self.m.drive8.clone(), self.m.drive_b.clone());
        let restored = restore_runtime_checkpoint(&mut self.m, cp);
        self.m.drive8 = drive_a;
        self.m.drive_b = drive_b;
        self.m.cartridge = cart;
        self.m.cartridge_image = cart_image;
        let mut notes = Vec::new();
        if let Some(live) = live_uci {
            let block = match uci {
                Some(anchor) => {
                    let (a, l) = (anchor.status(), live.status());
                    if !uci_idle(&a) {
                        notes.push("UCI kept live: a command was in flight at the anchor".to_string());
                        live
                    } else if !uci_idle(&l) || uci_busy {
                        notes.push("UCI kept live: the firmware is in a command".to_string());
                        live
                    } else if uci_setup(&a) != uci_setup(&l) {
                        notes.push("UCI kept live: the firmware set the block up anew since the anchor".to_string());
                        live
                    } else {
                        anchor.clone()
                    }
                }
                None => live,
            };
            if let Some(b) = self.m.uci_mut() {
                *b = block;
            }
        }
        // The frame count only goes forward: the S37 still and the display take a larger count as a new picture.
        self.m.vic.frame = frame + 1;
        restored?;
        let clk = self.m.c64_core.clk;
        self.m.clk = clk;
        self.m.drive_c64_ref = clk;
        // The bridge's own view of the C64's clock and inputs, as after a reset release.
        self.clock = Some(Clock::new(self.now, clk, self.cpu_hz()));
        self.sid.reanchor(clk);
        // UE2's part (§4.1). A physical cartridge whose node came back has its epoch from the anchor; otherwise its
        // flash timers run on the emulator's time and carry the cycles the C64 went back.
        let node = node.and_then(|n| serde_json::from_value::<Ue2Node>(n).ok());
        let epoch_restored = self.restore_ue2(node.as_ref(), &mut notes);
        if !epoch_restored {
            let back = live_clk.saturating_sub(clk);
            self.slot.with(|s| s.reset_release(back));
        }
        self.keys.apply(&mut self.m.keyboard);
        self.apply_joysticks();
        self.apply_pots();
        self.nmi_line = self.nmi;
        self.uci_wait = None;
        self.checkpoint_hit = None;
        self.update_pla();
        self.apply_interrupts();
        Ok(notes)
    }

    /// The memories to the anchor's log position, then the cartridge logic's registers and the physical cartridge.
    /// Whether the physical cartridge's node (and with it its epoch) came back.
    fn restore_ue2(&mut self, node: Option<&Ue2Node>, notes: &mut Vec<String>) -> bool {
        let Some(n) = node else {
            notes.push(format!("the anchor has no {UE2_NODE} node: the cartridge, the REU's memory and the EEPROM stay live"));
            return false;
        };
        match self.hwlog.uncovered(&n.log) {
            Some(why) => notes.push(format!("UE2 memories (cartridge, REU, EEPROM) not put back: {why}")),
            None => match self.hwlog.seek(&n.log) {
                None => notes.push("UE2 memories not put back: no guest DDR is lent".into()),
                Some(pokes) => self.poke(&pokes),
            },
        }
        if let Err(e) = self.cart.with(|c| c.set_regs(&n.cart)) {
            notes.push(format!("cartridge registers kept live: {e}"));
        }
        let (log, clk) = (self.hwlog.clone(), self.m.c64_core.clk);
        self.slot.with(|s| match (s.physical_mut(), &n.physical) {
            (Some(p), Some(pn)) => match p.set_node(pn) {
                Ok(()) => {
                    // What the log just put back is what it sees now.
                    p.log_image(&log, clk);
                    true
                }
                Err(e) => {
                    notes.push(format!("expansion-port cartridge kept live: {e}"));
                    false
                }
            },
            (Some(_), None) => {
                notes.push("expansion-port cartridge kept live: the anchor has none".into());
                false
            }
            _ => false,
        })
    }

    /// The write log's bytes for the memories it does not write itself.
    fn poke(&mut self, pokes: &[Poke]) {
        let mut physical = Vec::new();
        for p in pokes {
            match p.space {
                Space::Eeprom => self.cart.with(|c| c.eeprom_poke(p.addr, p.value)),
                Space::PhysMem | Space::PhysImage => physical.push(*p),
                Space::Ddr => {}
            }
        }
        if !physical.is_empty() {
            self.slot.with(|s| {
                if let Some(c) = s.physical_mut() {
                    c.poke(&physical);
                }
            });
        }
    }
}

