//! PTP traffic read straight from the network interface, for when another
//! program holds the PTP ports for itself.
//!
//! The follower normally opens UDP 319 and 320 to share them
//! (`SO_REUSEADDR`, `SO_REUSEPORT`), but a system lets two sockets share a
//! port only when both ask to. On a Mac, AirPlay's own clock (PTP too, on the
//! same address and ports) does not, and the follower cannot bind them while
//! AirPlay Receiver is on. This module reads the PTP datagrams from the
//! interface instead, as a packet capture does: BPF (`/dev/bpf*`) on macOS,
//! a packet socket (`AF_PACKET`) on Linux. Both need root, which the daemon
//! has. A filter in the kernel passes only UDP datagrams to the PTP group's
//! two ports, so nothing else is copied out. The follower sends its
//! Delay_Reqs from a socket on a port of its own: a PTPv1 leader answers to
//! the group, not to the sender's port.
//!
//! On macOS each datagram carries BPF's receive time, taken by the kernel as
//! the frame arrives, in microseconds of the wall clock; it is moved onto
//! [`crate::local_now_ns`]'s clock with the two clocks' difference at the time of
//! the read. On Linux, as for the ports, the time is read as the datagram is
//! received.

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Range;

/// A PTP datagram read from the interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Datagram {
    /// The UDP port it was sent to: the event or the general port.
    pub port: u16,
    pub src: SocketAddr,
    /// When it arrived, on [`crate::local_now_ns`]'s clock.
    pub at: u64,
    pub data: Vec<u8>,
}

/// What the filter passes: UDP datagrams to `group` on either port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wanted {
    pub group: Ipv4Addr,
    pub event_port: u16,
    pub general_port: u16,
}

/// One instruction of a classic BPF program, as both `<net/bpf.h>`
/// (`struct bpf_insn`) and Linux (`struct sock_filter`) lay it out.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

// Classic BPF opcodes.
const LD_W_ABS: u16 = 0x20;
const LD_H_ABS: u16 = 0x28;
const LD_B_ABS: u16 = 0x30;
const LD_H_IND: u16 = 0x48;
const LDX_B_MSH: u16 = 0xb1;
const JEQ_K: u16 = 0x15;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

/// How many bytes of a datagram the filter passes: all of it.
const SNAP_LEN: u32 = 0xffff;

/// Where a filter step goes next.
#[derive(Clone, Copy)]
enum To {
    Next,
    Accept,
    Drop,
}

/// The filter: IPv4, UDP, not a fragment, to `wanted.group` on one of its
/// ports. `link` is the length of the link-layer header before the IPv4
/// header: 14 for Ethernet (whose type must then be IPv4), 0 when the
/// packet starts at the IPv4 header (a Linux `SOCK_DGRAM` packet socket,
/// bound to IPv4 already).
pub(crate) fn filter(link: u32, wanted: Wanted) -> Vec<Insn> {
    let mut steps: Vec<(u16, u32, To, To)> = Vec::new();
    if link > 0 {
        steps.push((LD_H_ABS, link - 2, To::Next, To::Next)); // EtherType
        steps.push((JEQ_K, 0x0800, To::Next, To::Drop));
    }
    steps.push((LD_B_ABS, link + 9, To::Next, To::Next)); // protocol
    steps.push((JEQ_K, 17, To::Next, To::Drop));
    steps.push((LD_W_ABS, link + 16, To::Next, To::Next)); // destination
    steps.push((JEQ_K, u32::from(wanted.group), To::Next, To::Drop));
    steps.push((LD_H_ABS, link + 6, To::Next, To::Next)); // fragment offset
    steps.push((JSET_K, 0x1fff, To::Drop, To::Next));
    steps.push((LDX_B_MSH, link, To::Next, To::Next)); // X = IPv4 header length
    steps.push((LD_H_IND, link + 2, To::Next, To::Next)); // UDP destination port
    steps.push((JEQ_K, u32::from(wanted.event_port), To::Accept, To::Next));
    steps.push((JEQ_K, u32::from(wanted.general_port), To::Accept, To::Drop));
    let accept = steps.len();
    let drop = accept + 1;
    let jump = |at: usize, to: To| -> u8 {
        let target = match to {
            To::Next => at + 1,
            To::Accept => accept,
            To::Drop => drop,
        };
        u8::try_from(target - at - 1).expect("a short program")
    };
    let mut program: Vec<Insn> = steps
        .iter()
        .enumerate()
        .map(|(at, &(code, k, jt, jf))| Insn { code, jt: jump(at, jt), jf: jump(at, jf), k })
        .collect();
    program.push(Insn { code: RET_K, jt: 0, jf: 0, k: SNAP_LEN });
    program.push(Insn { code: RET_K, jt: 0, jf: 0, k: 0 });
    program
}

