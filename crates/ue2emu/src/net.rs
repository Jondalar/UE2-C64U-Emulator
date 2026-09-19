//! `--net` / `--hostfwd` / `--web-port`: the wired Ethernet backend. Spec: docs/specs/S11-S14-later.md §S12; host
//! setup and status: docs/status/network.md.
//!
//! The backend is built and pumped on the emulation thread (libslirp is single-threaded; vmnet packets are read
//! there too). Pumping between run slices never blocks, so realtime pacing is unaffected; frames wait at most one
//! slice. The web UI proxy of `--net user` runs on its own threads (`ue2_net::web_proxy`).

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use clap::Args;
use ue2_core::devices::board::U2pio;
use ue2_core::devices::flash::SpiFlash;
use ue2_core::devices::rmii::{Rmii, CAPAB_ETH_RMII};
use ue2_core::host::NetBackend;
use ue2_core::machine::{Machine, MachineConfig};
use ue2_net::web_proxy::{DEFAULT_WEB_PORT, WEB_GUEST_PORT};
use ue2_net::{HostFwd, DEFAULT_HOSTFWD, DEFAULT_SOCKET_VMNET as DEFAULT_SOCKET};

/// `--net MODE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetMode {
    /// `user`: libslirp; the guest gets 10.0.2.15 by DHCP and is reached through `--hostfwd` and the web UI proxy.
    User,
    /// `vmnet-bridged[:IFACE]`: vmnet.framework bridged onto IFACE, by default the interface of the default route.
    VmnetBridged(Option<String>),
    /// `socket-vmnet[:PATH]`: the socket_vmnet daemon listening on PATH.
    SocketVmnet(PathBuf),
}

impl FromStr for NetMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (name, arg) = match s.split_once(':') {
            Some((name, arg)) => (name, Some(arg)),
            None => (s, None),
        };
        match (name, arg) {
            (_, Some("")) => bail!("--net {s}: empty argument after ':'"),
            ("user", None) => Ok(NetMode::User),
            ("vmnet-bridged", iface) => Ok(NetMode::VmnetBridged(iface.map(str::to_owned))),
            ("socket-vmnet", path) => Ok(NetMode::SocketVmnet(PathBuf::from(path.unwrap_or(DEFAULT_SOCKET)))),
            _ => bail!("--net {s}: expected user, vmnet-bridged[:IFACE] or socket-vmnet[:PATH]"),
        }
    }
}

#[derive(Args)]
pub struct NetArgs {
    /// Wired Ethernet backend: user (libslirp NAT), vmnet-bridged[:IFACE] (LAN address, needs sudo; IFACE defaults
    /// to the default route's interface), socket-vmnet[:PATH] (LAN address through the socket_vmnet daemon;
    /// PATH defaults to /opt/homebrew/var/run/socket_vmnet) [default: no network]
    #[arg(long, value_name = "MODE")]
    net: Option<NetMode>,
    /// Port forwards for --net user, PROTO:[ADDR:]HOSTPORT:GUESTPORT, comma separated; ADDR defaults to 127.0.0.1
    /// [default: tcp:2323:23,tcp:2121:21,tcp:6464:64, and guest port 80 through the web UI proxy on 8080]
    #[arg(long, value_delimiter = ',', requires = "net")]
    hostfwd: Vec<HostFwd>,
    /// Web UI proxy for --net user: HTTP on 127.0.0.1:PORT to the firmware web server (guest port 80), with the
    /// web UI's API URLs made to carry the port (docs/status/network.md). 0 turns it off; without --hostfwd, guest
    /// port 80 is then forwarded plainly from 8080 [default: 8080 without --hostfwd, off with --hostfwd]
    #[arg(long, value_name = "PORT", requires = "net")]
    web_port: Option<u16>,
}

/// What the emulation thread needs to start the backend.
#[derive(Clone, Debug)]
pub struct NetOptions {
    pub mode: NetMode,
    /// Port forwards of `--net user`.
    #[cfg_attr(not(feature = "net"), allow(dead_code))]
    pub hostfwd: Vec<HostFwd>,
    /// Host port of the web UI proxy of `--net user`; `None` runs none.
    #[cfg_attr(not(feature = "net"), allow(dead_code))]
    pub web_port: Option<u16>,
    /// Flash unique ID for the bridged modes, where the guest shares a LAN with other devices; `None` keeps the
    /// flash model's own.
    pub unique_id: Option<[u8; 8]>,
}

