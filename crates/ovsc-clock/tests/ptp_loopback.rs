//! End-to-end tests over real UDP sockets on the loopback interface: the
//! development test master and the follower talk PTPv1 multicast on
//! 127.0.0.1, on high ports so no privileges are needed.
//!
//! Each test uses its own port pair, because multicast is delivered to every
//! socket bound to the port.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use ovsc_clock::ptp::master::{TestMaster, TestMasterConfig};
use ovsc_clock::ptp::wire::{
    Body, COMM_TECH_ETHERNET, Flags, Header, Message, PortIdentity, Subdomain, SyncBody, Timestamp,
};
use ovsc_clock::ptp::{PtpCapture, PtpConfig, PtpFollower};
use ovsc_clock::{ClockState, free_running_clock, system_clock};
use socket2::{Domain, Protocol, Socket, Type};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
const MASTER_UUID: [u8; 6] = [0x02, 0x00, 0x5e, 0x00, 0x00, 0x01];
const FOLLOWER_UUID: [u8; 6] = [0x02, 0x00, 0x5e, 0x00, 0x00, 0x02];

fn master_config(event_port: u16, sync_interval: Duration) -> TestMasterConfig {
    let mut c = TestMasterConfig::new(LOOPBACK, MASTER_UUID);
    c.event_port = event_port;
    c.general_port = event_port + 1;
    c.sync_interval = sync_interval;
    c
}

