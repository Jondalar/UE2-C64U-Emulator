//! Hand-written bindings for the part of `libslirp.h` that [`crate::UserNet`] uses. Layouts follow the header field by
//! field; the `layout` test checks them against the header when `build.rs` could compile it.
//!
//! - Unix: `SlirpConfig` version 4, the API of libslirp 4.7, which later releases keep. A socket is an `int`, so the
//!   4.7 of Debian 12 and Ubuntu 24.04 links.
//! - Windows: version 6 (libslirp 4.9). A socket is a `SOCKET`, 64 bits on Win64, and the `int` poll API is deprecated
//!   there, so the poll list goes through `slirp_pollfds_fill_socket` and `SlirpCb` has the version 6 socket callbacks
//!   (docs/specs/S22-windows.md §4). Version 6 includes the version 5 fields of `SlirpConfig`.

use std::ffi::{c_char, c_int, c_void};

/// Opaque `Slirp`.
#[repr(C)]
pub struct Slirp {
    _private: [u8; 0],
}

#[cfg(not(windows))]
pub const SLIRP_CONFIG_VERSION: u32 = 4;
#[cfg(windows)]
pub const SLIRP_CONFIG_VERSION: u32 = 6;

/// `slirp_os_socket`.
#[cfg(not(windows))]
pub type Socket = c_int;
#[cfg(windows)]
pub type Socket = usize;

/// `struct in_addr`, network order.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InAddr {
    pub s_addr: u32,
}

/// `struct in6_addr`: 4-byte aligned on Unix (a union with `uint32_t`), 2-byte on Windows (`IN6_ADDR` has `USHORT`),
/// which moves every later field of `SlirpConfig`.
#[cfg(not(windows))]
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct In6Addr {
    pub s6_addr: [u8; 16],
}
#[cfg(windows)]
#[repr(C, align(2))]
#[derive(Clone, Copy)]
pub struct In6Addr {
    pub s6_addr: [u8; 16],
}

pub const SLIRP_POLL_IN: c_int = 1 << 0;
pub const SLIRP_POLL_OUT: c_int = 1 << 1;
pub const SLIRP_POLL_PRI: c_int = 1 << 2;
pub const SLIRP_POLL_ERR: c_int = 1 << 3;
pub const SLIRP_POLL_HUP: c_int = 1 << 4;

/// `enum SlirpTimerId`.
pub type SlirpTimerId = c_int;

pub type SlirpWriteCb = unsafe extern "C" fn(buf: *const c_void, len: usize, opaque: *mut c_void) -> isize;
pub type SlirpTimerCb = unsafe extern "C" fn(opaque: *mut c_void);
/// `SlirpAddPollCb` on Unix, `SlirpAddPollSocketCb` on Windows.
pub type SlirpAddPollCb = unsafe extern "C" fn(fd: Socket, events: c_int, opaque: *mut c_void) -> c_int;
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
    // Version 6.
    #[cfg(windows)]
    pub register_poll_socket: unsafe extern "C" fn(socket: Socket, opaque: *mut c_void),
    #[cfg(windows)]
    pub unregister_poll_socket: unsafe extern "C" fn(socket: Socket, opaque: *mut c_void),
}

/// `SlirpConfig` (fields of versions 1-4 on Unix, 1-6 on Windows).
#[repr(C)]
pub struct SlirpConfig {
    pub version: u32,
    pub restricted: c_int,
    pub in_enabled: bool,
    pub vnetwork: InAddr,
    pub vnetmask: InAddr,
    pub vhost: InAddr,
    pub in6_enabled: bool,
    pub vprefix_addr6: In6Addr,
    pub vprefix_len: u8,
    pub vhost6: In6Addr,
    pub vhostname: *const c_char,
    pub tftp_server_name: *const c_char,
    pub tftp_path: *const c_char,
    pub bootfile: *const c_char,
    pub vdhcp_start: InAddr,
    pub vnameserver: InAddr,
    pub vnameserver6: In6Addr,
    pub vdnssearch: *const *const c_char,
    pub vdomainname: *const c_char,
    /// 0 = `IF_MTU_DEFAULT`.
    pub if_mtu: usize,
    /// 0 = `IF_MRU_DEFAULT`.
    pub if_mru: usize,
    pub disable_host_loopback: bool,
    pub enable_emu: bool,
    // Version 2: `struct sockaddr_in *`, `struct sockaddr_in6 *`, always null here.
    pub outbound_addr: *mut c_void,
    pub outbound_addr6: *mut c_void,
    // Version 3.
    pub disable_dns: bool,
    // Version 4.
    pub disable_dhcp: bool,
    // Version 5.
    #[cfg(windows)]
    pub mfr_id: u32,
    #[cfg(windows)]
    pub oob_eth_addr: [u8; 6],
}

