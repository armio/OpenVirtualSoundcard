//! Network interface discovery and socket helpers.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

use crate::config::parse_interface_ip;
use crate::{Error, Result};

/// The network interface the device runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub mac: [u8; 6],
    pub link_speed_mbps: u16,
}

/// How long after boot an interface that only has a link-local (169.254.0.0/16)
/// address is still "not ready": DHCP usually answers within that time, and
/// an engine started on a provisional address has to be moved later.
pub const LINK_LOCAL_AFTER_BOOT_S: u64 = 120;

/// Whether the engine may start on a link-local address when the system has
/// been up for `uptime`. A direct cable with no DHCP keeps working; it only
/// starts up to [`LINK_LOCAL_AFTER_BOOT_S`] seconds after boot.
pub fn link_local_allowed(uptime: Duration) -> bool {
    uptime >= Duration::from_secs(LINK_LOCAL_AFTER_BOOT_S)
}

/// How long the system has been up, if it says. Only macOS is asked
/// (`sysctl kern.boottime`); elsewhere, and when the system does not say,
/// `None` (the engine does not wait).
#[cfg(target_vendor = "apple")]
pub fn boot_uptime() -> Option<Duration> {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>();
    // SAFETY: a NUL-terminated name and a buffer of `len` bytes for a timeval.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut tv as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != std::mem::size_of::<libc::timeval>() || tv.tv_sec <= 0 {
        return None;
    }
    let booted = std::time::UNIX_EPOCH + Duration::new(tv.tv_sec as u64, 0);
    std::time::SystemTime::now().duration_since(booted).ok()
}

/// See the macOS version.
#[cfg(not(target_vendor = "apple"))]
pub fn boot_uptime() -> Option<Duration> {
    None
}

/// The address to run on among `addrs`: the first one outside
/// 169.254.0.0/16; if there is none and `allow_link_local`, the first
/// link-local one; else none.
pub fn pick_ipv4(addrs: &[Ipv4Addr], allow_link_local: bool) -> Option<Ipv4Addr> {
    addrs
        .iter()
        .find(|a| !a.is_link_local())
        .or_else(|| if allow_link_local { addrs.first() } else { None })
        .copied()
}

/// The address [`Interface::find_ready`] runs on, among `addrs` of the
/// interface `name`: `chosen` if the person gave an IP, else
/// [`pick_ipv4`], or why there is none yet.
fn select_ip(
    name: &str,
    addrs: &[Ipv4Addr],
    chosen: Option<Ipv4Addr>,
    allow_link_local: bool,
) -> Result<Ipv4Addr> {
    if let Some(ip) = chosen {
        return Ok(ip);
    }
    pick_ipv4(addrs, allow_link_local).ok_or_else(|| match addrs.first() {
        None => Error::Config(format!("interface {name} has no IPv4 address")),
        Some(first) => Error::NotReady(format!(
            "{name} has only the link-local address {first}; waiting for a routable one \
             (DHCP) until {LINK_LOCAL_AFTER_BOOT_S} s after boot"
        )),
    })
}

impl Interface {
    /// Finds the interface named `spec`, the interface holding the IPv4
    /// address `spec`, or (if `spec` is empty) the default-route interface.
    /// An interface with a routable and a link-local address gets the
    /// routable one; one with only a link-local address gets that.
    pub fn find(spec: &str) -> Result<Self> {
        Self::find_ready(spec, true)
    }

