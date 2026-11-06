//! The app's decisions, apart from the UI so they can be tested: what an
//! edit changes and restarts, what the status means for the audio, and the
//! words shown for values and problems.

use std::collections::VecDeque;
use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use ovsc_control::{
    BITS_PER_SAMPLE, ClockInfo, DeviceStatus, DriverInfo, InterfaceInfo, LATENCY_CHOICES_MS,
    MAX_CHANNELS, PROTOCOL_VERSION, PacketCounts, Restart, RxChannelStatus, SAMPLE_RATES, Settings,
    SettingsChange, Status, TxFlowStatus,
};

/// The command that starts the daemon, for when it does not run.
pub const START_COMMAND: &str =
    "sudo launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist";

/// The network interface menu's entry for the interface of the default
/// route (an empty setting), when no interface carries it.
pub const AUTOMATIC_INTERFACE: &str = "Automatic (default route)";

/// The longest device name, as for `ovsc_proto::validate_device_name`.
pub const MAX_NAME_LEN: usize = 31;

/// The placeholder for a value that is absent or not measured.
pub const NONE: &str = "—";

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
        lock_remote_settings: (edited.lock_remote_settings != current.lock_remote_settings)
            .then_some(edited.lock_remote_settings),
    }
}

/// `settings` with `change` applied: what the daemon holds once it accepted
/// the change. Fields left out keep their value, so an edit of other fields
/// stays pending after a change of some.
pub fn with_change(settings: &Settings, change: &SettingsChange) -> Settings {
    let mut s = settings.clone();
    if let Some(name) = &change.name {
        s.name = name.clone();
    }
    if let Some(interface) = &change.interface {
        s.interface = interface.clone();
    }
    if let Some(rate) = change.sample_rate {
        s.sample_rate = rate;
    }
    if let Some(bits) = change.bits_per_sample {
        s.bits_per_sample = bits;
    }
    if let Some(n) = change.rx_channels {
        s.rx_channels = n;
    }
    if let Some(n) = change.tx_channels {
        s.tx_channels = n;
    }
    if let Some(ms) = change.latency_ms {
        s.latency_ms = ms;
    }
    if let Some(lock) = change.lock_remote_settings {
        s.lock_remote_settings = lock;
    }
    s
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
        Restart::Receive
    } else {
        Restart::None
    }
}

/// What applying a change that restarts `restart` does to the audio, if
/// anything: the cost in one line, then the details. A new latency restarts
/// the inputs only. A restart of the network engine (by daemons before that)
/// or of the daemon closes the flows other devices receive from this one
/// too, so the returns to a console drop with the inputs.
pub fn interruption(restart: Restart) -> Option<(&'static str, &'static str)> {
    match restart {
        Restart::None => None,
        Restart::Receive => Some((
            "The inputs stop for about a second.",
            "This device requests its inputs again at the new latency. The returns to the \
             console carry on. Core Audio sees the inputs' timing change, and some applications \
             restart their audio engine on that.",
        )),
        Restart::Device => Some((
            "Audio stops for a second or two in both directions.",
            "The network engine restarts: this device's inputs stop, and so do the returns to \
             the console. The console requests the returns again on its own, which can take a \
             few more seconds.",
        )),
        Restart::Daemon => Some((
            "Audio stops for 10–20 s in both directions.",
            "OpenVirtualSoundcard restarts: this device's inputs stop, and so do the returns to \
             the console. The console requests the returns again on its own, which can take a \
             few more seconds.",
        )),
    }
}

/// The devices receiving from this one, as the status lists them: names,
/// or addresses for those that gave none.
pub fn receivers(device: Option<&DeviceStatus>) -> Vec<String> {
    let mut names: Vec<String> =
        device.map(|d| &d.tx_flows[..]).unwrap_or_default().iter().map(flow_receiver).collect();
    names.sort();
    names.dedup();
    names
}

/// What renaming the device from `old` to `new` does to the devices
/// subscribed to its channels (`receivers` receive from it now). Dante
/// subscriptions name the transmitting device (`channel@device`), so theirs
/// stop resolving, and stay so after any restart, until they are patched
/// again.
pub fn rename_warning(old: &str, new: &str, receivers: &[String]) -> String {
    let who = match receivers {
        [] => "Devices subscribed to this device's channels".to_owned(),
        [one] => format!("{one}, which receives from this device now,"),
        many => format!("{}, which receive from this device now,", list_words(many)),
    };
    format!(
        "{who} name it “{old}” in their subscriptions. Those stop resolving, and the returns to \
         the console go silent, until they are patched again to “{new}” in Dante Controller."
    )
}

/// What the user must accept before a change is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    pub title: &'static str,
    /// What changes, one line per field: `Receive channels: 8 → 16`.
    pub changes: Vec<String>,
    /// The cost in one line, shown first.
    pub cost: String,
    /// The rest, in paragraphs.
    pub text: String,
    pub button: &'static str,
}

/// The confirmation `change` needs, if it interrupts audio or breaks other
/// devices' subscriptions: a restart of either kind, or a new name.
/// `current` is the device's settings now, `receivers` who receives from it
/// and `interfaces` the interfaces it could use (for the words).
pub fn confirmation(
    change: &SettingsChange,
    current: &Settings,
    receivers: &[String],
    interfaces: &[InterfaceInfo],
) -> Option<Confirmation> {
    let restart = expected_restart(change);
    let rename = change.name.as_ref().map(|new| rename_warning(&current.name, new, receivers));
    let (title, button) = match (restart, rename.is_some()) {
        (Restart::None, false) => return None,
        (Restart::None, true) => ("Rename the device?", "Rename"),
        (Restart::Receive, false) => ("Change the receive latency?", "Change"),
        (Restart::Receive, true) => ("Rename the device and change its latency?", "Apply"),
        (Restart::Device, false) => ("Restart the network engine?", "Restart"),
        (Restart::Daemon, false) => ("Restart OpenVirtualSoundcard?", "Restart"),
        (Restart::Device | Restart::Daemon, true) => ("Rename the device and restart?", "Restart"),
    };
    let mut costs = Vec::new();
    let mut text = Vec::new();
    if let Some((cost, details)) = interruption(restart) {
        costs.push(cost);
        // A rename stops the returns whatever restarts: the latency's
        // details must not say that they carry on.
        text.push(if rename.is_some() && restart == Restart::Receive {
            RECEIVE_WITH_RENAME.to_owned()
        } else {
            details.to_owned()
        });
    }
    if let Some(warning) = rename {
        costs.push("Other devices lose their subscriptions to this one.");
        text.push(warning);
    }
    Some(Confirmation {
        title,
        changes: change_lines(current, change, interfaces),
        cost: costs.join(" "),
        text: text.join("\n\n"),
        button,
    })
}

/// [`interruption`]'s details for a new latency that comes with a new name,
/// which stops the returns: without the sentence that they carry on.
const RECEIVE_WITH_RENAME: &str = "This device requests its inputs again at the new latency. Core \
    Audio sees the inputs' timing change, and some applications restart their audio engine on \
    that.";

/// One line per changed field, old value to new: `Latency: 1 ms → 2 ms`.
pub fn change_lines(
    current: &Settings,
    change: &SettingsChange,
    interfaces: &[InterfaceInfo],
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = |label: &str, old: String, new: String| {
        lines.push(format!("{label}: {old} → {new}"));
    };
    if let Some(name) = &change.name {
        let old = if current.name.is_empty() { NONE.to_owned() } else { current.name.clone() };
        line("Name", old, name.clone());
    }
    if let Some(interface) = &change.interface {
        line(
            "Network interface",
            interface_setting_label(interfaces, &current.interface),
            interface_setting_label(interfaces, interface),
        );
    }
    if let Some(rate) = change.sample_rate {
        line("Sample rate", rate_label(current.sample_rate), rate_label(rate));
    }
    if let Some(bits) = change.bits_per_sample {
        line("Bit depth", bits_label(current.bits_per_sample), bits_label(bits));
    }
    if let Some(n) = change.rx_channels {
        line("Receive channels", current.rx_channels.to_string(), n.to_string());
    }
    if let Some(n) = change.tx_channels {
        line("Transmit channels", current.tx_channels.to_string(), n.to_string());
    }
    if let Some(ms) = change.latency_ms {
        line("Receive latency", latency_label(current.latency_ms), latency_label(ms));
    }
    if let Some(lock) = change.lock_remote_settings {
        line("Show lock", lock_label(current.lock_remote_settings).into(), lock_label(lock).into());
    }
    lines
}

