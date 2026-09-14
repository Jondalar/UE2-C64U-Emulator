//! Hand-written bindings for the part of `libslirp.h` that [`crate::UserNet`] uses, at `SlirpConfig` version 4: the
//! API of libslirp 4.7, which later releases keep. Layouts follow the header field by field; `libc` supplies the
//! socket types.

use std::ffi::{c_char, c_int, c_void};

use libc::{in6_addr, in_addr, sockaddr_in, sockaddr_in6};

/// Opaque `Slirp`.
#[repr(C)]
pub struct Slirp {
    _private: [u8; 0],
}

/// `SlirpConfig.version` 4 (libslirp 4.7) covers everything used here. Version 5 (4.8) adds NCSI fields, version 6
/// (4.9) the socket callbacks and `slirp_pollfds_fill_socket`; on Unix a socket is an `int`, so neither is needed and
/// the 4.7 of Debian 12 and Ubuntu 24.04 links.
pub const SLIRP_CONFIG_VERSION: u32 = 4;

pub const SLIRP_POLL_IN: c_int = 1 << 0;
pub const SLIRP_POLL_OUT: c_int = 1 << 1;
pub const SLIRP_POLL_PRI: c_int = 1 << 2;
pub const SLIRP_POLL_ERR: c_int = 1 << 3;
pub const SLIRP_POLL_HUP: c_int = 1 << 4;

/// `enum SlirpTimerId`.
pub type SlirpTimerId = c_int;

pub type SlirpWriteCb = unsafe extern "C" fn(buf: *const c_void, len: usize, opaque: *mut c_void) -> isize;
pub type SlirpTimerCb = unsafe extern "C" fn(opaque: *mut c_void);
pub type SlirpAddPollCb = unsafe extern "C" fn(fd: c_int, events: c_int, opaque: *mut c_void) -> c_int;
pub type SlirpGetREventsCb = unsafe extern "C" fn(idx: c_int, opaque: *mut c_void) -> c_int;

/// `SlirpCb`. libslirp keeps the pointer, so the table must outlive the instance.
#[repr(C)]
pub struct SlirpCb {
    pub send_packet: SlirpWriteCb,
    pub guest_error: unsafe extern "C" fn(msg: *const c_char, opaque: *mut c_void),
    pub clock_get_ns: unsafe extern "C" fn(opaque: *mut c_void) -> i64,
    /// Not needed when `timer_new_opaque` is provided.
    pub timer_new:
        Option<unsafe extern "C" fn(cb: SlirpTimerCb, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void>,
    pub timer_free: unsafe extern "C" fn(timer: *mut c_void, opaque: *mut c_void),
    /// `expire_time` in ms on the `clock_get_ns` clock.
    pub timer_mod: unsafe extern "C" fn(timer: *mut c_void, expire_time: i64, opaque: *mut c_void),
    pub register_poll_fd: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
    pub unregister_poll_fd: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
    pub notify: unsafe extern "C" fn(opaque: *mut c_void),
    // Version 4.
    pub init_completed: Option<unsafe extern "C" fn(slirp: *mut Slirp, opaque: *mut c_void)>,
    pub timer_new_opaque:
        unsafe extern "C" fn(id: SlirpTimerId, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void,
}

/// `SlirpConfig` (fields of versions 1-4).
#[repr(C)]
pub struct SlirpConfig {
    pub version: u32,
    pub restricted: c_int,
    pub in_enabled: bool,
    pub vnetwork: in_addr,
    pub vnetmask: in_addr,
    pub vhost: in_addr,
    pub in6_enabled: bool,
    pub vprefix_addr6: in6_addr,
    pub vprefix_len: u8,
    pub vhost6: in6_addr,
    pub vhostname: *const c_char,
    pub tftp_server_name: *const c_char,
    pub tftp_path: *const c_char,
    pub bootfile: *const c_char,
    pub vdhcp_start: in_addr,
    pub vnameserver: in_addr,
    pub vnameserver6: in6_addr,
    pub vdnssearch: *const *const c_char,
    pub vdomainname: *const c_char,
    /// 0 = `IF_MTU_DEFAULT`.
    pub if_mtu: usize,
    /// 0 = `IF_MRU_DEFAULT`.
    pub if_mru: usize,
    pub disable_host_loopback: bool,
    pub enable_emu: bool,
    // Version 2.
    pub outbound_addr: *mut sockaddr_in,
    pub outbound_addr6: *mut sockaddr_in6,
    // Version 3.
    pub disable_dns: bool,
    // Version 4.
    pub disable_dhcp: bool,
}

extern "C" {
    pub fn slirp_new(cfg: *const SlirpConfig, callbacks: *const SlirpCb, opaque: *mut c_void) -> *mut Slirp;
    pub fn slirp_cleanup(slirp: *mut Slirp);
    /// `timeout` (ms) is only ever lowered.
    pub fn slirp_pollfds_fill(slirp: *mut Slirp, timeout: *mut u32, add_poll: SlirpAddPollCb, opaque: *mut c_void);
    pub fn slirp_pollfds_poll(
        slirp: *mut Slirp,
        select_error: c_int,
        get_revents: SlirpGetREventsCb,
        opaque: *mut c_void,
    );
    pub fn slirp_input(slirp: *mut Slirp, pkt: *const u8, pkt_len: c_int);
    pub fn slirp_handle_timer(slirp: *mut Slirp, id: SlirpTimerId, cb_opaque: *mut c_void);
    /// `guest_addr` in network order, ports in host order. Returns < 0 on failure.
    pub fn slirp_add_hostfwd(
        slirp: *mut Slirp,
        is_udp: c_int,
        host_addr: in_addr,
        host_port: c_int,
        guest_addr: in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_version_string() -> *const c_char;
}
