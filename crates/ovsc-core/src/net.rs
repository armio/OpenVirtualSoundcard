//! Network interface discovery and socket helpers.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

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

impl Interface {
    /// Finds the interface named `spec`, the interface holding the IPv4
    /// address `spec`, or (if `spec` is empty) the default-route interface.
    pub fn find(spec: &str) -> Result<Self> {
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

        let net = iface.ipv4.first().ok_or_else(|| {
            Error::Config(format!("interface {} has no IPv4 address", iface.name))
        })?;
        let ip = match parse_interface_ip(spec) {
            Some(ip) => ip,
            None => net.addr(),
        };
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

    #[test]
    fn a_second_socket_on_the_same_address_conflicts() {
        let first = bind_udp(Ipv4Addr::LOCALHOST, 0).unwrap();
        let port = first.local_addr().unwrap().port();
        let e = bind_udp(Ipv4Addr::LOCALHOST, port).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
    }
}