/// What the show lock does, for the settings form.
pub const LOCK_HELP: &str = "While on, Dante Controller cannot change the latency, sample rate, \
    bit depth or names. Routing stays open, and changes made here still apply.";

/// The show lock's state.
pub fn lock_label(locked: bool) -> &'static str {
    if locked { "On" } else { "Off" }
}

/// What became of an accepted change, for the line under the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Done: a short confirmation.
    Saved(String),
    /// Done, and the user has something to do (re-patch after a rename).
    ToDo(String),
    /// The daemon restarts to apply it: the app follows the restart, and
    /// says what is left to do once it is back.
    Restarting { todo: Option<String> },
}

/// What a rename to `new` leaves to do; `receivers` received from this
/// device.
pub fn rename_todo(new: &str, receivers: &[String]) -> String {
    let who = if receivers.is_empty() {
        String::new()
    } else {
        format!(" ({} received from this device)", list_words(receivers))
    };
    format!(
        "To do: patch the subscriptions to this device's channels again, to “{new}”, in Dante \
         Controller{who}."
    )
}

/// What the user is told once the daemon accepted `change`, which restarts
/// `restart`. `receivers` receive from this device.
pub fn applied(change: &SettingsChange, restart: Restart, receivers: &[String]) -> Applied {
    let todo = change.name.as_deref().map(|new| rename_todo(new, receivers));
    let saved = |text: &str| match &todo {
        Some(todo) => Applied::ToDo(format!("{text} {todo}")),
        None => Applied::Saved(text.to_owned()),
    };
    match restart {
        Restart::Daemon => Applied::Restarting { todo },
        // With a rename the console cannot request the returns again
        // until it is re-patched.
        Restart::Device if todo.is_some() => {
            saved("Saved. The network engine restarts; the inputs return in a second or two.")
        }
        Restart::Device => saved(
            "Saved. The network engine restarts; audio returns in a second or two, and the \
             console requests the returns again on its own.",
        ),
        Restart::Receive if todo.is_some() => {
            saved("Saved. The inputs return at the new latency in about a second.")
        }
        Restart::Receive => saved(
            "Saved. The inputs return at the new latency in about a second; the returns carried \
             on.",
        ),
        Restart::None if todo.is_some() => saved("Renamed."),
        Restart::None => saved(match change.lock_remote_settings {
            Some(true) => "Show lock on: Dante Controller can no longer change these settings.",
            Some(false) => "Show lock off: Dante Controller can change these settings again.",
            None => "Saved.",
        }),
    }
}

/// Whether `change` only turns the show lock on or off, which the form
/// applies as soon as it is switched.
pub fn lock_only(change: &SettingsChange) -> bool {
    change.lock_remote_settings.is_some()
        && SettingsChange { lock_remote_settings: None, ..change.clone() }.is_empty()
}

/// A show lock the daemon refused to switch, in words.
pub fn lock_refusal(message: &str) -> String {
    refusal(message).replacen("Not saved:", "Show lock not changed:", 1)
}

/// A change the daemon refused, in words: `Not saved: The name is taken.`
pub fn refusal(message: &str) -> String {
    let message = message.trim();
    let mut text = format!("Not saved: {}", sentence_case(message));
    if !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
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
        lock_remote_settings: pick(
            &old.lock_remote_settings,
            &edited.lock_remote_settings,
            &new.lock_remote_settings,
        ),
    }
}

/// What is wrong with the name in the form, if the user changed it. An
/// unchanged name is never a problem, even an empty one: a device without a
/// name of its own (`name = ""`, named after its interface once it runs)
/// shows an empty name while its engine waits for the network, and the
/// other settings must still apply.
pub fn edited_name_problem(current: &str, edited: &str) -> Option<&'static str> {
    if edited == current { None } else { name_problem(edited) }
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

/// The channel counts to offer, with `current` among them. A menu rather
/// than a number to drag: every change of a count restarts the daemon.
pub fn channel_choices(current: u16) -> Vec<u16> {
    let mut choices: Vec<u16> = [1, 2, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, MAX_CHANNELS]
        .into_iter()
        .filter(|&n| n <= MAX_CHANNELS)
        .collect();
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_unstable();
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

/// The interface the empty setting resolves to now: the one with the
/// default route.
fn default_route(interfaces: &[InterfaceInfo]) -> Option<&InterfaceInfo> {
    interfaces.iter().find(|i| i.default_route)
}

/// The menu entry for the empty setting, with the interface it picks now:
/// `Automatic (now Wi-Fi — en0)`.
pub fn automatic_label(interfaces: &[InterfaceInfo]) -> String {
    match default_route(interfaces) {
        Some(i) if !i.description.is_empty() && i.description != i.name => {
            format!("Automatic (now {} — {})", i.description, i.name)
        }
        Some(i) => format!("Automatic (now {})", i.name),
        None => AUTOMATIC_INTERFACE.to_owned(),
    }
}

/// The network interface menu: each entry's setting and label. Automatic
/// comes first, then the interfaces, then each of `keep` (the configured and
/// the selected setting) that is none of them, so it stays visible.
pub fn interface_choices(interfaces: &[InterfaceInfo], keep: &[&str]) -> Vec<(String, String)> {
    let mut choices = vec![(String::new(), automatic_label(interfaces))];
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

/// An interface setting in words, as the menu shows it.
pub fn interface_setting_label(interfaces: &[InterfaceInfo], setting: &str) -> String {
    interface_choices(interfaces, &[setting])
        .into_iter()
        .find(|(v, _)| v == setting)
        .map_or_else(|| setting.to_owned(), |(_, label)| label)
}

/// A warning for a setting that would put Dante audio on Wi-Fi: the named
/// interface, or the one the automatic setting picks now.
pub fn wifi_warning(interfaces: &[InterfaceInfo], setting: &str) -> Option<&'static str> {
    let interface = if setting.is_empty() {
        default_route(interfaces)
    } else {
        interfaces.iter().find(|i| i.name == setting || i.ipv4.iter().any(|a| a == setting))
    }?;
    let d = interface.description.to_ascii_lowercase();
    (d.contains("wi-fi") || d.contains("wifi") || d.contains("airport") || d.contains("wlan"))
        .then_some("Wi-Fi is not suitable for Dante audio: choose the wired interface.")
}

/// The colour of a status light.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Light {
    Green,
    Amber,
    Red,
    Grey,
}

/// A status light: its colour, its word, a short note for the tile, and
/// what it means in a sentence (the tile's hover and VoiceOver).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lamp {
    pub light: Light,
    pub word: String,
    pub note: String,
    pub detail: String,
}

