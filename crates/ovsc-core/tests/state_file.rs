//! A device's saved state across restarts, damaged files included.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ovsc_clock::system_clock;
use ovsc_core::{Channels, Device, DeviceConfig, Ports, SavedState};

fn config(state_file: &Path, base: u16) -> DeviceConfig {
    DeviceConfig {
        name: "state-dev".into(),
        interface: "127.0.0.1".into(),
        tx_channels: Channels::Count(2),
        rx_channels: Channels::Count(2),
        ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
        discovery: false,
        process_id: 61,
        state_file: Some(state_file.to_owned()),
        ..Default::default()
    }
}

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ovsc-state-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn corrupt_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains(".corrupt-"))
        .collect()
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_state_file_is_kept_and_the_backup_restored() {
    let dir = dir("damaged");
    let path = dir.join("state.toml");

    // Two generations: a rename, then a subscription.
    let device = Device::start(config(&path, 26100), system_clock()).await.unwrap();
    assert_eq!(device.saved_state_problem(), None);
    assert_eq!(device.stopped_part(), None);
    device.rename("vocal-fx").unwrap();
    eventually("the rename saved", || {
        SavedState::load(&path).is_some_and(|s| s.name.as_deref() == Some("vocal-fx"))
    })
    .await;
    device.subscribe(1, "Vox", "stagebox").unwrap();
    eventually("the subscription saved", || {
        SavedState::load(&path).is_some_and(|s| s.subscriptions.len() == 1)
    })
    .await;
    device.shutdown().await;

    // A power cut leaves the newest generation empty.
    std::fs::write(&path, "").unwrap();
    let device = Device::start(config(&path, 26100), system_clock()).await.unwrap();
    let problem = device.saved_state_problem().expect("a problem reported").to_owned();
    assert!(problem.contains("could not be read"), "{problem}");
    // The previous generation: the name, without the subscription.
    assert_eq!(device.name(), "vocal-fx");
    assert_eq!(device.rx_channels()[0].subscription, None);
    // The damaged file is kept aside, untouched.
    let aside = corrupt_files(&dir);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(std::fs::read(&aside[0]).unwrap(), b"");
    // The device saves the state it runs with in its place.
    eventually("the restored state saved", || {
        SavedState::load(&path).is_some_and(|s| s.name.as_deref() == Some("vocal-fx"))
    })
    .await;
    device.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_state_is_not_written_again_at_start() {
    let dir = dir("unchanged");
    let path = dir.join("state.toml");
    let device = Device::start(config(&path, 26110), system_clock()).await.unwrap();
    device.rename("fx-rack").unwrap();
    eventually("the rename saved", || SavedState::load(&path).is_some()).await;
    device.shutdown().await;
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    let device = Device::start(config(&path, 26110), system_clock()).await.unwrap();
    assert_eq!(device.name(), "fx-rack");
    tokio::time::sleep(Duration::from_millis(200)).await;
    device.shutdown().await;
    // Neither the start nor the shutdown wrote it again: each save waits
    // for the disk, for nothing.
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}
