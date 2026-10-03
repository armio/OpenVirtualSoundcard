//! UDP sockets for PTPv1, shared by the follower and the test master.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

#[cfg(not(target_vendor = "apple"))]
use crate::local_now_ns;

/// Largest datagram we care about; PTPv1 messages are at most 124 bytes but
/// PTPv2 and management messages on the same ports can be longer.
pub(crate) const RECV_BUF_LEN: usize = 1500;

/// Where and how to send and receive PTP traffic.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Endpoints {
    pub interface: Ipv4Addr,
    pub group: Ipv4Addr,
    pub event_port: u16,
    pub general_port: u16,
}

impl Endpoints {
    pub fn event_dest(&self) -> SocketAddr {
        SocketAddrV4::new(self.group, self.event_port).into()
    }

    pub fn general_dest(&self) -> SocketAddr {
        SocketAddrV4::new(self.group, self.general_port).into()
    }

    /// Binds the event and general sockets.
    pub fn bind(&self) -> io::Result<(UdpSocket, UdpSocket)> {
        if !self.group.is_multicast() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PTP group {} is not a multicast address", self.group),
            ));
        }
        if self.event_port == self.general_port {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTP event and general ports must differ",
            ));
        }
        Ok((self.bind_port(self.event_port)?, self.bind_port(self.general_port)?))
    }

    /// Binds `0.0.0.0:port`, shared with other PTP software on the host,
    /// joins the PTP group on our interface and sends multicast through it.
    fn bind_port(&self, port: u16) -> io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Other PTP software (or a second OpenVirtualSoundcard instance) may already
        // listen on these well-known ports; multicast is delivered to all.
        socket.set_reuse_address(true)?;
        #[cfg(all(
            unix,
            not(any(
                target_os = "solaris",
                target_os = "illumos",
                target_os = "cygwin",
                target_os = "nuttx",
            ))
        ))]
        socket.set_reuse_port(true)?;
        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        socket.bind(&addr.into()).map_err(|e| bind_error(port, e))?;
        socket.join_multicast_v4(&self.group, &self.interface).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot join {} on interface {}: {e}", self.group, self.interface),
            )
        })?;
        socket.set_multicast_if_v4(&self.interface)?;
        // Keep loopback on so a master and followers on the same host (tests,
        // development setups) hear each other. Our own packets are ignored by
        // source UUID.
        socket.set_multicast_loop_v4(true)?;
        // PTPv1 is link-local: never let it be routed.
        socket.set_multicast_ttl_v4(1)?;
        #[cfg(target_vendor = "apple")]
        if let Err(e) = kernel_ts::enable(&socket) {
            tracing::warn!("no kernel receive timestamps on PTP port {port}: {e}");
        }
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket.into())
    }
}

fn bind_error(port: u16, e: io::Error) -> io::Error {
    let hint = if e.kind() == io::ErrorKind::PermissionDenied && port < 1024 {
        ". Ports below 1024 are privileged: run as root, or on Linux grant the \
         CAP_NET_BIND_SERVICE capability (e.g. `sudo setcap cap_net_bind_service=+ep <binary>`)"
    } else {
        ""
    };
    io::Error::new(e.kind(), format!("cannot bind UDP port {port}: {e}{hint}"))
}

/// Receives a datagram on a PTP socket, with the local time
/// ([`crate::local_now_ns`]) at which it arrived.
///
/// On macOS the kernel stamps each datagram as it queues it to the socket
/// (`SO_TIMESTAMP_MONOTONIC`, in `mach_absolute_time` ticks: the clock behind
/// `local_now_ns` there), so a late wake-up of the receiving task does not
/// count as clock error. On a busy Mac that wake-up can come milliseconds
/// late. Elsewhere, or if a datagram carries no usable stamp, the time is
/// read as soon as the receive returns.
pub(crate) async fn recv_timestamped(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, u64)> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        let (n, src, stamp) =
            socket.async_io(tokio::io::Interest::READABLE, || kernel_ts::recv(fd, buf)).await?;
        Ok((n, src, kernel_ts::plausible(stamp, crate::local_now_ns())))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let (n, src) = socket.recv_from(buf).await?;
        Ok((n, src, local_now_ns()))
    }
}

/// Kernel receive timestamps on macOS.
#[cfg(target_vendor = "apple")]
mod kernel_ts {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::OnceLock;

    use socket2::Socket;

