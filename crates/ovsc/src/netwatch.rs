//! Watches the network interface the engine runs on.
//!
//! The engine binds every socket to the interface's address and joins its
//! multicast groups (PTP, mDNS) on the interface once, at start. None of
//! that follows the interface afterwards:
//!
//! * when its address changes (a DHCP lease replacing a self-assigned
//!   169.254 address, a router handing out a new lease), every socket stays
//!   bound to an address that is gone and mDNS keeps advertising it;
//! * when the interface detaches and attaches again (a USB-C or Thunderbolt
//!   Ethernet adapter or a dock re-enumerating after a knock), the kernel
//!   drops the multicast memberships with it: PTP and mDNS go deaf, the
//!   clock is lost some 35 s later and the device goes silent for good.
//!
//! * when the engine started on a link-local address (a direct cable, or
//!   DHCP slower than the boot wait) and a routable one appears, even next
//!   to it: the self-assigned address is only a stand-in.
//!
//! So the engine loops look at the interface every second
//! ([`InterfaceWatch::check`]) and restart the whole engine (clock included:
//! the PTP follower is bound to the address too) when it came back after
//! vanishing, was attached again, or has another address. While it is gone
//! or has no address (a cable pull), the engine keeps running and waits: a
//! cable that comes back with the same address needs no restart, as long as
//! the clock locks again.

use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use ovsc_clock::{ClockState, MediaClock};
#[cfg(test)]
use ovsc_core::net::select_ip;
use ovsc_core::net::{Interface, boot_uptime, pick_ipv4};

/// How long the clock may stay unlocked after the interface got its address
/// back before the engine restarts anyway: the multicast memberships may
/// have been lost with the link without the interface ever looking gone
/// (an adapter that re-attached between two looks). A healthy clock locks
/// again within a few seconds of the link returning.
pub const RELOCK_GRACE: Duration = Duration::from_secs(15);

/// How long after the interface got its address back a locked clock shows
/// that Sync messages arrive again. Before that it proves nothing: the PTP
/// follower reads Locked until its master timeout (5 s, see
/// `PtpConfig::master_timeout`) after the last Sync it heard, which may
/// have come before the link went down.
const LOCK_EVIDENCE: Duration = Duration::from_secs(6);

/// What the system says about an interface at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The interface's index. A new one means it was detached and attached
    /// again.
    pub index: u32,
    /// Its IPv4 addresses; none while its link is down on macOS.
    pub ipv4: Vec<Ipv4Addr>,
}

/// What the engine should do about its interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// Restart the whole engine, for this reason.
    Restart(String),
}

/// How the engine loops learn about the interface.
#[derive(Clone, Copy, Debug)]
pub enum Probe {
    /// Ask the system. Loopback interfaces, which never change (tests), are
    /// not watched.
    System,
    /// Tests: a made-up view of the interfaces, watched whatever they are.
    #[cfg(test)]
    Fake(fn(&str) -> io::Result<Option<Seen>>),
    /// Tests: made-up addresses and uptime for the engine's start as well.
    #[cfg(test)]
    Scripted(Script),
}

/// A made-up system for [`Probe::Scripted`].
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct Script {
    /// What [`Probe::look`] says.
    pub look: fn(&str) -> io::Result<Option<Seen>>,
    /// The IPv4 addresses the interface has when the engine starts.
    pub addrs: fn() -> Vec<Ipv4Addr>,
    /// How long the system has been up.
    pub uptime: fn() -> Option<Duration>,
}

impl Probe {
    /// A watch of the interface `name` the engine started on at `ip`, if
    /// it is to be watched. `leave_link_local`: restart when the engine is
    /// on a link-local address and a routable one appears (see
    /// [`InterfaceWatch::leaving_link_local`]).
    pub fn watch(self, name: &str, ip: Ipv4Addr, leave_link_local: bool) -> Option<InterfaceWatch> {
        match self {
            Probe::System if ip.is_loopback() || ip.is_unspecified() => None,
            _ => Some(InterfaceWatch::new(name, ip).leaving_link_local(leave_link_local)),
        }
    }

