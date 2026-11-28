//! What the window shows, apart from how it draws it: the connection to the
//! daemon, the settings and the user's edit of them, a change on its way
//! and what became of it, and an expected restart. The views in
//! [`crate::view`], [`crate::status`] and [`crate::settings`] draw it;
//! the worker's events and the user's actions change it. Time comes in as
//! an argument, so the tests can move it.

use std::time::{Duration, Instant};

use ovsc_control::{InterfaceInfo, Restart, Settings, SettingsChange, Status};

use crate::logic::{self, Lamp, Light, PacketWindow, Problem};
use crate::worker::{Command, Event, Unapplied};

/// How long after a change that restarts the daemon its absence reads as
/// the restart rather than as a problem.
pub const RESTART_GRACE: Duration = Duration::from_secs(40);

/// How long without an answer before the status counts as stale. The app
/// asks every second.
pub const STALE_AFTER: Duration = Duration::from_millis(3500);

/// How long a plain confirmation stays.
pub const SAVED_FOR: Duration = Duration::from_secs(10);

/// How long an application must have played before packets sent without
/// its audio count, as for the daemon's warning.
const IO_SETTLE: Duration = Duration::from_secs(3);

/// The app's own name: the window's title before the device has one.
pub const APP_NAME: &str = "OpenVirtualSoundcard";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tab {
    Status,
    Settings,
}

/// The connection, as far as the app knows.
pub enum Connection {
    /// Nothing heard yet.
    Connecting,
    Up(Box<Status>),
    Down(Problem),
}

/// The daemon's settings and the user's edit of them.
pub struct Form {
    pub current: Settings,
    pub edited: Settings,
}

/// What became of the last change, for the Settings tab's bar.
pub enum Outcome {
    Saved {
        text: String,
        at: Instant,
    },
    /// Saved, with something left for the user to do.
    ToDo(String),
    /// The daemon restarts to apply it; `todo`: what is left to do once
    /// it is back (re-patch after a rename).
    Restarting {
        since: Instant,
        todo: Option<String>,
    },
    /// The daemon is back after the restart, and audio with it.
    Back {
        after: Duration,
        at: Instant,
    },
    /// Not applied, and why. `kept`: the edit is still in the form (a show
    /// lock that failed to switch goes back instead).
    Failed {
        text: String,
        kept: bool,
    },
}

/// How the user wants to leave, when that would drop edits not applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Leave {
    Close,
    Quit,
}