    /// Like [`Interface::find`], but with `allow_link_local` false an
    /// interface that has only link-local addresses is not ready:
    /// [`Error::NotReady`]. Ignored when `spec` is an IP address: the person
    /// chose that address.
    pub fn find_ready(spec: &str, allow_link_local: bool) -> Result<Self> {
        let interfaces = netdev::get_interfaces();
        let iface = if spec.is_empty() {
            netdev::get_default_interface()
                .map_err(|e| Error::Config(format!("no default network interface: {e}")))?
        } else if let Some(ip) = parse_interface_ip(spec) {
            if let Some(found) =
                interfaces.iter().find(|i| i.ipv4.iter().any(|n| n.addr() == ip)).cloned()
            {
                found
            } else if ip.is_loopback() {
                // Loopback addresses other than 127.0.0.1 are usually not
                // listed but still usable (tests).
                return Ok(Self::loopback(ip));
            } else {
                return Err(Error::Config(format!("no interface has address {ip}")));
            }
        } else {
            interfaces
                .iter()
                .find(|i| i.name == spec || i.friendly_name.as_deref() == Some(spec))
                .cloned()
                .ok_or_else(|| Error::Config(format!("no network interface named {spec:?}")))?
        };

        let addrs: Vec<Ipv4Addr> = iface.ipv4.iter().map(|n| n.addr()).collect();
        let ip = select_ip(&iface.name, &addrs, parse_interface_ip(spec), allow_link_local)?;
        let netmask = iface
            .ipv4
            .iter()
            .find(|n| n.addr() == ip)
            .map(|n| n.netmask())
            .unwrap_or(Ipv4Addr::UNSPECIFIED);
        let gateway = iface
            .gateway
            .as_ref()
            .and_then(|g| g.ipv4.first().copied())
            .unwrap_or(Ipv4Addr::UNSPECIFIED);
        let speed_bps = iface.transmit_speed.into_iter().chain(iface.receive_speed).max();
        Ok(Self {
            name: iface.name.clone(),
            ip,
            netmask,
            gateway,
            mac: iface.mac_addr.map(|m| m.octets()).unwrap_or_default(),
            link_speed_mbps: speed_bps.map_or(1000, |b| (b / 1_000_000).clamp(10, 65_535) as u16),
        })
    }

    fn loopback(ip: Ipv4Addr) -> Self {
        Self {
            name: "lo".into(),
            ip,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            mac: [0; 6],
            link_speed_mbps: 1000,
        }
    }
}

fn udp_socket(reuse: bool) -> io::Result<Socket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if reuse {
        s.set_reuse_address(true)?;
        #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
        s.set_reuse_port(true)?;
    }
    Ok(s)
}

/// A network interface with an IPv4 address, for choosing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetInterface {
    pub name: String,
    /// What the system calls it, if it says.
    pub description: Option<String>,
    pub ipv4: Vec<Ipv4Addr>,
    /// Whether it carries the default route.
    pub default_route: bool,
}

/// What an interface counted since it came up. The counters may wrap: on
/// macOS they are 32 bits wide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterfaceCounters {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_errors: u64,
    pub rx_errors: u64,
}

/// The counters of interface `name`, if the system has them.
#[cfg(target_vendor = "apple")]
pub fn interface_counters(name: &str) -> Option<InterfaceCounters> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned, alive until
        // freeifaddrs.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_data.is_null() {
            continue;
        }
        // SAFETY: ifa_addr points to a sockaddr; ifa_name to a C string.
        let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
        let ifname = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
        if family != libc::AF_LINK || ifname.to_bytes() != name.as_bytes() {
            continue;
        }
        // SAFETY: for AF_LINK entries ifa_data points to the link's if_data.
        let d = unsafe { &*(ifa.ifa_data as *const libc::if_data) };
        found = Some(InterfaceCounters {
            tx_bytes: u64::from(d.ifi_obytes),
            rx_bytes: u64::from(d.ifi_ibytes),
            tx_errors: u64::from(d.ifi_oerrors),
            rx_errors: u64::from(d.ifi_ierrors),
        });
        break;
    }
    // SAFETY: the list getifaddrs returned, not used after this.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The counters of interface `name`, if the system has them.
#[cfg(target_os = "linux")]
pub fn interface_counters(name: &str) -> Option<InterfaceCounters> {
    let read = |what: &str| -> Option<u64> {
        let path = format!("/sys/class/net/{name}/statistics/{what}");
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    };
    Some(InterfaceCounters {
        tx_bytes: read("tx_bytes")?,
        rx_bytes: read("rx_bytes")?,
        tx_errors: read("tx_errors")?,
        rx_errors: read("rx_errors")?,
    })
}

/// The counters of interface `name`, if the system has them.
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
pub fn interface_counters(_name: &str) -> Option<InterfaceCounters> {
    None
}

/// How much counter `now` grew since `before`, across one wrap of a 32-bit
/// counter.
pub fn counter_delta(before: u64, now: u64) -> u64 {
    if now >= before { now - before } else { now + (1 << 32) - before }
}