/// The UDP datagram in an IPv4 `packet` (starting at its IPv4 header) to
/// `wanted`: the port it was sent to, its sender, and where its payload
/// lies in `packet`. The kernel's filter passes only such packets; this
/// checks again, and the lengths, before trusting them.
pub(crate) fn parse(packet: &[u8], wanted: Wanted) -> Option<(u16, SocketAddr, Range<usize>)> {
    let byte = |i: usize| packet.get(i).copied();
    let u16_at = |i: usize| Some(u16::from_be_bytes([byte(i)?, byte(i + 1)?]));
    let version_ihl = byte(0)?;
    let ihl = usize::from(version_ihl & 0x0f) * 4;
    if version_ihl >> 4 != 4 || ihl < 20 || byte(9)? != 17 {
        return None;
    }
    let total = usize::from(u16_at(2)?);
    if u16_at(6)? & 0x3fff != 0 {
        return None; // a fragment, or more to come
    }
    let ip = |i: usize| Some(Ipv4Addr::new(byte(i)?, byte(i + 1)?, byte(i + 2)?, byte(i + 3)?));
    if ip(16)? != wanted.group {
        return None;
    }
    let src = SocketAddrV4::new(ip(12)?, u16_at(ihl)?);
    let port = u16_at(ihl + 2)?;
    if port != wanted.event_port && port != wanted.general_port {
        return None;
    }
    let udp_len = usize::from(u16_at(ihl + 4)?);
    let end = (ihl + udp_len).min(total).min(packet.len());
    let start = ihl + 8;
    (udp_len >= 8 && start <= end).then_some((port, src.into(), start..end))
}

/// A capture of the PTP datagrams on one interface.
pub(crate) struct Capture {
    handle: platform::Handle,
    wanted: Wanted,
    /// Datagrams read in one batch and not taken yet.
    queue: VecDeque<Datagram>,
}

impl Capture {
    /// Starts capturing on the interface with the IPv4 address `interface`.
    pub fn open(interface: Ipv4Addr, wanted: Wanted) -> io::Result<Capture> {
        let name = interface_name(interface)?;
        let handle = platform::Handle::open(&name, wanted).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot read PTP from interface {name}: {e}"))
        })?;
        Ok(Capture { handle, wanted, queue: VecDeque::new() })
    }

    /// The next datagram. Cancel-safe: a batch read is queued before any
    /// datagram is handed out.
    pub async fn recv(&mut self) -> io::Result<Datagram> {
        loop {
            if let Some(d) = self.queue.pop_front() {
                return Ok(d);
            }
            self.handle.read(self.wanted, &mut self.queue).await?;
        }
    }
}

/// The name of the interface that has the IPv4 address `ip`.
fn interface_name(ip: Ipv4Addr) -> io::Result<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: a valid out-pointer; the list is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut found = None;
    let mut entry = list;
    // SAFETY: getifaddrs returned a linked list of valid entries, each with
    // a NUL-terminated name and, when not null, an address of its family.
    unsafe {
        while !entry.is_null() {
            let addr = (*entry).ifa_addr;
            if !addr.is_null() && i32::from((*addr).sa_family) == libc::AF_INET {
                let sin = &*addr.cast::<libc::sockaddr_in>();
                if Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)) == ip {
                    let name = std::ffi::CStr::from_ptr((*entry).ifa_name);
                    found = Some(name.to_string_lossy().into_owned());
                    break;
                }
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no interface has the address {ip}"))
    })
}

/// Hands `fd` to the runtime, for reads when it is ready.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn register(
    fd: std::os::fd::OwnedFd,
) -> io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    // SAFETY: the AsyncFd owns `fd`, which stays open, and the same, until
    // the AsyncFd is dropped.
    Ok(unsafe { tokio::io::unix::AsyncFd::register(fd) }?)
}