impl Lamp {
    pub fn new(
        light: Light,
        word: &str,
        note: impl Into<String>,
        detail: impl Into<String>,
    ) -> Lamp {
        Lamp { light, word: word.to_owned(), note: note.into(), detail: detail.into() }
    }
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

/// The clock lamp: its light and word, and what the state means.
pub fn clock_lamp(clock: &ClockInfo) -> Lamp {
    let (light, word) = clock_light(clock);
    let leader = clock.leader.as_deref().unwrap_or("the leader");
    let (note, detail) = match clock.state.as_str() {
        "Locked" => (format!("To {leader}"), format!("Following the clock of {leader}.")),
        "Locking" => (
            "Settling on the leader".to_owned(),
            format!("Locking to {leader}: audio may be held back for a few seconds."),
        ),
        "Holdover" => (
            "No leader heard".to_owned(),
            "No leader heard: running on its own for now. Audio stops if none returns in time."
                .to_owned(),
        ),
        "Unlocked" => (
            "No clock leader".to_owned(),
            "No clock leader reaches this computer: inputs and returns are silent.".to_owned(),
        ),
        "FreeRunning" => (
            "This Mac's own clock".to_owned(),
            "This Mac's own clock: not in sync with a Dante network.".to_owned(),
        ),
        other => (String::new(), format!("Clock state: {other}.")),
    };
    Lamp { light, word, note, detail }
}

/// Where the clock comes from, in words.
pub fn clock_source_label(source: &str) -> String {
    match source {
        "ptp" => "The network (PTP)".to_owned(),
        "free" => "This Mac's own clock".to_owned(),
        other => other.to_owned(),
    }
}

/// Whether the clock's measurements mean anything: there is a leader to
/// measure against.
fn clock_measured(clock: &ClockInfo) -> bool {
    clock.leader.is_some() && matches!(clock.state.as_str(), "Locked" | "Locking" | "Holdover")
}

/// The clock section's rows. The measurements read `—` without a leader:
/// the daemon sends zeros then, which would read as perfect sync.
pub fn clock_rows(clock: &ClockInfo) -> Vec<(&'static str, String)> {
    let measured = clock_measured(clock);
    let or_none = |value: String| if measured { value } else { NONE.to_owned() };
    vec![
        ("Source", clock_source_label(&clock.source)),
        ("State", clock_light(clock).1),
        ("Leader", clock.leader.clone().unwrap_or_else(|| NONE.to_owned())),
        ("Offset", or_none(micros_label(clock.offset_ns))),
        ("Network delay", or_none(micros_label(clock.path_delay_ns))),
        ("Drift", or_none(ppm_label(clock.freq_offset_ppm))),
    ]
}

/// The command that brings a failed driver back.
pub const RESTART_CORE_AUDIO: &str = "sudo killall coreaudiod";

/// The driver's light and word: red when missing or failed.
pub fn driver_light(driver: &DriverInfo) -> (Light, &'static str) {
    if driver.faulted {
        (Light::Red, "Failed")
    } else if driver.connected {
        (Light::Green, "Loaded")
    } else {
        (Light::Red, "Not loaded")
    }
}

/// The driver lamp, with what the state means.
pub fn driver_lamp(driver: &DriverInfo) -> Lamp {
    let (light, word) = driver_light(driver);
    let (note, detail) = if driver.faulted {
        (
            "Restart Core Audio",
            "The driver has failed: every input and return is silent until Core Audio restarts.",
        )
    } else if !driver.connected {
        (
            "Restart Core Audio",
            "Core Audio has not loaded the driver: no application can use OpenVirtualSoundcard.",
        )
    } else if driver.io_running {
        ("In use", "Core Audio has loaded the driver, and an application is using it.")
    } else {
        ("Not in use", "Core Audio has loaded the driver; no application is using it.")
    };
    Lamp::new(light, word, note, detail)
}

/// The first sentence of a warning: what happened and what it does.
pub fn headline(text: &str) -> &str {
    warning_parts(text).0
}

/// The audio lamp, from everything the status says: red whenever audio is
/// down (the driver failed or missing, the engine stopped, or anything the
/// daemon marks urgent), amber while it flows with recent problems or is
/// held back for a moment, green only while an application plays through
/// it, and grey when none does.
pub fn audio_lamp(status: &Status) -> Lamp {
    let driver = &status.driver;
    let (urgent, _) = split_warnings(status);
    if driver.faulted {
        Lamp::new(
            Light::Red,
            "Silent",
            "The driver has failed",
            format!(
                "The driver has failed: every input and return is silent. Restart Core Audio \
                 with {RESTART_CORE_AUDIO} in Terminal."
            ),
        )
    } else if !driver.connected {
        Lamp::new(
            Light::Red,
            "No driver",
            "Core Audio has not loaded it",
            "Core Audio has not loaded the driver: no application can use OpenVirtualSoundcard.",
        )
    } else if !status.engine_running {
        Lamp::new(
            Light::Red,
            "No network",
            "The network engine stopped",
            "The network engine is not running: inputs and returns are silent.",
        )
    } else if let Some(first) = urgent.first() {
        Lamp::new(Light::Red, "Down", "See the alert below", headline(first))
    } else if driver.io_running && !driver.audio_flowing {
        Lamp::new(
            Light::Amber,
            "Held back",
            "Silent for a moment",
            "An application is using OpenVirtualSoundcard, but the driver holds the audio back \
             for now: inputs and returns are silent.",
        )
    } else if driver.io_running && driver.audio_trouble {
        Lamp::new(
            Light::Amber,
            "Dropouts",
            "In the last 30 s",
            "Audio flows, with dropouts or other problems in the last 30 s: see the warnings.",
        )
    } else if driver.io_running {
        Lamp::new(
            Light::Green,
            "Flowing",
            "An application is playing",
            "An application is playing and recording through OpenVirtualSoundcard.",
        )
    } else {
        let mut detail = "No application is using OpenVirtualSoundcard: the returns carry \
                          silence. Choose it as the host's input and output."
            .to_owned();
        if !driver.audio_flowing {
            let why = match status.clock.state.as_str() {
                "Locked" => "the driver holds it back for a moment".to_owned(),
                _ => format!("the clock is {}", clock_light(&status.clock).1.to_lowercase()),
            };
            detail.push_str(&format!(" Audio would not flow yet: {why}."));
        }
        Lamp::new(Light::Grey, "Idle", "No application is playing", detail)
    }
}

/// The audio lamp while a change the user accepted restarts the daemon,
/// until audio is back: the outage is expected.
pub fn restarting_lamp() -> Lamp {
    Lamp::new(
        Light::Amber,
        "Restarting",
        "Back in 10–20 s",
        "OpenVirtualSoundcard is restarting: audio returns in 10–20 s.",
    )
}

/// The lamps while the app has no status: what it knows about the audio
/// from the problem. A daemon that is not running, or not answering, means
/// a silent device; an expected restart is amber.
pub fn absent_lamps(problem: Option<&Problem>, restarting: bool) -> [Lamp; 3] {
    let unknown = || Lamp::new(Light::Grey, NONE, "", "");
    let audio = if restarting {
        restarting_lamp()
    } else {
        match problem {
            Some(Problem::NotRunning) => Lamp::new(
                Light::Red,
                "Off",
                "The daemon is not running",
                "The daemon is not running: OpenVirtualSoundcard stays in Core Audio, silent.",
            ),
            Some(Problem::NoAnswer) => Lamp::new(
                Light::Red,
                "No answer",
                "The daemon does not answer",
                "The daemon does not answer: audio may have stopped.",
            ),
            Some(Problem::Lost) => Lamp::new(
                Light::Red,
                "Lost",
                "Reconnecting",
                "The connection to the daemon was lost.",
            ),
            _ => Lamp::new(Light::Grey, "Unknown", "", "The app cannot read the daemon's status."),
        }
    };
    [audio, unknown(), unknown()]
}

/// The status's warnings, as the urgent ones (audio is down now: shown in
/// red) and the others. A daemon before the split marks none urgent.
pub fn split_warnings(status: &Status) -> (&[String], &[String]) {
    status.warnings.split_at(status.urgent_warnings.min(status.warnings.len()))
}

/// Whether a warning is news of a controller's change rather than a
/// problem: the daemon lists them after the problems, for ten minutes.
pub fn is_controller_news(warning: &str) -> bool {
    warning.starts_with("A controller ")
}

/// The other warnings as problems and controllers' changes.
pub fn problems_and_news(other: &[String]) -> (Vec<&str>, Vec<&str>) {
    let (news, problems): (Vec<&str>, Vec<&str>) =
        other.iter().map(String::as_str).partition(|w| is_controller_news(w));
    (problems, news)
}

/// A warning as its first sentence and the rest: the daemon writes `what
/// happened: effect. What to do.`
pub fn warning_parts(text: &str) -> (&str, &str) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text[i..].find(". ") {
        let end = i + at + 1;
        let next = bytes.get(end + 1).copied().unwrap_or(b' ');
        if next.is_ascii_uppercase() || next == b'`' || next == b'(' {
            return (&text[..end], text[end + 1..].trim_start());
        }
        i = end;
    }
    (text, "")
}