    /// Looks at interface `name`: `None` if the system has no such
    /// interface. An error means the system could not say.
    pub fn look(self, name: &str) -> io::Result<Option<Seen>> {
        match self {
            Probe::System => look(name),
            #[cfg(test)]
            Probe::Fake(f) => f(name),
            #[cfg(test)]
            Probe::Scripted(s) => (s.look)(name),
        }
    }

    /// How long the system has been up, if it says (see
    /// [`ovsc_core::net::boot_uptime`]).
    pub fn uptime(self) -> Option<Duration> {
        match self {
            Probe::System => boot_uptime(),
            #[cfg(test)]
            Probe::Fake(_) => boot_uptime(),
            #[cfg(test)]
            Probe::Scripted(s) => (s.uptime)(),
        }
    }

    /// The interface the engine is to start on, see
    /// [`Interface::find_ready`].
    pub fn find_ready(self, spec: &str, allow_link_local: bool) -> ovsc_core::Result<Interface> {
        match self {
            Probe::System => Interface::find_ready(spec, allow_link_local),
            #[cfg(test)]
            Probe::Fake(_) => Interface::find_ready(spec, allow_link_local),
            #[cfg(test)]
            Probe::Scripted(s) => {
                let ip = select_ip(
                    spec,
                    &(s.addrs)(),
                    ovsc_core::config::parse_interface_ip(spec),
                    allow_link_local,
                )?;
                Ok(Interface {
                    name: spec.to_owned(),
                    ip,
                    netmask: Ipv4Addr::new(255, 255, 0, 0),
                    gateway: Ipv4Addr::UNSPECIFIED,
                    mac: [0; 6],
                    link_speed_mbps: 1000,
                })
            }
        }
    }
}

/// Watches one interface for the engine started on it (see the module
/// documentation).
#[derive(Debug)]
pub struct InterfaceWatch {
    name: String,
    /// The address the engine runs on.
    ip: Ipv4Addr,
    /// The interface's index when first seen.
    index: Option<u32>,
    /// The interface vanished since the engine started.
    gone: bool,
    /// The interface lost its address since the engine started.
    lost: bool,
    /// When the interface got its address back, until the clock locks.
    back_at: Option<Instant>,
    /// The reason for a restart found at the last check: a restart needs
    /// two checks in a row, so a passing glitch of the system's view of the
    /// interfaces does not cost an engine restart.
    pending: Option<String>,
    /// Restart when the engine is on a link-local address and the
    /// interface now has a routable one, even if the link-local one is
    /// still listed.
    leave_link_local: bool,
}

impl InterfaceWatch {
    pub fn new(name: &str, ip: Ipv4Addr) -> InterfaceWatch {
        InterfaceWatch {
            name: name.to_owned(),
            ip,
            index: None,
            gone: false,
            lost: false,
            back_at: None,
            pending: None,
            leave_link_local: false,
        }
    }

    /// Whether the engine is to leave a link-local address for a routable
    /// one that appears next to it. Only for an engine that follows its
    /// interface: with an IP address as the configured `interface` every
    /// restart binds that address again, so the rule would restart the
    /// engine for good.
    pub fn leaving_link_local(mut self, on: bool) -> InterfaceWatch {
        self.leave_link_local = on;
        self
    }

    /// What is wrong with the interface now, if anything, for the status.
    pub fn problem(&self) -> Option<String> {
        if self.gone {
            Some(format!("the network interface {} is gone", self.name))
        } else if self.lost {
            Some(format!("the network interface {} has no IPv4 address (link down?)", self.name))
        } else {
            None
        }
    }

    /// Looks at the interface through `probe` and judges it, for an engine
    /// running on `clock`. When the system cannot say, nothing changes.
    pub fn poll(&mut self, probe: Probe, clock: &MediaClock) -> Verdict {
        match probe.look(&self.name) {
            Ok(seen) => {
                let locked =
                    matches!(clock.status().state, ClockState::Locked | ClockState::FreeRunning);
                self.check(seen.as_ref(), locked, Instant::now())
            }
            Err(e) => {
                debug!("cannot look at the network interface {}: {e}", self.name);
                Verdict::Keep
            }
        }
    }

