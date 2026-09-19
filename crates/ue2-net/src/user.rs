//! [`UserNet`]: libslirp user-mode networking (feature `slirp`). The poll of libslirp's sockets is `poll(2)` on Unix
//! and `WSAPoll` on Windows ([`sys`], docs/specs/S22-windows.md §4).

use std::ffi::{c_char, c_int, c_void, CStr};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::ptr::{self, NonNull};
use std::time::Instant;

use anyhow::{bail, Result};
use ue2_core::host::NetBackend;

use crate::{ffi, web_proxy, HostFwd, DNS, GUEST, HOST, NETMASK, NETWORK};

/// A libslirp timer handed out by `timer_new_opaque`; the handle given to libslirp is its index + 1.
struct Timer {
    id: ffi::SlirpTimerId,
    cb_opaque: *mut c_void,
    expire_ms: Option<i64>,
}

/// Callback state behind libslirp's `opaque` pointer.
struct State {
    epoch: Instant,
    /// Frames libslirp sent towards the guest since the last poll.
    to_guest: Vec<Vec<u8>>,
    timers: Vec<Option<Timer>>,
}

impl State {
    fn clock_ms(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }
}

/// libslirp instance with its port forwards. Dropping it closes every socket.
pub struct UserNet {
    slirp: NonNull<ffi::Slirp>,
    /// Owned; freed in `Drop` after `slirp_cleanup`, which still calls back into it.
    state: *mut State,
    /// Rebuilt by every poll.
    pollfds: Vec<sys::PollFd>,
    /// Stops accepting when the instance goes.
    web_proxy: Option<web_proxy::WebProxy>,
}

fn in_addr_of(ip: Ipv4Addr) -> ffi::InAddr {
    ffi::InAddr { s_addr: u32::from_ne_bytes(ip.octets()) }
}

const IN6_ANY: ffi::In6Addr = ffi::In6Addr { s6_addr: [0; 16] };

impl UserNet {
    /// Start the virtual network and listen on every forward. Fails when a host port cannot be bound.
    pub fn new(forwards: &[HostFwd]) -> Result<Self> {
        let cfg = ffi::SlirpConfig {
            version: ffi::SLIRP_CONFIG_VERSION,
            restricted: 0,
            in_enabled: true,
            vnetwork: in_addr_of(NETWORK),
            vnetmask: in_addr_of(NETMASK),
            vhost: in_addr_of(HOST),
            in6_enabled: false,
            vprefix_addr6: IN6_ANY,
            vprefix_len: 0,
            vhost6: IN6_ANY,
            vhostname: ptr::null(),
            tftp_server_name: ptr::null(),
            tftp_path: ptr::null(),
            bootfile: ptr::null(),
            vdhcp_start: in_addr_of(GUEST),
            vnameserver: in_addr_of(DNS),
            vnameserver6: IN6_ANY,
            vdnssearch: ptr::null(),
            vdomainname: ptr::null(),
            if_mtu: 0,
            if_mru: 0,
            disable_host_loopback: false,
            enable_emu: false,
            outbound_addr: ptr::null_mut(),
            outbound_addr6: ptr::null_mut(),
            disable_dns: false,
            disable_dhcp: false,
            #[cfg(windows)]
            mfr_id: 0,
            #[cfg(windows)]
            oob_eth_addr: [0; 6],
        };
        sys::init()?;
        let state = Box::into_raw(Box::new(State { epoch: Instant::now(), to_guest: Vec::new(), timers: Vec::new() }));
        // SAFETY: `cfg` is read during the call only; `CALLBACKS` is static and `state` lives until `Drop`.
        let Some(slirp) = NonNull::new(unsafe { ffi::slirp_new(&cfg, &CALLBACKS, state.cast()) }) else {
            // SAFETY: libslirp did not keep `state`.
            drop(unsafe { Box::from_raw(state) });
            bail!("libslirp: slirp_new failed");
        };
        let mut net = UserNet { slirp, state, pollfds: Vec::new(), web_proxy: None };
        for fwd in forwards {
            net.add_hostfwd(fwd)?;
        }
        Ok(net)
    }

