//! The window: the device and its status tiles at the top, alerts that
//! never scroll away under them, then the Status or the Settings tab.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Frame, Id, Key, KeyboardShortcut, Label, Margin, Modifiers, RichText, Ui,
    ViewportCommand, vec2,
};
use ovsc_control::{Restart, Settings, SettingsChange, Status};

use crate::logic::{self, Lamp, Light, PacketWindow, Problem};
use crate::theme;
use crate::widgets::{self, Copied, Severity};
use crate::worker::{Command, Event, Unapplied, Worker};
use crate::{settings, status};

/// How long after a change that restarts the daemon its absence reads as
/// the restart rather than as a problem.
const RESTART_GRACE: Duration = Duration::from_secs(40);

/// How long without an answer before the status counts as stale. The app
/// asks every second.
const STALE_AFTER: Duration = Duration::from_millis(3500);

/// How long a plain confirmation stays.
const SAVED_FOR: Duration = Duration::from_secs(10);

/// How long an application must have played before packets sent without
/// its audio count, as for the daemon's warning.
const IO_SETTLE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tab {
    Status,
    Settings,
}

/// The connection, as far as the UI knows.
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

/// What became of the last change, for the Settings tab's action bar.
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

pub struct App {
    pub worker: Worker,
    pub tab: Tab,
    pub connection: Connection,
    pub interfaces: Vec<ovsc_control::InterfaceInfo>,
    /// Once the settings were read.
    pub form: Option<Form>,
    /// A change waiting for the user to accept what it interrupts.
    pub confirm: Option<(SettingsChange, logic::Confirmation)>,
    /// The confirmation just opened: focus its Cancel button.
    pub confirm_opened: bool,
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
    pub copied: Copied,
    /// The window was asked to close with unapplied edits.
    pub ask_discard: bool,
    discarding: bool,
    /// The window title sent last, and whether the Dock was asked for
    /// attention for the current outage.
    title: String,
    alerted: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, socket: PathBuf) -> App {
        theme::install(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        App {
            worker: Worker::start(socket, move || ctx.request_repaint()),
            tab: Tab::Status,
            connection: Connection::Connecting,
            interfaces: Vec::new(),
            form: None,
            confirm: None,
            confirm_opened: false,
            applying: false,
            sent: SettingsChange::default(),
            outcome: None,
            restart_until: None,
            restart_seen_down: false,
            last_alive: None,
            io_since: None,
            packets: PacketWindow::default(),
            copied: Copied::default(),
            ask_discard: false,
            discarding: false,
            title: String::new(),
            alerted: false,
        }
    }

    fn handle(&mut self, event: Event) {
        let now = Instant::now();
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
                    self.worker.send(Command::Reload);
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
                        self.worker.send(Command::Reload);
                        if lock { logic::lock_refusal(&message) } else { logic::refusal(&message) }
                    }
                    Unapplied::Unsure(message) | Unapplied::NotSent(message) => message,
                };
                self.outcome = Some(Outcome::Failed { text, kept: !lock });
            }
        }
    }

    /// Ends the restart's grace once its time is up: an outage is news
    /// again, and the Settings bar says that audio is not back.
    fn age_restart(&mut self, now: Instant) {
        if self.restart_until.is_some_and(|until| now >= until) {
            self.restart_until = None;
            self.check_back(now);
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
            let after = since.elapsed();
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
        self.worker.send(Command::Apply(change));
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
    pub fn restarting(&self, ctx: &egui::Context) -> bool {
        let Some(until) = self.restart_until else { return false };
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        // Show the problem, if there still is one, when the time is up.
        ctx.request_repaint_after(left);
        true
    }

    /// How long the daemon has not answered, once that is long enough to
    /// matter.
    pub fn stale(&self) -> Option<Duration> {
        let Connection::Up(_) = self.connection else { return None };
        let since = self.last_alive?.elapsed();
        (since >= STALE_AFTER).then_some(since)
    }

    /// Whether the form holds edits not applied yet.
    pub fn pending(&self) -> bool {
        self.form.as_ref().is_some_and(|f| !logic::change(&f.current, &f.edited).is_empty())
    }

    /// The Audio, Clock and Driver lamps.
    pub fn lamps(&self, ctx: &egui::Context) -> [Lamp; 3] {
        match &self.connection {
            Connection::Up(status) => {
                if let Some(stale) = self.stale() {
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
                let audio = if self.restarting(ctx) {
                    logic::restarting_lamp()
                } else {
                    logic::audio_lamp(status)
                };
                [audio, logic::clock_lamp(&status.clock), logic::driver_lamp(&status.driver)]
            }
            Connection::Connecting => logic::absent_lamps(None, false),
            Connection::Down(problem) => logic::absent_lamps(Some(problem), self.restarting(ctx)),
        }
    }

    fn device_name(&self) -> String {
        if let Some(device) = self.device()
            && !device.name.is_empty()
        {
            return device.name.clone();
        }
        match &self.form {
            Some(form) if !form.current.name.is_empty() => form.current.name.clone(),
            _ => "OpenVirtualSoundcard".to_owned(),
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let locked = self.device().is_some_and(|d| d.remote_settings_locked);
        // Controllers' news has a section of its own on Status, and a
        // restart's alarms are expected: no badge for either.
        let warnings = match &self.connection {
            Connection::Up(_) if self.restarting(ui.ctx()) => None,
            Connection::Up(status) => {
                let (urgent, other) = logic::split_warnings(status);
                let (problems, _) = logic::problems_and_news(other);
                if !urgent.is_empty() || !status.engine_running {
                    Some((Light::Red, "audio is down"))
                } else {
                    (!problems.is_empty()).then_some((Light::Amber, "warnings"))
                }
            }
            _ => None,
        };
        let status_badge = if self.tab == Tab::Status { None } else { warnings };
        let settings_badge = (self.tab != Tab::Settings && self.pending())
            .then_some((Light::Amber, "changes not applied"));
        let mut tab = self.tab;
        egui::Sides::new().shrink_left().show(
            ui,
            |ui| {
                // The name gives way to the badge, not the badge to the
                // tabs.
                let badge = if locked { 100.0 } else { 0.0 };
                ui.scope(|ui| {
                    ui.set_max_width((ui.available_width() - badge).max(48.0));
                    ui.add(Label::new(RichText::new(self.device_name()).heading()).truncate());
                });
                if locked {
                    lock_badge(ui);
                }
            },
            |ui| {
                widgets::segmented(
                    ui,
                    Id::new("tabs"),
                    &mut tab,
                    &[
                        (Tab::Status, "Status", status_badge),
                        (Tab::Settings, "Settings", settings_badge),
                    ],
                );
            },
        );
        self.switch_tab(tab);
        let subtitle = match &self.connection {
            Connection::Connecting => "Connecting…".to_owned(),
            Connection::Up(status) => match &status.device {
                Some(device) => logic::device_summary(device),
                None => "Not on the network".to_owned(),
            },
            Connection::Down(_) if self.restarting(ui.ctx()) => "Restarting…".to_owned(),
            Connection::Down(problem) => problem.short().to_owned(),
        };
        let versions = match &self.connection {
            Connection::Up(status) => {
                format!("App {} · daemon {}", env!("CARGO_PKG_VERSION"), status.version)
            }
            _ => format!("App {}", env!("CARGO_PKG_VERSION")),
        };
        ui.add(Label::new(RichText::new(subtitle).color(p.text2)).truncate())
            .on_hover_text(versions);
        ui.add_space(8.0);
        let lamps = self.lamps(ui.ctx());
        let gap = 8.0;
        let width = ui.available_width();
        // Audio takes 40 % (180 pt at least) while the others keep 120 pt;
        // when even that does not fit (a narrow window, zoomed in), thirds.
        let third = ((width - 2.0 * gap) / 3.0).max(0.0);
        let audio = (width * 0.40).max(180.0).min(width - 2.0 * (120.0 + gap)).max(third);
        let other = ((width - audio - 2.0 * gap) / 2.0).max(0.0);
        let height = 76.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            widgets::tile(ui, "Audio", &lamps[0], vec2(audio, height), true);
            widgets::tile(ui, "Clock", &lamps[1], vec2(other, height), false);
            widgets::tile(ui, "Driver", &lamps[2], vec2(other, height), false);
        });
    }

    /// Changes the tab; entering Settings reads the interfaces again (an
    /// adapter may have come or gone).
    pub fn switch_tab(&mut self, tab: Tab) {
        if tab != self.tab {
            self.tab = tab;
            if tab == Tab::Settings {
                self.worker.send(Command::Reload);
            }
        }
    }

    /// Whether anything is pinned under the header.
    fn has_alerts(&self, ctx: &egui::Context) -> bool {
        if self.restarting(ctx) {
            return true;
        }
        match &self.connection {
            Connection::Connecting => false,
            Connection::Down(_) => self.tab == Tab::Settings,
            Connection::Up(status) => {
                self.stale().is_some() || !status.engine_running || status.urgent_warnings > 0
            }
        }
    }

    /// What is pinned under the header on both tabs: an outage, a restart,
    /// a stale status. It never scrolls away.
    fn alerts(&mut self, ui: &mut Ui) {
        let compact = self.tab == Tab::Settings;
        if self.restarting(ui.ctx()) {
            // Expected: the user accepted it. Until audio is back, the
            // daemon's own alarms are the restart's.
            let since = match self.outcome {
                Some(Outcome::Restarting { since, .. }) => Some(since.elapsed()),
                _ => None,
            };
            widgets::notice(ui, Severity::Progress, "OpenVirtualSoundcard is restarting", |ui| {
                let mut text = "Audio returns in 10–20 s, in both directions.".to_owned();
                if let Some(since) = since {
                    text = format!("{} so far. {text}", logic::ago_label(since));
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
                ui.label(text);
            });
            return;
        }
        match &self.connection {
            Connection::Down(problem) => {
                let problem = problem.clone();
                widgets::notice(ui, Severity::Urgent, &problem.title(), |ui| {
                    if let Some(detail) = problem.detail() {
                        ui.add(Label::new(detail).wrap());
                    }
                    if ui.link("Show status and what to do").clicked() {
                        self.switch_tab(Tab::Status);
                    }
                });
            }
            Connection::Up(status) => {
                let status = status.clone();
                if let Some(stale) = self.stale() {
                    widgets::notice(ui, Severity::Warning, "No update from the daemon", |ui| {
                        ui.add(
                            Label::new(format!(
                                "Nothing heard for {}: what is shown may be out of date. Audio \
                                 may still flow; check the console's meters.",
                                logic::ago_label(stale)
                            ))
                            .wrap(),
                        );
                    });
                }
                let (urgent, _) = logic::split_warnings(&status);
                if !status.engine_running || !urgent.is_empty() {
                    widgets::notice(ui, Severity::Urgent, "Audio is down", |ui| {
                        if !status.engine_running {
                            ui.add(
                                Label::new(theme::headline("The network engine is not running."))
                                    .wrap(),
                            );
                            if let Some(error) = &status.engine_error {
                                ui.add(Label::new(logic::sentence_case(error)).wrap());
                            }
                            if !compact {
                                ui.label(
                                    "OpenVirtualSoundcard retries on its own. Check the cable and \
                                     the network interface in Settings.",
                                );
                            }
                        }
                        for warning in urgent {
                            if compact {
                                ui.add(
                                    Label::new(theme::headline(logic::headline(warning))).wrap(),
                                );
                            } else {
                                widgets::warning(ui, warning, &mut self.copied);
                            }
                        }
                        if compact && ui.link("Show status and what to do").clicked() {
                            self.switch_tab(Tab::Status);
                        }
                    });
                }
            }
            Connection::Connecting => {}
        }
    }

    /// Cmd-1 and Cmd-2 for the tabs, Cmd-comma for Settings, Cmd-W to
    /// close. No Cmd-M: egui-winit marks a window it minimises as minimised
    /// for good on macOS, and eframe would stop drawing it once restored.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = |key| KeyboardShortcut::new(Modifiers::COMMAND, key);
        let (status, settings, close) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&cmd(Key::Num1)),
                i.consume_shortcut(&cmd(Key::Num2)) || i.consume_shortcut(&cmd(Key::Comma)),
                i.consume_shortcut(&cmd(Key::W)),
            )
        });
        if status {
            self.switch_tab(Tab::Status);
        }
        if settings {
            self.switch_tab(Tab::Settings);
        }
        if close {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    /// Closing the window with edits not applied asks first.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.pending() && !self.discarding {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.ask_discard = true;
        }
    }

    fn discard_modal(&mut self, ctx: &egui::Context) {
        if !self.ask_discard {
            return;
        }
        let mut answer = None;
        let modal = egui::Modal::new(Id::new("discard"))
            .frame(Frame::popup(&ctx.global_style()).corner_radius(12).inner_margin(20))
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(theme::semibold("Discard the changes you have not applied?", 15.0));
                ui.add_space(6.0);
                ui.add(
                    Label::new(
                        "Closing the window drops them. OpenVirtualSoundcard keeps running.",
                    )
                    .wrap(),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 2.0 * 112.0 - 8.0).max(0.0));
                    let keep =
                        ui.add(egui::Button::new("Keep editing").min_size(vec2(112.0, 26.0)));
                    if keep.clicked() {
                        answer = Some(false);
                    }
                    if ui.memory(|m| m.focused().is_none()) {
                        keep.request_focus();
                    }
                    if ui
                        .add(widgets::destructive_button(ui, "Discard").min_size(vec2(112.0, 26.0)))
                        .clicked()
                    {
                        answer = Some(true);
                    }
                });
            });
        if answer.is_none() && modal.should_close() {
            answer = Some(false);
        }
        match answer {
            Some(true) => {
                self.ask_discard = false;
                self.discarding = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Some(false) => self.ask_discard = false,
            None => {}
        }
    }

    /// Outside the window: its title names the device and says when audio
    /// is down, and an outage asks for attention while the window is in the
    /// background: the Dock icon bounces once, since macOS cannot be told
    /// to stop a critical request when audio comes back. No sound: the
    /// Mac's default output may be this very device. Called from
    /// [`eframe::App::logic`], which runs while the window is covered or
    /// minimised too.
    fn window_signals(&mut self, ctx: &egui::Context, audio: &Lamp) {
        let name = self.device_name();
        let down = audio.light == Light::Red;
        let mut title = if name == "OpenVirtualSoundcard" {
            name
        } else {
            format!("{name} — OpenVirtualSoundcard")
        };
        if down {
            title = format!("Audio down · {title}");
        }
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        if down && !self.alerted {
            if !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
            self.alerted = true;
        } else if !down && self.alerted {
            ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Reset,
            ));
            self.alerted = false;
        }
    }

    /// Clears a plain confirmation once it has been read.
    fn age_outcome(&mut self, ctx: &egui::Context) {
        let age = match &self.outcome {
            Some(Outcome::Saved { at, .. }) => Some((*at, SAVED_FOR)),
            Some(Outcome::Back { at, .. }) => Some((*at, SAVED_FOR + Duration::from_secs(5))),
            _ => None,
        };
        if let Some((at, keep)) = age {
            let left = keep.saturating_sub(at.elapsed());
            if left.is_zero() {
                self.outcome = None;
            } else {
                ctx.request_repaint_after(left);
            }
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

/// The show lock, beside the device name while it is on.
fn lock_badge(ui: &mut Ui) {
    let p = theme::palette(ui);
    Frame::new()
        .fill(theme::mix(p.window, p.focus, 0.18))
        .stroke(egui::Stroke::new(1.0, p.focus))
        .corner_radius(255)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(theme::semibold("Show lock on", 11.0).color(p.text));
        })
        .response
        .on_hover_text(logic::LOCK_HELP);
}