/// The interfaces a device could run on: up, not loopback, with IPv4.
pub fn list_interfaces() -> Vec<NetInterface> {
    let default = netdev::get_default_interface().ok().map(|i| i.name);
    netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.is_up() && !i.is_loopback() && !i.ipv4.is_empty())
        .map(|i| NetInterface {
            default_route: default.as_deref() == Some(i.name.as_str()),
            description: i.friendly_name.clone().filter(|d| !d.is_empty() && *d != i.name),
            ipv4: i.ipv4.iter().map(|n| n.addr()).collect(),
            name: i.name,
        })
        .collect()
}

/// Binds a UDP socket to `ip:port` (port 0 = ephemeral) for unicast traffic,
/// configured to send multicast out of `ip`'s interface.
pub fn bind_udp(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let s = udp_socket(false)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(ip, port)).into()).map_err(|e| annotate(e, port))?;
    if !ip.is_loopback() && !ip.is_unspecified() {
        let _ = s.set_multicast_if_v4(&ip);
    }
    s.set_multicast_ttl_v4(1)?;
    Ok(s.into())
}

/// The IPv4 type-of-service byte of audio packets: DSCP 46, Expedited
/// Forwarding, with ECN off. Audinate's published QoS settings for Dante
/// networks put audio at EF (and PTP at CS7). Switches with DSCP-based
/// priority queues forward EF ahead of bulk traffic, so an unmarked audio
/// packet waits behind file copies and video on a shared or congested
/// link while marked Dante hardware on the same path does not. Networks
/// without QoS ignore the marking.
pub const MEDIA_TOS: u32 = 46 << 2;

/// Marks the packets `socket` sends as audio ([`MEDIA_TOS`]).
#[cfg(not(any(
    target_os = "fuchsia",
    target_os = "redox",
    target_os = "solaris",
    target_os = "haiku",
    target_os = "wasi",
)))]
pub fn mark_media(socket: &UdpSocket) -> io::Result<()> {
    socket2::SockRef::from(socket).set_tos_v4(MEDIA_TOS)
}

/// Marks the packets `socket` sends as audio: not supported on this OS.
#[cfg(any(
    target_os = "fuchsia",
    target_os = "redox",
    target_os = "solaris",
    target_os = "haiku",
    target_os = "wasi",
))]
pub fn mark_media(_socket: &UdpSocket) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Binds `0.0.0.0:port` with address reuse and joins `group` on the
/// interface `iface_ip`.
pub fn bind_multicast(group: Ipv4Addr, port: u16, iface_ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = udp_socket(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())
        .map_err(|e| annotate(e, port))?;
    s.join_multicast_v4(&group, &iface_ip)?;
    s.set_multicast_if_v4(&iface_ip)?;
    s.set_multicast_loop_v4(true)?;
    s.set_multicast_ttl_v4(255)?;
    Ok(s.into())
}

/// Converts a std socket for use with tokio.
pub fn into_tokio(s: UdpSocket) -> io::Result<tokio::net::UdpSocket> {
    s.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(s)
}