    /// Listen on one more forward. Fails when the host port cannot be bound.
    pub fn add_hostfwd(&mut self, fwd: &HostFwd) -> Result<()> {
        // SAFETY: valid instance; plain values.
        let rc = unsafe {
            ffi::slirp_add_hostfwd(
                self.slirp.as_ptr(),
                c_int::from(fwd.udp),
                in_addr_of(fwd.host_addr),
                c_int::from(fwd.host_port),
                in_addr_of(GUEST),
                c_int::from(fwd.guest_port),
            )
        };
        if rc < 0 {
            bail!("hostfwd {fwd}: cannot listen on {}:{} (port in use?)", fwd.host_addr, fwd.host_port);
        }
        Ok(())
    }

    /// Forward a free TCP port on 127.0.0.1 to `guest_port` and return it. The OS picks the port; one taken again
    /// before libslirp binds it is replaced by the next.
    pub fn add_loopback_forward(&mut self, guest_port: u16) -> Result<u16> {
        for _ in 0..8 {
            let host_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port();
            let fwd = HostFwd { udp: false, host_addr: Ipv4Addr::LOCALHOST, host_port, guest_port };
            if self.add_hostfwd(&fwd).is_ok() {
                return Ok(host_port);
            }
        }
        bail!("no free loopback port for a forward to guest port {guest_port}")
    }

    /// Serve the web UI proxy on `listener` (docs/status/network.md, "Web UI proxy"): it connects through a new
    /// loopback forward to the firmware web server on guest port 80. Returns the proxy address and the forward's
    /// host port. The proxy runs on its own threads and stops accepting when this instance is dropped.
    pub fn start_web_proxy(&mut self, listener: TcpListener) -> Result<(SocketAddr, u16)> {
        let via = self.add_loopback_forward(web_proxy::WEB_GUEST_PORT)?;
        let proxy = web_proxy::WebProxy::start(listener, SocketAddr::from((Ipv4Addr::LOCALHOST, via)))?;
        let addr = proxy.local_addr();
        self.web_proxy = Some(proxy);
        Ok((addr, via))
    }

    /// libslirp version string, e.g. "4.9.4".
    pub fn version() -> String {
        // SAFETY: returns a static NUL-terminated string.
        unsafe { CStr::from_ptr(ffi::slirp_version_string()) }.to_string_lossy().into_owned()
    }

    /// Run the libslirp timers that are due.
    fn fire_timers(&mut self) {
        let due: Vec<(ffi::SlirpTimerId, *mut c_void)> = {
            // SAFETY: no libslirp call is active, so no callback holds the state.
            let state = unsafe { &mut *self.state };
            let now = state.clock_ms();
            state
                .timers
                .iter_mut()
                .flatten()
                .filter(|t| t.expire_ms.is_some_and(|at| at <= now))
                .map(|t| {
                    t.expire_ms = None;
                    (t.id, t.cb_opaque)
                })
                .collect()
        };
        for (id, cb_opaque) in due {
            // SAFETY: id and cb_opaque come from `timer_new_opaque` of this instance.
            unsafe { ffi::slirp_handle_timer(self.slirp.as_ptr(), id, cb_opaque) };
        }
    }
}

impl NetBackend for UserNet {
    fn send(&mut self, frame: &[u8]) {
        let Ok(len) = c_int::try_from(frame.len()) else { return };
        // SAFETY: libslirp copies what it keeps; callbacks only touch `state`.
        unsafe { ffi::slirp_input(self.slirp.as_ptr(), frame.as_ptr(), len) };
    }