extern "C" {
    pub fn slirp_new(cfg: *const SlirpConfig, callbacks: *const SlirpCb, opaque: *mut c_void) -> *mut Slirp;
    pub fn slirp_cleanup(slirp: *mut Slirp);
    /// `timeout` (ms) is only ever lowered.
    #[cfg(not(windows))]
    pub fn slirp_pollfds_fill(slirp: *mut Slirp, timeout: *mut u32, add_poll: SlirpAddPollCb, opaque: *mut c_void);
    #[cfg(windows)]
    pub fn slirp_pollfds_fill_socket(
        slirp: *mut Slirp,
        timeout: *mut u32,
        add_poll: SlirpAddPollCb,
        opaque: *mut c_void,
    );
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
        host_addr: InAddr,
        host_port: c_int,
        guest_addr: InAddr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_version_string() -> *const c_char;
}

/// `slirp_pollfds_fill` on Unix, `slirp_pollfds_fill_socket` on Windows.
///
/// # Safety
/// As the libslirp functions: a valid instance, and `add_poll` may be called with `opaque` during the call.
pub unsafe fn pollfds_fill(slirp: *mut Slirp, timeout: *mut u32, add_poll: SlirpAddPollCb, opaque: *mut c_void) {
    #[cfg(not(windows))]
    slirp_pollfds_fill(slirp, timeout, add_poll, opaque);
    #[cfg(windows)]
    slirp_pollfds_fill_socket(slirp, timeout, add_poll, opaque);
}

#[cfg(all(test, ue2_slirp_layout))]
extern "C" {
    /// The header's layout as `build.rs` compiled it (`layout.c`), for the `layout` test.
    pub static ue2_slirp_layout: [usize; 20];
}

#[cfg(test)]
mod tests {
    /// Sizes and offsets against `layout.c` (same order). Skipped when `build.rs` found no header.
    #[test]
    fn layout_matches_the_header() {
        #[cfg(ue2_slirp_layout)]
        {
            use std::mem::{offset_of, size_of};

            use super::*;
            // SAFETY: a constant array compiled by build.rs.
            let c = unsafe { ue2_slirp_layout };
            let rust = [
                offset_of!(SlirpConfig, vnetwork),
                offset_of!(SlirpConfig, vprefix_addr6),
                offset_of!(SlirpConfig, vprefix_len),
                offset_of!(SlirpConfig, vhost6),
                offset_of!(SlirpConfig, vhostname),
                offset_of!(SlirpConfig, vdhcp_start),
                offset_of!(SlirpConfig, vnameserver6),
                offset_of!(SlirpConfig, vdnssearch),
                offset_of!(SlirpConfig, if_mtu),
                offset_of!(SlirpConfig, disable_host_loopback),
                offset_of!(SlirpConfig, outbound_addr),
                offset_of!(SlirpConfig, outbound_addr6),
                offset_of!(SlirpConfig, disable_dns),
                offset_of!(SlirpConfig, disable_dhcp),
                offset_of!(SlirpCb, register_poll_fd),
                offset_of!(SlirpCb, notify),
                offset_of!(SlirpCb, timer_new_opaque),
                size_of::<InAddr>(),
                size_of::<In6Addr>(),
                std::mem::align_of::<In6Addr>(),
            ];
            assert_eq!(rust, c, "field offsets differ from libslirp.h");
            #[cfg(windows)]
            {
                // SAFETY: as above.
                let v6 = unsafe { ue2_slirp_layout_v6 };
                let rust6 = [
                    size_of::<SlirpConfig>(),
                    offset_of!(SlirpConfig, mfr_id),
                    offset_of!(SlirpConfig, oob_eth_addr),
                    size_of::<SlirpCb>(),
                    offset_of!(SlirpCb, register_poll_socket),
                    offset_of!(SlirpCb, unregister_poll_socket),
                ];
                assert_eq!(rust6, v6, "version 5/6 layout differs from libslirp.h");
            }
        }
        #[cfg(not(ue2_slirp_layout))]
        eprintln!("skipping: build.rs could not compile libslirp.h (docs/specs/S22-windows.md §4)");
    }

    #[cfg(all(windows, ue2_slirp_layout))]
    extern "C" {
        static ue2_slirp_layout_v6: [usize; 6];
    }
}
