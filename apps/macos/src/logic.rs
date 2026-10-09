//! The app's decisions, apart from the UI so they can be tested: what an
//! edit changes and restarts, and the words shown for values and problems.

use std::io;

use ovsc_control::{
    BITS_PER_SAMPLE, ClockInfo, DriverInfo, InterfaceInfo, LATENCY_CHOICES_MS, PROTOCOL_VERSION,
    Restart, SAMPLE_RATES, Settings, SettingsChange, Status,
};

/// The command that starts the daemon, for when it does not run.
pub const START_COMMAND: &str =
    "sudo launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist";

/// The network interface menu's entry for the interface of the default
/// route (an empty setting).
pub const AUTOMATIC_INTERFACE: &str = "Automatic (default route)";

/// The longest device name, as for `ovsc_proto::validate_device_name`.
pub const MAX_NAME_LEN: usize = 31;

/// The fields of `edited` that differ from the daemon's `current` settings.
/// The name is sent without surrounding spaces.
pub fn change(current: &Settings, edited: &Settings) -> SettingsChange {
    let name = edited.name.trim();
    SettingsChange {
        name: (name != current.name).then(|| name.to_owned()),
        interface: (edited.interface != current.interface).then(|| edited.interface.clone()),
        sample_rate: (edited.sample_rate != current.sample_rate).then_some(edited.sample_rate),
        bits_per_sample: (edited.bits_per_sample != current.bits_per_sample)
            .then_some(edited.bits_per_sample),
        rx_channels: (edited.rx_channels != current.rx_channels).then_some(edited.rx_channels),
        tx_channels: (edited.tx_channels != current.tx_channels).then_some(edited.tx_channels),
        latency_ms: (edited.latency_ms != current.latency_ms).then_some(edited.latency_ms),
    }
}

/// What applying `change` should restart. The daemon's answer has the last
/// word; this decides whether to ask first.
pub fn expected_restart(change: &SettingsChange) -> Restart {
    if change.interface.is_some()
        || change.sample_rate.is_some()
        || change.bits_per_sample.is_some()
        || change.rx_channels.is_some()
        || change.tx_channels.is_some()
    {
        Restart::Daemon
    } else if change.latency_ms.is_some() {
        Restart::Device
    } else {
        Restart::None
    }
}

/// What applying a change that restarts `restart` does to the audio, if
/// anything.
pub fn interruption(restart: Restart) -> Option<&'static str> {
    match restart {
        Restart::None => None,
        Restart::Device => {
            Some("Audio stops for a second or two while the network engine restarts.")
        }
        Restart::Daemon => {
            Some("Audio stops for 10–20 seconds while OpenVirtualSoundcard restarts.")
        }
    }
}

/// What the user is told once the daemon accepted a change.
pub fn applied_message(restart: Restart) -> &'static str {
    match restart {
        Restart::None => "Saved.",
        Restart::Device => "Saved. The network engine restarts; audio returns in a second or two.",
        Restart::Daemon => "Saved. OpenVirtualSoundcard restarts; audio returns in 10–20 seconds.",
    }
}

/// The user's edit of the `old` settings carried over to `new` ones from the
/// daemon: fields the user changed keep the edit, the others take the new
/// value.
pub fn rebase(old: &Settings, edited: &Settings, new: &Settings) -> Settings {
    fn pick<T: PartialEq + Clone>(old: &T, edited: &T, new: &T) -> T {
        if edited == old { new.clone() } else { edited.clone() }
    }
    Settings {
        name: if edited.name.trim() == old.name { new.name.clone() } else { edited.name.clone() },
        interface: pick(&old.interface, &edited.interface, &new.interface),
        sample_rate: pick(&old.sample_rate, &edited.sample_rate, &new.sample_rate),
        bits_per_sample: pick(&old.bits_per_sample, &edited.bits_per_sample, &new.bits_per_sample),
        rx_channels: pick(&old.rx_channels, &edited.rx_channels, &new.rx_channels),
        tx_channels: pick(&old.tx_channels, &edited.tx_channels, &new.tx_channels),
        latency_ms: pick(&old.latency_ms, &edited.latency_ms, &new.latency_ms),
    }
}