    /// One non-blocking round: collect libslirp's sockets, `poll` them with a zero timeout, let libslirp
    /// process the results and its TCP timers, fire due timers, then hand over the frames for the guest.
    fn poll(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        self.pollfds.clear();
        let fds: *mut Vec<sys::PollFd> = &mut self.pollfds;
        let mut timeout = 0;
        // SAFETY: `add_poll` only pushes onto `self.pollfds`, which nothing else touches during the call.
        unsafe { ffi::pollfds_fill(self.slirp.as_ptr(), &mut timeout, add_poll, fds.cast()) };
        let ready = sys::poll(&mut self.pollfds);
        // SAFETY: as above; `get_revents` only reads `self.pollfds`.
        unsafe { ffi::slirp_pollfds_poll(self.slirp.as_ptr(), c_int::from(ready < 0), get_revents, fds.cast()) };
        self.fire_timers();
        // SAFETY: no libslirp call is active.
        let frames = std::mem::take(unsafe { &mut (*self.state).to_guest });
        for frame in &frames {
            deliver(frame);
        }
    }
}

impl Drop for UserNet {
    fn drop(&mut self) {
        // SAFETY: the instance is valid and never used again; cleanup may still call `timer_free`, so the state
        // goes last.
        unsafe {
            ffi::slirp_cleanup(self.slirp.as_ptr());
            drop(Box::from_raw(self.state));
        }
    }
}

static CALLBACKS: ffi::SlirpCb = ffi::SlirpCb {
    send_packet,
    guest_error,
    clock_get_ns,
    timer_new: None,
    timer_free,
    timer_mod,
    register_poll_fd: ignore_fd,
    unregister_poll_fd: ignore_fd,
    notify,
    init_completed: None,
    timer_new_opaque,
    #[cfg(windows)]
    register_poll_socket: ignore_socket,
    #[cfg(windows)]
    unregister_poll_socket: ignore_socket,
};

/// # Safety
/// `opaque` is the `State` given to `slirp_new`, not otherwise borrowed while libslirp runs.
unsafe fn state<'a>(opaque: *mut c_void) -> &'a mut State {
    &mut *opaque.cast::<State>()
}

unsafe extern "C" fn send_packet(buf: *const c_void, len: usize, opaque: *mut c_void) -> isize {
    let frame = std::slice::from_raw_parts(buf.cast::<u8>(), len);
    state(opaque).to_guest.push(frame.to_vec());
    len as isize
}

unsafe extern "C" fn guest_error(msg: *const c_char, _opaque: *mut c_void) {
    eprintln!("net: libslirp: guest error: {}", CStr::from_ptr(msg).to_string_lossy());
}

unsafe extern "C" fn clock_get_ns(opaque: *mut c_void) -> i64 {
    state(opaque).epoch.elapsed().as_nanos() as i64
}

unsafe extern "C" fn timer_new_opaque(
    id: ffi::SlirpTimerId,
    cb_opaque: *mut c_void,
    opaque: *mut c_void,
) -> *mut c_void {
    let timers = &mut state(opaque).timers;
    timers.push(Some(Timer { id, cb_opaque, expire_ms: None }));
    timers.len() as *mut c_void
}

/// The `Timer` slot of a handle from `timer_new_opaque`.
unsafe fn timer<'a>(handle: *mut c_void, opaque: *mut c_void) -> &'a mut Option<Timer> {
    &mut state(opaque).timers[handle as usize - 1]
}

unsafe extern "C" fn timer_free(handle: *mut c_void, opaque: *mut c_void) {
    *timer(handle, opaque) = None;
}

unsafe extern "C" fn timer_mod(handle: *mut c_void, expire_ms: i64, opaque: *mut c_void) {
    if let Some(t) = timer(handle, opaque) {
        t.expire_ms = Some(expire_ms);
    }
}

/// Sockets are collected afresh by every `slirp_pollfds_fill`, so registration needs no bookkeeping.
unsafe extern "C" fn ignore_fd(_fd: c_int, _opaque: *mut c_void) {}

#[cfg(windows)]
unsafe extern "C" fn ignore_socket(_socket: ffi::Socket, _opaque: *mut c_void) {}

/// Nothing sleeps on libslirp: the emulation thread polls it after every slice.
unsafe extern "C" fn notify(_opaque: *mut c_void) {}

