//! vmnet.framework in bridged mode (`VMNET_BRIDGED_MODE`): guest frames go straight onto a host interface, so the
//! firmware gets its DHCP lease from the LAN. Starting the interface needs root or the `com.apple.vm.networking`
//! entitlement; without either, `vmnet_start_interface` completes with `VMNET_FAILURE` (measured on macOS 27 as
//! uid 501), which [`VmnetBridged::start`] turns into an explicit message.
//!
//! vmnet calls back on a private serial dispatch queue. The event callback only raises a flag; packets are read and
//! written on the emulation thread in [`NetBackend::poll`] and [`NetBackend::send`].

use std::ffi::{c_int, CStr, CString};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use block2::RcBlock;
use libc::iovec;
use ue2_core::host::NetBackend;

/// How long `start` waits for vmnet's completion handler.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long dropping the backend waits for `vmnet_stop_interface` to complete.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Packets per `vmnet_read` (socket_vmnet reads 32 at once as well).
const READ_BATCH: usize = 32;
/// Entitlement that lets an unprivileged process use vmnet (vmnet.h, `vmnet_start_interface`).
const VM_NETWORKING_ENTITLEMENT: &CStr = c"com.apple.vm.networking";

/// Hand-written bindings for vmnet.h (macOS 27 SDK), the libxpc/libdispatch calls it needs, and the Security
/// framework entitlement query.
#[allow(non_camel_case_types, non_upper_case_globals)]
mod sys {
    use std::ffi::{c_char, c_int, c_void};

    use block2::Block;

    pub type xpc_object_t = *mut c_void;
    pub type dispatch_queue_t = *mut c_void;
    pub type interface_ref = *mut c_void;
    /// `vmnet_return_t`.
    pub type vmnet_return_t = u32;
    /// `vmnet_start_interface_completion_handler_t` and `vmnet_interface_event_callback_t`.
    pub type StatusParamBlock = Block<dyn Fn(u32, xpc_object_t)>;
    /// `vmnet_interface_completion_handler_t`.
    pub type StatusBlock = Block<dyn Fn(vmnet_return_t)>;

    pub const VMNET_BRIDGED_MODE: u64 = 1002;
    pub const VMNET_INTERFACE_PACKETS_AVAILABLE: u32 = 1 << 0;
    pub const VMNET_SUCCESS: vmnet_return_t = 1000;
    pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

    /// `struct vmpktdesc`.
    #[repr(C)]
    pub struct vmpktdesc {
        pub vm_pkt_size: usize,
        pub vm_pkt_iov: *mut libc::iovec,
        pub vm_pkt_iovcnt: u32,
        pub vm_flags: u32,
    }

    #[link(name = "vmnet", kind = "framework")]
    extern "C" {
        pub static vmnet_operation_mode_key: *const c_char;
        pub static vmnet_shared_interface_name_key: *const c_char;
        pub static vmnet_allocate_mac_address_key: *const c_char;
        pub static vmnet_mtu_key: *const c_char;
        pub static vmnet_max_packet_size_key: *const c_char;
        pub fn vmnet_copy_shared_interface_list() -> xpc_object_t;
        pub fn vmnet_start_interface(
            desc: xpc_object_t,
            queue: dispatch_queue_t,
            handler: &StatusParamBlock,
        ) -> interface_ref;
        pub fn vmnet_interface_set_event_callback(
            interface: interface_ref,
            event_mask: u32,
            queue: dispatch_queue_t,
            callback: Option<&StatusParamBlock>,
        ) -> vmnet_return_t;
        pub fn vmnet_read(interface: interface_ref, packets: *mut vmpktdesc, pktcnt: *mut c_int) -> vmnet_return_t;
        pub fn vmnet_write(interface: interface_ref, packets: *mut vmpktdesc, pktcnt: *mut c_int) -> vmnet_return_t;
        pub fn vmnet_stop_interface(
            interface: interface_ref,
            queue: dispatch_queue_t,
            handler: &StatusBlock,
        ) -> vmnet_return_t;
    }

    // libxpc and libdispatch are part of libSystem.
    extern "C" {
        pub fn xpc_dictionary_create(
            keys: *const *const c_char,
            values: *const xpc_object_t,
            count: usize,
        ) -> xpc_object_t;
        pub fn xpc_dictionary_set_uint64(dict: xpc_object_t, key: *const c_char, value: u64);
        pub fn xpc_dictionary_set_string(dict: xpc_object_t, key: *const c_char, value: *const c_char);
        pub fn xpc_dictionary_set_bool(dict: xpc_object_t, key: *const c_char, value: bool);
        pub fn xpc_dictionary_get_uint64(dict: xpc_object_t, key: *const c_char) -> u64;
        pub fn xpc_array_get_count(array: xpc_object_t) -> usize;
        pub fn xpc_array_get_string(array: xpc_object_t, index: usize) -> *const c_char;
        pub fn xpc_release(object: xpc_object_t);
        pub fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> dispatch_queue_t;
        pub fn dispatch_release(object: *mut c_void);
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        pub fn SecTaskCreateFromSelf(allocator: *const c_void) -> *const c_void;
        pub fn SecTaskCopyValueForEntitlement(
            task: *const c_void,
            entitlement: *const c_void,
            error: *mut *const c_void,
        ) -> *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFBooleanTrue: *const c_void;
        pub fn CFStringCreateWithCString(allocator: *const c_void, cstr: *const c_char, encoding: u32)
            -> *const c_void;
        pub fn CFRelease(object: *const c_void);
    }
}