impl eframe::App for App {
    /// Runs before every frame, and while the window is hidden too (covered
    /// or minimised), whenever the worker has news.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let events: Vec<Event> = self.worker.events().collect();
        for event in events {
            self.handle(event);
        }
        self.age_restart(Instant::now());
        let audio = self.lamps(ctx)[0].clone();
        self.window_signals(ctx, &audio);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        self.guard_close(&ctx);
        self.age_outcome(&ctx);
        if matches!(self.connection, Connection::Up(_)) {
            // Ticks the stale check and the counters' half minute.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        let p = theme::palette(ui);
        let gutter = theme::GUTTER as i8;
        egui::Panel::top("header")
            .frame(Frame::new().fill(p.window).inner_margin(Margin {
                left: gutter,
                right: gutter,
                top: 12,
                bottom: 12,
            }))
            .show(ui, |ui| self.header(ui));
        if self.has_alerts(&ctx) {
            egui::Panel::top("alerts")
                .frame(Frame::new().fill(p.window).inner_margin(Margin {
                    left: gutter,
                    right: gutter,
                    top: 10,
                    bottom: 2,
                }))
                .show_separator_line(false)
                .show(ui, |ui| self.alerts(ui));
        }
        if self.tab == Tab::Settings && self.form.is_some() {
            egui::Panel::bottom("actions")
                .frame(Frame::new().fill(p.window).inner_margin(Margin {
                    left: gutter,
                    right: gutter,
                    top: 10,
                    bottom: 12,
                }))
                .show(ui, |ui| settings::action_bar(self, ui));
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.window).inner_margin(Margin {
                left: gutter,
                right: gutter,
                top: 0,
                bottom: 0,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt(self.tab).auto_shrink(false).show(ui, |ui| {
                    match self.tab {
                        Tab::Status => status::show(self, ui),
                        Tab::Settings => settings::show(self, ui),
                    }
                    ui.add_space(theme::GUTTER);
                });
            });
        settings::confirm_modal(self, &ctx);
        self.discard_modal(&ctx);
    }
}
