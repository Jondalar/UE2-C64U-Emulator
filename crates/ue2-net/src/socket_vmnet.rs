//! Client for lima's `socket_vmnet` daemon (github.com/lima-vm/socket_vmnet). The daemon runs as root, owns a
//! vmnet.framework interface (shared or bridged) and serves it on a unix stream socket, so the emulator itself needs
//! no privileges.
//!
//! Wire format, both directions: every Ethernet frame (no FCS) is preceded by its length as a 4-byte big-endian
//! integer (socket_vmnet main.c: `ntohl` on the client header, `htonl` + `writev` towards clients; QEMU
//! `-netdev stream` speaks the same). The daemon floods frames between the vmnet interface and all its clients, so
//! several emulators can share one daemon as long as their MACs differ.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ue2_core::host::NetBackend;

/// Socket of the Homebrew `socket_vmnet` setup (`${HOMEBREW_PREFIX}/var/run/socket_vmnet`, socket_vmnet README).
pub use crate::DEFAULT_SOCKET_VMNET as DEFAULT_SOCKET;

/// Length prefix of every frame.
const HEADER_LEN: usize = 4;
/// Largest length accepted from the daemon. vmnet frames stay far below it (`vmnet_max_packet_size_key`, 1514 bytes
/// at MTU 1500); a larger header means the byte stream lost its framing.
const MAX_FRAME: usize = 0xFFFF;
/// Encoded guest frames kept while the socket is full; frames beyond it are dropped, like on a congested wire.
const TX_BACKLOG_LIMIT: usize = 1 << 20;
/// Bytes taken from the socket per `read`.
const READ_CHUNK: usize = 16 * 1024;

/// A connection to the daemon. Non-blocking: [`NetBackend::poll`] reads what has arrived and writes what the
/// socket accepts. When the connection fails, the failure is printed once and the backend drops all traffic.
pub struct SocketVmnet {
    stream: UnixStream,
    path: PathBuf,
    /// Received bytes that do not form a complete frame yet.
    rx: Vec<u8>,
    /// Encoded frames the socket has not accepted yet.
    tx: Vec<u8>,
    dead: bool,
}

impl SocketVmnet {
    /// Connect to the daemon socket at `path`. The error names the likely cause: no daemon installed or started,
    /// a stale socket, or a socket this user may not open.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).map_err(|e| {
            let shown = path.display();
            match e.kind() {
                ErrorKind::NotFound => anyhow!(
                    "no socket_vmnet socket at {shown}: install and start the daemon (brew install socket_vmnet; \
                     docs/status/network.md), or pass its socket as --net socket-vmnet:PATH"
                ),
                ErrorKind::ConnectionRefused => {
                    anyhow!("nothing listens on {shown}: the socket_vmnet daemon is not running (stale socket)")
                }
                ErrorKind::PermissionDenied => anyhow!(
                    "permission denied on {shown}: the socket_vmnet socket must be accessible to this user \
                     (socket_vmnet --socket-group)"
                ),
                _ => anyhow!("connecting to socket_vmnet at {shown}: {e}"),
            }
        })?;
        stream.set_nonblocking(true).context("socket_vmnet: non-blocking mode")?;
        Ok(SocketVmnet { stream, path: path.to_owned(), rx: Vec::new(), tx: Vec::new(), dead: false })
    }

    /// Write as much of the backlog as the socket takes.
    fn flush(&mut self) {
        while !self.tx.is_empty() && !self.dead {
            match self.stream.write(&self.tx) {
                Ok(n) => drop(self.tx.drain(..n)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => self.fail(&e.to_string()),
            }
        }
    }

    fn fail(&mut self, why: &str) {
        let path = self.path.display();
        eprintln!("net: socket_vmnet {path}: {why}; the network stays down until the emulator restarts");
        self.dead = true;
        self.rx.clear();
        self.tx.clear();
    }
}

impl NetBackend for SocketVmnet {
    fn send(&mut self, frame: &[u8]) {
        if self.dead || self.tx.len() + HEADER_LEN + frame.len() > TX_BACKLOG_LIMIT {
            return;
        }
        encode(frame, &mut self.tx);
        self.flush();
    }