/// Makes `fd` non-blocking.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: plain fcntl calls on an open descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The records of a BPF read buffer (macOS layout): each record's kernel
/// receive time in microseconds of the wall clock, and its captured bytes.
/// Each record is a `struct bpf_hdr` (a 32-bit `timeval`, the captured
/// and original lengths, and the header's own length), the bytes, and
/// padding to a multiple of 4.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) fn bpf_records(buf: &[u8]) -> Vec<(u64, &[u8])> {
    let mut records = Vec::new();
    let mut at = 0;
    let u32_at = |i: usize| buf.get(i..i + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
    while at + 18 <= buf.len() {
        let (Some(sec), Some(usec), Some(caplen)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8))
        else {
            break;
        };
        let hdrlen = usize::from(u16::from_ne_bytes([buf[at + 16], buf[at + 17]]));
        let start = at + hdrlen;
        let end = start + caplen as usize;
        if hdrlen < 18 || end > buf.len() {
            break;
        }
        records.push((u64::from(sec) * 1_000_000 + u64::from(usec), &buf[start..end]));
        at = (end + 3) & !3;
    }
    records
}

/// The local time ([`crate::local_now_ns`]) of `wall_us`, a time in microseconds of
/// the wall clock, given both clocks read together just now. A time that
/// is not within the last second (the wall clock was set meanwhile) reads
/// as now.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) fn wall_to_local(wall_us: u64, now_wall_ns: u64, now_local_ns: u64) -> u64 {
    let age = now_wall_ns.checked_sub(wall_us.saturating_mul(1000));
    match age {
        Some(age) if age < 1_000_000_000 => now_local_ns.saturating_sub(age),
        _ => now_local_ns,
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    //! BPF, as libpcap uses it on macOS.

    use std::collections::VecDeque;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    use super::{Datagram, Insn, Wanted, bpf_records, filter, parse, set_nonblocking};

    // <net/bpf.h>, for 64-bit processes.
    const BIOCGBLEN: libc::c_ulong = 0x4004_4266;
    const BIOCSBLEN: libc::c_ulong = 0xc004_4266;
    const BIOCSETF: libc::c_ulong = 0x8010_4267;
    const BIOCGDLT: libc::c_ulong = 0x4004_426a;
    const BIOCSETIF: libc::c_ulong = 0x8020_426c;
    const BIOCIMMEDIATE: libc::c_ulong = 0x8004_4270;
    const BIOCSSEESENT: libc::c_ulong = 0x8004_4277;
    const DLT_EN10MB: u32 = 1;
    /// The buffer asked for: plenty for PTP's few datagrams a second.
    const BUFFER: u32 = 32 * 1024;

    #[repr(C)]
    struct BpfProgram {
        bf_len: libc::c_uint,
        bf_insns: *const Insn,
    }

    pub struct Handle {
        fd: AsyncFd<OwnedFd>,
        buf: Vec<u8>,
    }

    fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, arg: *mut T) -> io::Result<()> {
        // SAFETY: each request is given a pointer to the type <net/bpf.h>
        // declares for it.
        if unsafe { libc::ioctl(fd.as_raw_fd(), request, arg) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The first BPF device not in use.
    fn open_device() -> io::Result<OwnedFd> {
        let mut last = io::Error::from(io::ErrorKind::NotFound);
        for i in 0..256 {
            let path = std::ffi::CString::new(format!("/dev/bpf{i}")).expect("no NUL");
            // SAFETY: a NUL-terminated path.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd >= 0 {
                // SAFETY: a descriptor just opened, owned from here on.
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            last = io::Error::last_os_error();
            match last.raw_os_error() {
                Some(libc::EBUSY) => continue,
                _ => break,
            }
        }
        Err(last)
    }

    impl Handle {
        pub fn open(interface: &str, wanted: Wanted) -> io::Result<Handle> {
            let fd = open_device()?;
            // The buffer's size is set before the interface.
            let mut len = BUFFER;
            ioctl(&fd, BIOCSBLEN, &mut len)?;
            // struct ifreq: the name, then a union of 16 bytes.
            let mut ifr = [0u8; 32];
            let name = interface.as_bytes();
            if name.len() >= libc::IFNAMSIZ {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "interface name too long"));
            }
            ifr[..name.len()].copy_from_slice(name);
            ioctl(&fd, BIOCSETIF, ifr.as_mut_ptr())?;
            let mut dlt: u32 = 0;
            ioctl(&fd, BIOCGDLT, &mut dlt)?;
            if dlt != DLT_EN10MB {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{interface} is not an Ethernet interface (link type {dlt})"),
                ));
            }
            // Each datagram as it arrives, not when the buffer fills; and
            // not our own.
            let mut on: u32 = 1;
            ioctl(&fd, BIOCIMMEDIATE, &mut on)?;
            let mut off: u32 = 0;
            ioctl(&fd, BIOCSSEESENT, &mut off)?;
            let program = filter(14, wanted);
            let mut prog =
                BpfProgram { bf_len: program.len() as libc::c_uint, bf_insns: program.as_ptr() };
            ioctl(&fd, BIOCSETF, &mut prog)?;
            let mut len: u32 = 0;
            ioctl(&fd, BIOCGBLEN, &mut len)?;
            set_nonblocking(fd.as_raw_fd())?;
            Ok(Handle { fd: super::register(fd)?, buf: vec![0; len as usize] })
        }

        /// Reads a batch of records into `queue`.
        pub async fn read(
            &mut self,
            wanted: Wanted,
            queue: &mut VecDeque<Datagram>,
        ) -> io::Result<()> {
            loop {
                let mut guard = self.fd.readable().await?;
                let buf = &mut self.buf;
                // A read must ask for the whole buffer.
                let r = guard.try_io(|fd| {
                    // SAFETY: `buf` is a live buffer of the stated length.
                    let n =
                        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
                });
                let n = match r {
                    Ok(r) => r?,
                    Err(_would_block) => continue,
                };
                let now_local = crate::local_now_ns();
                let now_wall = wall_now_ns();
                for (wall_us, frame) in bpf_records(&self.buf[..n]) {
                    // The IPv4 header follows the Ethernet header.
                    let Some(packet) = frame.get(14..) else { continue };
                    if let Some((port, src, payload)) = parse(packet, wanted) {
                        queue.push_back(Datagram {
                            port,
                            src,
                            at: super::wall_to_local(wall_us, now_wall, now_local),
                            data: packet[payload].to_vec(),
                        });
                    }
                }
                return Ok(());
            }
        }
    }

    fn wall_now_ns() -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid out-pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! A packet socket, bound to the interface, cooked: datagrams start at
    //! their IPv4 header.

    use std::collections::VecDeque;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    use super::{Datagram, Wanted, filter, parse, set_nonblocking};

    const PACKET_OUTGOING: u8 = 4;

    pub struct Handle {
        fd: AsyncFd<OwnedFd>,
        buf: Vec<u8>,
    }

    impl Handle {
        pub fn open(interface: &str, wanted: Wanted) -> io::Result<Handle> {
            let protocol = (libc::ETH_P_IP as u16).to_be();
            // SAFETY: plain socket call.
            let raw = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                    protocol.into(),
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a descriptor just opened, owned from here on.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // The filter first, so that nothing else is queued before it.
            let program = filter(0, wanted);
            let prog = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr() as *mut libc::sock_filter,
            };
            // SAFETY: a sock_fprog pointing at the program, whose
            // instructions have sock_filter's layout.
            let r = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ATTACH_FILTER,
                    (&prog as *const libc::sock_fprog).cast(),
                    std::mem::size_of_val(&prog) as libc::socklen_t,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            let name = std::ffi::CString::new(interface)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad interface name"))?;
            // SAFETY: a NUL-terminated name.
            let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
            if index == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: all-zero is a valid sockaddr_ll.
            let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = protocol;
            addr.sll_ifindex = index as i32;
            // SAFETY: a sockaddr_ll of the stated size.
            let r = unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&addr as *const libc::sockaddr_ll).cast(),
                    std::mem::size_of_val(&addr) as libc::socklen_t,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            set_nonblocking(fd.as_raw_fd())?;
            Ok(Handle { fd: super::register(fd)?, buf: vec![0; 2048] })
        }

        /// Reads the datagrams waiting into `queue`, at least one.
        pub async fn read(
            &mut self,
            wanted: Wanted,
            queue: &mut VecDeque<Datagram>,
        ) -> io::Result<()> {
            loop {
                let mut guard = self.fd.readable().await?;
                let buf = &mut self.buf;
                let r = guard.try_io(|fd| {
                    // SAFETY: all-zero is a valid sockaddr_ll.
                    let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
                    let mut from_len = std::mem::size_of_val(&from) as libc::socklen_t;
                    // SAFETY: `buf` and `from` are live buffers of the stated
                    // lengths.
                    let n = unsafe {
                        libc::recvfrom(
                            fd.as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                            0,
                            (&mut from as *mut libc::sockaddr_ll).cast(),
                            &mut from_len,
                        )
                    };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok((n as usize, from.sll_pkttype))
                    }
                });
                let (n, kind) = match r {
                    Ok(r) => r?,
                    Err(_would_block) => continue,
                };
                let at = crate::local_now_ns();
                // Our own datagrams, on their way out: not news.
                if kind == PACKET_OUTGOING {
                    continue;
                }
                let packet = &self.buf[..n];
                if let Some((port, src, payload)) = parse(packet, wanted) {
                    queue.push_back(Datagram { port, src, at, data: packet[payload].to_vec() });
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod platform {
    use std::collections::VecDeque;
    use std::io;

    use super::{Datagram, Wanted};

    pub enum Handle {}

    impl Handle {
        pub fn open(_: &str, _: Wanted) -> io::Result<Handle> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "reading PTP from the interface works on macOS and Linux only",
            ))
        }

        pub async fn read(&mut self, _: Wanted, _: &mut VecDeque<Datagram>) -> io::Result<()> {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WANTED: Wanted =
        Wanted { group: Ipv4Addr::new(224, 0, 1, 129), event_port: 319, general_port: 320 };

    /// An IPv4 UDP datagram from 192.168.1.10:`sport` to `dst`:`dport`.
    fn packet(dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8], frag: u16) -> Vec<u8> {
        let total = 20 + 8 + payload.len();
        let mut p = vec![0x45, 0];
        p.extend((total as u16).to_be_bytes());
        p.extend([0, 1]);
        p.extend(frag.to_be_bytes());
        p.extend([1, 17, 0, 0]);
        p.extend([192, 168, 1, 10]);
        p.extend(dst.octets());
        p.extend(sport.to_be_bytes());
        p.extend(dport.to_be_bytes());
        p.extend(((8 + payload.len()) as u16).to_be_bytes());
        p.extend([0, 0]);
        p.extend(payload);
        p
    }

    fn ethernet(ip: &[u8], ethertype: u16) -> Vec<u8> {
        let mut f = vec![0x01, 0x00, 0x5e, 0x00, 0x01, 0x81, 2, 0, 0, 0, 0, 1];
        f.extend(ethertype.to_be_bytes());
        f.extend(ip);
        f
    }

    /// Runs a classic BPF program on `packet`, as the kernel does: the
    /// number of bytes it accepts. A load past the end drops the packet.
    fn run(program: &[Insn], packet: &[u8]) -> u32 {
        let load = |at: u32, size: usize| -> Option<u32> {
            let b = packet.get(at as usize..at as usize + size)?;
            Some(b.iter().fold(0u32, |v, &byte| (v << 8) | u32::from(byte)))
        };
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        loop {
            let i = program[pc];
            pc += 1;
            let loaded = match i.code {
                LD_W_ABS => load(i.k, 4),
                LD_H_ABS => load(i.k, 2),
                LD_B_ABS => load(i.k, 1),
                LD_H_IND => load(x + i.k, 2),
                LDX_B_MSH => load(i.k, 1).map(|b| (b & 0xf) * 4),
                _ => Some(0),
            };
            let Some(value) = loaded else { return 0 };
            match i.code {
                LDX_B_MSH => x = value,
                LD_W_ABS | LD_H_ABS | LD_B_ABS | LD_H_IND => a = value,
                JEQ_K => pc += usize::from(if a == i.k { i.jt } else { i.jf }),
                JSET_K => pc += usize::from(if a & i.k != 0 { i.jt } else { i.jf }),
                RET_K => return i.k,
                code => panic!("unexpected opcode {code:#x}"),
            }
        }
    }

    #[test]
    fn the_filter_passes_ptp_and_nothing_else() {
        let group = WANTED.group;
        let other = Ipv4Addr::new(224, 0, 0, 251);
        let ptp = |dport| packet(group, 319, dport, &[1, 2, 3], 0);
        for (link, wrap) in [
            (14, &(|ip: &[u8]| ethernet(ip, 0x0800)) as &dyn Fn(&[u8]) -> Vec<u8>),
            (0, &|ip: &[u8]| ip.to_vec()),
        ] {
            let program = filter(link, WANTED);
            assert!(program.len() < 20);
            assert_eq!(run(&program, &wrap(&ptp(319))), SNAP_LEN);
            assert_eq!(run(&program, &wrap(&ptp(320))), SNAP_LEN);
            assert_eq!(run(&program, &wrap(&ptp(321))), 0, "another port");
            assert_eq!(run(&program, &wrap(&packet(other, 5353, 319, &[], 0))), 0, "another group");
            assert_eq!(
                run(&program, &wrap(&packet(group, 319, 319, &[], 0x2000))),
                SNAP_LEN,
                "the first fragment passes; parse drops it"
            );
            assert_eq!(
                run(&program, &wrap(&packet(group, 319, 319, &[], 0x0010))),
                0,
                "a later fragment"
            );
            let mut tcp = ptp(319);
            tcp[9] = 6;
            assert_eq!(run(&program, &wrap(&tcp)), 0, "not UDP");
            // A longer IPv4 header (options) moves the UDP header.
            let mut opts = ptp(319);
            opts[0] = 0x46;
            opts.splice(20..20, [1, 1, 1, 0]);
            assert_eq!(run(&program, &wrap(&opts)), SNAP_LEN);
        }
        // Not IPv4.
        assert_eq!(run(&filter(14, WANTED), &ethernet(&ptp(319), 0x86dd)), 0);
    }

    #[test]
    fn datagrams_are_parsed_and_checked() {
        let p = packet(WANTED.group, 319, 320, b"follow-up", 0);
        let (port, src, payload) = parse(&p, WANTED).expect("a PTP datagram");
        assert_eq!((port, src), (320, "192.168.1.10:319".parse().unwrap()));
        assert_eq!(&p[payload], b"follow-up");
        // Trailing bytes (Ethernet padding) are not payload.
        let mut padded = packet(WANTED.group, 319, 319, b"sync", 0);
        padded.extend([0; 12]);
        let (_, _, payload) = parse(&padded, WANTED).unwrap();
        assert_eq!(&padded[payload], b"sync");
        // A payload cut short by the capture is what there is.
        let (_, _, payload) = parse(&p[..p.len() - 3], WANTED).unwrap();
        assert_eq!(&p[payload], b"follow");
        for bad in [
            packet(WANTED.group, 319, 321, b"x", 0),
            packet(Ipv4Addr::new(224, 0, 0, 1), 319, 319, b"x", 0),
            packet(WANTED.group, 319, 319, b"x", 0x2000),
            p[..20].to_vec(),
            vec![],
        ] {
            assert!(parse(&bad, WANTED).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn bpf_buffers_are_walked() {
        // Two records: 18-byte headers, padded to 20 (as BPF_WORDALIGN).
        let record = |sec: u32, usec: u32, data: &[u8]| {
            let mut r = Vec::new();
            r.extend(sec.to_ne_bytes());
            r.extend(usec.to_ne_bytes());
            r.extend((data.len() as u32).to_ne_bytes());
            r.extend((data.len() as u32).to_ne_bytes());
            r.extend(20u16.to_ne_bytes());
            r.extend([0, 0]);
            r.extend(data);
            while r.len() % 4 != 0 {
                r.push(0);
            }
            r
        };
        let mut buf = record(1_700_000_000, 250, b"abcde");
        buf.extend(record(1_700_000_001, 5, b"xyz"));
        let records = bpf_records(&buf);
        assert_eq!(
            records,
            vec![(1_700_000_000_000_250, &b"abcde"[..]), (1_700_000_001_000_005, &b"xyz"[..])]
        );
        // The last record's padding may be missing; a record cut short ends
        // the walk.
        assert_eq!(bpf_records(&buf[..buf.len() - 1]).len(), 2);
        assert_eq!(bpf_records(&buf[..buf.len() - 2]).len(), 1);
        assert!(bpf_records(&[]).is_empty());
    }

    #[test]
    fn wall_times_move_onto_the_local_clock() {
        let now_wall = 1_700_000_000_000_000_000;
        let now_local = 5_000_000_000;
        // 300 µs ago.
        assert_eq!(
            wall_to_local(1_700_000_000_000_000 - 300, now_wall, now_local),
            now_local - 300_000
        );
        // In the future, or long ago (the wall clock was set): now.
        assert_eq!(wall_to_local(1_700_000_000_000_010, now_wall, now_local), now_local);
        assert_eq!(wall_to_local(1_600_000_000_000_000, now_wall, now_local), now_local);
    }

    #[test]
    fn interfaces_are_found_by_address() {
        assert_eq!(
            interface_name(Ipv4Addr::LOCALHOST).unwrap(),
            if cfg!(target_vendor = "apple") { "lo0" } else { "lo" }
        );
        assert_eq!(
            interface_name(Ipv4Addr::new(192, 0, 2, 77)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
