//! The window: the device and its status lights at the top, then a Status
//! tab and a Settings tab.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText, Ui, vec2};
use ovsc_control::{
    DeviceStatus, InterfaceInfo, MAX_CHANNELS, Restart, Settings, SettingsChange, Status,
};

use crate::logic::{self, Light, Problem};
use crate::worker::{Command, Event, Worker};

/// How long after a change that restarts the daemon its absence reads as
/// the restart rather than as a problem.
const RESTART_GRACE: Duration = Duration::from_secs(40);

/// The width of each status light in the header.
const INDICATOR_WIDTH: f32 = 112.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Settings,
}

/// The connection, as far as the UI knows.
enum Connection {
    /// Nothing heard yet.
    Connecting,
    Up(Box<Status>),
    Down(Problem),
}

/// The daemon's settings and the user's edit of them.
struct Form {
    current: Settings,
    edited: Settings,
}

pub struct App {
    worker: Worker,
    tab: Tab,
    connection: Connection,
    interfaces: Vec<InterfaceInfo>,
    /// Once the settings were read.
    form: Option<Form>,
    /// A change waiting for the user to accept a restart.
    confirm: Option<SettingsChange>,
    /// A change was sent and is not answered yet.
    applying: bool,
    /// What became of the last change.
    outcome: Option<Result<&'static str, String>>,
    /// The next settings follow an applied change: they replace the edit.
    take_next: bool,
    /// Until when the daemon is expected to be restarting.
    restart_until: Option<Instant>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, socket: PathBuf) -> App {
        style(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        App {
            worker: Worker::start(socket, move || ctx.request_repaint()),
            tab: Tab::Status,
            connection: Connection::Connecting,
            interfaces: Vec::new(),
            form: None,
            confirm: None,
            applying: false,
            outcome: None,
            take_next: false,
            restart_until: None,
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Problem(problem) => self.connection = Connection::Down(problem),
            Event::Status(status) => {
                if matches!(self.connection, Connection::Down(_)) {
                    // Back from the restart, if one was expected.
                    self.restart_until = None;
                }
                self.connection = Connection::Up(status);
            }
            Event::Settings(new) => {
                let edited = match &self.form {
                    Some(form) if !self.take_next => {
                        logic::rebase(&form.current, &form.edited, &new)
                    }
                    _ => new.clone(),
                };
                self.form = Some(Form { current: new, edited });
                self.take_next = false;
            }
            Event::Interfaces(interfaces) => self.interfaces = interfaces,
            Event::Applied(Ok(restart)) => {
                self.applying = false;
                self.outcome = Some(Ok(logic::applied_message(restart)));
                // Saved: the edit is the daemon's now, until it says more.
                if let Some(form) = &mut self.form {
                    form.edited.name = form.edited.name.trim().to_owned();
                    form.current = form.edited.clone();
                }
                self.take_next = true;
                if restart == Restart::Daemon {
                    self.restart_until = Some(Instant::now() + RESTART_GRACE);
                }
            }
            Event::Applied(Err(message)) => {
                self.applying = false;
                self.outcome = Some(Err(message));
            }
        }
    }

    /// Whether the daemon is away because a change restarts it.
    fn restarting(&self, ctx: &egui::Context) -> bool {
        let Some(until) = self.restart_until else { return false };
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || !matches!(self.connection, Connection::Down(_)) {
            return false;
        }
        // Show the problem, if there still is one, when the time is up.
        ctx.request_repaint_after(left);
        true
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            // The name and subtitle get what the lights leave.
            let lights = 3.0 * (INDICATOR_WIDTH + ui.spacing().item_spacing.x);
            let width = (ui.available_width() - lights).max(120.0);
            ui.allocate_ui_with_layout(vec2(width, 48.0), Layout::top_down(Align::Min), |ui| {
                let name = RichText::new(self.device_name()).size(20.0).strong();
                ui.add(egui::Label::new(name).truncate());
                let subtitle = match &self.connection {
                    Connection::Connecting => "Connecting…".to_owned(),
                    Connection::Up(status) => format!("OpenVirtualSoundcard {}", status.version),
                    Connection::Down(_) if self.restarting(ui.ctx()) => "Restarting…".to_owned(),
                    Connection::Down(Problem::Version { .. }) => "Different version".to_owned(),
                    Connection::Down(_) => "Not connected".to_owned(),
                };
                ui.add(egui::Label::new(RichText::new(subtitle).weak()).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (title, light, hover) in self.lights().into_iter().rev() {
                    indicator(ui, title, &light, &hover);
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let was = self.tab;
            ui.selectable_value(&mut self.tab, Tab::Status, RichText::new("Status").size(14.0));
            ui.selectable_value(&mut self.tab, Tab::Settings, RichText::new("Settings").size(14.0));
            if self.tab == Tab::Settings && was != Tab::Settings {
                // The interfaces may have changed since.
                self.worker.send(Command::Reload);
            }
        });
        ui.add_space(6.0);
    }

    /// The Clock, Driver and Audio lights, each with its word and more to
    /// show on hover.
    fn lights(&self) -> [(&'static str, (Light, String), String); 3] {
        let Connection::Up(status) = &self.connection else {
            let unknown = || (Light::Grey, "Unknown".to_owned());
            return [
                ("Clock", unknown(), String::new()),
                ("Driver", unknown(), String::new()),
                ("Audio", unknown(), String::new()),
            ];
        };
        let driver = &status.driver;
        let (driver_light, driver_word) = logic::driver_light(driver);
        let (audio_light, audio_word) = logic::audio_light(driver);
        let audio_hover = if driver.io_running {
            "An application is playing or recording through OpenVirtualSoundcard."
        } else {
            "No application is using OpenVirtualSoundcard right now."
        };
        [
            (
                "Clock",
                logic::clock_light(&status.clock),
                format!("Source: {}", logic::clock_source_label(&status.clock.source)),
            ),
            ("Driver", (driver_light, driver_word.to_owned()), driver.detail.clone()),
            ("Audio", (audio_light, audio_word.to_owned()), audio_hover.to_owned()),
        ]
    }

    fn device_name(&self) -> String {
        if let Connection::Up(status) = &self.connection
            && let Some(device) = &status.device
        {
            return device.name.clone();
        }
        match &self.form {
            Some(form) => form.current.name.clone(),
            None => "OpenVirtualSoundcard".to_owned(),
        }
    }

    /// What keeps the app from working, above either tab.
    fn connection_notice(&self, ui: &mut Ui) {
        match &self.connection {
            Connection::Up(_) => {}
            Connection::Down(_) if self.restarting(ui.ctx()) => notice(ui, Light::Amber, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("OpenVirtualSoundcard is restarting. Audio returns in 10–20 seconds.");
                });
            }),
            Connection::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to OpenVirtualSoundcard…");
                });
            }
            Connection::Down(problem) => notice(ui, Light::Red, |ui| {
                ui.label(RichText::new(problem.message()).strong());
                if let Some(hint) = problem.hint() {
                    ui.horizontal(|ui| {
                        ui.label(hint);
                        if let Some(command) = problem.command()
                            && ui.small_button("Copy").clicked()
                        {
                            ui.ctx().copy_text(command.to_owned());
                        }
                    });
                }
                if let Some(command) = problem.command() {
                    ui.label(RichText::new(command).monospace());
                }
            }),
        }
    }

    fn status_tab(&self, ui: &mut Ui) {
        let Connection::Up(status) = &self.connection else { return };
        if !status.warnings.is_empty() {
            notice(ui, Light::Amber, |ui| {
                for warning in &status.warnings {
                    ui.label(RichText::new(format!("⚠ {warning}")).strong());
                }
            });
        }
        if !status.engine_running {
            notice(ui, Light::Red, |ui| {
                ui.label(RichText::new("The network engine is not running.").strong());
                if let Some(error) = &status.engine_error {
                    ui.label(error);
                }
            });
        }
        ui.columns(2, |columns| {
            section(&mut columns[0], "Device", |ui| match &status.device {
                Some(device) => device_rows(ui, device),
                None => {
                    ui.label(RichText::new("Not running.").weak());
                }
            });
            section(&mut columns[1], "Clock", |ui| {
                let clock = &status.clock;
                rows(
                    ui,
                    "clock",
                    &[
                        ("Source", logic::clock_source_label(&clock.source)),
                        ("State", logic::clock_light(clock).1),
                        ("Leader", clock.leader.clone().unwrap_or_else(|| "None".to_owned())),
                        ("Offset", logic::micros_label(clock.offset_ns)),
                        ("Network delay", logic::micros_label(clock.path_delay_ns)),
                        ("Rate", logic::ppm_label(clock.freq_offset_ppm)),
                    ],
                );
            });
        });
        if let Some(device) = &status.device {
            section(ui, "Receive channels", |ui| rx_table(ui, device));
            section(ui, "Transmit", |ui| tx_table(ui, device));
            ui.columns(2, |columns| {
                section(&mut columns[0], "Packets", |ui| {
                    let p = device.packets;
                    rows(
                        ui,
                        "packets",
                        &[
                            ("Sent", logic::count_label(p.tx)),
                            ("Sent without audio", logic::count_label(p.tx_underruns)),
                            ("Received", logic::count_label(p.rx)),
                            ("Arrived too late", logic::count_label(p.rx_late)),
                        ],
                    );
                });
                section(&mut columns[1], "Core Audio driver", |ui| driver_rows(ui, status));
            });
        } else {
            section(ui, "Core Audio driver", |ui| driver_rows(ui, status));
        }
    }

    fn settings_tab(&mut self, ui: &mut Ui) {
        let up = matches!(self.connection, Connection::Up(_));
        let Some(form) = &mut self.form else {
            if up {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the settings…");
                });
            }
            return;
        };
        let usable = up && !self.applying;
        let before = form.edited.clone();
        let name_problem = logic::name_problem(&form.edited.name);
        ui.add_enabled_ui(usable, |ui| {
            section(ui, "Device", |ui| settings_form(ui, form, &self.interfaces, name_problem));
        });
        if form.edited != before {
            self.outcome = None;
        }

        let change = logic::change(&form.current, &form.edited);
        let restart = logic::expected_restart(&change);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(note) = logic::interruption(restart) {
                ui.label(RichText::new(note).color(color(Light::Amber)));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let apply = usable && !change.is_empty() && name_problem.is_none();
                if ui.add_enabled(apply, egui::Button::new("Apply")).clicked() {
                    if restart == Restart::Daemon {
                        self.confirm = Some(change.clone());
                    } else {
                        self.applying = true;
                        self.outcome = None;
                        self.worker.send(Command::Apply(change.clone()));
                    }
                }
                let revert = !self.applying && !change.is_empty();
                if ui.add_enabled(revert, egui::Button::new("Revert")).clicked() {
                    form.edited = form.current.clone();
                    self.outcome = None;
                    self.worker.send(Command::Reload);
                }
                if self.applying {
                    ui.label("Applying…");
                    ui.spinner();
                }
            });
        });
        match &self.outcome {
            Some(Ok(message)) => {
                ui.label(RichText::new(*message).color(color(Light::Green)));
            }
            Some(Err(message)) => {
                ui.label(RichText::new(message).color(color(Light::Red)));
            }
            None => {}
        }
    }

    /// Asks before a change that restarts the daemon.
    fn confirm_restart(&mut self, ctx: &egui::Context) {
        if self.confirm.is_none() {
            return;
        }
        let mut accept = None;
        let modal = egui::Modal::new(egui::Id::new("confirm-restart")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new("Restart OpenVirtualSoundcard?").strong().size(15.0));
            ui.add_space(4.0);
            ui.label(logic::interruption(Restart::Daemon).unwrap_or_default());
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Restart").clicked() {
                    accept = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    accept = Some(false);
                }
            });
        });
        if accept.is_none() && modal.should_close() {
            accept = Some(false);
        }
        match accept {
            Some(true) => {
                if let Some(change) = self.confirm.take() {
                    self.applying = true;
                    self.outcome = None;
                    self.worker.send(Command::Apply(change));
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let events: Vec<Event> = self.worker.events().collect();
        for event in events {
            self.handle(event);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.add_space(4.0);
                self.connection_notice(ui);
                match self.tab {
                    Tab::Status => self.status_tab(ui),
                    Tab::Settings => self.settings_tab(ui),
                }
                ui.add_space(8.0);
            });
        });
        self.confirm_restart(ui.ctx());
    }
}