/// A badge on a tab: its light, and what it means.
pub type Badge = Option<(Light, &'static str)>;

pub struct Model {
    pub tab: Tab,
    pub connection: Connection,
    pub interfaces: Vec<InterfaceInfo>,
    /// Once the settings were read.
    pub form: Option<Form>,
    /// A change waiting for the user to accept what it interrupts.
    pub confirm: Option<(SettingsChange, logic::Confirmation)>,
    /// A change was sent and is not answered yet.
    pub applying: bool,
    /// The change sent last.
    pub sent: SettingsChange,
    pub outcome: Option<Outcome>,
    /// Until when the daemon is expected to be restarting, or coming back
    /// from the restart with audio still down; cleared once audio is back.
    pub restart_until: Option<Instant>,
    /// The connection dropped since the restart began: a status from
    /// before that is the old daemon's.
    restart_seen_down: bool,
    /// When the daemon last answered.
    pub last_alive: Option<Instant>,
    /// Since when an application has been playing through the device.
    io_since: Option<Instant>,
    /// The packet counters of the last half minute.
    pub packets: PacketWindow,
    /// The user asked to leave with edits not applied: ask first.
    pub ask_discard: Option<Leave>,
    /// The user chose to drop the edits: leaving goes ahead.
    pub discarding: bool,
    /// Commands for the worker, sent by the view.
    outbox: Vec<Command>,
}

impl Default for Model {
    fn default() -> Self {
        Model::new()
    }
}

impl Model {
    pub fn new() -> Model {
        Model {
            tab: Tab::Status,
            connection: Connection::Connecting,
            interfaces: Vec::new(),
            form: None,
            confirm: None,
            applying: false,
            sent: SettingsChange::default(),
            outcome: None,
            restart_until: None,
            restart_seen_down: false,
            last_alive: None,
            io_since: None,
            packets: PacketWindow::default(),
            ask_discard: None,
            discarding: false,
            outbox: Vec::new(),
        }
    }

    /// The commands for the worker since the last call.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    pub fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::Problem(problem) => {
                self.connection = Connection::Down(problem);
                self.last_alive = None;
                self.io_since = None;
                self.packets.clear();
                self.restart_seen_down |= self.restart_until.is_some();
            }
            Event::Status(status) => {
                self.io_since = match (status.driver.io_running, self.io_since) {
                    (true, since) => Some(since.unwrap_or(now)),
                    (false, _) => None,
                };
                if let Some(device) = &status.device {
                    self.packets.push(now, device.packets, self.playing(now));
                }
                // A controller (or the daemon) changed a setting: read the
                // settings again; the user's own edits survive (rebase).
                if let Connection::Up(old) = &self.connection
                    && !self.applying
                    && settings_moved(old, &status)
                {
                    self.outbox.push(Command::Reload);
                }
                self.connection = Connection::Up(status);
                self.check_back(now);
            }
            Event::Alive => {
                self.last_alive = Some(now);
                let playing = self.playing(now);
                if let Connection::Up(status) = &self.connection
                    && let Some(device) = &status.device
                {
                    self.packets.push(now, device.packets, playing);
                }
            }
            Event::Settings(new) => {
                let edited = match &self.form {
                    Some(form) => logic::rebase(&form.current, &form.edited, &new),
                    None => new.clone(),
                };
                self.form = Some(Form { current: new, edited });
            }
            Event::Interfaces(interfaces) => self.interfaces = interfaces,
            Event::Applied(Ok(restart)) => {
                self.applying = false;
                let receivers = logic::receivers(self.device());
                self.outcome = Some(match logic::applied(&self.sent, restart, &receivers) {
                    logic::Applied::Saved(text) => Outcome::Saved { text, at: now },
                    logic::Applied::ToDo(text) => Outcome::ToDo(text),
                    logic::Applied::Restarting { todo } => Outcome::Restarting { since: now, todo },
                });
                // Saved: those fields are the daemon's now; other edits
                // stay pending.
                if let Some(form) = &mut self.form {
                    form.current = logic::with_change(&form.current, &self.sent);
                    if self.sent.name.is_some() {
                        form.edited.name = form.edited.name.trim().to_owned();
                    }
                }
                if restart == Restart::Daemon {
                    self.restart_until = Some(now + RESTART_GRACE);
                    self.restart_seen_down = false;
                }
            }
            Event::Applied(Err(why)) => {
                self.applying = false;
                // A refused show lock goes back to what the daemon holds.
                let lock = logic::lock_only(&self.sent);
                if let (Some(form), Some(_)) = (&mut self.form, self.sent.lock_remote_settings) {
                    form.edited.lock_remote_settings = form.current.lock_remote_settings;
                }
                let text = match why {
                    Unapplied::Refused(message) => {
                        // What the daemon holds may have moved meanwhile.
                        self.outbox.push(Command::Reload);
                        if lock { logic::lock_refusal(&message) } else { logic::refusal(&message) }
                    }
                    Unapplied::Unsure(message) | Unapplied::NotSent(message) => message,
                };
                self.outcome = Some(Outcome::Failed { text, kept: !lock });
            }
        }
    }

    /// Moves time on: ends the restart's grace once its time is up (an
    /// outage is news again, and the Settings bar says that audio is not
    /// back), and clears a plain confirmation once it has been read.
    pub fn tick(&mut self, now: Instant) {
        if self.restart_until.is_some_and(|until| now >= until) {
            self.restart_until = None;
            self.check_back(now);
        }
        let shown_for = match &self.outcome {
            Some(Outcome::Saved { at, .. }) => Some((*at, SAVED_FOR)),
            Some(Outcome::Back { at, .. }) => Some((*at, SAVED_FOR + Duration::from_secs(5))),
            _ => None,
        };
        if let Some((at, keep)) = shown_for
            && now.saturating_duration_since(at) >= keep
        {
            self.outcome = None;
        }
    }

    /// Whether an application has played through the device long enough
    /// for packets sent without its audio to mean something.
    fn playing(&self, now: Instant) -> bool {
        self.io_since.is_some_and(|since| now.saturating_duration_since(since) >= IO_SETTLE)
    }

    /// Ends an expected restart once the new daemon answers and audio is no
    /// longer down: until then the outage is the restart's, not news.
    fn check_back(&mut self, now: Instant) {
        let Connection::Up(status) = &self.connection else { return };
        let expected = self.restart_until.is_some()
            || matches!(self.outcome, Some(Outcome::Restarting { .. }));
        // Until the connection drops, the old daemon is still answering.
        let old = self.restart_until.is_some() && !self.restart_seen_down;
        if !expected || old || logic::audio_lamp(status).light == Light::Red {
            return;
        }
        self.restart_until = None;
        if let Some(Outcome::Restarting { since, todo }) = &self.outcome {
            let after = now.saturating_duration_since(*since);
            self.outcome = Some(match todo {
                Some(todo) => {
                    Outcome::ToDo(format!("Back after {}. {todo}", logic::ago_label(after)))
                }
                None => Outcome::Back { after, at: now },
            });
        }
    }

    /// Sends `change` to the daemon.
    pub fn apply(&mut self, change: SettingsChange) {
        self.applying = true;
        self.outcome = None;
        self.sent = change.clone();
        self.outbox.push(Command::Apply(change));
    }

    /// The device status, while connected.
    pub fn device(&self) -> Option<&ovsc_control::DeviceStatus> {
        match &self.connection {
            Connection::Up(status) => status.device.as_ref(),
            _ => None,
        }
    }

    /// Whether a change the user accepted restarts the daemon, and audio is
    /// not back yet: the daemon is away, or back with its clock still
    /// locking. After [`RESTART_GRACE`] an outage is news again.
    pub fn restarting(&self, now: Instant) -> bool {
        self.restart_until.is_some_and(|until| now < until)
    }

    /// How long the daemon has not answered, once that is long enough to
    /// matter.
    pub fn stale(&self, now: Instant) -> Option<Duration> {
        let Connection::Up(_) = self.connection else { return None };
        let since = now.saturating_duration_since(self.last_alive?);
        (since >= STALE_AFTER).then_some(since)
    }

    /// Whether the form holds edits not applied yet.
    pub fn pending(&self) -> bool {
        self.form.as_ref().is_some_and(|f| !logic::change(&f.current, &f.edited).is_empty())
    }

    /// The Audio, Clock and Driver lamps.
    pub fn lamps(&self, now: Instant) -> [Lamp; 3] {
        match &self.connection {
            Connection::Up(status) => {
                if let Some(stale) = self.stale(now) {
                    let words = format!(
                        "No update from the daemon for {}: this may be out of date.",
                        logic::ago_label(stale)
                    );
                    let note = format!("No update for {}", logic::ago_label(stale));
                    let grey =
                        |word: &str, note: &str| Lamp::new(Light::Grey, word, note, words.clone());
                    return [
                        grey("No update", &note),
                        grey(logic::NONE, ""),
                        grey(logic::NONE, ""),
                    ];
                }
                // Back from a restart, with the clock still locking: the
                // silence is the restart's.
                let audio = if self.restarting(now) {
                    logic::restarting_lamp()
                } else {
                    logic::audio_lamp(status)
                };
                [audio, logic::clock_lamp(&status.clock), logic::driver_lamp(&status.driver)]
            }
            Connection::Connecting => logic::absent_lamps(None, false),
            Connection::Down(problem) => logic::absent_lamps(Some(problem), self.restarting(now)),
        }
    }

    /// The device's name, or the app's before there is one.
    pub fn device_name(&self) -> String {
        if let Some(device) = self.device()
            && !device.name.is_empty()
        {
            return device.name.clone();
        }
        match &self.form {
            Some(form) if !form.current.name.is_empty() => form.current.name.clone(),
            _ => APP_NAME.to_owned(),
        }
    }

    /// The line under the name: the device's format and interface, or what
    /// keeps it away.
    pub fn subtitle(&self, now: Instant) -> String {
        match &self.connection {
            Connection::Connecting => "Connecting…".to_owned(),
            Connection::Up(status) => match &status.device {
                Some(device) => logic::device_summary(device),
                None => "Not on the network".to_owned(),
            },
            Connection::Down(_) if self.restarting(now) => "Restarting…".to_owned(),
            Connection::Down(problem) => problem.short().to_owned(),
        }
    }

    /// The app's version, and the daemon's once it answers.
    pub fn versions(&self) -> String {
        match &self.connection {
            Connection::Up(status) => {
                format!("App {} · daemon {}", env!("CARGO_PKG_VERSION"), status.version)
            }
            _ => format!("App {}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// The Status tab's badge, while the user is on another tab: audio is
    /// down, or there are warnings. Controllers' news has a section of its
    /// own, and a restart's alarms are expected: no badge for either.
    pub fn status_badge(&self, now: Instant) -> Badge {
        if self.tab == Tab::Status || self.restarting(now) {
            return None;
        }
        let Connection::Up(status) = &self.connection else { return None };
        let (urgent, other) = logic::split_warnings(status);
        let (problems, _) = logic::problems_and_news(other);
        if !urgent.is_empty() || !status.engine_running {
            Some((Light::Red, "audio is down"))
        } else {
            (!problems.is_empty()).then_some((Light::Amber, "warnings"))
        }
    }

    /// The Settings tab's badge, while the user is on another tab and edits
    /// are not applied.
    pub fn settings_badge(&self) -> Badge {
        (self.tab != Tab::Settings && self.pending())
            .then_some((Light::Amber, "changes not applied"))
    }

    /// Changes the tab; entering Settings reads the settings and the
    /// interfaces again (an adapter may have come or gone).
    pub fn switch_tab(&mut self, tab: Tab) {
        if tab != self.tab {
            self.tab = tab;
            if tab == Tab::Settings {
                self.outbox.push(Command::Reload);
            }
        }
    }

    /// Whether anything is pinned under the header: an outage, a restart,
    /// a stale status.
    pub fn has_alerts(&self, now: Instant) -> bool {
        if self.restarting(now) {
            return true;
        }
        match &self.connection {
            Connection::Connecting => false,
            // The Status tab shows it in full instead.
            Connection::Down(_) => self.tab == Tab::Settings,
            Connection::Up(status) => {
                self.stale(now).is_some() || !status.engine_running || status.urgent_warnings > 0
            }
        }
    }

    /// The window's title: the device's name, and first that audio is down,
    /// so that it shows also while the window is covered or minimised.
    pub fn window_title(&self, now: Instant) -> String {
        let name = self.device_name();
        let title = if name == APP_NAME { name } else { format!("{name} — {APP_NAME}") };
        if self.audio_down(now) { format!("Audio down · {title}") } else { title }
    }

    /// Whether the Audio tile is red.
    pub fn audio_down(&self, now: Instant) -> bool {
        let [audio, _, _] = self.lamps(now);
        audio.light == Light::Red
    }

    /// The user edited the form through `edit`. A new edit clears what
    /// became of the last change, except a restart still on its way.
    pub fn edit(&mut self, edit: impl FnOnce(&mut Settings)) {
        let Some(form) = &mut self.form else { return };
        let before = form.edited.clone();
        edit(&mut form.edited);
        if form.edited != before && !matches!(self.outcome, Some(Outcome::Restarting { .. })) {
            self.outcome = None;
        }
    }

    /// Whether the Settings tab's controls can be used now.
    pub fn usable(&self) -> bool {
        matches!(self.connection, Connection::Up(_)) && !self.applying
    }

    /// Whether the show lock is being switched.
    pub fn applying_lock(&self) -> bool {
        self.applying && self.sent.lock_remote_settings.is_some()
    }

    /// Switches the show lock, at once.
    pub fn set_lock(&mut self, on: bool) {
        let can = matches!(self.connection, Connection::Up(_)) && !self.applying;
        if !can || self.form.is_none() {
            return;
        }
        self.edit(|s| s.lock_remote_settings = on);
        self.apply(SettingsChange { lock_remote_settings: Some(on), ..Default::default() });
    }

    /// The edit as a change, with the name's problem if it has one.
    pub fn pending_change(&self) -> Option<(SettingsChange, Option<&'static str>)> {
        let form = self.form.as_ref()?;
        let change = logic::change(&form.current, &form.edited);
        let problem = logic::edited_name_problem(&form.current.name, &form.edited.name);
        Some((change, problem))
    }

    /// The confirmation the pending edit needs before it is applied, if
    /// any.
    pub fn pending_confirmation(&self) -> Option<logic::Confirmation> {
        let form = self.form.as_ref()?;
        let (change, problem) = self.pending_change()?;
        if change.is_empty() || problem.is_some() {
            return None;
        }
        let receivers = logic::receivers(self.device());
        logic::confirmation(&change, &form.current, &receivers, &self.interfaces)
    }

    /// Whether Apply can be pressed.
    pub fn can_apply(&self) -> bool {
        self.usable()
            && self
                .pending_change()
                .is_some_and(|(change, problem)| !change.is_empty() && problem.is_none())
    }

    /// Applies the edit: at once, or after the confirmation when it
    /// interrupts audio or other devices' subscriptions.
    pub fn request_apply(&mut self) {
        if !self.can_apply() {
            return;
        }
        let Some((change, _)) = self.pending_change() else { return };
        match self.pending_confirmation() {
            Some(confirmation) => self.confirm = Some((change, confirmation)),
            None => self.apply(change),
        }
    }

    /// The user's answer to the confirmation.
    pub fn answer_confirmation(&mut self, go: bool) {
        if let Some((change, _)) = self.confirm.take()
            && go
        {
            self.apply(change);
        }
    }

    /// Drops the edits.
    pub fn revert(&mut self) {
        if self.applying {
            return;
        }
        if let Some(form) = &mut self.form {
            form.edited = form.current.clone();
        }
        self.outcome = None;
        self.outbox.push(Command::Reload);
    }

    /// Whether the window may close or the app quit now. With edits not
    /// applied, it asks first.
    pub fn may_leave(&mut self, how: Leave) -> bool {
        if self.discarding || !self.pending() {
            return true;
        }
        // One question at a time: a confirmation still up goes, and Apply
        // brings it back if the user keeps editing.
        self.confirm = None;
        self.ask_discard = Some(how);
        false
    }

    /// The user's answer to the question whether to drop the edits: how to
    /// leave, if they are dropped.
    pub fn answer_discard(&mut self, discard: bool) -> Option<Leave> {
        let how = self.ask_discard.take()?;
        if discard {
            self.discarding = true;
            Some(how)
        } else {
            None
        }
    }
}

/// Whether a setting the Settings tab shows moved between two statuses:
/// a controller changed it, or the daemon did.
fn settings_moved(old: &Status, new: &Status) -> bool {
    match (&old.device, &new.device) {
        (Some(a), Some(b)) => {
            a.name != b.name
                || a.latency_ms != b.latency_ms
                || a.sample_rate != b.sample_rate
                || a.bits_per_sample != b.bits_per_sample
                || a.remote_settings_locked != b.remote_settings_locked
                || a.rx_channels.len() != b.rx_channels.len()
                || a.tx_channels.len() != b.tx_channels.len()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_control::{ClockInfo, DriverInfo, PROTOCOL_VERSION};

    fn status(audio_flowing: bool) -> Box<Status> {
        Box::new(Status {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            engine_running: true,
            engine_error: None,
            device: None,
            clock: ClockInfo {
                source: "ptp".into(),
                state: "Locked".into(),
                locked: true,
                leader: None,
                offset_ns: 0,
                path_delay_ns: 0,
                freq_offset_ppm: 0.0,
            },
            driver: DriverInfo {
                connected: true,
                io_running: true,
                audio_flowing,
                faulted: !audio_flowing,
                audio_trouble: false,
                detail: String::new(),
            },
            warnings: vec![],
            urgent_warnings: 0,
            earlier: vec![],
        })
    }

    fn settings() -> Settings {
        Settings {
            name: "ovsc-mac".into(),
            interface: "en7".into(),
            secondary_interface: String::new(),
            sample_rate: 48000,
            bits_per_sample: 24,
            rx_channels: 8,
            tx_channels: 8,
            latency_ms: 1.0,
            lock_remote_settings: false,
        }
    }

    fn connected(now: Instant) -> Model {
        let mut m = Model::new();
        m.handle(Event::Status(status(true)), now);
        m.handle(Event::Alive, now);
        m.handle(Event::Settings(settings()), now);
        m.take_commands();
        m
    }

    #[test]
    fn a_change_that_restarts_is_expected_until_audio_is_back() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.edit(|s| s.sample_rate = 96000);
        assert!(m.pending());
        assert!(m.pending_confirmation().is_some(), "a new rate interrupts audio");
        m.request_apply();
        assert!(m.confirm.is_some() && !m.applying);
        m.answer_confirmation(true);
        assert!(m.applying);
        assert!(
            matches!(m.take_commands().as_slice(), [Command::Apply(c)] if c.sample_rate == Some(96000))
        );
        m.handle(Event::Applied(Ok(Restart::Daemon)), t0);
        assert!(m.restarting(t0) && !m.pending());
        // The daemon goes away: not an alarm while it restarts.
        let t1 = t0 + Duration::from_secs(2);
        m.handle(Event::Problem(Problem::NotRunning), t1);
        assert_eq!(m.lamps(t1)[0].word, "Restarting");
        assert!(!m.audio_down(t1));
        assert_eq!(m.subtitle(t1), "Restarting…");
        // Back, but audio still down: still the restart's.
        let t2 = t0 + Duration::from_secs(5);
        m.handle(Event::Status(status(false)), t2);
        assert!(m.restarting(t2));
        assert!(matches!(m.outcome, Some(Outcome::Restarting { .. })));
        // Audio is back.
        let t3 = t0 + Duration::from_secs(12);
        m.handle(Event::Status(status(true)), t3);
        assert!(!m.restarting(t3));
        assert!(
            matches!(m.outcome, Some(Outcome::Back { after, .. }) if after == Duration::from_secs(12))
        );
        // The confirmation goes after a while.
        m.tick(t3 + SAVED_FOR + Duration::from_secs(6));
        assert!(m.outcome.is_none());
    }

    #[test]
    fn a_restart_that_takes_too_long_is_news_again() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.apply(SettingsChange { rx_channels: Some(16), ..Default::default() });
        m.handle(Event::Applied(Ok(Restart::Daemon)), t0);
        m.handle(Event::Problem(Problem::NotRunning), t0 + Duration::from_secs(1));
        let late = t0 + RESTART_GRACE + Duration::from_secs(1);
        m.tick(late);
        assert!(!m.restarting(late));
        assert!(m.audio_down(late));
        assert!(m.window_title(late).starts_with("Audio down · "));
        assert!(
            matches!(m.outcome, Some(Outcome::Restarting { .. })),
            "the bar says it is not back"
        );
    }

    #[test]
    fn the_old_daemon_answering_does_not_end_the_restart() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.apply(SettingsChange { tx_channels: Some(16), ..Default::default() });
        m.handle(Event::Applied(Ok(Restart::Daemon)), t0);
        m.handle(Event::Status(status(true)), t0 + Duration::from_millis(100));
        assert!(m.restarting(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn a_stale_status_greys_the_tiles() {
        let t0 = Instant::now();
        let m = connected(t0);
        assert!(m.stale(t0 + Duration::from_secs(1)).is_none());
        let later = t0 + STALE_AFTER;
        assert!(m.stale(later).is_some());
        assert!(m.lamps(later).iter().all(|l| l.light == Light::Grey));
        assert!(m.has_alerts(later));
    }

    #[test]
    fn badges_show_on_the_other_tab_only() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.handle(
            Event::Status(Box::new(Status {
                warnings: vec!["Packets arrived too late.".into()],
                ..*status(true)
            })),
            t0,
        );
        assert_eq!(m.status_badge(t0), None, "the user is on Status");
        m.switch_tab(Tab::Settings);
        assert!(matches!(m.take_commands().as_slice(), [Command::Reload]));
        assert_eq!(m.status_badge(t0).map(|b| b.0), Some(Light::Amber));
        m.edit(|s| s.latency_ms = 2.0);
        assert_eq!(m.settings_badge(), None);
        m.switch_tab(Tab::Status);
        assert_eq!(m.settings_badge().map(|b| b.0), Some(Light::Amber));
    }

    #[test]
    fn leaving_with_edits_asks_first() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        assert!(m.may_leave(Leave::Close));
        m.edit(|s| s.name = "stage-left".into());
        assert!(!m.may_leave(Leave::Quit));
        assert_eq!(m.ask_discard, Some(Leave::Quit));
        assert_eq!(m.answer_discard(false), None);
        assert!(!m.may_leave(Leave::Close));
        assert_eq!(m.answer_discard(true), Some(Leave::Close));
        assert!(m.may_leave(Leave::Close));
    }

    #[test]
    fn leaving_drops_a_confirmation_that_is_up() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.edit(|s| s.rx_channels = 16);
        m.request_apply();
        assert!(m.confirm.is_some());
        assert!(!m.may_leave(Leave::Close));
        assert!(m.confirm.is_none(), "only the question about leaving is up");
        assert_eq!(m.answer_discard(false), None);
        assert!(m.pending(), "the edit stays");
    }

    #[test]
    fn a_refused_show_lock_goes_back() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.set_lock(true);
        assert!(m.applying_lock());
        m.handle(Event::Applied(Err(Unapplied::Refused("no".into()))), t0);
        assert!(!m.form.as_ref().unwrap().edited.lock_remote_settings);
        assert!(matches!(m.outcome, Some(Outcome::Failed { kept: false, .. })));
        assert!(matches!(m.take_commands().as_slice(), [Command::Apply(_), Command::Reload]));
    }

    #[test]
    fn an_edit_clears_the_last_outcome_but_not_a_restart() {
        let t0 = Instant::now();
        let mut m = connected(t0);
        m.outcome = Some(Outcome::ToDo("Re-patch".into()));
        m.edit(|s| s.latency_ms = 2.0);
        assert!(m.outcome.is_none());
        m.outcome = Some(Outcome::Restarting { since: t0, todo: None });
        m.edit(|s| s.latency_ms = 3.0);
        assert!(m.outcome.is_some());
    }

    #[test]
    fn titles() {
        let t0 = Instant::now();
        let mut m = Model::new();
        assert_eq!(m.window_title(t0), "OpenVirtualSoundcard");
        m.handle(Event::Settings(settings()), t0);
        assert_eq!(m.window_title(t0), "ovsc-mac — OpenVirtualSoundcard");
        m.handle(Event::Problem(Problem::NotRunning), t0);
        assert_eq!(m.window_title(t0), "Audio down · ovsc-mac — OpenVirtualSoundcard");
    }
}