/// With `--net`, advertise the wired interface to the firmware (`CAPAB_ETH_RMII`, docs/hw/08 H1) and return the
/// backend options.
pub fn configure(args: NetArgs, cfg: &mut MachineConfig) -> Result<Option<NetOptions>> {
    let Some(mode) = args.net else { return Ok(None) };
    cfg.capabilities |= CAPAB_ETH_RMII;
    if mode != NetMode::User {
        if !args.hostfwd.is_empty() {
            bail!("--hostfwd applies to --net user only; a bridged guest is reached at its own address");
        }
        if args.web_port.is_some_and(|port| port != 0) {
            bail!(
                "--web-port applies to --net user only; a bridged guest serves its web UI on port 80 of its own \
                 address"
            );
        }
        let unique_id = Some(instance_unique_id(cfg.flash_image.as_deref())?);
        return Ok(Some(NetOptions { mode, hostfwd: Vec::new(), web_port: None, unique_id }));
    }
    let explicit = !args.hostfwd.is_empty();
    // Without --hostfwd the proxy serves guest port 80 on 8080; explicit forwards get one only with --web-port.
    let web_port = match args.web_port {
        Some(0) => None,
        Some(port) => Some(port),
        None => (!explicit).then_some(DEFAULT_WEB_PORT),
    };
    let mut hostfwd = if explicit { args.hostfwd } else { HostFwd::parse_list(DEFAULT_HOSTFWD)? };
    if let Some(port) = web_port {
        if !explicit {
            hostfwd.retain(|f| f.udp || f.guest_port != WEB_GUEST_PORT);
        }
        if let Some(fwd) = hostfwd.iter().find(|f| !f.udp && f.host_port == port) {
            bail!(
                "--web-port {port} is also the host port of --hostfwd {fwd}; choose another --web-port, or 0 for none"
            );
        }
    }
    Ok(Some(NetOptions { mode, hostfwd, web_port, unique_id: None }))
}

/// The backend behind the RMII MAC.
pub type Backend = Box<dyn NetBackend>;

/// Start the backend and plug the cable into the PHY, so the RMII task brings the link up (docs/hw/08 H3). For the
/// bridged modes the flash unique ID is replaced first, which gives the guest its own MAC.
pub fn attach(machine: &mut Machine, opts: &NetOptions) -> Result<Backend> {
    let backend: Backend = match &opts.mode {
        NetMode::User => user(opts)?,
        NetMode::VmnetBridged(iface) => bridged(iface.as_deref())?,
        NetMode::SocketVmnet(path) => socket_vmnet(path)?,
    };
    let io = &mut machine.bus.io;
    if let Some(uid) = opts.unique_id {
        io.get_mut::<SpiFlash>().context("--net: no SPI flash installed")?.set_unique_id(uid);
        eprintln!("net: guest MAC {}, address by DHCP from the host network", guest_mac(uid));
    }
    io.get_mut::<U2pio>().context("--net: no U2PIO page installed")?.phy.set_link(true);
    Ok(backend)
}

/// `--net user`: libslirp with the forwards and the web UI proxy.
#[cfg(feature = "net")]
fn user(opts: &NetOptions) -> Result<Backend> {
    use std::net::{Ipv4Addr, TcpListener};

    use ue2_net::{UserNet, GUEST};

    // Bound before libslirp starts, so a taken web port is reported as such.
    let web = opts
        .web_port
        .map(|port| {
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
                format!(
                    "--web-port {port}: cannot listen on 127.0.0.1:{port} (port in use? choose another \
                     --web-port, or 0 for none)"
                )
            })
        })
        .transpose()?;
    let mut net = UserNet::new(&opts.hostfwd).context("--net user")?;
    let forwards: Vec<String> =
        opts.hostfwd.iter().map(|f| format!("{}:{} -> {}", f.host_addr, f.host_port, f.guest_port)).collect();
    eprintln!(
        "net: libslirp {} user network, guest {GUEST} by DHCP, forwards {}",
        UserNet::version(),
        forwards.join(", ")
    );
    if let Some(listener) = web {
        let (addr, via) = net.start_web_proxy(listener).context("--web-port")?;
        eprintln!(
            "net: web UI http://{addr}/ (proxy to guest port {WEB_GUEST_PORT} through 127.0.0.1:{via}; \
             location.hostname becomes location.host in HTML and JavaScript)"
        );
    }
    Ok(Box::new(net))
}