/// The settings that can be changed, as a form.
fn settings_form(
    ui: &mut Ui,
    form: &mut Form,
    interfaces: &[InterfaceInfo],
    name_problem: Option<&str>,
) {
    let Form { current, edited } = form;
    egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Device name");
        ui.vertical(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut edited.name)
                    .desired_width(240.0)
                    .char_limit(logic::MAX_NAME_LEN),
            );
            match name_problem {
                Some(problem) => ui.label(RichText::new(problem).small().color(color(Light::Red))),
                None => ui.label(RichText::new("Shown in Dante Controller.").small().weak()),
            };
        });
        ui.end_row();

        ui.label("Network interface");
        let choices = logic::interface_choices(interfaces, &[&current.interface, &edited.interface]);
        let selected = choices
            .iter()
            .find(|(value, _)| *value == edited.interface)
            .map_or_else(|| edited.interface.clone(), |(_, label)| label.clone());
        egui::ComboBox::from_id_salt("interface").width(340.0).selected_text(selected).show_ui(
            ui,
            |ui| {
                for (value, label) in choices {
                    ui.selectable_value(&mut edited.interface, value, label);
                }
            },
        );
        ui.end_row();

        ui.label("Sample rate");
        egui::ComboBox::from_id_salt("rate")
            .selected_text(logic::rate_label(edited.sample_rate))
            .show_ui(ui, |ui| {
                for rate in logic::rate_choices(current.sample_rate) {
                    ui.selectable_value(&mut edited.sample_rate, rate, logic::rate_label(rate));
                }
            });
        ui.end_row();

        ui.label("Bit depth");
        egui::ComboBox::from_id_salt("bits")
            .selected_text(logic::bits_label(edited.bits_per_sample))
            .show_ui(ui, |ui| {
                for bits in logic::bits_choices(current.bits_per_sample) {
                    ui.selectable_value(&mut edited.bits_per_sample, bits, logic::bits_label(bits));
                }
            });
        ui.end_row();

        ui.label("Receive channels");
        ui.add(egui::DragValue::new(&mut edited.rx_channels).range(1..=MAX_CHANNELS));
        ui.end_row();

        ui.label("Transmit channels");
        ui.add(egui::DragValue::new(&mut edited.tx_channels).range(1..=MAX_CHANNELS));
        ui.end_row();

        ui.label("Latency");
        ui.vertical(|ui| {
            egui::ComboBox::from_id_salt("latency")
                .selected_text(logic::latency_label(edited.latency_ms))
                .show_ui(ui, |ui| {
                    for ms in logic::latency_choices(current.latency_ms) {
                        ui.selectable_value(&mut edited.latency_ms, ms, logic::latency_label(ms));
                    }
                });
            ui.label(
                RichText::new("How long received audio waits before it plays: more is safer on a busy network.")
                    .small()
                    .weak(),
            );
        });
        ui.end_row();
    });
}