fn annotate(e: io::Error, port: u16) -> io::Error {
    let hint = match e.kind() {
        io::ErrorKind::AddrInUse => {
            " (another Dante device or service holds it: Dante Virtual Soundcard, Dante Via, \
             Audinate's ConMon (conmon_cm) or another OpenVirtualSoundcard instance; [device.ports] moves \
             the device's ports)"
        }
        io::ErrorKind::PermissionDenied if port < 1024 => {
            " (ports below 1024 need root or CAP_NET_BIND_SERVICE)"
        }
        _ => "",
    };
    io::Error::new(e.kind(), format!("cannot bind UDP port {port}: {e}{hint}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_loopback_interface_has_counters() {
        let name = if cfg!(target_vendor = "apple") { "lo0" } else { "lo" };
        if cfg!(any(target_vendor = "apple", target_os = "linux")) {
            assert!(interface_counters(name).is_some());
        }
        assert_eq!(interface_counters("no-such-interface"), None);
        assert_eq!(counter_delta(10, 15), 5);
        // A 32-bit counter that wrapped.
        assert_eq!(counter_delta(u64::from(u32::MAX) - 1, 3), 5);
    }

    use super::*;

    #[test]
    fn loopback_interface_by_address() {
        let iface = Interface::find("127.0.0.1").unwrap();
        assert_eq!(iface.ip, Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn unknown_interface_is_a_config_error() {
        assert!(matches!(Interface::find("no-such-if0"), Err(Error::Config(_))));
    }

    #[test]
    fn ephemeral_bind() {
        let s = bind_udp(Ipv4Addr::LOCALHOST, 0).unwrap();
        assert_ne!(s.local_addr().unwrap().port(), 0);
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn media_sockets_are_marked_expedited_forwarding() {
        let s = bind_udp(Ipv4Addr::LOCALHOST, 0).unwrap();
        assert_eq!(socket2::SockRef::from(&s).tos_v4().unwrap(), 0, "unmarked by default");
        mark_media(&s).unwrap();
        assert_eq!(socket2::SockRef::from(&s).tos_v4().unwrap(), 0xb8);
        // The marking survives what a flow socket does next.
        s.connect((Ipv4Addr::LOCALHOST, 9)).unwrap();
        s.set_nonblocking(true).unwrap();
        assert_eq!(socket2::SockRef::from(&s).tos_v4().unwrap(), 0xb8);
    }

    #[test]
    fn a_second_socket_on_the_same_address_conflicts() {
        let first = bind_udp(Ipv4Addr::LOCALHOST, 0).unwrap();
        let port = first.local_addr().unwrap().port();
        let e = bind_udp(Ipv4Addr::LOCALHOST, port).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
    }

    const LL: Ipv4Addr = Ipv4Addr::new(169, 254, 7, 9);
    const ROUTABLE: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 44);

    #[test]
    fn pick_ipv4_takes_a_link_local_address_only_when_allowed() {
        assert_eq!(pick_ipv4(&[LL], true), Some(LL));
        assert_eq!(pick_ipv4(&[LL], false), None);
    }

    #[test]
    fn pick_ipv4_prefers_the_routable_address_in_either_order() {
        for allow in [true, false] {
            assert_eq!(pick_ipv4(&[LL, ROUTABLE], allow), Some(ROUTABLE));
            assert_eq!(pick_ipv4(&[ROUTABLE, LL], allow), Some(ROUTABLE));
            assert_eq!(pick_ipv4(&[ROUTABLE], allow), Some(ROUTABLE));
            assert_eq!(pick_ipv4(&[], allow), None);
        }
    }

    #[test]
    fn link_local_is_allowed_from_120_s_after_boot() {
        assert!(!link_local_allowed(Duration::ZERO));
        assert!(!link_local_allowed(Duration::from_secs(119)));
        assert!(link_local_allowed(Duration::from_secs(120)));
        assert!(link_local_allowed(Duration::from_secs(86_400)));
    }

    #[test]
    fn find_prefers_the_routable_address_when_both_are_listed() {
        // `Interface::find` is `find_ready` with link-local allowed.
        assert_eq!(select_ip("en9", &[LL, ROUTABLE], None, true).unwrap(), ROUTABLE);
        assert_eq!(select_ip("en9", &[ROUTABLE, LL], None, true).unwrap(), ROUTABLE);
        assert_eq!(select_ip("en9", &[LL], None, true).unwrap(), LL);
    }

    #[test]
    fn a_link_local_only_interface_is_not_ready_until_allowed() {
        let e = select_ip("en9", &[LL], None, false).unwrap_err();
        assert!(matches!(e, Error::NotReady(_)), "{e:?}");
        assert_eq!(
            e.to_string(),
            "en9 has only the link-local address 169.254.7.9; waiting for a routable one (DHCP) until 120 s after boot"
        );
        assert!(matches!(select_ip("en9", &[], None, false), Err(Error::Config(_))));
        // The person's own address is never waited for.
        assert_eq!(select_ip("en9", &[LL], Some(LL), false).unwrap(), LL);
    }

    #[test]
    fn find_ready_with_an_ip_ignores_the_link_local_rule() {
        let iface = Interface::find_ready("127.0.0.1", false).unwrap();
        assert_eq!(iface.ip, Ipv4Addr::LOCALHOST);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_boot_uptime_is_read_on_macos() {
        let up = boot_uptime().expect("kern.boottime");
        assert!(up > Duration::from_secs(1), "{up:?}");
    }
}
