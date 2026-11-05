//! Device names on a network: unique defaults, names another device already
//! uses, names kept through restarts, and subscriptions to a name two
//! devices share.

use std::future::Future;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ovsc_clock::system_clock;
use ovsc_core::directory::StaticDirectory;
use ovsc_core::mdns::{self, Advertisement, MDNS_GROUP, MDNS_PORT, Mdns};
use ovsc_core::net::{Interface, bind_multicast, into_tokio};
use ovsc_core::{
    Channels, Device, DeviceConfig, DeviceInfo, NameConflict, Ports, SavedClaim, SavedState,
    StartOptions,
};
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

/// Waits until the device called `name` answers for its channel `01`: it
/// has probed for its name (about a second) and announced it.
async fn announced(name: &str) {
    let mdns = Mdns::start(Ipv4Addr::LOCALHOST).unwrap();
    let found = mdns.resolve_channel("01", name, Duration::from_secs(5)).await;
    mdns.stop();
    found.unwrap_or_else(|e| panic!("{name} not announced: {e}"));
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
    announced(&name).await;
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

/// The mDNS records of another device called `name`.
fn twin_records(name: &str) -> Vec<Record> {
    mdns::device_records(&Advertisement {
        name,
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
    })
}

/// Another device announcing a running device's name, as one that does
/// not probe would: reported while it lasts, and the name stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_announcing_our_name_is_reported() {
    let name = unique("live");
    let mut cfg = config(&name, 25660, 43);
    cfg.discovery = true;
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    // Announced, not probing any more: the announcement below is not an
    // answer to a probe.
    announced(&name).await;
    assert_eq!(dev.name_conflict(), None);

    let twin = twin_records(&name);
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

/// A device that the daemons restart in their process (on a new address)
/// keeps the name it announced, also while another device announces that
/// name too: it announces it again without probing, which the other device
/// would answer, and reports the conflict as it did before the restart. It
/// never renames itself mid-show. Another name is probed for, as at a first
/// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_device_keeps_its_name_through_a_live_conflict() {
    let name = unique("keep");
    let mut cfg = config(&name, 25690, 46);
    cfg.discovery = true;
    let dev = Device::start(cfg.clone(), system_clock()).await.unwrap();
    announced(&name).await;
    assert_eq!(dev.announced_name().as_deref(), Some(name.as_str()));

    // Another device announces the name too, and answers for it, as a
    // second Mac given the same name would.
    let twin = Mdns::start(Ipv4Addr::LOCALHOST).unwrap();
    twin.set_records(twin_records(&name)).await;
    let live = NameConflict { name: name.clone(), peer: Ipv4Addr::LOCALHOST, renamed_to: None };
    assert_eq!(eventually("the conflict", || dev.name_conflict()).await, live);

    // Restarted as the daemons do, handing on its state and the name it
    // announced.
    let restart = |dev: &Device| StartOptions {
        state: Some(dev.saved_state()),
        announced: dev.announced_name(),
        ..Default::default()
    };
    let options = restart(&dev);
    soon(dev.shutdown()).await;
    let dev = Device::start_with_options(cfg.clone(), system_clock(), options).await.unwrap();
    assert_eq!(eventually("the conflict again", || dev.name_conflict()).await, live);
    // Longer than a probe takes: the name stays, also in what is saved.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(dev.name(), name);
    assert_eq!(dev.saved_state().name.as_deref(), Some(name.as_str()));
    assert_eq!(dev.announced_name().as_deref(), Some(name.as_str()));
    assert_eq!(dev.name_conflict(), Some(live));

    // A name other than the announced one is probed for: taken, it gives
    // way.
    let mut options = restart(&dev);
    options.announced = Some(unique("other"));
    soon(dev.shutdown()).await;
    let dev = Device::start_with_options(cfg, system_clock(), options).await.unwrap();
    let taken = format!("{name}-2");
    eventually("a rename", || (dev.name() == taken).then_some(())).await;
    soon(dev.shutdown()).await;
    twin.stop();
}

/// A fresh directory for a test's state file.
fn state_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ovsc-naming-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Waits until the state file at `path` holds a claim of `name`.
async fn saved_claim(path: &Path, name: &str) -> SavedClaim {
    eventually(&format!("a saved claim of {name}"), || {
        SavedState::load(path).and_then(|s| s.claim).filter(|c| c.name == name)
    })
    .await
}