fn device_rows(ui: &mut Ui, device: &DeviceStatus) {
    rows(
        ui,
        "device",
        &[
            ("Name", device.name.clone()),
            ("Network interface", device.interface.clone()),
            ("IP address", device.ip.clone()),
            ("Sample rate", logic::rate_label(device.sample_rate)),
            ("Bit depth", format!("{}-bit", device.bits_per_sample)),
            ("Latency", logic::latency_label(device.latency_ms)),
        ],
    );
}

fn driver_rows(ui: &mut Ui, status: &Status) {
    let yes_no = |b: bool| if b { "Yes" } else { "No" }.to_owned();
    let driver = &status.driver;
    let mut lines = vec![
        ("Connected", yes_no(driver.connected)),
        ("In use", yes_no(driver.io_running)),
        ("Audio", logic::audio_light(driver).1.to_owned()),
    ];
    if !driver.detail.is_empty() {
        lines.push(("Details", driver.detail.clone()));
    }
    rows(ui, "driver", &lines);
}

fn rx_table(ui: &mut Ui, device: &DeviceStatus) {
    if device.rx_channels.is_empty() {
        ui.label(RichText::new("No receive channels.").weak());
        return;
    }
    egui::Grid::new("rx").num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
        ui.label("");
        ui.label(RichText::new("Channel").weak());
        ui.label(RichText::new("Source").weak());
        ui.label(RichText::new("State").weak());
        ui.end_row();
        for channel in &device.rx_channels {
            let receiving = if channel.receiving { Some(color(Light::Green)) } else { None };
            dot(ui, receiving).on_hover_text(if channel.receiving {
                "Receiving"
            } else {
                "Not receiving"
            });
            ui.label(&channel.name);
            ui.label(channel.source.as_deref().unwrap_or("—"));
            ui.label(if channel.state.is_empty() { "—" } else { &channel.state });
            ui.end_row();
        }
    });
}