#[cfg(not(feature = "net"))]
fn user(_opts: &NetOptions) -> Result<Backend> {
    bail!("--net user: this ue2emu was built without libslirp (cargo feature `net`, docs/specs/S22-windows.md)")
}

#[cfg(unix)]
fn socket_vmnet(path: &Path) -> Result<Backend> {
    let net = ue2_net::socket_vmnet::SocketVmnet::connect(path).context("--net socket-vmnet")?;
    eprintln!("net: socket_vmnet daemon at {}", path.display());
    Ok(Box::new(net))
}

#[cfg(not(unix))]
fn socket_vmnet(_path: &Path) -> Result<Backend> {
    bail!("--net socket-vmnet needs a Unix host (the socket_vmnet daemon listens on a Unix domain socket)")
}

#[cfg(target_os = "macos")]
fn bridged(iface: Option<&str>) -> Result<Backend> {
    use ue2_net::vmnet::{default_route_interface, VmnetBridged};

    let iface = match iface {
        Some(iface) => iface.to_owned(),
        None => default_route_interface().context("--net vmnet-bridged")?,
    };
    let net = VmnetBridged::start(&iface).context("--net vmnet-bridged")?;
    eprintln!("net: vmnet bridged onto {iface}, MTU {}", net.mtu());
    Ok(Box::new(net))
}

#[cfg(not(target_os = "macos"))]
fn bridged(_iface: Option<&str>) -> Result<Backend> {
    bail!("--net vmnet-bridged needs macOS (vmnet.framework)")
}

/// Exchange frames between the MAC and the backend.
pub fn pump(machine: &mut Machine, net: &mut Backend) {
    let bus = &mut machine.bus;
    if let Some(mac) = bus.io.get_mut::<Rmii>() {
        mac.exchange(net.as_mut(), &mut bus.ram, &mut bus.irq);
    }
}

/// A flash unique ID for one emulated device on a shared LAN. With `--flash` it follows the image's absolute path,
/// so the device keeps its MAC, DHCP lease and hostname across runs; without one every run is a new device. The
/// hash sits in bytes 1..=3 with 5..=7 zero, so it becomes MAC octets 3..=5 (rmii_interface.cc:126-128).
fn instance_unique_id(flash: Option<&Path>) -> Result<[u8; 8]> {
    let seed = match flash {
        // The same bytes as `OsStrExt::as_bytes` on Unix, so a flash keeps its MAC.
        Some(path) => std::path::absolute(path).context("--flash path")?.as_os_str().as_encoded_bytes().to_vec(),
        None => format!("{}:{:?}", std::process::id(), SystemTime::now()).into_bytes(),
    };
    // FNV-1a: stable across Rust releases, unlike `DefaultHasher`.
    let hash = seed.iter().fold(0xCBF2_9CE4_8422_2325_u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3));
    let [h0, h1, h2, ..] = hash.to_le_bytes();
    Ok([b'U', h0, h1, h2, 0, 0, 0, 0])
}

/// The MAC the firmware derives from the flash unique ID (rmii_interface.cc:123-128).
fn guest_mac(uid: [u8; 8]) -> String {
    format!("02:15:41:{:02x}:{:02x}:{:02x}", uid[1] ^ uid[5], uid[2] ^ uid[6], uid[3] ^ uid[7])
}

#[cfg(test)]
mod tests {
    use ue2_core::devices::flash::UID;

    use super::*;

    #[test]
    fn net_mode_syntax() {
        assert_eq!("user".parse::<NetMode>().unwrap(), NetMode::User);
        assert_eq!("vmnet-bridged".parse::<NetMode>().unwrap(), NetMode::VmnetBridged(None));
        assert_eq!("vmnet-bridged:en7".parse::<NetMode>().unwrap(), NetMode::VmnetBridged(Some("en7".into())));
        assert_eq!("socket-vmnet".parse::<NetMode>().unwrap(), NetMode::SocketVmnet(DEFAULT_SOCKET.into()));
        assert_eq!("socket-vmnet:/tmp/v.sock".parse::<NetMode>().unwrap(), NetMode::SocketVmnet("/tmp/v.sock".into()));
        for bad in ["", "tap", "user:x", "vmnet-bridged:", "socket-vmnet:"] {
            assert!(bad.parse::<NetMode>().is_err(), "{bad}");
        }
    }

