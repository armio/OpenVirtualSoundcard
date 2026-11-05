//! The least latency follows the transmit guard however a latency arrives:
//! from the configuration, from a controller or the app, or from the state
//! file. Otherwise a value accepted on one path would stop the device from
//! starting on its next restart.

use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use ovsc_clock::system_clock;
use ovsc_core::{Channels, Device, DeviceConfig, Ports, SavedState};

fn config(dir: &Path) -> DeviceConfig {
    DeviceConfig {
        name: "lat-limits".into(),
        interface: Ipv4Addr::LOCALHOST.to_string(),
        tx_channels: Channels::Count(2),
        rx_channels: Channels::Count(2),
        latency_ms: 4.0,
        tx_latency_ms: Some(1.5),
        // 0.8 ms: the least latency is 1.3 ms.
        tx_guard_us: 800,
        ports: Ports { arc: 24700, cmc: 24701, flow_control: 24702, settings: 24703 },
        discovery: false,
        process_id: 11,
        state_file: Some(dir.join("state.toml")),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_longer_guard_raises_the_least_latency_on_every_path() {
    let dir = std::env::temp_dir().join(format!("ovsc-latency-limits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let state = dir.join("state.toml");

    // A latency saved earlier (1 ms, as Dante Controller allows) that this
    // guard rules out is ignored: the device starts at its configured one.
    SavedState::update(&state, |s| s.latency_ns = Some(1_000_000)).unwrap();
    let dev = Device::start(config(&dir), system_clock()).await.unwrap();
    assert_eq!(dev.latency_ns(), 4_000_000);
    assert_eq!(dev.info().tx_latency_ns, 1_500_000);

    // Controllers and the app go through request_latency.
    assert!(dev.request_latency(1_000_000).is_err());
    assert!(dev.request_latency(1_299_000).is_err());
    assert_eq!(dev.configured_latency_ns(), 4_000_000);
    dev.request_latency(1_300_000).unwrap();
    assert_eq!(dev.configured_latency_ns(), 1_300_000);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while SavedState::load(&state).and_then(|s| s.latency_ns) != Some(1_300_000) {
        assert!(std::time::Instant::now() < deadline, "the latency was not saved");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Applied while the device runs: the receive side only.
    assert!(dev.restart_rx());
    assert_eq!((dev.latency_ns(), dev.latency_samples()), (1_300_000, 62));
    assert_eq!(dev.info().tx_latency_ns, 1_500_000);
    dev.shutdown().await;

    // The saved latency applies on the next start; the transmit latency
    // stays the configured one.
    let dev = Device::start(config(&dir), system_clock()).await.unwrap();
    assert_eq!(dev.latency_ns(), 1_300_000);
    assert_eq!(dev.info().tx_latency_ns, 1_500_000);
    dev.shutdown().await;
    std::fs::remove_dir_all(&dir).unwrap();
}
