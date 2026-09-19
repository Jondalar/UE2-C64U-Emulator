/* libslirp.h's layout for ue2-net's `layout` test (src/ffi.rs, docs/specs/S22-windows.md section 4). The order
 * matches the Rust side. */
#include <stddef.h>
#include <slirp/libslirp.h>

/* The alignment of struct in6_addr, without C11's _Alignof, which MSVC's default C mode lacks. */
struct ue2_in6_align {
    char c;
    struct in6_addr a;
};

const size_t ue2_slirp_layout[20] = {
    offsetof(SlirpConfig, vnetwork),
    offsetof(SlirpConfig, vprefix_addr6),
    offsetof(SlirpConfig, vprefix_len),
    offsetof(SlirpConfig, vhost6),
    offsetof(SlirpConfig, vhostname),
    offsetof(SlirpConfig, vdhcp_start),
    offsetof(SlirpConfig, vnameserver6),
    offsetof(SlirpConfig, vdnssearch),
    offsetof(SlirpConfig, if_mtu),
    offsetof(SlirpConfig, disable_host_loopback),
    offsetof(SlirpConfig, outbound_addr),
    offsetof(SlirpConfig, outbound_addr6),
    offsetof(SlirpConfig, disable_dns),
    offsetof(SlirpConfig, disable_dhcp),
    offsetof(SlirpCb, register_poll_fd),
    offsetof(SlirpCb, notify),
    offsetof(SlirpCb, timer_new_opaque),
    sizeof(struct in_addr),
    sizeof(struct in6_addr),
    offsetof(struct ue2_in6_align, a),
};

#ifdef _WIN32
const size_t ue2_slirp_layout_v6[6] = {
    sizeof(SlirpConfig),
    offsetof(SlirpConfig, mfr_id),
    offsetof(SlirpConfig, oob_eth_addr),
    sizeof(SlirpCb),
    offsetof(SlirpCb, register_poll_socket),
    offsetof(SlirpCb, unregister_poll_socket),
};
#endif
