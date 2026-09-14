//! A socket_vmnet stand-in for checking `ue2emu --net socket-vmnet:SOCKET` without root or the real daemon: it
//! speaks the socket_vmnet wire format (u32 big-endian length + frame) on a unix socket and puts the frames onto a
//! libslirp network, so the guest gets 10.0.2.15 by DHCP and the port forwards reach it. One client, then it exits.
//!
//! cargo run --release -p ue2-net --example mock_socket_vmnet -- SOCKET tcp:8080:80,tcp:2323:23

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ue2_core::host::NetBackend;
use ue2_net::{HostFwd, UserNet};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(forwards)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: mock_socket_vmnet SOCKET HOSTFWD[,HOSTFWD...]");
    };
    let mut net = UserNet::new(&HostFwd::parse_list(&forwards)?)?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {path}"))?;
    eprintln!("mock socket_vmnet: listening on {path}, forwards {forwards}");
    let (mut conn, _) = listener.accept()?;
    conn.set_nonblocking(true)?;
    eprintln!("mock socket_vmnet: client connected");
    let (mut rx, mut tx) = (Vec::new(), Vec::new());
    let (mut from_client, mut to_client) = (0u64, 0u64);
    let mut report = Instant::now();
    let mut chunk = [0; 16 * 1024];
    loop {
        match conn.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => rx.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        let mut pos = 0;
        while let Some(header) = rx.get(pos..pos + 4) {
            let len = u32::from_be_bytes(header.try_into()?) as usize;
            let Some(frame) = rx.get(pos + 4..pos + 4 + len) else { break };
            net.send(frame);
            from_client += 1;
            pos += 4 + len;
        }
        rx.drain(..pos);
        net.poll(&mut |frame| {
            tx.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            tx.extend_from_slice(frame);
            to_client += 1;
        });
        while !tx.is_empty() {
            match conn.write(&tx) {
                Ok(n) => drop(tx.drain(..n)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => return Err(e.into()),
            }
        }
        if report.elapsed() >= Duration::from_secs(10) {
            eprintln!("mock socket_vmnet: {from_client} frames from the client, {to_client} to it");
            report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!("mock socket_vmnet: client closed after {from_client} frames from it, {to_client} to it");
    Ok(())
}