/// A plain multicast socket on `port`, to watch or inject traffic.
fn raw_socket(port: u16) -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_reuse_address(true).unwrap();
    #[cfg(unix)]
    s.set_reuse_port(true).unwrap();
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into()).unwrap();
    s.join_multicast_v4(&GROUP, &LOOPBACK).unwrap();
    s.set_multicast_if_v4(&LOOPBACK).unwrap();
    s.set_multicast_loop_v4(true).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    s.into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_locks_to_test_master_over_loopback() {
    // A free-running master: the wall clock (system_clock) can be slewed by
    // NTP faster than PTP's ±500 ppm range on CI VMs, which no follower can
    // track.
    let reference = free_running_clock();
    let master =
        TestMaster::start(master_config(31319, Duration::from_millis(125)), reference.clone())
            .await
            .expect("start test master");

    let mut config = PtpConfig::new(LOOPBACK, FOLLOWER_UUID);
    config.event_port = 31319;
    config.general_port = 31320;
    config.delay_req_interval = Duration::from_millis(500);
    let (follower, clock) = PtpFollower::start(config).await.expect("start follower");

    let started = Instant::now();
    loop {
        let status = clock.status();
        if status.state == ClockState::Locked {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "not locked after 10 s: {status:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let status = follower.status();
    eprintln!("locked after {:?}: {status:?}", started.elapsed());
    let info = status.master.expect("master info");
    assert_eq!(info.uuid, MASTER_UUID);
    assert_eq!(info.port_id, 1);
    assert!(status.mean_path_delay_ns < 1_000_000, "{status:?}");

    // The follower now tells the master's time.
    for _ in 0..10 {
        let ours = clock.now_ns().expect("clock has time") as i64;
        let theirs = reference.now_ns().unwrap() as i64;
        let diff = ours - theirs;
        eprintln!("follower - master: {diff} ns");
        assert!(diff.abs() < 1_000_000, "follower is {diff} ns off the master; {status:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    follower.shutdown();
    assert!(!clock.is_ready(), "shutdown invalidates the clock");
    assert_eq!(clock.status().state, ClockState::Unlocked);
    master.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_master_goes_silent_when_another_master_appears() {
    const PORT: u16 = 31419;
    let watch = raw_socket(PORT);
    let master = TestMaster::start(master_config(PORT, Duration::from_millis(50)), system_clock())
        .await
        .expect("start test master");

    // The test master is sending Syncs...
    let syncs_from_master = |timeout: Duration| {
        let mut buf = [0u8; 1500];
        let end = Instant::now() + timeout;
        let mut count = 0;
        while Instant::now() < end {
            if let Ok(n) = watch.recv(&mut buf) {
                if let Ok(Message { header, body: Body::Sync(_) }) = Message::decode(&buf[..n]) {
                    if header.source.uuid == MASTER_UUID {
                        count += 1;
                    }
                }
            }
        }
        count
    };
    let watch_for = |d| tokio::task::block_in_place(|| syncs_from_master(d));
    assert!(watch_for(Duration::from_millis(500)) > 0, "test master sends Syncs");
    assert!(!master.is_silenced());

    // ...until a real master shows up.
    let other = PortIdentity { uuid: [0x00, 0x1d, 0xc1, 0xaa, 0xbb, 0xcc], port_id: 1 };
    let sync = Message {
        header: Header::new(Subdomain::DEFAULT, other, 1, Flags::ASSIST),
        body: Body::Sync(SyncBody {
            origin_timestamp: Timestamp::from_ns(system_clock().now_ns().unwrap()),
            grandmaster_communication_technology: COMM_TECH_ETHERNET,
            grandmaster_clock_uuid: other.uuid,
            grandmaster_clock_stratum: 4,
            grandmaster_clock_identifier: *b"DFLT",
            ..SyncBody::default()
        }),
    };
    watch.send_to(&sync.encode(), SocketAddrV4::new(GROUP, PORT)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while !master.is_silenced() {
        assert!(Instant::now() < deadline, "test master did not go silent");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Let anything already in flight arrive, then expect silence.
    watch_for(Duration::from_millis(100));
    assert_eq!(watch_for(Duration::from_millis(500)), 0, "test master still sends Syncs");
    master.shutdown();
}

/// Whether `e`, from starting a follower that reads the interface, says
/// this process may not: such tests then stop, saying so. Reading the
/// interface needs root (or CAP_NET_RAW on Linux, access to /dev/bpf on
/// macOS).
fn not_allowed(e: &std::io::Error) -> bool {
    let denied = e.kind() == std::io::ErrorKind::PermissionDenied;
    if denied {
        eprintln!("not run: reading the interface is not allowed here ({e})");
    }
    denied
}

/// A Sync from a master no test runs, to the group on `port`, sent from a
/// port of its own.
fn foreign_sync(sender: &UdpSocket, port: u16, seq: u16) {
    let other = PortIdentity { uuid: [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c], port_id: 1 };
    let sync = Message {
        header: Header::new(Subdomain::DEFAULT, other, seq, Flags::ASSIST),
        body: Body::Sync(SyncBody {
            origin_timestamp: Timestamp::from_ns(system_clock().now_ns().unwrap()),
            grandmaster_communication_technology: COMM_TECH_ETHERNET,
            grandmaster_clock_uuid: other.uuid,
            grandmaster_clock_stratum: 4,
            grandmaster_clock_identifier: *b"DFLT",
            ..SyncBody::default()
        }),
    };
    sender.send_to(&sync.encode(), SocketAddrV4::new(GROUP, port)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_locks_reading_the_interface() {
    const PORT: u16 = 31519;
    let reference = free_running_clock();
    let master =
        TestMaster::start(master_config(PORT, Duration::from_millis(125)), reference.clone())
            .await
            .expect("start test master");
    let mut config = PtpConfig::new(LOOPBACK, FOLLOWER_UUID);
    config.event_port = PORT;
    config.general_port = PORT + 1;
    config.delay_req_interval = Duration::from_millis(500);
    config.capture = PtpCapture::Always;
    let (follower, clock) = match PtpFollower::start(config).await {
        Ok(f) => f,
        Err(e) if not_allowed(&e) => return,
        Err(e) => panic!("start follower: {e}"),
    };
    assert!(follower.reads_interface());

    let started = Instant::now();
    while clock.status().state != ClockState::Locked {
        assert!(started.elapsed() < Duration::from_secs(10), "not locked: {:?}", clock.status());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = follower.status();
    eprintln!("locked after {:?}: {status:?}", started.elapsed());
    assert!(status.reads_interface);
    assert_eq!(status.master.expect("master info").uuid, MASTER_UUID);
    // The master answered the Delay_Reqs sent from the follower's own port.
    assert!(status.mean_path_delay_ns > 0 && status.mean_path_delay_ns < 1_000_000, "{status:?}");
    let diff = clock.now_ns().unwrap() as i64 - reference.now_ns().unwrap() as i64;
    assert!(diff.abs() < 1_000_000, "follower is {diff} ns off the master");
    follower.shutdown();
    master.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ports_held_by_another_program_are_read_from_the_interface() {
    const PORT: u16 = 31619;
    // Another program holds the event port for itself (no SO_REUSEADDR or
    // SO_REUSEPORT), as AirPlay does on a Mac.
    let _squatter = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT)).unwrap();
    let mut config = PtpConfig::new(LOOPBACK, FOLLOWER_UUID);
    config.event_port = PORT;
    config.general_port = PORT + 1;

    // Without the fallback the follower cannot start.
    let never = PtpConfig { capture: PtpCapture::Never, ..config.clone() };
    let e = PtpFollower::start(never).await.expect_err("the port is held");
    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");

    // With it, the follower reads the interface and hears a master.
    let (follower, clock) = match PtpFollower::start(config).await {
        Ok(f) => f,
        Err(e) if not_allowed(&e) => return,
        Err(e) => panic!("start follower: {e}"),
    };
    assert!(follower.reads_interface());
    let sender = raw_socket(0);
    let started = Instant::now();
    let mut seq = 0;
    while clock.status().master.is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no master heard: {:?}",
            clock.status()
        );
        foreign_sync(&sender, PORT, seq);
        seq += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(clock.status().reads_interface);
    follower.shutdown();
}