    // <sys/socket.h>: the option, and the control message it adds, which
    // carries a uint64_t `mach_absolute_time`.
    const SO_TIMESTAMP_MONOTONIC: libc::c_int = 0x0800;
    const SCM_TIMESTAMP_MONOTONIC: libc::c_int = 0x04;
    /// A stamp older than this, or in the future, is not trusted.
    const MAX_AGE_NS: u64 = 1_000_000_000;

    #[repr(C)]
    #[derive(Default)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }

    pub fn enable(socket: &Socket) -> io::Result<()> {
        let on: libc::c_int = 1;
        // SAFETY: a valid socket and an int option value of the right size.
        let r = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_TIMESTAMP_MONOTONIC,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
        if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// One `recvmsg`: the length, the sender and the kernel's receive time
    /// in nanoseconds, if the datagram carries one.
    pub fn recv(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, Option<u64>)> {
        // SAFETY: all-zero is a valid sockaddr_storage and msghdr.
        let mut name: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        // u64 elements keep the control buffer aligned for cmsghdr.
        let mut control = [0u64; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut name as *mut libc::sockaddr_storage).cast();
        msg.msg_namelen = std::mem::size_of_val(&name) as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as libc::socklen_t;
        // SAFETY: every pointer in `msg` points at a live buffer of the
        // stated length.
        let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if name.ss_family as libc::c_int != libc::AF_INET {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an IPv4 sender"));
        }
        // SAFETY: recvmsg wrote an AF_INET address, a sockaddr_in.
        let sin = unsafe { &*(&name as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
        let src = SocketAddr::from((
            Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
            u16::from_be(sin.sin_port),
        ));
        let mut ticks = None;
        // SAFETY: the CMSG macros walk the control data recvmsg wrote, within
        // `msg_controllen`; the payload is read unaligned.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == SCM_TIMESTAMP_MONOTONIC
                {
                    ticks = Some(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<u64>()));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        Ok((n as usize, src, ticks.map(ticks_to_ns)))
    }

    /// `mach_absolute_time` ticks to nanoseconds, as `CLOCK_UPTIME_RAW`
    /// counts them.
    fn ticks_to_ns(ticks: u64) -> u64 {
        static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
        let &(numer, denom) = TIMEBASE.get_or_init(|| {
            let mut info = MachTimebaseInfo::default();
            // SAFETY: a valid out-pointer.
            unsafe { mach_timebase_info(&mut info) };
            if info.denom == 0 { (1, 1) } else { (info.numer as u64, info.denom as u64) }
        });
        (ticks as u128 * numer as u128 / denom as u128) as u64
    }

    /// The kernel's stamp if it is plausible against `now`, else `now`.
    pub fn plausible(stamp: Option<u64>, now: u64) -> u64 {
        match stamp {
            Some(t) if t <= now && now - t < MAX_AGE_NS => t,
            _ => now,
        }
    }

    #[cfg(test)]
    mod tests {
        use std::net::UdpSocket;
        use std::os::fd::AsRawFd;

        use super::*;

        #[test]
        fn datagrams_carry_the_kernel_receive_time() {
            let rx = Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
            rx.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into()).unwrap();
            enable(&rx).unwrap();
            let rx: UdpSocket = rx.into();
            let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let before = crate::local_now_ns();
            tx.send_to(b"ptp", rx.local_addr().unwrap()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            let mut buf = [0u8; 16];
            let (n, src, stamp) = recv(rx.as_raw_fd(), &mut buf).unwrap();
            let after = crate::local_now_ns();
            assert_eq!((&buf[..n], src), (&b"ptp"[..], tx.local_addr().unwrap()));
            let stamp = stamp.expect("no SCM_TIMESTAMP_MONOTONIC");
            // Stamped on arrival, not when read 20 ms later.
            assert!(before <= stamp && stamp + 15_000_000 < after, "{before} {stamp} {after}");
        }

        #[test]
        fn implausible_stamps_fall_back_to_now() {
            assert_eq!(plausible(Some(90), 100), 90);
            assert_eq!(plausible(Some(110), 100), 100);
            assert_eq!(plausible(None, 100), 100);
            assert_eq!(plausible(Some(1), 2_000_000_000), 2_000_000_000);
        }
    }
}

/// The IPv4 address of a datagram's sender, if it is IPv4.
pub(crate) fn source_ipv4(addr: SocketAddr) -> Option<Ipv4Addr> {
    match addr {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(a) => a.ip().to_ipv4_mapped(),
    }
}

/// A small, fast, non-cryptographic PRNG (SplitMix64) for timer jitter.
#[derive(Clone, Debug)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}