/// A started bridged vmnet interface. Dropping it stops the interface.
pub struct VmnetBridged {
    interface: sys::interface_ref,
    queue: sys::dispatch_queue_t,
    /// Raised by the vmnet event callback when packets are available, taken by `poll`.
    pending: Arc<AtomicBool>,
    /// `READ_BATCH` receive buffers of `vmnet_max_packet_size_key` bytes.
    buffers: Vec<Vec<u8>>,
    mtu: u64,
    /// A failed read or write was reported; later ones stay quiet.
    warned: bool,
}

impl VmnetBridged {
    /// Bridge onto host interface `ifname` (e.g. `en0`). The guest keeps its own MAC; vmnet allocates none.
    pub fn start(ifname: &str) -> Result<Self> {
        let bridgeable = interfaces();
        if !bridgeable.iter().any(|name| name == ifname) {
            bail!("vmnet cannot bridge '{ifname}'; bridgeable interfaces: {}", bridgeable.join(", "));
        }
        let name = CString::new(ifname).context("interface name")?;
        let (done, completion) = mpsc::sync_channel(1);
        let handler = RcBlock::new(move |status: u32, param: sys::xpc_object_t| {
            // SAFETY: vmnet passes the interface parameter dictionary, valid during the call, on success.
            let sizes = (status == sys::VMNET_SUCCESS && !param.is_null()).then(|| unsafe {
                (
                    sys::xpc_dictionary_get_uint64(param, sys::vmnet_mtu_key),
                    sys::xpc_dictionary_get_uint64(param, sys::vmnet_max_packet_size_key),
                )
            });
            let _ = done.send((status, sizes));
        });
        // SAFETY: plain libxpc/libdispatch/vmnet calls on objects created here; vmnet copies the handler block and
        // the dictionary is released after the call that reads it.
        let (interface, queue) = unsafe {
            let desc = sys::xpc_dictionary_create(ptr::null(), ptr::null(), 0);
            sys::xpc_dictionary_set_uint64(desc, sys::vmnet_operation_mode_key, sys::VMNET_BRIDGED_MODE);
            sys::xpc_dictionary_set_string(desc, sys::vmnet_shared_interface_name_key, name.as_ptr());
            // The firmware's MAC (rmii_interface.cc:123-128) goes onto the wire, not one vmnet invents.
            sys::xpc_dictionary_set_bool(desc, sys::vmnet_allocate_mac_address_key, false);
            let queue = sys::dispatch_queue_create(c"ue2-net.vmnet".as_ptr(), ptr::null_mut());
            let interface = sys::vmnet_start_interface(desc, queue, &handler);
            sys::xpc_release(desc);
            (interface, queue)
        };
        let outcome = completion.recv_timeout(START_TIMEOUT);
        let (mtu, max_packet_size) = match outcome {
            Ok((sys::VMNET_SUCCESS, Some(sizes))) if !interface.is_null() => sizes,
            // SAFETY: nothing was started, the queue has no other user.
            failed => unsafe {
                sys::dispatch_release(queue);
                match failed {
                    Ok((status, _)) => bail!(start_error(ifname, status, Privileges::current())),
                    Err(_) => {
                        bail!("vmnet did not complete starting bridged mode on {ifname} within {START_TIMEOUT:?}")
                    }
                }
            },
        };
        // From here on, dropping `net` stops the interface.
        let net = VmnetBridged {
            interface,
            queue,
            pending: Arc::new(AtomicBool::new(false)),
            buffers: vec![vec![0; max_packet_size as usize]; READ_BATCH],
            mtu,
            warned: false,
        };
        let pending = net.pending.clone();
        let on_event =
            RcBlock::new(move |_events: u32, _info: sys::xpc_object_t| pending.store(true, Ordering::Relaxed));
        // SAFETY: the interface is started; vmnet copies the block.
        let status = unsafe {
            sys::vmnet_interface_set_event_callback(
                net.interface,
                sys::VMNET_INTERFACE_PACKETS_AVAILABLE,
                net.queue,
                Some(&on_event),
            )
        };
        if status != sys::VMNET_SUCCESS {
            bail!("vmnet event callback on {ifname}: {}", status_text(status));
        }
        Ok(net)
    }