unsafe extern "C" fn add_poll(fd: ffi::Socket, events: c_int, opaque: *mut c_void) -> c_int {
    let fds = &mut *opaque.cast::<Vec<sys::PollFd>>();
    let events = sys::REQUEST.iter().filter(|(s, _)| events & s != 0).fold(0, |acc, (_, p)| acc | p);
    fds.push(sys::poll_fd(fd, events));
    fds.len() as c_int - 1
}

unsafe extern "C" fn get_revents(idx: c_int, opaque: *mut c_void) -> c_int {
    let fds = &*opaque.cast::<Vec<sys::PollFd>>();
    let Some(fd) = usize::try_from(idx).ok().and_then(|i| fds.get(i)) else { return 0 };
    sys::RESULT.iter().filter(|(_, p)| fd.revents & p != 0).fold(0, |acc, (s, _)| acc | s)
}

/// `poll(2)` over libslirp's sockets.
#[cfg(unix)]
mod sys {
    use std::ffi::c_int;

    use crate::ffi;

    pub type PollFd = libc::pollfd;

    /// libslirp's poll flags and the `poll(2)` events they request.
    pub const REQUEST: [(c_int, libc::c_short); 5] = [
        (ffi::SLIRP_POLL_IN, libc::POLLIN),
        (ffi::SLIRP_POLL_OUT, libc::POLLOUT),
        (ffi::SLIRP_POLL_PRI, libc::POLLPRI),
        (ffi::SLIRP_POLL_ERR, libc::POLLERR),
        (ffi::SLIRP_POLL_HUP, libc::POLLHUP),
    ];
    /// The returned events libslirp is told about.
    pub const RESULT: [(c_int, libc::c_short); 5] = REQUEST;

    pub fn poll_fd(fd: ffi::Socket, events: libc::c_short) -> PollFd {
        libc::pollfd { fd, events, revents: 0 }
    }

    pub fn init() -> anyhow::Result<()> {
        Ok(())
    }

    /// Zero timeout; < 0 on error.
    pub fn poll(fds: &mut [PollFd]) -> c_int {
        // SAFETY: the slice holds `len` initialised entries.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) }
    }
}

/// `WSAPoll` over libslirp's sockets (docs/specs/S22-windows.md §4). It fails with WSAEINVAL when asked for
/// `POLLPRI` or with no socket, and `POLLERR`/`POLLHUP` are results only.
#[cfg(windows)]
mod sys {
    use std::ffi::c_int;
    use std::sync::OnceLock;

    use windows_sys::Win32::Networking::WinSock::{
        WSAPoll, WSAStartup, POLLERR, POLLHUP, POLLRDBAND, POLLRDNORM, POLLWRNORM, WSADATA, WSAPOLLFD,
    };

    use crate::ffi;

    pub type PollFd = WSAPOLLFD;

    pub const REQUEST: [(c_int, i16); 2] = [(ffi::SLIRP_POLL_IN, POLLRDNORM), (ffi::SLIRP_POLL_OUT, POLLWRNORM)];
    pub const RESULT: [(c_int, i16); 5] = [
        (ffi::SLIRP_POLL_IN, POLLRDNORM),
        (ffi::SLIRP_POLL_OUT, POLLWRNORM),
        (ffi::SLIRP_POLL_PRI, POLLRDBAND),
        (ffi::SLIRP_POLL_ERR, POLLERR),
        (ffi::SLIRP_POLL_HUP, POLLHUP),
    ];

    pub fn poll_fd(fd: ffi::Socket, events: i16) -> PollFd {
        WSAPOLLFD { fd, events, revents: 0 }
    }

    /// Winsock for libslirp's sockets: std starts it only with its own first socket.
    pub fn init() -> anyhow::Result<()> {
        static STARTED: OnceLock<i32> = OnceLock::new();
        let rc = *STARTED.get_or_init(|| {
            // SAFETY: WSADATA is plain data the call fills in.
            let mut data: WSADATA = unsafe { std::mem::zeroed() };
            // SAFETY: version 2.2 into a valid WSADATA; never paired with WSACleanup, as std does not either.
            unsafe { WSAStartup(0x0202, &mut data) }
        });
        anyhow::ensure!(rc == 0, "WSAStartup failed ({rc})");
        Ok(())
    }