    /// Judges what `seen` (`None`: no such interface) says about the
    /// interface at `now`, given whether the engine's clock is locked (or
    /// free-running). Logs each change of state once.
    pub fn check(&mut self, seen: Option<&Seen>, clock_locked: bool, now: Instant) -> Verdict {
        let reason = self.reason(seen, clock_locked, now);
        match (reason, self.pending.take()) {
            (Some(reason), Some(_)) => Verdict::Restart(reason),
            (Some(reason), None) => {
                self.pending = Some(reason);
                Verdict::Keep
            }
            (None, _) => Verdict::Keep,
        }
    }

    fn reason(&mut self, seen: Option<&Seen>, clock_locked: bool, now: Instant) -> Option<String> {
        let name = &self.name;
        let Some(seen) = seen else {
            if !self.gone {
                warn!(
                    "the network interface {name} is gone: audio stops until it is back; the \
                     engine restarts on it then"
                );
                self.gone = true;
            }
            self.back_at = None;
            return None;
        };
        if seen.ipv4.is_empty() {
            if !self.lost && !self.gone {
                warn!(
                    "the network interface {name} lost its IPv4 address {} (link down?): \
                     waiting for it",
                    self.ip
                );
            }
            self.lost = true;
            self.back_at = None;
            return None;
        }
        if self.gone {
            return Some(format!("the network interface {name} is back"));
        }
        match self.index {
            Some(index) if index != seen.index => {
                return Some(format!(
                    "the network interface {name} was attached again (index {index}, now {})",
                    seen.index
                ));
            }
            Some(_) => {}
            None => self.index = Some(seen.index),
        }
        if !seen.ipv4.contains(&self.ip) {
            let now: Vec<String> = seen.ipv4.iter().map(Ipv4Addr::to_string).collect();
            return Some(format!(
                "the address of the network interface {name} changed from {} to {}",
                self.ip,
                now.join(", ")
            ));
        }
        if self.leave_link_local && self.ip.is_link_local() {
            if let Some(routable) = pick_ipv4(&seen.ipv4, false) {
                return Some(format!(
                    "{name} now has {routable}: leaving the link-local {}",
                    self.ip
                ));
            }
        }
        if self.lost {
            info!("the network interface {name} has its address {} back", self.ip);
            self.lost = false;
            self.back_at = Some(now);
        }
        match self.back_at {
            Some(at) if clock_locked && now.duration_since(at) >= LOCK_EVIDENCE => {
                self.back_at = None;
                None
            }
            Some(at) if now.duration_since(at) >= RELOCK_GRACE => Some(format!(
                "the clock did not lock within {} s of the network interface {name} coming back",
                RELOCK_GRACE.as_secs()
            )),
            _ => None,
        }
    }
}

/// `; ` and what is wrong with the interface `watch` watches, if anything,
/// for the end of a status line.
pub fn status_note(watch: Option<&InterfaceWatch>) -> String {
    watch.and_then(InterfaceWatch::problem).map(|p| format!("; {p}")).unwrap_or_default()
}

/// Looks at interface `name` (see [`Probe::look`]).
#[cfg(unix)]
pub fn look(name: &str) -> io::Result<Option<Seen>> {
    let Ok(c_name) = std::ffi::CString::new(name) else { return Ok(None) };
    // SAFETY: a NUL-terminated name.
    let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        return Ok(None);
    }
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut ipv4 = Vec::new();
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned, alive until
        // freeifaddrs.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a C string, ifa_addr points to a sockaddr.
        let (ifname, family) = unsafe {
            (std::ffi::CStr::from_ptr(ifa.ifa_name), i32::from((*ifa.ifa_addr).sa_family))
        };
        if family != libc::AF_INET || ifname.to_bytes() != name.as_bytes() {
            continue;
        }
        // SAFETY: an AF_INET address is a sockaddr_in.
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        ipv4.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
    }
    // SAFETY: the list getifaddrs returned, not used after this.
    unsafe { libc::freeifaddrs(list) };
    Ok(Some(Seen { index, ipv4 }))
}