    /// MTU of the bridged interface as vmnet reports it.
    pub fn mtu(&self) -> u64 {
        self.mtu
    }

    fn warn_once(&mut self, call: &str, status: sys::vmnet_return_t) {
        if !self.warned {
            eprintln!("net: {call}: {} (further errors are not shown)", status_text(status));
            self.warned = true;
        }
    }
}

impl NetBackend for VmnetBridged {
    fn send(&mut self, frame: &[u8]) {
        let mut iov = iovec { iov_base: frame.as_ptr().cast_mut().cast(), iov_len: frame.len() };
        let mut packet =
            sys::vmpktdesc { vm_pkt_size: frame.len(), vm_pkt_iov: &mut iov, vm_pkt_iovcnt: 1, vm_flags: 0 };
        let mut count: c_int = 1;
        // SAFETY: one packet whose single iovec covers `frame`; vmnet only reads it.
        let status = unsafe { sys::vmnet_write(self.interface, &mut packet, &mut count) };
        if status != sys::VMNET_SUCCESS {
            self.warn_once("vmnet_write", status);
        }
    }

    /// Reads only after the event callback announced packets, then drains the interface batch by batch.
    fn poll(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        if !self.pending.swap(false, Ordering::Relaxed) {
            return;
        }
        loop {
            let mut iovs: Vec<iovec> =
                self.buffers.iter_mut().map(|b| iovec { iov_base: b.as_mut_ptr().cast(), iov_len: b.len() }).collect();
            let mut packets: Vec<sys::vmpktdesc> = iovs
                .iter_mut()
                .map(|iov| sys::vmpktdesc { vm_pkt_size: iov.iov_len, vm_pkt_iov: iov, vm_pkt_iovcnt: 1, vm_flags: 0 })
                .collect();
            let mut count = READ_BATCH as c_int;
            // SAFETY: `count` descriptors, each with one iovec over a buffer of `vmnet_max_packet_size_key` bytes.
            let status = unsafe { sys::vmnet_read(self.interface, packets.as_mut_ptr(), &mut count) };
            if status != sys::VMNET_SUCCESS {
                return self.warn_once("vmnet_read", status);
            }
            let count = usize::try_from(count).unwrap_or(0);
            for (packet, buffer) in packets.iter().zip(&self.buffers).take(count) {
                deliver(&buffer[..packet.vm_pkt_size.min(buffer.len())]);
            }
            if count < READ_BATCH {
                return;
            }
        }
    }
}

impl Drop for VmnetBridged {
    fn drop(&mut self) {
        let (done, stopped) = mpsc::sync_channel(1);
        let handler = RcBlock::new(move |_status: u32| {
            let _ = done.send(());
        });
        // SAFETY: the interface and queue were created by `start` and are not used after this.
        unsafe {
            sys::vmnet_interface_set_event_callback(
                self.interface,
                sys::VMNET_INTERFACE_PACKETS_AVAILABLE,
                ptr::null_mut(),
                None,
            );
            if sys::vmnet_stop_interface(self.interface, self.queue, &handler) == sys::VMNET_SUCCESS {
                let _ = stopped.recv_timeout(STOP_TIMEOUT);
            }
            sys::dispatch_release(self.queue);
        }
    }
}

/// Host interfaces vmnet can bridge onto (`vmnet_copy_shared_interface_list`; needs no privileges).
pub fn interfaces() -> Vec<String> {
    // SAFETY: the list is an xpc array of C strings owned by the list, released after copying them out.
    unsafe {
        let list = sys::vmnet_copy_shared_interface_list();
        if list.is_null() {
            return Vec::new();
        }
        let names = (0..sys::xpc_array_get_count(list))
            .map(|i| sys::xpc_array_get_string(list, i))
            .filter(|name| !name.is_null())
            .map(|name| CStr::from_ptr(name).to_string_lossy().into_owned())
            .collect();
        sys::xpc_release(list);
        names
    }
}

/// Interface of the IPv4 default route (`route -n get default`): the dock's Ethernet or Wi-Fi, whichever macOS
/// currently routes through.
pub fn default_route_interface() -> Result<String> {
    let out = Command::new("/sbin/route").args(["-n", "get", "default"]).output().context("running /sbin/route")?;
    route_interface(&String::from_utf8_lossy(&out.stdout))
        .map(str::to_owned)
        .with_context(|| format!("no IPv4 default route; name the interface (bridgeable: {})", interfaces().join(", ")))
}

/// The `interface:` line of `route -n get` output.
fn route_interface(output: &str) -> Option<&str> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// What decides whether vmnet lets this process start a bridged interface.
struct Privileges {
    euid: u32,
    entitled: bool,
}