/// Why `name` cannot be the device name, if it cannot: the daemon takes 1–31
/// ASCII letters, digits and inner hyphens.
pub fn name_problem(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        Some("Enter a name.")
    } else if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Some("Use only letters A–Z, digits and hyphens.")
    } else if name.len() > MAX_NAME_LEN {
        Some("Use at most 31 characters.")
    } else if name.starts_with('-') || name.ends_with('-') {
        Some("The name cannot start or end with a hyphen.")
    } else {
        None
    }
}

/// `x` with at most three decimals and no trailing zeros: `44.1`, `48`.
fn decimal(x: f64) -> String {
    let s = format!("{x:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A sample rate in kHz: `44.1 kHz`.
pub fn rate_label(hz: u32) -> String {
    format!("{} kHz", decimal(f64::from(hz) / 1000.0))
}

/// A bit depth: `24-bit`.
pub fn bits_label(bits: u16) -> String {
    format!("{bits}-bit")
}

/// The bit depths to offer, with `current` among them.
pub fn bits_choices(current: u16) -> Vec<u16> {
    let mut bits = BITS_PER_SAMPLE.to_vec();
    if !bits.contains(&current) {
        bits.push(current);
        bits.sort_unstable();
    }
    bits
}

/// A latency: `0.25 ms`, `4 ms`.
pub fn latency_label(ms: f64) -> String {
    format!("{} ms", decimal(ms))
}

/// The sample rates to offer, with `current` among them.
pub fn rate_choices(current: u32) -> Vec<u32> {
    let mut rates = SAMPLE_RATES.to_vec();
    if !rates.contains(&current) {
        rates.push(current);
        rates.sort_unstable();
    }
    rates
}

/// The latencies to offer, with `current` among them.
pub fn latency_choices(current: f64) -> Vec<f64> {
    let mut choices = LATENCY_CHOICES_MS.to_vec();
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_by(f64::total_cmp);
    }
    choices
}

/// An interface as the menu shows it: `USB 10/100/1000 LAN — en7 (192.168.0.8)`.
pub fn interface_label(interface: &InterfaceInfo) -> String {
    let mut label = String::new();
    if !interface.description.is_empty() && interface.description != interface.name {
        label.push_str(&interface.description);
        label.push_str(" — ");
    }
    label.push_str(&interface.name);
    if !interface.ipv4.is_empty() {
        label.push_str(&format!(" ({})", interface.ipv4.join(", ")));
    }
    label
}

/// The network interface menu: each entry's setting and label. Automatic
/// comes first, then the interfaces, then each of `keep` (the configured and
/// the selected setting) that is none of them, so it stays visible.
pub fn interface_choices(interfaces: &[InterfaceInfo], keep: &[&str]) -> Vec<(String, String)> {
    let mut choices = vec![(String::new(), AUTOMATIC_INTERFACE.to_owned())];
    choices.extend(interfaces.iter().map(|i| (i.name.clone(), interface_label(i))));
    for &value in keep {
        if choices.iter().any(|(v, _)| v == value) {
            continue;
        }
        // The setting may be an address rather than a name.
        let label = match interfaces.iter().find(|i| i.ipv4.iter().any(|a| a == value)) {
            Some(i) => format!("{value} (on {})", i.name),
            None => format!("{value} (not available)"),
        };
        choices.push((value.to_owned(), label));
    }
    choices
}

/// The colour of a status light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Light {
    Green,
    Amber,
    Red,
    Grey,
}

/// The clock's light and word.
pub fn clock_light(clock: &ClockInfo) -> (Light, String) {
    let (light, text) = match clock.state.as_str() {
        "Locked" => (Light::Green, "Locked"),
        "Locking" => (Light::Amber, "Locking"),
        "Holdover" => (Light::Amber, "Holdover"),
        "Unlocked" => (Light::Red, "Unlocked"),
        "FreeRunning" => (Light::Grey, "Free running"),
        other => (if clock.locked { Light::Green } else { Light::Grey }, other),
    };
    (light, text.to_owned())
}

/// Where the clock comes from, in words.
pub fn clock_source_label(source: &str) -> String {
    match source {
        "ptp" => "The network (PTP)".to_owned(),
        "free" => "This Mac's own clock".to_owned(),
        other => other.to_owned(),
    }
}

