//! Host network backends for the RMII MAC. Spec: docs/specs/S11-S14-later.md §S12; host setup:
//! docs/status/network.md.
//!
//! Every backend implements [`NetBackend`]; the emulation thread pumps it between run slices through
//! `Rmii::exchange`. None is `Send`: build a backend on the thread that pumps it.
//!
//! - [`UserNet`] (feature `slirp`) is libslirp user-mode networking: a virtual 10.0.2.0/24 segment where the host is
//!   10.0.2.2, DNS is 10.0.2.3 and DHCP hands the guest 10.0.2.15, plus port forwards from the Mac into the guest.
//!   libslirp is not thread-safe.
//! - [`socket_vmnet::SocketVmnet`] (Unix) connects to lima's socket_vmnet daemon, which owns a vmnet interface as root;
//!   the guest's DHCP goes to the daemon's network (the LAN when the daemon runs bridged).
//! - [`vmnet::VmnetBridged`] (macOS) bridges onto a host interface through vmnet.framework itself; needs root.
//! - [`web_proxy::WebProxy`] is the HTTP proxy [`UserNet::start_web_proxy`] puts in front of the firmware web
//!   server, so that the web UI's API calls carry the host port.

#[cfg(feature = "slirp")]
mod ffi;
#[cfg(unix)]
pub mod socket_vmnet;
#[cfg(feature = "slirp")]
mod user;
#[cfg(target_os = "macos")]
pub mod vmnet;
pub mod web_proxy;

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use anyhow::{bail, Result};

#[cfg(feature = "slirp")]
pub use user::UserNet;

/// Virtual network and the slirp-side addresses (QEMU user networking layout).
pub const NETWORK: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 0);
pub const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
pub const HOST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
/// First (and in practice only) DHCP lease: the address port forwards target.
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// Forwards used when `--net user` comes without `--hostfwd`: HTTP/REST, Telnet, FTP control and the
/// Ultimate DMA socket (docs/hw/08-network-rmii.md §Services). With the web UI proxy on (the default), the proxy
/// takes the place of the HTTP forward.
pub const DEFAULT_HOSTFWD: &str = "tcp:8080:80,tcp:2323:23,tcp:2121:21,tcp:6464:64";

/// Where the socket_vmnet daemon listens by default (Homebrew's service). `--net socket-vmnet` itself is Unix only.
pub const DEFAULT_SOCKET_VMNET: &str = "/opt/homebrew/var/run/socket_vmnet";

/// One port forward: connections to `host_addr:host_port` on the Mac reach `GUEST:guest_port`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFwd {
    pub udp: bool,
    pub host_addr: Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
}

impl HostFwd {
    /// A comma-separated list, e.g. [`DEFAULT_HOSTFWD`].
    pub fn parse_list(list: &str) -> Result<Vec<HostFwd>> {
        list.split(',').map(str::parse).collect()
    }
}

/// `PROTO:[ADDR:]HOSTPORT:GUESTPORT` with PROTO `tcp` or `udp`; ADDR defaults to 127.0.0.1.
impl FromStr for HostFwd {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        let (proto, addr, host, guest) = match parts[..] {
            [proto, host, guest] => (proto, None, host, guest),
            [proto, addr, host, guest] => (proto, Some(addr), host, guest),
            _ => bail!("hostfwd '{s}': expected PROTO:[ADDR:]HOSTPORT:GUESTPORT"),
        };
        let udp = match proto {
            "tcp" => false,
            "udp" => true,
            _ => bail!("hostfwd '{s}': protocol must be tcp or udp"),
        };
        let host_addr = match addr {
            Some(a) => a.parse().map_err(|e| anyhow::anyhow!("hostfwd '{s}': address '{a}': {e}"))?,
            None => Ipv4Addr::LOCALHOST,
        };
        let port = |p: &str| match p.parse::<u16>() {
            Ok(port) if port != 0 => Ok(port),
            _ => Err(anyhow::anyhow!("hostfwd '{s}': port '{p}' must be 1-65535")),
        };
        Ok(HostFwd { udp, host_addr, host_port: port(host)?, guest_port: port(guest)? })
    }
}

impl fmt::Display for HostFwd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proto = if self.udp { "udp" } else { "tcp" };
        write!(f, "{proto}:{}:{}:{}", self.host_addr, self.host_port, self.guest_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostfwd_syntax() {
        assert_eq!(
            HostFwd::parse_list(DEFAULT_HOSTFWD).unwrap().iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["tcp:127.0.0.1:8080:80", "tcp:127.0.0.1:2323:23", "tcp:127.0.0.1:2121:21", "tcp:127.0.0.1:6464:64"]
        );
        assert_eq!(
            "udp:0.0.0.0:6464:64".parse::<HostFwd>().unwrap(),
            HostFwd { udp: true, host_addr: Ipv4Addr::UNSPECIFIED, host_port: 6464, guest_port: 64 }
        );
        for bad in ["icmp:1:2", "tcp:80", "tcp:x:80", "tcp:8080:0", "tcp:1.2.3:1:2", "tcp:1:2:3:4:5"] {
            assert!(bad.parse::<HostFwd>().is_err(), "{bad}");
        }
    }
}