    #[test]
    fn hostfwd_is_for_user_mode_only() {
        let mut cfg = MachineConfig::new("fw.elf".into(), "roms".into());
        let fwd = HostFwd::parse_list("tcp:8080:80").unwrap();
        let args =
            NetArgs { net: Some(NetMode::SocketVmnet(DEFAULT_SOCKET.into())), hostfwd: fwd.clone(), web_port: None };
        assert!(configure(args, &mut cfg).is_err());
        let args = NetArgs { net: Some(NetMode::User), hostfwd: fwd.clone(), web_port: None };
        let opts = configure(args, &mut cfg).unwrap().unwrap();
        assert_eq!((opts.hostfwd, opts.web_port, opts.unique_id), (fwd, None, None));
        assert_eq!(cfg.capabilities & CAPAB_ETH_RMII, CAPAB_ETH_RMII);
    }

    #[test]
    fn web_proxy_takes_the_default_http_forward() {
        let mut cfg = MachineConfig::new("fw.elf".into(), "roms".into());
        let user = |hostfwd: &str, web_port| NetArgs {
            net: Some(NetMode::User),
            hostfwd: if hostfwd.is_empty() { Vec::new() } else { HostFwd::parse_list(hostfwd).unwrap() },
            web_port,
        };
        let opts = configure(user("", None), &mut cfg).unwrap().unwrap();
        assert_eq!(opts.web_port, Some(8080));
        assert_eq!(opts.hostfwd, HostFwd::parse_list("tcp:2323:23,tcp:2121:21,tcp:6464:64").unwrap());
        let opts = configure(user("", Some(18080)), &mut cfg).unwrap().unwrap();
        assert_eq!((opts.web_port, opts.hostfwd.len()), (Some(18080), 3));
        // 0: no proxy, and the plain forward from 8080 is back.
        let opts = configure(user("", Some(0)), &mut cfg).unwrap().unwrap();
        assert_eq!((opts.web_port, opts.hostfwd), (None, HostFwd::parse_list(DEFAULT_HOSTFWD).unwrap()));
        // Explicit forwards stay as given; a proxy comes only with --web-port.
        let opts = configure(user("tcp:18080:80", None), &mut cfg).unwrap().unwrap();
        assert_eq!((opts.web_port, opts.hostfwd.len()), (None, 1));
        let opts = configure(user("tcp:18023:23", Some(18080)), &mut cfg).unwrap().unwrap();
        assert_eq!((opts.web_port, opts.hostfwd.len()), (Some(18080), 1));
        assert!(configure(user("tcp:18080:80", Some(18080)), &mut cfg).is_err(), "same host port twice");
        assert!(configure(user("", Some(2323)), &mut cfg).is_err(), "same host port as the telnet default");
        let bridged = |web_port| NetArgs {
            net: Some(NetMode::SocketVmnet(DEFAULT_SOCKET.into())),
            hostfwd: Vec::new(),
            web_port,
        };
        assert!(configure(bridged(Some(8080)), &mut cfg).is_err());
        assert_eq!(configure(bridged(Some(0)), &mut cfg).unwrap().unwrap().web_port, None);
    }

    #[test]
    fn bridged_instances_get_their_own_mac() {
        assert_eq!(guest_mac(UID), "02:15:41:71:67:42", "C64U 1.1.0 names itself C64-Ultimate-716742 with it");
        let a = instance_unique_id(Some(Path::new("/tmp/a/flash.bin"))).unwrap();
        assert_eq!(a, instance_unique_id(Some(Path::new("/tmp/a/flash.bin"))).unwrap(), "stable per image");
        let b = instance_unique_id(Some(Path::new("/tmp/b/flash.bin"))).unwrap();
        assert_ne!(guest_mac(a), guest_mac(b));
        assert_ne!(guest_mac(a), guest_mac(UID));
        let mut cfg = MachineConfig::new("fw.elf".into(), "roms".into());
        cfg.flash_image = Some("/tmp/a/flash.bin".into());
        let args = NetArgs { net: Some(NetMode::VmnetBridged(None)), hostfwd: Vec::new(), web_port: None };
        assert_eq!(configure(args, &mut cfg).unwrap().unwrap().unique_id, Some(a));
    }
}