/// The driver's light and word.
pub fn driver_light(driver: &DriverInfo) -> (Light, &'static str) {
    if driver.connected { (Light::Green, "Connected") } else { (Light::Red, "Not connected") }
}

/// The audio's light and word: amber while the driver is there but holds
/// the audio back.
pub fn audio_light(driver: &DriverInfo) -> (Light, &'static str) {
    if driver.audio_flowing {
        (Light::Green, "Flowing")
    } else if driver.connected {
        (Light::Amber, "Stopped")
    } else {
        (Light::Grey, "Stopped")
    }
}

/// Nanoseconds in microseconds: `-1.2 µs`.
pub fn micros_label(ns: i64) -> String {
    format!("{:.1} µs", ns as f64 / 1000.0)
}

/// A clock rate difference: `+1.250 ppm`.
pub fn ppm_label(ppm: f64) -> String {
    format!("{ppm:+.3} ppm")
}

/// A count with thousands separators: `1,234,567`.
pub fn count_label(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A flow's channels, 1-based with 0 for an empty slot, as ranges: `1–4, 7`.
pub fn channels_label(channels: &[u16]) -> String {
    let mut used: Vec<u16> = channels.iter().copied().filter(|&c| c != 0).collect();
    used.sort_unstable();
    used.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < used.len() {
        let mut j = i;
        while j + 1 < used.len() && used[j + 1] == used[j] + 1 {
            j += 1;
        }
        parts.push(if j == i { used[i].to_string() } else { format!("{}–{}", used[i], used[j]) });
        i = j + 1;
    }
    if parts.is_empty() { "none".to_owned() } else { parts.join(", ") }
}

/// Why the app cannot show or change anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The socket is missing, or nobody listens on it.
    NotRunning,
    /// The socket is closed to this user.
    NotAdmin,
    /// The daemon took too long to answer.
    NoAnswer,
    /// The daemon closed the connection.
    Lost,
    /// The daemon speaks another protocol version (`None`: an answer this app
    /// could not read).
    Version {
        daemon: Option<(String, u32)>,
    },
    /// The daemon answered with an error.
    Daemon(String),
    Other(String),
}