    /// Zero timeout; < 0 on error. No socket: nothing to wait for.
    pub fn poll(fds: &mut [PollFd]) -> c_int {
        if fds.is_empty() {
            return 0;
        }
        // SAFETY: the slice holds `len` initialised entries.
        unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, 0) }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use super::*;

    const GUEST_MAC: [u8; 6] = [0x02, 0x15, 0x41, 0x01, 0x02, 0x03];

    /// Poll until a frame satisfying `want` arrives, or one second passes.
    fn wait_for(net: &mut UserNet, want: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            let mut found = None;
            net.poll(&mut |f| {
                if found.is_none() && want(f) {
                    found = Some(f.to_vec());
                }
            });
            if found.is_some() {
                return found;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// Minimal DHCPDISCOVER from `GUEST_MAC` (RFC 2131). UDP checksum 0 means none; slirp checks the IP header's.
    fn dhcp_discover() -> Vec<u8> {
        let mut bootp = vec![0u8; 240];
        bootp[..4].copy_from_slice(&[1, 1, 6, 0]);
        bootp[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        bootp[28..34].copy_from_slice(&GUEST_MAC);
        bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bootp.extend_from_slice(&[53, 1, 1, 255]);
        let udp_len = 8 + bootp.len() as u16;
        let mut f = [0xFF; 6].to_vec();
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&[0x08, 0x00, 0x45, 0x00]);
        f.extend_from_slice(&(20 + udp_len).to_be_bytes());
        f.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 0, 68, 0, 67]);
        f.extend_from_slice(&udp_len.to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&bootp);
        let sum = f[14..34].chunks(2).fold(0u32, |acc, w| acc + u32::from(u16::from_be_bytes([w[0], w[1]])));
        let sum = !((sum & 0xFFFF) + (sum >> 16)) as u16;
        f[24..26].copy_from_slice(&sum.to_be_bytes());
        f
    }

    #[test]
    fn dhcp_offers_the_guest_address() {
        let mut net = UserNet::new(&[]).unwrap();
        assert!(UserNet::version().starts_with('4'));
        net.send(&dhcp_discover());
        let is_offer = |f: &[u8]| f.len() > 58 && f[23] == 17 && f[34..38] == [0, 67, 0, 68];
        let offer = wait_for(&mut net, is_offer).expect("DHCPOFFER");
        assert_eq!(offer[0..6], [0xFF; 6], "broadcast: the client has no address yet");
        assert_eq!(offer[42 + 16..42 + 20], GUEST.octets(), "yiaddr");
    }

    #[test]
    fn hostfwd_connection_reaches_the_guest_segment() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let fwd = HostFwd { udp: false, host_addr: Ipv4Addr::LOCALHOST, host_port: port, guest_port: 80 };
        let mut net = UserNet::new(&[fwd]).unwrap();
        assert!(UserNet::new(&[fwd]).is_err(), "port already taken by the first instance");
        let _client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // slirp accepts, then resolves the guest: an ARP request for 10.0.2.15 goes onto the segment.
        let arp = wait_for(&mut net, |f| f.len() >= 42 && f[12..14] == [0x08, 0x06]).expect("ARP request");
        assert_eq!((arp[0..6] == [0xFF; 6], &arp[38..42]), (true, &GUEST.octets()[..]));
    }

    #[test]
    fn web_proxy_connects_through_a_loopback_forward() {
        let mut net = UserNet::new(&[]).unwrap();
        let (addr, via) = net.start_web_proxy(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
        assert_ne!(addr.port(), via);
        assert!(TcpListener::bind(("127.0.0.1", via)).is_err(), "libslirp listens on the forward");
        let _client = TcpStream::connect(addr).unwrap();
        // The proxy connects on to libslirp, which resolves the guest for port 80.
        let arp = wait_for(&mut net, |f| f.len() >= 42 && f[12..14] == [0x08, 0x06]).expect("ARP request");
        assert_eq!(&arp[38..42], &GUEST.octets()[..]);
    }
}