/// Looking at interfaces is not supported here: nothing is watched.
#[cfg(not(unix))]
pub fn look(_name: &str) -> io::Result<Option<Seen>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "interfaces cannot be watched here"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: Ipv4Addr = Ipv4Addr::new(169, 254, 10, 20);

    fn seen(index: u32, ipv4: &[Ipv4Addr]) -> Option<Seen> {
        Some(Seen { index, ipv4: ipv4.to_vec() })
    }

    /// Runs `looks` one second apart through a fresh watch; the verdicts.
    fn run(looks: &[(Option<Seen>, bool)]) -> Vec<Verdict> {
        let mut w = InterfaceWatch::new("en7", IP);
        let start = Instant::now();
        looks
            .iter()
            .enumerate()
            .map(|(i, (s, locked))| {
                w.check(s.as_ref(), *locked, start + Duration::from_secs(i as u64))
            })
            .collect()
    }

    fn restarts(verdicts: &[Verdict]) -> Vec<usize> {
        verdicts
            .iter()
            .enumerate()
            .filter(|(_, v)| matches!(v, Verdict::Restart(_)))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn a_steady_interface_is_kept() {
        let v = run(&vec![(seen(4, &[IP]), true); 10]);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn a_new_address_restarts_the_engine_once_confirmed() {
        let new = Ipv4Addr::new(192, 168, 1, 20);
        let v = run(&[(seen(4, &[IP]), true), (seen(4, &[new]), true), (seen(4, &[new]), true)]);
        assert_eq!(restarts(&v), [2]);
        let Verdict::Restart(why) = &v[2] else { unreachable!() };
        assert!(why.contains("changed from 169.254.10.20 to 192.168.1.20"), "{why}");
        // A one-off glitch is not enough.
        let v = run(&[(seen(4, &[IP]), true), (seen(4, &[new]), true), (seen(4, &[IP]), true)]);
        assert!(restarts(&v).is_empty(), "{v:?}");
        // A second address next to the engine's is fine.
        let v = run(&vec![(seen(4, &[new, IP]), true); 3]);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn an_interface_that_comes_back_restarts_the_engine() {
        // The adapter is unplugged for 3 s, then gets its address again.
        let v = run(&[
            (seen(4, &[IP]), true),
            (None, true),
            (None, false),
            (None, false),
            (seen(4, &[]), false),
            (seen(4, &[IP]), false),
            (seen(4, &[IP]), false),
        ]);
        // Kept while gone; restarted once it is back, even on the same
        // address and index: its multicast memberships went with it.
        assert_eq!(restarts(&v), [6]);
    }

    #[test]
    fn a_new_index_restarts_the_engine() {
        let v = run(&[(seen(4, &[IP]), true), (seen(9, &[IP]), true), (seen(9, &[IP]), true)]);
        assert_eq!(restarts(&v), [2]);
    }

    #[test]
    fn a_cable_pull_with_the_same_address_back_is_waited_out() {
        // Link down for 4 s (macOS removes the address meanwhile), then the
        // same address, and the clock locks again 3 s later.
        let mut looks = vec![(seen(4, &[IP]), true)];
        looks.extend(vec![(seen(4, &[]), false); 4]);
        looks.extend(vec![(seen(4, &[IP]), false); 3]);
        looks.extend(vec![(seen(4, &[IP]), true); 20]);
        let v = run(&looks);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn a_clock_that_does_not_lock_after_the_link_returns_restarts_the_engine() {
        let mut looks = vec![(seen(4, &[IP]), true), (seen(4, &[]), false)];
        looks.extend(vec![(seen(4, &[IP]), false); 20]);
        let v = run(&looks);
        // Back at look 2; the grace runs out at look 17, confirmed at 18.
        assert_eq!(restarts(&v)[0], 2 + RELOCK_GRACE.as_secs() as usize + 1, "{v:?}");
    }

    #[test]
    fn a_clock_that_only_still_reads_locked_does_not_count() {
        // A 3 s outage: the clock still reads Locked when the address comes
        // back, from Syncs heard before it went (the master timeout is
        // 5 s), then drops: no Sync arrives any more (the memberships went
        // with an adapter that re-attached between two looks).
        let mut looks = vec![(seen(4, &[IP]), true)];
        looks.extend(vec![(seen(4, &[]), true); 3]);
        looks.extend(vec![(seen(4, &[IP]), true); 2]);
        looks.extend(vec![(seen(4, &[IP]), false); 20]);
        let v = run(&looks);
        // Back at look 4; the grace runs out at look 19, confirmed at 20.
        assert_eq!(restarts(&v)[0], 4 + RELOCK_GRACE.as_secs() as usize + 1, "{v:?}");
    }

    #[test]
    fn problems_are_reported_while_they_last() {
        let mut w = InterfaceWatch::new("en7", IP);
        let now = Instant::now();
        assert_eq!(w.problem(), None);
        w.check(seen(4, &[]).as_ref(), true, now);
        assert!(w.problem().unwrap().contains("no IPv4 address"));
        w.check(seen(4, &[IP]).as_ref(), true, now);
        assert_eq!(w.problem(), None);
        w.check(None, true, now);
        assert!(w.problem().unwrap().contains("is gone"));
    }

    #[test]
    fn loopback_is_not_watched_but_real_interfaces_are() {
        assert!(Probe::System.watch("lo", Ipv4Addr::LOCALHOST, false).is_none());
        assert!(Probe::System.watch("en7", IP, false).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_system_reports_the_loopback_interface() {
        let name = if cfg!(target_vendor = "apple") { "lo0" } else { "lo" };
        let seen = look(name).unwrap().expect("a loopback interface");
        assert!(seen.index > 0);
        assert!(seen.ipv4.contains(&Ipv4Addr::LOCALHOST), "{seen:?}");
        assert_eq!(look("no-such-interface0").unwrap(), None);
    }

    const ROUTABLE: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 44);

    /// Like [`run`], with the watch of an engine that follows the
    /// interface's address (`interface` is a name or empty, not an IP).
    fn run_following(looks: &[(Option<Seen>, bool)]) -> Vec<Verdict> {
        let mut w = InterfaceWatch::new("en7", IP).leaving_link_local(true);
        let start = Instant::now();
        looks
            .iter()
            .enumerate()
            .map(|(i, (s, locked))| {
                w.check(s.as_ref(), *locked, start + Duration::from_secs(i as u64))
            })
            .collect()
    }

    #[test]
    fn an_engine_on_link_local_leaves_it_when_a_routable_address_appears() {
        // The link-local address stays listed next to the new one.
        let v = run_following(&[
            (seen(4, &[IP]), true),
            (seen(4, &[IP, ROUTABLE]), true),
            (seen(4, &[IP, ROUTABLE]), true),
            (seen(4, &[IP, ROUTABLE]), true),
        ]);
        assert_eq!(restarts(&v), [2], "{v:?}");
        let Verdict::Restart(why) = &v[2] else { unreachable!() };
        assert_eq!(why, "en7 now has 192.0.2.44: leaving the link-local 169.254.10.20");
    }

    #[test]
    fn one_look_at_a_routable_address_is_not_enough() {
        let v = run_following(&[
            (seen(4, &[IP]), true),
            (seen(4, &[IP, ROUTABLE]), true),
            (seen(4, &[IP]), true),
            (seen(4, &[IP]), true),
        ]);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn an_engine_on_link_local_stays_while_nothing_new_appears() {
        let v = run_following(&vec![(seen(4, &[IP]), true); 10]);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn an_engine_given_a_link_local_ip_is_not_moved() {
        // `interface` is that IP: every restart would bind it again.
        let v = run(&vec![(seen(4, &[IP, ROUTABLE]), true); 10]);
        assert!(restarts(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn an_engine_on_a_routable_address_ignores_an_extra_link_local_one() {
        let mut w = InterfaceWatch::new("en7", ROUTABLE).leaving_link_local(true);
        let start = Instant::now();
        for i in 0..10 {
            let s = seen(4, &[ROUTABLE, IP]);
            let v = w.check(s.as_ref(), true, start + Duration::from_secs(i));
            assert_eq!(v, Verdict::Keep);
        }
    }

    #[test]
    fn the_watch_is_told_whether_to_follow_the_address() {
        let w = Probe::System.watch("en7", IP, true).unwrap();
        assert!(w.leave_link_local);
        let w = Probe::System.watch("en7", IP, false).unwrap();
        assert!(!w.leave_link_local);
    }
}