fn tx_table(ui: &mut Ui, device: &DeviceStatus) {
    ui.label(format!("{} transmit channels.", device.tx_channels.len()));
    if device.tx_flows.is_empty() {
        ui.label(RichText::new("No other device receives from this one.").weak());
        return;
    }
    ui.add_space(4.0);
    egui::Grid::new("tx").num_columns(3).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
        ui.label(RichText::new("Receiver").weak());
        ui.label(RichText::new("Destination").weak());
        ui.label(RichText::new("Channels").weak());
        ui.end_row();
        for flow in &device.tx_flows {
            ui.label(flow.receiver.as_deref().unwrap_or("Unknown"));
            ui.label(&flow.destination);
            ui.label(logic::channels_label(&flow.channels));
            ui.end_row();
        }
    });
}

/// Name and value pairs in two columns.
fn rows(ui: &mut Ui, id: &str, rows: &[(&str, String)]) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        for (name, value) in rows {
            ui.label(RichText::new(*name).weak());
            ui.label(value);
            ui.end_row();
        }
    });
}

/// A titled group.
fn section(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().size(14.0));
    ui.add_space(2.0);
    egui::Frame::group(ui.style()).inner_margin(10.0).corner_radius(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// A tinted box for news that matters.
fn notice(ui: &mut Ui, light: Light, add: impl FnOnce(&mut Ui)) {
    let c = color(light);
    egui::Frame::new()
        .fill(c.gamma_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, c.gamma_multiply(0.7)))
        .corner_radius(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(6.0);
}

/// A status light with its title and word, and more on hover.
fn indicator(ui: &mut Ui, title: &str, (light, text): &(Light, String), hover: &str) {
    let response = ui
        .allocate_ui_with_layout(vec2(INDICATOR_WIDTH, 40.0), Layout::top_down(Align::Min), |ui| {
            ui.label(RichText::new(title).small().weak());
            ui.horizontal(|ui| {
                dot(ui, Some(color(*light)));
                ui.label(RichText::new(text).strong());
            });
        })
        .response;
    if !hover.is_empty() {
        response.on_hover_text(hover);
    }
}

/// A round light, or its space when `None`.
fn dot(ui: &mut Ui, color: Option<Color32>) -> egui::Response {
    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, response) = ui.allocate_exact_size(vec2(height * 0.8, height), egui::Sense::hover());
    if let Some(color) = color {
        ui.painter().circle_filled(rect.center(), height * 0.32, color);
    }
    response
}

/// macOS's system colours.
fn color(light: Light) -> Color32 {
    match light {
        Light::Green => Color32::from_rgb(52, 199, 89),
        Light::Amber => Color32::from_rgb(255, 159, 10),
        Light::Red => Color32::from_rgb(255, 69, 58),
        Light::Grey => Color32::from_rgb(142, 142, 147),
    }
}

fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 6.0);
        // Room for every latency without scrolling the menu.
        style.spacing.combo_height = 360.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        style.spacing.button_padding = vec2(12.0, 4.0);
        style.spacing.interact_size.y = 22.0;
        if let Some(small) = style.text_styles.get_mut(&egui::TextStyle::Small) {
            small.size = 11.0;
        }
    });
}
