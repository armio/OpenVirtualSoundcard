//! Device names on a network: unique defaults, and names another device
//! already uses.

use std::future::Future;
use std::net::Ipv4Addr;
use std::time::Duration;

use ovsc_clock::system_clock;
use ovsc_core::mdns::{self, Advertisement, MDNS_GROUP, MDNS_PORT};
use ovsc_core::net::{Interface, bind_multicast, into_tokio};
use ovsc_core::{Channels, Device, DeviceConfig, DeviceInfo, NameConflict, Ports};
use ovsc_proto::discovery::ChannelTxt;
use ovsc_proto::dns::{Message, Record};

fn config(name: &str, base: u16, process_id: u16) -> DeviceConfig {
    DeviceConfig {
        name: name.into(),
        interface: "127.0.0.1".into(),
        tx_channels: Channels::Count(2),
        rx_channels: Channels::Count(2),
        latency_ms: 10.0,
        ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
        discovery: false,
        process_id,
        ..Default::default()
    }
}

/// A device name no other test run on this computer uses at the same time:
/// devices on 127.0.0.1 hear each other's mDNS.
fn unique(prefix: &str) -> String {
    format!("{prefix}{}", std::process::id())
}

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = check() {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits for `fut`, which must finish within a few seconds.
async fn soon<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), fut).await.expect("timed out")
}

fn interface(ip: [u8; 4], mac: [u8; 6]) -> Interface {
    Interface {
        name: "en7".into(),
        ip: ip.into(),
        netmask: Ipv4Addr::new(255, 255, 0, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
        mac,
        link_speed_mbps: 1000,
    }
}

/// The name a device has until someone names it identifies the computer and
/// survives an address change, which subscriptions to it must.
#[test]
fn the_default_name_follows_the_mac_address_not_the_ip_address() {
    let cfg = DeviceConfig { name: String::new(), ..Default::default() };
    let mac = [0x00, 0x1d, 0xc1, 0x52, 0x79, 0xb6];
    let name = |ip, mac, cfg: &DeviceConfig| {
        DeviceInfo::new(cfg, interface(ip, mac)).unwrap().factory_name
    };
    assert_eq!(name([169, 254, 3, 4], mac, &cfg), "ovsc-5279b6");
    assert_eq!(name([10, 0, 0, 9], mac, &cfg), "ovsc-5279b6");
    // Another computer.
    assert_eq!(name([169, 254, 3, 4], [0x00, 0x1d, 0xc1, 0x08, 0x12, 0x58], &cfg), "ovsc-081258");
    // A second device on this one.
    let second = DeviceConfig { process_id: 2, ..cfg.clone() };
    assert_eq!(name([169, 254, 3, 4], mac, &second), "ovsc-5279b6-2");
    // No MAC address: the device id, from the address, as before.
    assert_eq!(name([127, 0, 0, 1], [0; 6], &cfg), "ovsc-7f0000010000");
    for n in [name([169, 254, 3, 4], mac, &second), name([127, 0, 0, 1], [0; 6], &second)] {
        ovsc_proto::discovery::validate_device_name(&n).unwrap();
    }
}

/// A device that finds its name taken when it starts takes the next free
/// one; the device that had the name keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_whose_name_is_taken_takes_another() {
    let name = unique("dup");
    let mut first_cfg = config(&name, 25640, 41);
    first_cfg.discovery = true;
    let first = Device::start(first_cfg, system_clock()).await.unwrap();
    // Probing (about a second) and the first announcement.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(first.name(), name);

    let mut second_cfg = config(&name, 25650, 42);
    second_cfg.discovery = true;
    let second = Device::start(second_cfg, system_clock()).await.unwrap();
    let taken = format!("{name}-2");
    eventually("the second device to rename itself", || (second.name() == taken).then_some(()))
        .await;
    let conflict = eventually("the rename to be reported", || second.name_conflict()).await;
    assert_eq!(
        conflict,
        NameConflict { name: name.clone(), peer: Ipv4Addr::LOCALHOST, renamed_to: Some(taken) }
    );
    // The first device never saw its name announced by another.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(first.name(), name);
    assert_eq!(first.name_conflict(), None);
    assert_eq!(second.observer().name_conflict(), Some(Some(conflict)));

    // Renamed by the user: the note goes.
    second.rename(&unique("solo")).unwrap();
    eventually("the note to clear", || second.name_conflict().is_none().then_some(())).await;
    soon(second.shutdown()).await;
    soon(first.shutdown()).await;
}

fn txt(id: u16) -> ChannelTxt {
    ChannelTxt {
        id,
        sample_rate: 48_000,
        bits_per_sample: 24,
        pcm_type: 0x0e,
        latency_ns: 10_000_000,
        fpp_max: 32,
        fpp_min: 2,
        nchan: 2,
        dbcp1: 0x1102,
        is_default_name: true,
        multicast: None,
    }
}

/// Another device announcing a running device's name, as one that does
/// not probe would: reported while it lasts, and the name stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_announcing_our_name_is_reported() {
    let name = unique("live");
    let mut cfg = config(&name, 25660, 43);
    cfg.discovery = true;
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(dev.name_conflict(), None);

    let twin = mdns::device_records(&Advertisement {
        name: &name,
        host: &unique("twin-host"),
        ip: Ipv4Addr::LOCALHOST,
        arc_port: 4440,
        cmc_port: 8800,
        flow_control_port: 4455,
        device_id: &[0, 0, 127, 0, 0, 1, 0, 99],
        process_id: 99,
        manufacturer: "Someone",
        board_name: "Something",
        model: "_000000000000000b",
        tx_channels: vec![("01", "01", txt(1))],
    });
    let announcer =
        into_tokio(bind_multicast(MDNS_GROUP, MDNS_PORT, Ipv4Addr::LOCALHOST).unwrap()).unwrap();
    let announce = |records: Vec<Record>| {
        let mut msg = Message::response();
        msg.answers = records;
        msg.encode()
    };
    let group = (MDNS_GROUP, MDNS_PORT);
    announcer.send_to(&announce(twin.clone()), group).await.unwrap();
    let conflict = eventually("the conflict to be reported", || dev.name_conflict()).await;
    assert_eq!(
        conflict,
        NameConflict { name: name.clone(), peer: Ipv4Addr::LOCALHOST, renamed_to: None }
    );
    assert_eq!(dev.name(), name);

    let goodbye = twin
        .into_iter()
        .map(|mut r| {
            r.ttl = 0;
            r
        })
        .collect();
    announcer.send_to(&announce(goodbye), group).await.unwrap();
    eventually("the conflict to end", || dev.name_conflict().is_none().then_some(())).await;
    assert_eq!(dev.name(), name);
    soon(dev.shutdown()).await;
}