impl Problem {
    /// The problem behind a failed connection or request.
    pub fn from_io(e: &io::Error) -> Problem {
        match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Problem::NotRunning,
            io::ErrorKind::PermissionDenied => Problem::NotAdmin,
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Problem::NoAnswer,
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted => Problem::Lost,
            io::ErrorKind::InvalidData => Problem::Version { daemon: None },
            _ => Problem::Other(e.to_string()),
        }
    }

    /// The problem with a daemon that sends `status`, if any.
    pub fn check(status: &Status) -> Option<Problem> {
        (status.protocol != PROTOCOL_VERSION)
            .then(|| Problem::Version { daemon: Some((status.version.clone(), status.protocol)) })
    }

    /// What went wrong, in a sentence.
    pub fn message(&self) -> String {
        match self {
            Problem::NotRunning => "The OpenVirtualSoundcard daemon is not running.".to_owned(),
            Problem::NotAdmin => "Only administrators can control OpenVirtualSoundcard.".to_owned(),
            Problem::NoAnswer => "The OpenVirtualSoundcard daemon does not answer.".to_owned(),
            Problem::Lost => {
                "The connection to the OpenVirtualSoundcard daemon was lost.".to_owned()
            }
            Problem::Version { daemon: Some((version, protocol)) } => format!(
                "This app and the OpenVirtualSoundcard daemon (version {version}) are different versions: \
                 the app speaks control protocol {PROTOCOL_VERSION}, the daemon {protocol}."
            ),
            Problem::Version { daemon: None } => {
                "This app cannot read the OpenVirtualSoundcard daemon's \
                answers: the app and the daemon are probably different versions."
                    .to_owned()
            }
            Problem::Daemon(message) => {
                format!("The OpenVirtualSoundcard daemon reports: {message}")
            }
            Problem::Other(message) => {
                format!("Cannot reach the OpenVirtualSoundcard daemon: {message}")
            }
        }
    }

    /// What to do about it, if there is advice.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Problem::NotRunning => Some("To start it, run this in Terminal:"),
            Problem::NotAdmin => {
                Some("Log in with an administrator account to see and change its settings.")
            }
            Problem::Version { .. } => {
                Some("Install the app and the daemon from the same release.")
            }
            _ => None,
        }
    }

    /// The command that fixes it, if one does.
    pub fn command(&self) -> Option<&'static str> {
        matches!(self, Problem::NotRunning).then_some(START_COMMAND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_control::{Response, decode_line};

    fn settings() -> Settings {
        Settings {
            name: "studio-mac".into(),
            interface: "en7".into(),
            sample_rate: 48_000,
            bits_per_sample: 24,
            rx_channels: 8,
            tx_channels: 8,
            latency_ms: 4.0,
        }
    }

    fn interface(name: &str, description: &str, ipv4: &[&str]) -> InterfaceInfo {
        InterfaceInfo {
            name: name.into(),
            description: description.into(),
            ipv4: ipv4.iter().map(|&a| a.into()).collect(),
            default_route: false,
        }
    }

    #[test]
    fn an_unchanged_edit_changes_nothing() {
        let s = settings();
        assert!(change(&s, &s).is_empty());
        let spaced = Settings { name: "  studio-mac ".into(), ..settings() };
        assert!(change(&s, &spaced).is_empty());
        assert_eq!(expected_restart(&change(&s, &s)), Restart::None);
    }

    #[test]
    fn the_change_holds_only_the_edited_fields() {
        let s = settings();
        let edited = Settings { name: " stage-left ".into(), latency_ms: 2.0, ..settings() };
        assert_eq!(
            change(&s, &edited),
            SettingsChange {
                name: Some("stage-left".into()),
                latency_ms: Some(2.0),
                ..Default::default()
            }
        );
        let edited = Settings {
            interface: String::new(),
            sample_rate: 96_000,
            rx_channels: 2,
            tx_channels: 64,
            ..settings()
        };
        assert_eq!(
            change(&s, &edited),
            SettingsChange {
                interface: Some(String::new()),
                sample_rate: Some(96_000),
                rx_channels: Some(2),
                tx_channels: Some(64),
                ..Default::default()
            }
        );
    }

    #[test]
    fn restarts_are_predicted_from_the_fields() {
        let only = |f: fn(&mut SettingsChange)| {
            let mut c = SettingsChange::default();
            f(&mut c);
            expected_restart(&c)
        };
        assert_eq!(only(|c| c.name = Some("x".into())), Restart::None);
        assert_eq!(only(|c| c.latency_ms = Some(1.0)), Restart::Device);
        assert_eq!(only(|c| c.interface = Some("en0".into())), Restart::Daemon);
        assert_eq!(only(|c| c.sample_rate = Some(44_100)), Restart::Daemon);
        assert_eq!(only(|c| c.bits_per_sample = Some(16)), Restart::Daemon);
        assert_eq!(only(|c| c.rx_channels = Some(2)), Restart::Daemon);
        assert_eq!(only(|c| c.tx_channels = Some(2)), Restart::Daemon);
        let both = SettingsChange {
            name: Some("x".into()),
            latency_ms: Some(1.0),
            tx_channels: Some(4),
            ..Default::default()
        };
        assert_eq!(expected_restart(&both), Restart::Daemon);
        assert_eq!(interruption(Restart::None), None);
        assert!(interruption(Restart::Daemon).unwrap().contains("10–20 seconds"));
    }

    #[test]
    fn applied_messages() {
        assert_eq!(applied_message(Restart::None), "Saved.");
        assert!(applied_message(Restart::Device).contains("a second or two"));
        assert!(applied_message(Restart::Daemon).contains("10–20 seconds"));
    }

    #[test]
    fn rebase_keeps_edits_and_takes_the_rest() {
        let old = settings();
        let edited = Settings { latency_ms: 1.0, ..settings() };
        let new = Settings { name: "renamed".into(), latency_ms: 10.0, ..settings() };
        let rebased = rebase(&old, &edited, &new);
        assert_eq!(rebased.name, "renamed");
        assert_eq!(rebased.latency_ms, 1.0);
        // Nothing edited: the new settings as they are.
        assert_eq!(rebase(&old, &old, &new), new);
    }

    #[test]
    fn device_names_follow_the_daemons_rule() {
        assert_eq!(name_problem("studio-mac-2"), None);
        assert_eq!(name_problem(" studio "), None);
        assert_eq!(name_problem(&"a".repeat(31)), None);
        assert!(name_problem("").is_some());
        assert!(name_problem("   ").is_some());
        assert!(name_problem(&"a".repeat(32)).is_some());
        assert!(name_problem("studio mac").is_some());
        assert!(name_problem("studio_mac").is_some());
        assert!(name_problem("stüdio").is_some());
        assert!(name_problem("-studio").is_some());
        assert!(name_problem("studio-").is_some());
    }

    #[test]
    fn rate_and_latency_labels() {
        let rates: Vec<String> = SAMPLE_RATES.iter().map(|&r| rate_label(r)).collect();
        assert_eq!(rates, ["44.1 kHz", "48 kHz", "88.2 kHz", "96 kHz", "176.4 kHz", "192 kHz"]);
        assert_eq!(rate_label(22_050), "22.05 kHz");
        assert_eq!(latency_label(1.0), "1 ms");
        assert_eq!(latency_label(0.25), "0.25 ms");
        assert_eq!(latency_label(0.5), "0.5 ms");
        assert_eq!(latency_label(40.0), "40 ms");
    }

    #[test]
    fn current_values_stay_in_the_menus() {
        assert_eq!(latency_choices(4.0), LATENCY_CHOICES_MS);
        let choices = latency_choices(3.0);
        assert_eq!(choices.len(), LATENCY_CHOICES_MS.len() + 1);
        assert!(choices.windows(2).all(|w| w[0] < w[1]));
        assert!(choices.contains(&3.0));
        assert_eq!(rate_choices(48_000), SAMPLE_RATES);
        assert_eq!(rate_choices(32_000)[0], 32_000);
    }

    #[test]
    fn interface_labels() {
        let en7 = interface("en7", "USB 10/100/1000 LAN", &["192.168.0.8"]);
        assert_eq!(interface_label(&en7), "USB 10/100/1000 LAN — en7 (192.168.0.8)");
        assert_eq!(interface_label(&interface("en0", "", &[])), "en0");
        assert_eq!(interface_label(&interface("en0", "en0", &["10.0.0.2"])), "en0 (10.0.0.2)");
        assert_eq!(
            interface_label(&interface("en1", "Wi-Fi", &["10.0.0.3", "169.254.1.2"])),
            "Wi-Fi — en1 (10.0.0.3, 169.254.1.2)"
        );
    }

    #[test]
    fn the_interface_menu_keeps_the_configured_value() {
        let list = [
            interface("en0", "Ethernet", &["10.0.0.2"]),
            interface("en7", "USB 10/100/1000 LAN", &["192.168.0.8"]),
        ];
        let values = |keep: &[&str]| -> Vec<String> {
            interface_choices(&list, keep).into_iter().map(|(v, _)| v).collect()
        };
        assert_eq!(values(&["en7", ""]), ["", "en0", "en7"]);
        assert_eq!(values(&["en9", "en9"]), ["", "en0", "en7", "en9"]);
        let choices = interface_choices(&list, &["en9", "192.168.0.8"]);
        assert_eq!(choices[0].1, AUTOMATIC_INTERFACE);
        assert_eq!(choices[1].1, "Ethernet — en0 (10.0.0.2)");
        assert_eq!(choices[3].1, "en9 (not available)");
        assert_eq!(choices[4].1, "192.168.0.8 (on en7)");
        assert_eq!(interface_choices(&[], &[""]).len(), 1);
    }

    #[test]
    fn lights() {
        let clock = |state: &str, locked: bool| ClockInfo {
            source: "ptp".into(),
            state: state.into(),
            locked,
            leader: None,
            offset_ns: 0,
            path_delay_ns: 0,
            freq_offset_ppm: 0.0,
        };
        assert_eq!(clock_light(&clock("Locked", true)), (Light::Green, "Locked".into()));
        assert_eq!(clock_light(&clock("Locking", false)).0, Light::Amber);
        assert_eq!(clock_light(&clock("Holdover", true)).0, Light::Amber);
        assert_eq!(clock_light(&clock("Unlocked", false)).0, Light::Red);
        assert_eq!(clock_light(&clock("FreeRunning", true)), (Light::Grey, "Free running".into()));
        assert_eq!(clock_light(&clock("Strange", false)), (Light::Grey, "Strange".into()));

        let driver = |connected, audio_flowing| DriverInfo {
            connected,
            io_running: false,
            audio_flowing,
            detail: String::new(),
        };
        assert_eq!(driver_light(&driver(true, false)).0, Light::Green);
        assert_eq!(driver_light(&driver(false, false)).0, Light::Red);
        assert_eq!(audio_light(&driver(true, true)), (Light::Green, "Flowing"));
        assert_eq!(audio_light(&driver(true, false)), (Light::Amber, "Stopped"));
        assert_eq!(audio_light(&driver(false, false)), (Light::Grey, "Stopped"));
    }

    #[test]
    fn number_labels() {
        assert_eq!(micros_label(1_234), "1.2 µs");
        assert_eq!(micros_label(-56_789), "-56.8 µs");
        assert_eq!(ppm_label(1.25), "+1.250 ppm");
        assert_eq!(ppm_label(-0.5), "-0.500 ppm");
        assert_eq!(count_label(0), "0");
        assert_eq!(count_label(999), "999");
        assert_eq!(count_label(1_000), "1,000");
        assert_eq!(count_label(1_234_567), "1,234,567");
        assert_eq!(channels_label(&[1, 2, 3, 4]), "1–4");
        assert_eq!(channels_label(&[1, 0, 3, 4, 7]), "1, 3–4, 7");
        assert_eq!(channels_label(&[0, 0]), "none");
        assert_eq!(channels_label(&[]), "none");
    }

    #[test]
    fn connection_errors_in_words() {
        let problem = |kind| Problem::from_io(&io::Error::from(kind));
        assert_eq!(problem(io::ErrorKind::NotFound), Problem::NotRunning);
        assert_eq!(problem(io::ErrorKind::ConnectionRefused), Problem::NotRunning);
        assert_eq!(problem(io::ErrorKind::PermissionDenied), Problem::NotAdmin);
        assert_eq!(problem(io::ErrorKind::WouldBlock), Problem::NoAnswer);
        assert_eq!(problem(io::ErrorKind::UnexpectedEof), Problem::Lost);
        assert_eq!(problem(io::ErrorKind::InvalidData), Problem::Version { daemon: None });
        assert_eq!(
            Problem::NotRunning.message(),
            "The OpenVirtualSoundcard daemon is not running."
        );
        assert_eq!(Problem::NotRunning.command(), Some(START_COMMAND));
        assert_eq!(
            Problem::NotAdmin.message(),
            "Only administrators can control OpenVirtualSoundcard."
        );
        assert_eq!(Problem::NotAdmin.command(), None);
        assert!(matches!(problem(io::ErrorKind::OutOfMemory), Problem::Other(_)));
    }

    #[test]
    fn another_protocol_is_a_problem() {
        let line = r#"{"result":"status","protocol":1,"version":"0.1.0","engine_running":false,
            "engine_error":"no interface","device":null,
            "clock":{"source":"ptp","state":"Unlocked","locked":false,"leader":null,
                "offset_ns":0,"path_delay_ns":0,"freq_offset_ppm":0.0},
            "driver":{"connected":false,"io_running":false,"audio_flowing":false,"detail":""},
            "warnings":[]}"#
            .replace('\n', " ");
        let Response::Status(mut status) = decode_line(&line).unwrap() else {
            panic!("not a status");
        };
        status.protocol = PROTOCOL_VERSION;
        assert_eq!(Problem::check(&status), None);
        status.protocol = PROTOCOL_VERSION + 1;
        status.version = "9.0.0".into();
        let problem = Problem::check(&status).unwrap();
        assert!(problem.message().contains("different versions"));
        assert!(problem.message().contains("9.0.0"));
    }
}