/// A text as plain and code spans, the code between backticks. An odd
/// number of backticks leaves the text plain.
pub fn code_spans(text: &str) -> Vec<(bool, &str)> {
    if text.matches('`').count() % 2 == 1 {
        return vec![(false, text)];
    }
    text.split('`')
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| (i % 2 == 1, s))
        .collect()
}

/// The commands in a text: code spans that run something as root.
pub fn commands(text: &str) -> Vec<&str> {
    code_spans(text)
        .into_iter()
        .filter(|&(code, s)| code && s.starts_with("sudo "))
        .map(|(_, s)| s)
        .collect()
}

/// Nanoseconds in microseconds, with a true minus sign: `−1.2 µs`.
pub fn micros_label(ns: i64) -> String {
    format!("{} µs", signed(ns as f64 / 1000.0, 1, false))
}

/// A clock's drift against its leader: `+1.25 ppm`.
pub fn ppm_label(ppm: f64) -> String {
    format!("{} ppm", signed(ppm, 2, true))
}

/// `x` with `decimals`, a true minus sign, and a plus sign if asked.
fn signed(x: f64, decimals: usize, plus: bool) -> String {
    let s = format!("{:.*}", decimals, x.abs());
    let zero = s.bytes().all(|b| b == b'0' || b == b'.');
    if x < 0.0 && !zero {
        format!("−{s}")
    } else if plus && !zero {
        format!("+{s}")
    } else {
        s
    }
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

/// Words joined for a sentence: `A`, `A and B`, `A, B and C`.
fn list_words(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// `text` with its first letter capitalised.
pub fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A receive channel's light: green while it receives, amber while it
/// connects, red when subscribed and not receiving, grey when unused.
pub fn channel_light(channel: &RxChannelStatus) -> Light {
    if channel.source.is_none() {
        Light::Grey
    } else if channel.receiving {
        Light::Green
    } else if channel.state == "connecting" {
        Light::Amber
    } else {
        Light::Red
    }
}

/// A receive channel's subscription state in words: `Receiving`, `—`.
pub fn subscription_label(channel: &RxChannelStatus) -> String {
    if channel.state.is_empty() { NONE.to_owned() } else { sentence_case(&channel.state) }
}

/// One line on the receive channels, and its light: `7 of 7 receiving · 1
/// unused`, or the subscribed channels that get no audio, by name.
pub fn rx_summary(channels: &[RxChannelStatus]) -> (Light, String) {
    let subscribed: Vec<&RxChannelStatus> =
        channels.iter().filter(|c| c.source.is_some()).collect();
    let unused = channels.len() - subscribed.len();
    let unused_words = if unused > 0 { format!(" · {unused} unused") } else { String::new() };
    if subscribed.is_empty() {
        return (Light::Grey, "No subscriptions".to_owned());
    }
    let silent: Vec<&RxChannelStatus> =
        subscribed.iter().copied().filter(|c| !c.receiving).collect();
    if silent.is_empty() {
        return (
            Light::Green,
            format!("{} of {} receiving{unused_words}", subscribed.len(), subscribed.len()),
        );
    }
    let names: Vec<String> = silent.iter().take(3).map(|c| c.name.clone()).collect();
    let mut who = names.join(", ");
    if silent.len() > 3 {
        who.push_str(&format!(" and {} more", silent.len() - 3));
    }
    let light = if silent.iter().all(|c| channel_light(c) == Light::Amber) {
        Light::Amber
    } else {
        Light::Red
    };
    (light, format!("{} of {} not receiving: {who}", silent.len(), subscribed.len()))
}

/// Who receives a flow: its name, `Multicast`, or its address.
pub fn flow_receiver(flow: &TxFlowStatus) -> String {
    if let Some(name) = &flow.receiver {
        return name.clone();
    }
    let ip = address_label(&flow.destination);
    match ip.parse::<Ipv4Addr>() {
        Ok(addr) if addr.is_multicast() => "Multicast".to_owned(),
        _ => ip,
    }
}

/// An address without its port: `192.168.1.10`.
pub fn address_label(destination: &str) -> String {
    destination.rsplit_once(':').map_or(destination, |(ip, _)| ip).to_owned()
}

/// One line on the returns, and its light: who receives from this device.
pub fn tx_summary(device: &DeviceStatus) -> (Light, String) {
    let channels = device.tx_channels.len();
    let receivers = receivers(Some(device));
    if receivers.is_empty() {
        return (Light::Grey, format!("{channels} channels · no device receives from this one"));
    }
    (Light::Green, format!("{channels} channels · received by {}", list_words(&receivers)))
}

/// The device in one line, for under its name: `48 kHz · 24-bit · 1 ms ·
/// en7 (192.168.1.50)`.
pub fn device_summary(device: &DeviceStatus) -> String {
    let mut parts = vec![
        rate_label(device.sample_rate),
        bits_label(device.bits_per_sample),
        format!("{} latency", latency_label(device.latency_ms)),
    ];
    match (device.interface.is_empty(), device.ip.is_empty()) {
        (false, false) => parts.push(format!("{} ({})", device.interface, device.ip)),
        (false, true) => parts.push(device.interface.clone()),
        (true, false) => parts.push(device.ip.clone()),
        (true, true) => {}
    }
    parts.join(" · ")
}

/// How long ago, in a few characters: `12 s`, `4 min`, `2 h 5 min`.
pub fn ago_label(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        let (h, m) = (s / 3600, s % 3600 / 60);
        if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
    }
}

/// The packet counters of the last half minute, to show how much a total
/// rose lately rather than since the engine started.
#[derive(Default)]
pub struct PacketWindow {
    /// When, the counters, and whether an application was playing then.
    samples: VecDeque<(Instant, PacketCounts, bool)>,
}

/// How far back [`PacketWindow`] looks.
pub const PACKET_WINDOW: Duration = Duration::from_secs(30);

impl PacketWindow {
    /// Records `counts` read at `now`; `playing`: an application has been
    /// playing through the device for a few seconds. A counter that went
    /// back (the engine restarted) starts the window again.
    pub fn push(&mut self, now: Instant, counts: PacketCounts, playing: bool) {
        if let Some((_, last, _)) = self.samples.back()
            && (counts.rx_late < last.rx_late
                || counts.rx_lost < last.rx_lost
                || counts.tx_underruns < last.tx_underruns
                || counts.rx < last.rx)
        {
            self.samples.clear();
        }
        self.samples.push_back((now, counts, playing));
        // Keep the newest sample from before the window as the baseline.
        while self.samples.len() > 1
            && now.saturating_duration_since(self.samples[1].0) >= PACKET_WINDOW
        {
            self.samples.pop_front();
        }
    }

    /// How much each counter rose over the window: late, lost, sent
    /// without audio. Packets go out without audio whenever no application
    /// plays, so that one counts only since one has been playing.
    pub fn rises(&self) -> (u64, u64, u64) {
        let (Some(a), Some(b)) = (self.samples.front(), self.samples.back()) else {
            return (0, 0, 0);
        };
        let quiet = self.samples.iter().rev().find(|s| !s.2).unwrap_or(a);
        (
            b.1.rx_late.saturating_sub(a.1.rx_late),
            b.1.rx_lost.saturating_sub(a.1.rx_lost),
            b.1.tx_underruns.saturating_sub(quiet.1.tx_underruns),
        )
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
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

    /// The problem in a few words, for under the device name.
    pub fn short(&self) -> &'static str {
        match self {
            Problem::NotRunning => "Daemon not running",
            Problem::NotAdmin => "Needs an administrator",
            Problem::NoAnswer => "Daemon not answering",
            Problem::Lost => "Connection lost, reconnecting…",
            Problem::Version { .. } => "App and daemon versions differ",
            Problem::Daemon(_) => "Daemon error",
            Problem::Other(_) => "Cannot reach the daemon",
        }
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
            Problem::Version { daemon: Some((version, _)) } => format!(
                "This app ({}) and the OpenVirtualSoundcard daemon ({version}) are different \
                 versions.",
                env!("CARGO_PKG_VERSION")
            ),
            Problem::Version { daemon: None } => {
                "This app cannot read the OpenVirtualSoundcard daemon's answers: the app and the \
                 daemon are probably different versions."
                    .to_owned()
            }
            Problem::Daemon(message) => {
                format!("The OpenVirtualSoundcard daemon reports: {}", sentence_case(message))
            }
            Problem::Other(message) => {
                format!("Cannot reach the OpenVirtualSoundcard daemon: {message}")
            }
        }
    }

    /// What went wrong, as a title: [`Problem::message`] without the
    /// daemon's or the system's own words, which [`Problem::detail`] gives.
    pub fn title(&self) -> String {
        match self {
            Problem::Daemon(_) => "The OpenVirtualSoundcard daemon reports an error.".to_owned(),
            Problem::Other(_) => "Cannot reach the OpenVirtualSoundcard daemon.".to_owned(),
            other => other.message(),
        }
    }

    /// The daemon's or the system's own words, if they say more.
    pub fn detail(&self) -> Option<String> {
        match self {
            Problem::Daemon(message) => Some(sentence_case(message)),
            Problem::Other(message) => Some(sentence_case(message)),
            _ => None,
        }
    }

    /// What it means and what to do about it.
    pub fn hint(&self) -> String {
        match self {
            Problem::NotRunning => "No audio passes through OpenVirtualSoundcard until it runs. \
                                    To start it, run this in Terminal:"
                .to_owned(),
            Problem::NotAdmin => {
                "Log in with an administrator account to see and change its settings.".to_owned()
            }
            Problem::NoAnswer => "The app keeps asking. Audio may still flow: check the \
                                  console's meters before restarting the daemon."
                .to_owned(),
            Problem::Lost | Problem::Other(_) => "Trying again every second.".to_owned(),
            Problem::Version { daemon } => {
                let protocols = match daemon {
                    Some((_, protocol)) => format!(
                        " (control protocol {PROTOCOL_VERSION} here, {protocol} in the daemon)"
                    ),
                    None => String::new(),
                };
                format!("Install the app and the daemon from the same release{protocols}.")
            }
            Problem::Daemon(_) => {
                "The daemon's log, /Library/Logs/OpenVirtualSoundcard/ovsc.log, says more."
                    .to_owned()
            }
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
            lock_remote_settings: false,
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
    fn an_applied_change_moves_only_its_fields() {
        let s = settings();
        // The show lock applied on its own leaves a pending latency edit
        // pending.
        let edited = Settings { latency_ms: 2.0, lock_remote_settings: true, ..settings() };
        let lock = SettingsChange { lock_remote_settings: Some(true), ..Default::default() };
        let now = with_change(&s, &lock);
        assert!(now.lock_remote_settings);
        assert_eq!(now.latency_ms, 4.0);
        assert_eq!(
            change(&now, &edited),
            SettingsChange { latency_ms: Some(2.0), ..Default::default() }
        );
        // The whole edit applied is the edit.
        let edited = Settings { name: "vox".into(), sample_rate: 96_000, ..settings() };
        assert_eq!(with_change(&s, &change(&s, &edited)), edited);
    }

    #[test]
    fn restarts_are_predicted_from_the_fields() {
        let only = |f: fn(&mut SettingsChange)| {
            let mut c = SettingsChange::default();
            f(&mut c);
            expected_restart(&c)
        };
        assert_eq!(only(|c| c.name = Some("x".into())), Restart::None);
        assert_eq!(only(|c| c.latency_ms = Some(1.0)), Restart::Receive);
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
        assert_eq!(only(|c| c.lock_remote_settings = Some(true)), Restart::None);
        assert_eq!(interruption(Restart::None), None);
        assert!(interruption(Restart::Daemon).unwrap().0.contains("10–20 s"));
    }

    #[test]
    fn interruptions_name_the_returns_to_the_console() {
        // Either restart closes the flows other devices receive from this
        // one, not only its inputs.
        for restart in [Restart::Device, Restart::Daemon] {
            let (cost, details) = interruption(restart).unwrap();
            assert!(cost.contains("both directions"), "{cost}");
            assert!(details.contains("returns to the console"), "{details}");
        }
        // A new latency restarts the inputs only, and says so.
        let (cost, details) = interruption(Restart::Receive).unwrap();
        assert!(cost.contains("inputs stop for about a second"), "{cost}");
        assert!(details.contains("returns to the console carry on"), "{details}");
        assert!(!cost.contains("both directions"), "{cost}");
    }

    fn device_with_flows(receivers: &[Option<&str>]) -> DeviceStatus {
        DeviceStatus {
            name: "studio-mac".into(),
            interface: "en7".into(),
            ip: "192.168.0.8".into(),
            sample_rate: 48_000,
            bits_per_sample: 24,
            latency_ms: 4.0,
            rx_channels: Vec::new(),
            tx_channels: vec!["01".into(), "02".into()],
            tx_flows: receivers
                .iter()
                .map(|r| ovsc_control::TxFlowStatus {
                    receiver: r.map(str::to_owned),
                    destination: "192.168.0.20:14400".into(),
                    channels: vec![1, 2],
                })
                .collect(),
            packets: Default::default(),
            remote_settings_locked: false,
        }
    }

    #[test]
    fn a_rename_asks_first_and_names_the_receivers() {
        let s = settings();
        let renamed = change(&s, &Settings { name: "vox-fx".into(), ..settings() });
        // The daemon applies it at once, but other devices lose their
        // subscriptions: the app asks first.
        assert_eq!(expected_restart(&renamed), Restart::None);
        let device = device_with_flows(&[Some("desk"), None, Some("desk")]);
        let receivers = receivers(Some(&device));
        assert_eq!(receivers, ["192.168.0.20", "desk"]);
        let c = confirmation(&renamed, &s, &receivers, &[]).unwrap();
        assert_eq!((c.title, c.button), ("Rename the device?", "Rename"));
        assert_eq!(c.changes, ["Name: studio-mac → vox-fx"]);
        assert!(c.text.contains("192.168.0.20 and desk, which receive"), "{}", c.text);
        assert!(c.text.contains("“studio-mac”"), "{}", c.text);
        assert!(c.text.contains("patched again to “vox-fx”"), "{}", c.text);
        // Nobody receiving now: subscriptions may still exist.
        let c = confirmation(&renamed, &s, &[], &[]).unwrap();
        assert!(c.text.starts_with("Devices subscribed to this device's channels name"));
        let Applied::ToDo(text) = applied(&renamed, Restart::None, &receivers) else {
            panic!("a rename leaves something to do");
        };
        assert!(text.contains("again, to “vox-fx”") && text.contains("desk"), "{text}");
    }

    #[test]
    fn restarts_ask_first_too() {
        let s = settings();
        let latency = change(&s, &Settings { latency_ms: 2.0, ..settings() });
        let c = confirmation(&latency, &s, &[], &[]).unwrap();
        assert_eq!((c.title, c.button), ("Change the receive latency?", "Change"));
        assert_eq!(c.changes, ["Receive latency: 4 ms → 2 ms"]);
        assert_eq!(c.cost, interruption(Restart::Receive).unwrap().0);
        let both = change(&s, &Settings { name: "x".into(), sample_rate: 96_000, ..settings() });
        let c = confirmation(&both, &s, &[], &[]).unwrap();
        assert_eq!(c.title, "Rename the device and restart?");
        assert!(c.cost.contains("10–20 s") && c.text.contains("“x”"), "{c:?}");
        assert_eq!(c.changes, ["Name: studio-mac → x", "Sample rate: 48 kHz → 96 kHz"]);
        let rate = change(&s, &Settings { sample_rate: 96_000, ..settings() });
        let c = confirmation(&rate, &s, &[], &[]).unwrap();
        assert_eq!((c.title, c.button), ("Restart OpenVirtualSoundcard?", "Restart"));
        // The lock interrupts nothing.
        let lock = change(&s, &Settings { lock_remote_settings: true, ..settings() });
        assert_eq!(lock, SettingsChange { lock_remote_settings: Some(true), ..Default::default() });
        assert_eq!(confirmation(&lock, &s, &[], &[]), None);
        assert!(lock_only(&lock) && !lock_only(&latency) && !lock_only(&SettingsChange::default()));
    }

    #[test]
    fn a_rename_with_another_change_says_both() {
        let s = settings();
        let receivers = ["desk".to_owned()];
        // With a new latency: the returns stop for the rename, so nothing
        // may say they carry on.
        let both = change(&s, &Settings { name: "vox-fx".into(), latency_ms: 2.0, ..settings() });
        let c = confirmation(&both, &s, &receivers, &[]).unwrap();
        assert_eq!(c.title, "Rename the device and change its latency?");
        assert!(c.cost.contains("about a second") && c.cost.contains("subscriptions"), "{c:?}");
        assert!(!c.text.contains("carry on") && c.text.contains("“vox-fx”"), "{c:?}");
        let Applied::ToDo(text) = applied(&both, Restart::Receive, &receivers) else {
            panic!("the rename leaves something to do");
        };
        assert!(text.contains("new latency") && text.contains("again, to “vox-fx”"), "{text}");
        assert!(!text.contains("carried on"), "{text}");
        // With a restart: the to-do waits for the daemon to come back.
        let both =
            change(&s, &Settings { name: "vox-fx".into(), sample_rate: 96_000, ..settings() });
        let Applied::Restarting { todo: Some(todo) } = applied(&both, Restart::Daemon, &receivers)
        else {
            panic!("the rename's to-do follows the restart");
        };
        assert!(todo.starts_with("To do:") && todo.contains("(desk received"), "{todo}");
    }

    #[test]
    fn change_lines_name_every_field() {
        let s = settings();
        let interfaces = [interface("en7", "USB LAN", &["192.168.0.8"])];
        let edited = Settings {
            interface: String::new(),
            bits_per_sample: 32,
            rx_channels: 16,
            tx_channels: 2,
            lock_remote_settings: true,
            ..settings()
        };
        assert_eq!(
            change_lines(&s, &change(&s, &edited), &interfaces),
            [
                "Network interface: USB LAN — en7 (192.168.0.8) → Automatic (default route)",
                "Bit depth: 24-bit → 32-bit",
                "Receive channels: 8 → 16",
                "Transmit channels: 8 → 2",
                "Show lock: Off → On",
            ]
        );
    }

    #[test]
    fn what_an_accepted_change_says() {
        let none = SettingsChange::default();
        assert_eq!(applied(&none, Restart::None, &[]), Applied::Saved("Saved.".into()));
        assert_eq!(applied(&none, Restart::Daemon, &[]), Applied::Restarting { todo: None });
        let Applied::Saved(text) = applied(&none, Restart::Receive, &[]) else { panic!() };
        assert!(text.contains("carried on"), "{text}");
        let Applied::Saved(text) = applied(&none, Restart::Device, &[]) else { panic!() };
        assert!(text.contains("a second or two"), "{text}");
        let lock = |on| SettingsChange { lock_remote_settings: Some(on), ..Default::default() };
        let Applied::Saved(text) = applied(&lock(true), Restart::None, &[]) else { panic!() };
        assert!(text.contains("can no longer"), "{text}");
        let Applied::Saved(text) = applied(&lock(false), Restart::None, &[]) else { panic!() };
        assert!(text.contains("again"), "{text}");
        assert_eq!(lock_label(true), "On");
        assert_eq!(
            refusal("the settings are locked against changes from the network"),
            "Not saved: The settings are locked against changes from the network."
        );
        assert_eq!(refusal("Bad name."), "Not saved: Bad name.");
        assert_eq!(lock_refusal("busy"), "Show lock not changed: Busy.");
    }

    #[test]
    fn rebase_keeps_edits_and_takes_the_rest() {
        let old = settings();
        let edited = Settings { latency_ms: 1.0, ..settings() };
        let new = Settings {
            name: "renamed".into(),
            latency_ms: 10.0,
            lock_remote_settings: true,
            ..settings()
        };
        let rebased = rebase(&old, &edited, &new);
        assert_eq!(rebased.name, "renamed");
        assert_eq!(rebased.latency_ms, 1.0);
        assert!(rebased.lock_remote_settings);
        // Nothing edited: the new settings as they are.
        assert_eq!(rebase(&old, &old, &new), new);
    }

    #[test]
    fn device_names_follow_the_daemons_rule() {
        assert_eq!(name_problem("studio-mac-2"), None);
        assert_eq!(name_problem(" studio "), None);
        assert_eq!(name_problem(&"a".repeat(31)), None);
        assert!(name_problem("").is_some());
        // Only a changed name is checked: an empty one from the daemon
        // must not block the other settings.
        assert_eq!(edited_name_problem("", ""), None);
        assert_eq!(edited_name_problem("vox-mac", "vox-mac"), None);
        assert!(edited_name_problem("vox-mac", "").is_some());
        assert!(edited_name_problem("", "bad name!").is_some());
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
        let counts = channel_choices(8);
        assert!(counts.windows(2).all(|w| w[0] < w[1]));
        assert_eq!((counts[0], *counts.last().unwrap()), (1, MAX_CHANNELS));
        assert!(channel_choices(11).contains(&11));
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
    fn automatic_says_what_it_picks_and_wifi_is_flagged() {
        let mut wifi = interface("en0", "Wi-Fi", &["10.0.0.23"]);
        wifi.default_route = true;
        let list = [wifi, interface("en7", "USB 10/100/1000 LAN", &["192.168.1.50"])];
        assert_eq!(automatic_label(&list), "Automatic (now Wi-Fi — en0)");
        assert_eq!(interface_choices(&list, &[])[0].1, "Automatic (now Wi-Fi — en0)");
        assert!(wifi_warning(&list, "").is_some());
        assert!(wifi_warning(&list, "en0").is_some());
        assert!(wifi_warning(&list, "10.0.0.23").is_some());
        assert_eq!(wifi_warning(&list, "en7"), None);
        assert_eq!(wifi_warning(&list[1..], ""), None);
        assert_eq!(interface_setting_label(&list, "en9"), "en9 (not available)");
    }

    fn clock(state: &str, locked: bool) -> ClockInfo {
        ClockInfo {
            source: "ptp".into(),
            state: state.into(),
            locked,
            leader: None,
            offset_ns: 0,
            path_delay_ns: 0,
            freq_offset_ppm: 0.0,
        }
    }

    fn driver(connected: bool, io_running: bool, audio_flowing: bool) -> DriverInfo {
        DriverInfo {
            connected,
            io_running,
            audio_flowing,
            faulted: false,
            audio_trouble: false,
            detail: String::new(),
        }
    }

    fn status(driver: DriverInfo) -> Status {
        Status {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            engine_running: true,
            engine_error: None,
            device: None,
            clock: clock("Locked", true),
            driver,
            warnings: vec![],
            urgent_warnings: 0,
            earlier: vec![],
        }
    }

    #[test]
    fn clock_and_driver_lamps() {
        assert_eq!(clock_light(&clock("Locked", true)), (Light::Green, "Locked".into()));
        assert_eq!(clock_light(&clock("Locking", false)).0, Light::Amber);
        assert_eq!(clock_light(&clock("Holdover", true)).0, Light::Amber);
        assert_eq!(clock_light(&clock("Unlocked", false)).0, Light::Red);
        assert_eq!(clock_light(&clock("FreeRunning", true)), (Light::Grey, "Free running".into()));
        assert_eq!(clock_light(&clock("Strange", false)), (Light::Grey, "Strange".into()));
        let locked = ClockInfo { leader: Some("192.168.1.10".into()), ..clock("Locked", true) };
        assert_eq!(clock_lamp(&locked).detail, "Following the clock of 192.168.1.10.");
        assert!(clock_lamp(&clock("Unlocked", false)).detail.contains("silent"));

        assert_eq!(driver_light(&driver(true, false, false)), (Light::Green, "Loaded"));
        assert_eq!(driver_light(&driver(false, false, false)), (Light::Red, "Not loaded"));
        let failed = DriverInfo { faulted: true, ..driver(true, true, false) };
        assert_eq!(driver_light(&failed), (Light::Red, "Failed"));
        // The hover is a sentence, never the daemon's diagnostic line.
        let lamp =
            driver_lamp(&DriverInfo { detail: "peers=1 io=1".into(), ..driver(true, true, true) });
        assert!(!lamp.detail.contains("peers="), "{}", lamp.detail);
    }

    #[test]
    fn the_audio_lamp_reads_the_whole_status() {
        let lamp = |s: &Status| (audio_lamp(s).light, audio_lamp(s).word);
        // Green only while an application plays.
        assert_eq!(lamp(&status(driver(true, true, true))), (Light::Green, "Flowing".into()));
        let idle = status(driver(true, false, true));
        assert_eq!(lamp(&idle), (Light::Grey, "Idle".into()));
        assert!(audio_lamp(&idle).detail.starts_with("No application"));
        let idle_unlocked =
            Status { clock: clock("Unlocked", false), ..status(driver(true, false, false)) };
        assert!(audio_lamp(&idle_unlocked).detail.contains("the clock is unlocked"));
        // Problems in the last 30 s.
        let troubled = status(DriverInfo { audio_trouble: true, ..driver(true, true, true) });
        assert_eq!(lamp(&troubled), (Light::Amber, "Dropouts".into()));
        // Held back for a moment, before the daemon calls it urgent.
        assert_eq!(lamp(&status(driver(true, true, false))), (Light::Amber, "Held back".into()));
        // Red for everything that takes audio down.
        let failed = status(DriverInfo { faulted: true, ..driver(true, true, false) });
        assert_eq!(lamp(&failed), (Light::Red, "Silent".into()));
        assert!(audio_lamp(&failed).detail.contains(RESTART_CORE_AUDIO));
        assert_eq!(lamp(&status(driver(false, false, false))), (Light::Red, "No driver".into()));
        let no_engine = Status { engine_running: false, ..status(driver(true, true, true)) };
        assert_eq!(lamp(&no_engine), (Light::Red, "No network".into()));
        // A stopped transmit engine leaves the gate open: the daemon's
        // urgent warning makes the lamp red, with its first sentence.
        let urgent = Status {
            warnings: vec![
                "The transmit engine has stopped: the returns are down. Restart it.".into(),
                "late".into(),
            ],
            urgent_warnings: 1,
            ..troubled
        };
        assert_eq!(lamp(&urgent), (Light::Red, "Down".into()));
        assert_eq!(
            audio_lamp(&urgent).detail,
            "The transmit engine has stopped: the returns are down."
        );
    }

    #[test]
    fn notes_fit_a_tile() {
        let statuses = [
            status(driver(true, true, true)),
            status(driver(true, false, true)),
            status(driver(true, true, false)),
            status(driver(false, false, false)),
            status(DriverInfo { faulted: true, ..driver(true, true, false) }),
            status(DriverInfo { audio_trouble: true, ..driver(true, true, true) }),
            Status { engine_running: false, ..status(driver(true, true, true)) },
        ];
        let mut lamps: Vec<Lamp> = statuses.iter().map(audio_lamp).collect();
        lamps.extend(statuses.iter().map(|s| driver_lamp(&s.driver)));
        for state in ["Locked", "Locking", "Holdover", "Unlocked", "FreeRunning"] {
            lamps.push(clock_lamp(&ClockInfo {
                leader: Some("192.168.100.200".into()),
                ..clock(state, true)
            }));
        }
        for problem in [Problem::NotRunning, Problem::NoAnswer, Problem::Lost, Problem::NotAdmin] {
            lamps.extend(absent_lamps(Some(&problem), false));
        }
        lamps.extend(absent_lamps(None, true));
        for lamp in lamps {
            assert!(lamp.note.chars().count() <= 28, "too long for a tile: {:?}", lamp.note);
            assert!(lamp.word.chars().count() <= 12, "too long for a tile: {:?}", lamp.word);
        }
    }

    #[test]
    fn lamps_without_a_status() {
        let [audio, clock, driver] = absent_lamps(Some(&Problem::NotRunning), false);
        assert_eq!((audio.light, audio.word.as_str()), (Light::Red, "Off"));
        assert_eq!((clock.light, driver.light), (Light::Grey, Light::Grey));
        assert_eq!(absent_lamps(Some(&Problem::NotRunning), true)[0].word, "Restarting");
        assert_eq!(absent_lamps(Some(&Problem::NoAnswer), false)[0].light, Light::Red);
        assert_eq!(absent_lamps(Some(&Problem::NotAdmin), false)[0].light, Light::Grey);
        assert_eq!(absent_lamps(None, false)[0].word, "Unknown");
    }

    #[test]
    fn urgent_warnings_come_apart() {
        let line = r#"{"result":"status","protocol":1,"version":"0.1.0","engine_running":true,
            "engine_error":null,"device":null,
            "clock":{"source":"ptp","state":"Locked","locked":true,"leader":null,
                "offset_ns":0,"path_delay_ns":0,"freq_offset_ppm":0.0},
            "driver":{"connected":true,"io_running":true,"audio_flowing":false,"faulted":true,
                "audio_trouble":false,"detail":""},
            "warnings":["down","late","A controller at 10.0.0.1 changed the latency 5 s ago."],
            "urgent_warnings":1,"earlier":["old"]}"#
            .replace('\n', " ");
        let Response::Status(mut status) = decode_line(&line).unwrap() else {
            panic!("not a status");
        };
        let (urgent, other) = split_warnings(&status);
        assert_eq!(urgent, ["down"]);
        assert_eq!(problems_and_news(other), (vec!["late"], vec![other[1].as_str()]));
        assert_eq!(audio_lamp(&status).light, Light::Red);
        assert_eq!(status.earlier, ["old"]);
        // A count beyond the list (a confused daemon) takes the list.
        status.urgent_warnings = 5;
        assert_eq!(split_warnings(&status).0.len(), 3);
        // A daemon before the split: all amber.
        status.urgent_warnings = 0;
        assert!(split_warnings(&status).0.is_empty());
    }

    #[test]
    fn warnings_split_into_headline_code_and_commands() {
        // The daemon's own sentences.
        let late = "37 received packets arrived too late to play in the last 30 s: short dropouts \
                    on the inputs. If it goes on, raise the latency (now 1 ms), or look for \
                    congestion between the transmitters and this computer.";
        let (head, rest) = warning_parts(late);
        assert_eq!(
            head,
            "37 received packets arrived too late to play in the last 30 s: short dropouts on the inputs."
        );
        assert!(rest.starts_with("If it goes on"));
        let failed = "The Core Audio driver has failed (it caught an internal error) and is silent: \
                      every input and every return stays silent until Core Audio restarts. \
                      Restart it with `sudo killall coreaudiod` (all audio on this Mac stops for a \
                      few seconds), then restart the audio of the application.";
        assert!(warning_parts(failed).1.starts_with("Restart it with"));
        assert_eq!(commands(failed), ["sudo killall coreaudiod"]);
        let spans = code_spans("raise `[coreaudio] output_margin_us`.");
        assert_eq!(
            spans,
            [(false, "raise "), (true, "[coreaudio] output_margin_us"), (false, ".")]
        );
        assert!(commands("raise `[coreaudio] output_margin_us`.").is_empty());
        // One sentence, decimals and an odd backtick stay whole.
        let one =
            "The clock has not locked: no Dante clock leader is reaching this computer on en7.";
        assert_eq!(warning_parts(one), (one, ""));
        assert_eq!(warning_parts("Latency 1.5 ms. raise it."), ("Latency 1.5 ms. raise it.", ""));
        assert_eq!(code_spans("a ` b"), [(false, "a ` b")]);
    }

    #[test]
    fn number_labels() {
        assert_eq!(micros_label(1_234), "1.2 µs");
        assert_eq!(micros_label(-56_789), "−56.8 µs");
        assert_eq!(micros_label(-20), "0.0 µs");
        assert_eq!(ppm_label(1.25), "+1.25 ppm");
        assert_eq!(ppm_label(-0.5), "−0.50 ppm");
        assert_eq!(ppm_label(0.0), "0.00 ppm");
        assert_eq!(count_label(0), "0");
        assert_eq!(count_label(999), "999");
        assert_eq!(count_label(1_000), "1,000");
        assert_eq!(count_label(1_234_567), "1,234,567");
        assert_eq!(channels_label(&[1, 2, 3, 4]), "1–4");
        assert_eq!(channels_label(&[1, 0, 3, 4, 7]), "1, 3–4, 7");
        assert_eq!(channels_label(&[0, 0]), "none");
        assert_eq!(channels_label(&[]), "none");
        assert_eq!(ago_label(Duration::from_secs(12)), "12 s");
        assert_eq!(ago_label(Duration::from_secs(250)), "4 min");
        assert_eq!(ago_label(Duration::from_secs(7_500)), "2 h 5 min");
        assert_eq!(ago_label(Duration::from_secs(7_200)), "2 h");
    }

    #[test]
    fn the_clock_shows_dashes_without_a_leader() {
        let rows = clock_rows(&clock("Unlocked", false));
        assert_eq!(rows[2], ("Leader", NONE.to_owned()));
        assert!(rows[3..].iter().all(|(_, v)| v == NONE), "{rows:?}");
        let locked = ClockInfo {
            leader: Some("192.168.1.10".into()),
            offset_ns: -180,
            path_delay_ns: 41_250,
            freq_offset_ppm: 1.284,
            ..clock("Locked", true)
        };
        let rows = clock_rows(&locked);
        assert_eq!(rows[3].1, "−0.2 µs");
        assert_eq!(rows[4].1, "41.2 µs");
        assert_eq!(rows[5], ("Drift", "+1.28 ppm".to_owned()));
    }

    fn channel(name: &str, source: Option<&str>, state: &str, receiving: bool) -> RxChannelStatus {
        RxChannelStatus {
            name: name.into(),
            source: source.map(str::to_owned),
            state: state.into(),
            receiving,
        }
    }

    #[test]
    fn channels_and_returns_in_one_line() {
        let ok = channel("Lead Vox", Some("Ch 1@desk"), "receiving", true);
        let spare = channel("Spare", None, "", false);
        let gone = channel("BV 1", Some("Ch 2@desk"), "transmitter not found", false);
        let wait = channel("BV 2", Some("Ch 3@desk"), "connecting", false);
        assert_eq!(
            rx_summary(&[ok.clone(), spare.clone()]),
            (Light::Green, "1 of 1 receiving · 1 unused".into())
        );
        assert_eq!(
            rx_summary(&[ok.clone(), gone.clone()]),
            (Light::Red, "1 of 2 not receiving: BV 1".into())
        );
        assert_eq!(rx_summary(std::slice::from_ref(&wait)).0, Light::Amber);
        assert_eq!(
            rx_summary(std::slice::from_ref(&spare)),
            (Light::Grey, "No subscriptions".into())
        );
        let many: Vec<_> = (0..5)
            .map(|i| channel(&format!("{i}"), Some("x@y"), "transmitter not found", false))
            .collect();
        assert_eq!(rx_summary(&many).1, "5 of 5 not receiving: 0, 1, 2 and 2 more");
        assert_eq!(channel_light(&ok), Light::Green);
        assert_eq!(channel_light(&gone), Light::Red);
        assert_eq!(channel_light(&wait), Light::Amber);
        assert_eq!(channel_light(&spare), Light::Grey);
        assert_eq!(subscription_label(&gone), "Transmitter not found");
        assert_eq!(subscription_label(&spare), NONE);

        let device = device_with_flows(&[Some("desk"), None]);
        assert_eq!(
            tx_summary(&device),
            (Light::Green, "2 channels · received by 192.168.0.20 and desk".into())
        );
        assert_eq!(tx_summary(&device_with_flows(&[])).0, Light::Grey);
        let multicast = TxFlowStatus {
            receiver: None,
            destination: "239.69.12.4:4321".into(),
            channels: vec![1],
        };
        assert_eq!(flow_receiver(&multicast), "Multicast");
        assert_eq!(address_label("192.168.1.10:4321"), "192.168.1.10");
        assert_eq!(device_summary(&device), "48 kHz · 24-bit · 4 ms latency · en7 (192.168.0.8)");
    }

    #[test]
    fn the_packet_window_counts_the_last_half_minute() {
        let t0 = Instant::now();
        let counts =
            |late, lost| PacketCounts { rx_late: late, rx_lost: lost, ..Default::default() };
        let mut w = PacketWindow::default();
        w.push(t0, counts(10, 1), true);
        w.push(t0 + Duration::from_secs(10), counts(30, 1), true);
        assert_eq!(w.rises(), (20, 0, 0));
        w.push(t0 + Duration::from_secs(45), counts(30, 4), true);
        // The first sample is older than the window now.
        assert_eq!(w.rises(), (0, 3, 0));
        // An engine restart sets the counters back: start over.
        w.push(t0 + Duration::from_secs(46), counts(0, 0), true);
        assert_eq!(w.rises(), (0, 0, 0));
    }

    #[test]
    fn packets_sent_without_audio_count_only_while_an_application_plays() {
        let t0 = Instant::now();
        let sent = |n| PacketCounts { tx_underruns: n, ..Default::default() };
        let at = |s| t0 + Duration::from_secs(s);
        let mut w = PacketWindow::default();
        // Nothing plays: every packet goes out without audio, and that is
        // no news.
        w.push(at(0), sent(0), false);
        w.push(at(10), sent(10_000), false);
        assert_eq!(w.rises().2, 0);
        // An application starts: only what happens after counts.
        w.push(at(14), sent(14_000), true);
        assert_eq!(w.rises().2, 4_000);
        w.push(at(20), sent(14_000), true);
        assert_eq!(w.rises().2, 4_000);
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
        // Every problem has a short form and a hint.
        for p in
            [Problem::NotRunning, Problem::NoAnswer, Problem::Lost, Problem::Daemon("x".into())]
        {
            assert!(!p.short().is_empty() && !p.hint().is_empty());
        }
        let daemon = Problem::Daemon("bad config".into());
        assert_eq!(daemon.message(), "The OpenVirtualSoundcard daemon reports: Bad config");
        // Shown, the daemon's own words go under a title of their own.
        assert_eq!(daemon.title(), "The OpenVirtualSoundcard daemon reports an error.");
        assert_eq!(daemon.detail().as_deref(), Some("Bad config"));
        assert_eq!(Problem::NotRunning.title(), Problem::NotRunning.message());
        assert_eq!(Problem::NotRunning.detail(), None);
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
        assert_eq!(Problem::check(&status), None);
        status.protocol = PROTOCOL_VERSION + 1;
        status.version = "9.0.0".into();
        let problem = Problem::check(&status).unwrap();
        assert!(problem.message().contains("different versions"));
        assert!(problem.message().contains("9.0.0"));
        assert!(problem.hint().contains(&format!("{}", PROTOCOL_VERSION + 1)));
    }
}