impl Privileges {
    fn current() -> Self {
        // SAFETY: geteuid cannot fail.
        Privileges { euid: unsafe { libc::geteuid() }, entitled: has_entitlement(VM_NETWORKING_ENTITLEMENT) }
    }
}

/// Error for a completed but failed `vmnet_start_interface`.
fn start_error(ifname: &str, status: sys::vmnet_return_t, privileges: Privileges) -> String {
    let status = status_text(status);
    if privileges.euid == 0 || privileges.entitled {
        format!("vmnet could not start bridged mode on {ifname}: {status}")
    } else {
        format!(
            "vmnet refused bridged mode on {ifname} ({status}): it needs root or the com.apple.vm.networking \
             entitlement, and this process runs as uid {} without it. Run ue2emu with sudo, or use \
             --net socket-vmnet with a bridged socket_vmnet daemon (docs/status/network.md)",
            privileges.euid
        )
    }
}

/// `vmnet_return_t` name and its vmnet.h description.
fn status_text(status: sys::vmnet_return_t) -> String {
    let (name, meaning) = match status {
        1000 => ("VMNET_SUCCESS", "successfully completed"),
        1001 => ("VMNET_FAILURE", "general failure"),
        1002 => ("VMNET_MEM_FAILURE", "memory allocation failure"),
        1003 => ("VMNET_INVALID_ARGUMENT", "invalid argument specified"),
        1004 => ("VMNET_SETUP_INCOMPLETE", "interface setup is not complete"),
        1005 => ("VMNET_INVALID_ACCESS", "permission denied"),
        1006 => ("VMNET_PACKET_TOO_BIG", "packet size larger than MTU"),
        1007 => ("VMNET_BUFFER_EXHAUSTED", "buffers exhausted in kernel"),
        1008 => ("VMNET_TOO_MANY_PACKETS", "packet count exceeds limit"),
        1009 => ("VMNET_SHARING_SERVICE_BUSY", "a conflicting sharing service is in use"),
        1010 => ("VMNET_NOT_AUTHORIZED", "missing authorization"),
        _ => return format!("vmnet status {status}"),
    };
    format!("{name} ({meaning})")
}

/// True when the process carries `entitlement` with the value true.
fn has_entitlement(entitlement: &CStr) -> bool {
    // SAFETY: Core Foundation create/copy calls; every non-null result is released once.
    unsafe {
        let task = sys::SecTaskCreateFromSelf(ptr::null());
        let key = sys::CFStringCreateWithCString(ptr::null(), entitlement.as_ptr(), sys::kCFStringEncodingUTF8);
        let value = if task.is_null() || key.is_null() {
            ptr::null()
        } else {
            sys::SecTaskCopyValueForEntitlement(task, key, ptr::null_mut())
        };
        let entitled = !value.is_null() && value == sys::kCFBooleanTrue;
        for object in [value, key, task] {
            if !object.is_null() {
                sys::CFRelease(object);
            }
        }
        entitled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_output_names_the_interface() {
        let out = "   route to: default\ndestination: default\n       mask: default\n    gateway: 10.0.0.1\n  \
                   interface: en7\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(route_interface(out), Some("en7"));
        assert_eq!(route_interface("route: writing to routing socket: not in table\n"), None);
    }

    #[test]
    fn start_errors_explain_privileges() {
        let user = start_error("en0", 1001, Privileges { euid: 501, entitled: false });
        assert!(user.contains("VMNET_FAILURE (general failure)") && user.contains("needs root or the"), "{user}");
        assert!(user.contains("uid 501") && user.contains("--net socket-vmnet"), "{user}");
        let root = start_error("en0", 1003, Privileges { euid: 0, entitled: false });
        assert_eq!(
            root,
            "vmnet could not start bridged mode on en0: VMNET_INVALID_ARGUMENT (invalid argument specified)"
        );
        assert_eq!(status_text(4242), "vmnet status 4242");
    }

    #[test]
    fn unknown_interface_is_refused_with_the_bridgeable_list() {
        let err = VmnetBridged::start("ue2-nosuch0").err().unwrap().to_string();
        assert!(err.starts_with("vmnet cannot bridge 'ue2-nosuch0'; bridgeable interfaces:"), "{err}");
    }

    /// The real vmnet call without privileges: it must fail and say why. Skipped as root, where it would bridge.
    #[test]
    fn start_without_privileges_explains_root_or_entitlement() {
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 || has_entitlement(VM_NETWORKING_ENTITLEMENT) {
            eprintln!("skipped: running privileged");
            return;
        }
        let Some(ifname) = interfaces().into_iter().next() else {
            eprintln!("skipped: no bridgeable interface");
            return;
        };
        let err = VmnetBridged::start(&ifname).err().unwrap().to_string();
        assert!(err.contains("needs root or the com.apple.vm.networking entitlement"), "{err}");
    }
}