    fn poll(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        self.flush();
        let mut chunk = [0; READ_CHUNK];
        while !self.dead {
            match self.stream.read(&mut chunk) {
                Ok(0) => self.fail("the daemon closed the connection"),
                Ok(n) => {
                    self.rx.extend_from_slice(&chunk[..n]);
                    if let Err(len) = decode(&mut self.rx, deliver) {
                        self.fail(&format!("frame length {len} exceeds {MAX_FRAME}: the stream lost its framing"));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => self.fail(&e.to_string()),
            }
        }
    }
}

/// Append `frame` with its length prefix.
fn encode(frame: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    out.extend_from_slice(frame);
}

/// Hand every complete frame at the front of `buf` to `deliver` and remove it; a partial frame stays. Fails with the
/// length of a header above [`MAX_FRAME`].
fn decode(buf: &mut Vec<u8>, deliver: &mut dyn FnMut(&[u8])) -> Result<(), usize> {
    let mut pos = 0;
    let result = loop {
        let Some(header) = buf.get(pos..pos + HEADER_LEN) else { break Ok(()) };
        let len = u32::from_be_bytes(header.try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            break Err(len);
        }
        let Some(frame) = buf.get(pos + HEADER_LEN..pos + HEADER_LEN + len) else { break Ok(()) };
        deliver(frame);
        pos += HEADER_LEN + len;
    };
    buf.drain(..pos);
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// A socket path of its own per test, short enough for `sun_path` (104 bytes on macOS).
    fn socket_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("ue2-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Poll until `count` frames arrived, the backend failed, or two seconds passed.
    fn collect(net: &mut SocketVmnet, count: usize) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let start = Instant::now();
        while frames.len() < count && !net.dead && start.elapsed() < Duration::from_secs(2) {
            net.poll(&mut |f| frames.push(f.to_vec()));
            thread::sleep(Duration::from_millis(2));
        }
        frames
    }

    fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut header = [0; HEADER_LEN];
        stream.read_exact(&mut header).unwrap();
        let mut frame = vec![0; u32::from_be_bytes(header) as usize];
        stream.read_exact(&mut frame).unwrap();
        frame
    }

    #[test]
    fn decode_reassembles_frames_across_any_split() {
        let frames: [&[u8]; 3] = [&[0xAA; 60], &[], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]];
        let mut wire = Vec::new();
        for f in frames {
            encode(f, &mut wire);
        }
        assert_eq!(wire[..HEADER_LEN], [0, 0, 0, 60], "big-endian length");
        for split in 0..=wire.len() {
            let mut buf = Vec::new();
            let mut got = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                buf.extend_from_slice(part);
                decode(&mut buf, &mut |f| got.push(f.to_vec())).unwrap();
            }
            assert_eq!(got, frames, "split at {split}");
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn decode_rejects_a_length_beyond_any_frame() {
        let mut buf = vec![0, 1, 0, 0, 0xEE];
        assert_eq!(decode(&mut buf, &mut |_| panic!("no frame")), Err(0x1_0000));
    }

    /// Mock daemon: takes one guest frame, then answers with two frames in one write and a third split across writes.
    #[test]
    fn exchanges_frames_with_a_mock_daemon() {
        let path = socket_path("mock");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let guest = read_frame(&mut conn);
            let mut burst = Vec::new();
            encode(&[0xFF; 42], &mut burst);
            encode(&guest, &mut burst);
            conn.write_all(&burst).unwrap();
            let mut last = Vec::new();
            encode(&[7; 1514], &mut last);
            conn.write_all(&last[..100]).unwrap();
            thread::sleep(Duration::from_millis(20));
            conn.write_all(&last[100..]).unwrap();
            guest
        });
        let mut net = SocketVmnet::connect(&path).unwrap();
        let frame: Vec<u8> = (0..64).collect();
        net.send(&frame);
        let frames = collect(&mut net, 3);
        assert_eq!(daemon.join().unwrap(), frame, "the daemon got the guest frame");
        assert_eq!(frames, [vec![0xFF; 42], frame, vec![7; 1514]]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_closed_connection_stops_the_backend() {
        let path = socket_path("closed");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = thread::spawn(move || drop(listener.accept().unwrap()));
        let mut net = SocketVmnet::connect(&path).unwrap();
        daemon.join().unwrap();
        assert!(collect(&mut net, 1).is_empty());
        assert!(net.dead);
        net.send(&[0; 60]);
        assert!(net.tx.is_empty(), "a dead backend drops guest frames");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn connect_errors_name_the_cause() {
        let missing = socket_path("missing");
        let err = SocketVmnet::connect(&missing).err().unwrap().to_string();
        assert!(err.contains("no socket_vmnet socket at") && err.contains("brew install socket_vmnet"), "{err}");

        let stale = socket_path("stale");
        drop(UnixListener::bind(&stale).unwrap());
        let err = SocketVmnet::connect(&stale).err().unwrap().to_string();
        assert!(err.contains("the socket_vmnet daemon is not running"), "{err}");
        std::fs::remove_file(&stale).unwrap();
    }
}
