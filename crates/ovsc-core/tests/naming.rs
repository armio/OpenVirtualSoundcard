//! Device names on a network: unique defaults.

use std::net::Ipv4Addr;

use ovsc_core::net::Interface;
use ovsc_core::{DeviceConfig, DeviceInfo};

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
