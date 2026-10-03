//! Dante discovery conventions on top of mDNS / DNS-SD.
//!
//! A device advertises:
//! * `<device>._netaudio-arc._udp.local` – ARC control port (4440);
//! * `<device>._netaudio-cmc._udp.local` – CMC port (8800) and identity;
//! * `<channel>@<device>._netaudio-chan._udp.local` – one per transmit
//!   channel (and one more per user-assigned name), SRV pointing at the flow
//!   control port (4455) and TXT describing the audio format;
//! * `<bundle id>@<device>._netaudio-bund._udp.local` – multicast flows.

use std::collections::BTreeMap;

use crate::dns::Name;
use crate::{DeviceId, Error, Result};

pub const ARC_SERVICE: &str = "_netaudio-arc._udp.local";
pub const CMC_SERVICE: &str = "_netaudio-cmc._udp.local";
pub const DBC_SERVICE: &str = "_netaudio-dbc._udp.local";
pub const CHAN_SERVICE: &str = "_netaudio-chan._udp.local";
pub const BUND_SERVICE: &str = "_netaudio-bund._udp.local";

/// Longest device name accepted by Dante Controller.
pub const MAX_NAME_LEN: usize = 31;

pub fn service_name(service: &str) -> Name {
    Name::parse(service)
}

/// `<channel>@<device>` – the DNS-SD instance name of a transmit channel.
pub fn channel_instance(channel: &str, device: &str) -> String {
    format!("{channel}@{device}")
}

/// Splits a `<channel>@<device>` instance name. Channel names cannot contain
/// `@`, so the first `@` is the separator.
pub fn split_channel_instance(instance: &str) -> Option<(&str, &str)> {
    instance.split_once('@')
}

/// Validates a device name: 1–31 ASCII letters, digits and inner hyphens.
pub fn validate_device_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    ok.then_some(()).ok_or(Error::Invalid("device name (1-31 of A-Z a-z 0-9 -)"))
}

/// Validates a channel name: 1–31 characters, no `@`, `=` or `.`.
pub fn validate_channel_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_LEN
        && !name.contains(['@', '=', '.'])
        && !name.chars().any(char::is_control);
    ok.then_some(()).ok_or(Error::Invalid("channel name (1-31 chars, no @ = .)"))
}

/// Parses TXT entries (`key=value` or bare `key`) into a map.
pub fn parse_txt(entries: &[Vec<u8>]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .filter(|e| !e.is_empty())
        .map(|e| {
            let s = String::from_utf8_lossy(e);
            match s.split_once('=') {
                Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
                None => (s.into_owned(), None),
            }
        })
        .collect()
}

fn to_entries(items: Vec<String>) -> Vec<Vec<u8>> {
    items.into_iter().map(String::into_bytes).collect()
}

fn parse_int(map: &BTreeMap<String, Option<String>>, key: &'static str) -> Result<u64> {
    let v = map.get(key).and_then(Option::as_deref).ok_or(Error::Invalid(key))?;
    let parsed = match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => v.parse(),
    };
    parsed.map_err(|_| Error::Invalid(key))
}

/// TXT record of `_netaudio-arc._udp`.
pub fn arc_txt(board_name: &str, manufacturer: &str, model: &str) -> Vec<Vec<u8>> {
    to_entries(vec![
        "arcp_vers=2.7.41".into(),
        "arcp_min=0.2.4".into(),
        "router_vers=4.0.2".into(),
        format!("router_info={board_name}"),
        format!("mf={manufacturer}"),
        format!("model={model}"),
    ])
}

/// TXT record of `_netaudio-cmc._udp`.
pub fn cmc_txt(
    device_id: &DeviceId,
    process_id: u16,
    manufacturer: &str,
    model: &str,
) -> Vec<Vec<u8>> {
    let id: String = device_id.iter().map(|b| format!("{b:02x}")).collect();
    to_entries(vec![
        format!("id={id}"),
        format!("process={process_id}"),
        "cmcp_vers=1.2.0".into(),
        "cmcp_min=1.0.0".into(),
        "server_vers=4.0.2".into(),
        "channels=0x6000004d".into(),
        format!("mf={manufacturer}"),
        format!("model={model}"),
    ])
}

/// What a transmit channel's `_netaudio-chan._udp` TXT record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelTxt {
    /// 1-based transmit channel id.
    pub id: u16,
    pub sample_rate: u32,
    /// Encoding bit depth (`enc`).
    pub bits_per_sample: u16,
    /// PCM type (second field of `pcm`), usually `0x0e`.
    pub pcm_type: u8,
    /// Minimum receive latency the transmitter recommends.
    pub latency_ns: u32,
    pub fpp_max: u16,
    pub fpp_min: u16,
    /// Maximum channels per flow.
    pub nchan: u16,
    /// Flow-control protocol id to use when requesting flows.
    pub dbcp1: u16,
    /// Set on the entry for the channel's factory (default) name.
    pub is_default_name: bool,
    /// `(bundle id, 1-based channel in bundle)` if the channel is available
    /// in a multicast flow.
    pub multicast: Option<(u16, u16)>,
}