/// A new process on the same computer (the daemon started again after a
/// crash, or at a reboot) starts the device from its state file alone. The
/// file holds the device's claim of its name, made on this interface (the
/// same MAC address), so the device keeps the name while another device
/// announces it too, as a device restarted within its process does: it
/// announces the name again without probing for it, which the other device
/// would answer, and reports the conflict. It never renames itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_process_keeps_the_name_it_claimed_through_a_live_conflict() {
    let name = unique("claim");
    let dir = state_dir("claim");
    let state_file = dir.join("state.toml");
    let mut cfg = config(&name, 25700, 47);
    cfg.discovery = true;
    cfg.state_file = Some(state_file.clone());
    let dev = Device::start(cfg.clone(), system_clock()).await.unwrap();
    announced(&name).await;
    // Saved once claimed, while the device runs: a crash saves nothing more.
    let claim = saved_claim(&state_file, &name).await;

    // Another device announces the name too, and answers for it, as a
    // second Mac given the same name would.
    let twin = Mdns::start(Ipv4Addr::LOCALHOST).unwrap();
    twin.set_records(twin_records(&name)).await;
    let live = NameConflict { name: name.clone(), peer: Ipv4Addr::LOCALHOST, renamed_to: None };
    assert_eq!(eventually("the conflict", || dev.name_conflict()).await, live);

    // The process ends, and a new one starts the device with nothing handed
    // on but the state file.
    soon(dev.shutdown()).await;
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    assert_eq!(eventually("the conflict again", || dev.name_conflict()).await, live);
    // Longer than a probe takes: the name stays, also in what is saved.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(dev.name(), name);
    assert_eq!(dev.announced_name().as_deref(), Some(name.as_str()));
    assert_eq!(dev.name_conflict(), Some(live));
    let saved = SavedState::load(&state_file).unwrap();
    assert_eq!((saved.name.as_deref(), saved.claim), (Some(name.as_str()), Some(claim)));
    soon(dev.shutdown()).await;
    twin.stop();
    std::fs::remove_dir_all(dir).unwrap();
}

/// A disk cloned from another computer holds that computer's state file,
/// with its device's claim of the name, made on that computer's interface.
/// Here the MAC address differs, so the device probes for the name as at a
/// first start: while the other computer's device has the name, this one
/// takes the next free one, and saves its own claim of that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloned_state_file_probes_and_takes_another_name() {
    let name = unique("clone");
    let dir = state_dir("clone");
    let state_file = dir.join("state.toml");
    let copied = SavedClaim { name: name.clone(), mac: "00:1d:c1:52:79:b6".into() };
    SavedState::update(&state_file, |s| {
        s.name = Some(name.clone());
        s.claim = Some(copied.clone());
    })
    .unwrap();
    // The other computer's device, which has the name.
    let original = Mdns::start(Ipv4Addr::LOCALHOST).unwrap();
    original.set_records(twin_records(&name)).await;

    let mut cfg = config(&name, 25710, 48);
    cfg.discovery = true;
    cfg.state_file = Some(state_file.clone());
    let dev = Device::start(cfg, system_clock()).await.unwrap();
    let taken = format!("{name}-2");
    eventually("the device to rename itself", || (dev.name() == taken).then_some(())).await;
    let conflict = eventually("the rename to be reported", || dev.name_conflict()).await;
    let renamed = Some(taken.clone());
    assert_eq!(conflict, NameConflict { name, peer: Ipv4Addr::LOCALHOST, renamed_to: renamed });
    // Its own claim, of the name it took, replaces the copied one: a new
    // process on this computer keeps that name.
    let claim = saved_claim(&state_file, &taken).await;
    assert_ne!(claim.mac, copied.mac);
    let saved = SavedState::load(&state_file).unwrap();
    assert_eq!(saved.claimed_name(&dev.info().iface), Some(taken.as_str()));
    soon(dev.shutdown()).await;
    original.stop();
    std::fs::remove_dir_all(dir).unwrap();
}

/// A subscription to a channel of our own name goes to another device that
/// has the name too, rather than silently looping back; without one it
/// loops back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_to_a_shared_name_goes_to_the_other_device() {
    let name = unique("same");
    let clock = system_clock();
    let other = Device::start(config(&name, 25670, 44), clock.clone()).await.unwrap();
    let directory = StaticDirectory::new();
    for entry in other.directory_entries() {
        directory.insert(entry);
    }
    let dev = Device::start_with_directory(config(&name, 25680, 45), clock, directory.clone())
        .await
        .unwrap();

    dev.subscribe(1, "01", &name).unwrap();
    eventually("a flow from the other device", || (other.tx_flows().len() == 1).then_some(()))
        .await;
    assert!(dev.tx_flows().is_empty(), "looped back: {:?}", dev.tx_flows());

    // Our own channel, when that is what the directory knows.
    for entry in dev.directory_entries() {
        directory.insert(entry);
    }
    dev.subscribe(2, "02", &name).unwrap();
    eventually("a flow to ourselves", || (dev.tx_flows().len() == 1).then_some(())).await;
    assert_eq!(other.tx_flows().len(), 1);
    soon(dev.shutdown()).await;
    soon(other.shutdown()).await;
}