impl ChannelTxt {
    pub fn to_entries(&self) -> Vec<Vec<u8>> {
        let mut items = vec![
            "txtvers=2".to_owned(),
            format!("dbcp1=0x{:04x}", self.dbcp1),
            "dbcp=0x1004".to_owned(),
            format!("id={}", self.id),
            format!("rate={}", self.sample_rate),
            format!("pcm={} {:x}", self.bits_per_sample / 8, self.pcm_type),
            format!("enc={}", self.bits_per_sample),
            format!("en={}", self.bits_per_sample),
            format!("latency_ns={}", self.latency_ns),
            format!("fpp={},{}", self.fpp_max, self.fpp_min),
            format!("nchan={}", self.nchan),
        ];
        if self.is_default_name {
            items.push("default".into());
        }
        if let Some((bundle, chan)) = self.multicast {
            items.push(format!("b.{bundle}={chan}"));
        }
        to_entries(items)
    }

    pub fn from_entries(entries: &[Vec<u8>]) -> Result<Self> {
        let map = parse_txt(entries);
        let (fpp_max, fpp_min) = map
            .get("fpp")
            .and_then(Option::as_deref)
            .and_then(|v| v.split_once(','))
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
            .ok_or(Error::Invalid("fpp"))?;
        let bits = parse_int(&map, "enc").or_else(|_| parse_int(&map, "en"))?;
        let pcm_type = map
            .get("pcm")
            .and_then(Option::as_deref)
            .and_then(|v| v.split_whitespace().nth(1))
            .and_then(|t| u8::from_str_radix(t, 16).ok())
            .unwrap_or(0x0e);
        let multicast = map.iter().find_map(|(k, v)| {
            let bundle = k.strip_prefix("b.")?.parse().ok()?;
            let chan = v.as_deref()?.parse().ok()?;
            Some((bundle, chan))
        });
        Ok(Self {
            id: parse_int(&map, "id")? as u16,
            sample_rate: parse_int(&map, "rate")? as u32,
            bits_per_sample: bits as u16,
            pcm_type,
            latency_ns: parse_int(&map, "latency_ns").unwrap_or(1_000_000) as u32,
            fpp_max,
            fpp_min,
            nchan: parse_int(&map, "nchan")? as u16,
            dbcp1: parse_int(&map, "dbcp1").unwrap_or(0x1102) as u16,
            is_default_name: map.contains_key("default"),
            multicast,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_txt_round_trip() {
        let txt = ChannelTxt {
            id: 3,
            sample_rate: 48_000,
            bits_per_sample: 24,
            pcm_type: 0x0e,
            latency_ns: 1_000_000,
            fpp_max: 32,
            fpp_min: 2,
            nchan: 8,
            dbcp1: 0x1102,
            is_default_name: true,
            multicast: Some((2, 5)),
        };
        let entries = txt.to_entries();
        assert!(entries.contains(&b"pcm=3 e".to_vec()));
        assert!(entries.contains(&b"fpp=32,2".to_vec()));
        assert_eq!(ChannelTxt::from_entries(&entries).unwrap(), txt);
    }

    #[test]
    fn channel_txt_from_device_without_optional_keys() {
        let entries: Vec<Vec<u8>> =
            ["txtvers=2", "id=12", "rate=96000", "en=32", "fpp=16,4", "nchan=4", "dbcp1=0x1102"]
                .iter()
                .map(|s| s.as_bytes().to_vec())
                .collect();
        let txt = ChannelTxt::from_entries(&entries).unwrap();
        assert_eq!((txt.id, txt.sample_rate, txt.bits_per_sample), (12, 96_000, 32));
        assert_eq!((txt.fpp_max, txt.fpp_min, txt.nchan), (16, 4, 4));
        assert!(!txt.is_default_name);
        assert_eq!(txt.multicast, None);
    }

    #[test]
    fn instance_names() {
        assert_eq!(channel_instance("Out 1", "desk"), "Out 1@desk");
        assert_eq!(split_channel_instance("Out 1@desk"), Some(("Out 1", "desk")));
    }

    #[test]
    fn name_validation() {
        assert!(validate_device_name("studio-mac-2").is_ok());
        assert!(validate_device_name("-bad").is_err());
        assert!(validate_device_name("has space").is_err());
        assert!(validate_device_name(&"x".repeat(32)).is_err());
        assert!(validate_channel_name("Left Main").is_ok());
        assert!(validate_channel_name("a@b").is_err());
    }

    #[test]
    fn cmc_txt_contains_hex_id() {
        let entries =
            cmc_txt(&[0, 0, 10, 0, 0, 2, 0, 1], 1, "OpenVirtualSoundcard", "_000000000000000b");
        let map = parse_txt(&entries);
        assert_eq!(map["id"].as_deref(), Some("00000a0000020001"));
        assert_eq!(map["process"].as_deref(), Some("1"));
    }
}
