//! The Status tab: what is worth a look, the inputs and the returns, then
//! the clock and the counters, the device and the driver. Outages are
//! pinned above both tabs (see [`crate::app`]); this tab shows the rest.

use eframe::egui::{self, Label, RichText, Ui};
use ovsc_control::{DeviceStatus, Status};

use crate::app::{App, Connection};
use crate::logic::{self, Light, PacketWindow, Problem};
use crate::theme;
use crate::widgets::{self, Copied, Severity, Width};

pub fn show(app: &mut App, ui: &mut Ui) {
    let restarting = app.restarting(ui.ctx());
    let App { connection, copied, packets, .. } = app;
    let Connection::Up(status) = connection else {
        empty_state(connection, restarting, copied, ui);
        return;
    };
    let (_, other) = logic::split_warnings(status);
    let (problems, news) = logic::problems_and_news(other);
    if !problems.is_empty() {
        ui.add_space(theme::SECTION_GAP - 6.0);
        let heading = match problems.len() {
            1 => "Warning".to_owned(),
            n => format!("{n} warnings"),
        };
        widgets::notice(ui, Severity::Warning, &heading, |ui| {
            for (i, warning) in problems.iter().enumerate() {
                if i > 0 {
                    ui.add_space(6.0);
                }
                widgets::warning(ui, warning, copied);
            }
        });
    }
    if let Some(device) = &status.device {
        let (light, summary) = logic::rx_summary(&device.rx_channels);
        widgets::section(ui, "Receive channels", Some((light, &summary)), |ui| {
            rx_table(ui, device)
        });
        let (light, summary) = logic::tx_summary(device);
        widgets::section(ui, "Transmit channels", Some((light, &summary)), |ui| {
            tx_table(ui, device)
        });
    }
    widgets::pair(
        ui,
        |ui| clock_section(ui, status),
        |ui| match &status.device {
            Some(device) => packets_section(ui, status, device, packets),
            None => {
                widgets::section(ui, "Packets", None, |ui| {
                    ui.label(RichText::new("Not on the network.").weak());
                });
            }
        },
    );
    widgets::pair(ui, |ui| device_section(ui, status), |ui| driver_section(ui, status));
    if !news.is_empty() || !status.earlier.is_empty() {
        let p = theme::palette(ui);
        widgets::section(ui, "Changes from controllers", None, |ui| {
            for item in &news {
                ui.add(Label::new(*item).wrap());
            }
            if !news.is_empty() && !status.earlier.is_empty() {
                widgets::separator(ui);
            }
            for item in &status.earlier {
                ui.add(Label::new(RichText::new(item).color(p.text2)).wrap());
            }
        });
    }
    if !status.driver.detail.is_empty() {
        ui.add_space(theme::SECTION_GAP - 6.0);
        egui::CollapsingHeader::new(theme::headline("Diagnostics"))
            .id_salt("diagnostics")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(theme::caption("The driver's status, for a bug report."));
                widgets::card(ui, |ui| {
                    ui.add(
                        Label::new(RichText::new(&status.driver.detail).monospace().small())
                            .wrap()
                            .selectable(true),
                    );
                });
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(status.driver.detail.clone());
                }
            });
    }
}

/// Instead of the tab, when there is no status: what is wrong, what it
/// means for the audio, and the way back.
fn empty_state(connection: &Connection, restarting: bool, copied: &mut Copied, ui: &mut Ui) {
    let p = theme::palette(ui);
    ui.add_space((ui.available_height() * 0.10).clamp(16.0, 64.0));
    ui.vertical_centered(|ui| {
        // Wide enough for the start command's path on one line.
        ui.set_max_width(600.0);
        match connection {
            Connection::Connecting => {
                ui.spinner();
                ui.label("Connecting to OpenVirtualSoundcard…");
            }
            Connection::Down(_) if restarting => {
                ui.spinner();
                ui.label(theme::semibold("Waiting for OpenVirtualSoundcard to come back…", 16.0));
            }
            Connection::Down(problem) => {
                let light = match problem {
                    Problem::NotRunning | Problem::NoAnswer | Problem::Lost => Light::Red,
                    _ => Light::Amber,
                };
                widgets::mark(ui, light, 36.0);
                ui.add_space(6.0);
                ui.add(Label::new(theme::semibold(problem.title(), 18.0)).wrap());
                if let Some(detail) = problem.detail() {
                    ui.add_space(4.0);
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let job = widgets::rich_text(ui, &detail, font, p.text);
                    ui.add(Label::new(job).wrap().selectable(true));
                }
                ui.add_space(4.0);
                ui.add(Label::new(RichText::new(problem.hint()).color(p.text2)).wrap());
                if let Some(command) = problem.command() {
                    ui.add_space(8.0);
                    widgets::command_row(ui, command, copied);
                }
                ui.add_space(10.0);
                ui.label(theme::caption("The app tries again every second."));
            }
            Connection::Up(_) => {}
        }
    });
}

fn rx_table(ui: &mut Ui, device: &DeviceStatus) {
    let p = theme::palette(ui);
    if device.rx_channels.is_empty() {
        ui.label(RichText::new("No receive channels.").weak());
        return;
    }
    let columns = [
        ("", Width::Fixed(14.0)),
        ("#", Width::Fixed(22.0)),
        ("Channel", Width::Share(1.0)),
        ("Source", Width::Share(1.5)),
        ("Subscription", Width::Share(1.3)),
    ];
    widgets::table(ui, "rx", &columns, device.rx_channels.len(), |ui, row, col| {
        let channel = &device.rx_channels[row];
        let light = logic::channel_light(channel);
        match col {
            0 if light == Light::Grey => {
                ui.label(RichText::new(logic::NONE).color(p.text3));
            }
            0 => {
                widgets::mark(ui, light, 12.0).on_hover_text(logic::subscription_label(channel));
            }
            1 => {
                ui.label(RichText::new((row + 1).to_string()).color(p.text2));
            }
            2 => {
                ui.add(Label::new(&channel.name).truncate());
            }
            3 => {
                let (source, color) = match &channel.source {
                    Some(source) => (source.as_str(), p.text),
                    None => (logic::NONE, p.text3),
                };
                ui.add(Label::new(RichText::new(source).color(color)).truncate());
            }
            _ => {
                let color = match light {
                    Light::Red | Light::Amber => p.ink(light),
                    Light::Grey => p.text3,
                    Light::Green => p.text,
                };
                let state = logic::subscription_label(channel);
                ui.add(Label::new(RichText::new(state).color(color)).truncate());
            }
        }
    });
}

fn tx_table(ui: &mut Ui, device: &DeviceStatus) {
    let p = theme::palette(ui);
    if device.tx_flows.is_empty() {
        ui.add(
            Label::new(
                RichText::new(
                    "No device receives from this one: patch the returns in Dante Controller.",
                )
                .color(p.text2),
            )
            .wrap(),
        );
        return;
    }
    let columns = [
        ("Received by", Width::Share(1.4)),
        ("Address", Width::Share(1.0)),
        ("Channels", Width::Share(1.0)),
    ];
    widgets::table(ui, "tx", &columns, device.tx_flows.len(), |ui, row, col| {
        let flow = &device.tx_flows[row];
        match col {
            0 => ui.add(Label::new(logic::flow_receiver(flow)).truncate()),
            1 => ui.add(
                Label::new(RichText::new(logic::address_label(&flow.destination)).color(p.text2))
                    .truncate(),
            ),
            _ => ui.add(Label::new(logic::channels_label(&flow.channels)).truncate()),
        };
    });
}

fn clock_section(ui: &mut Ui, status: &Status) {
    let p = theme::palette(ui);
    let (light, _) = logic::clock_light(&status.clock);
    widgets::section(ui, "Clock", None, |ui| {
        let rows: Vec<(&str, RichText)> = logic::clock_rows(&status.clock)
            .into_iter()
            .map(|(name, value)| {
                let text = match name {
                    "State" if light != Light::Green => RichText::new(value).color(p.ink(light)),
                    "Offset" | "Network delay" | "Drift" => RichText::new(value).monospace(),
                    _ => RichText::new(value),
                };
                (name, text)
            })
            .collect();
        widgets::rows(ui, "clock", &rows);
    });
}

fn packets_section(ui: &mut Ui, status: &Status, device: &DeviceStatus, window: &PacketWindow) {
    let p = theme::palette(ui);
    let counts = device.packets;
    let (late, lost, without_audio) = window.rises();
    let counter = |total: u64, rise: u64| {
        if rise > 0 {
            RichText::new(format!(
                "{} · +{} in 30 s",
                logic::count_label(total),
                logic::count_label(rise)
            ))
            .monospace()
            .color(p.amber_ink)
        } else {
            RichText::new(logic::count_label(total)).monospace()
        }
    };
    let underruns = if status.driver.io_running {
        counter(counts.tx_underruns, without_audio)
    } else {
        RichText::new(format!("{} (no application playing)", logic::NONE)).color(p.text2)
    };
    widgets::section(ui, "Packets", Some((Light::Grey, "since the engine started")), |ui| {
        widgets::rows(
            ui,
            "packets",
            &[
                ("Received", RichText::new(logic::count_label(counts.rx)).monospace()),
                ("Arrived too late", counter(counts.rx_late, late)),
                ("Lost", counter(counts.rx_lost, lost)),
                ("Sent", RichText::new(logic::count_label(counts.tx)).monospace()),
                ("Sent without audio", underruns),
            ],
        );
    });
}

fn device_section(ui: &mut Ui, status: &Status) {
    let p = theme::palette(ui);
    let version = format!("App {} · daemon {}", env!("CARGO_PKG_VERSION"), status.version);
    widgets::section(ui, "Device", None, |ui| match &status.device {
        Some(d) => {
            let interface = match (d.interface.is_empty(), d.ip.is_empty()) {
                (false, false) => format!("{} ({})", d.interface, d.ip),
                (false, true) => d.interface.clone(),
                (true, false) => d.ip.clone(),
                (true, true) => logic::NONE.to_owned(),
            };
            widgets::rows(
                ui,
                "device",
                &[
                    ("Interface", RichText::new(interface)),
                    ("Sample rate", RichText::new(logic::rate_label(d.sample_rate))),
                    ("Bit depth", RichText::new(logic::bits_label(d.bits_per_sample))),
                    ("Receive latency", RichText::new(logic::latency_label(d.latency_ms))),
                    ("Show lock", RichText::new(logic::lock_label(d.remote_settings_locked))),
                    ("Version", RichText::new(version).color(p.text2)),
                ],
            );
        }
        None => {
            ui.label(RichText::new("Not on the network: see above.").color(p.text2));
            widgets::rows(ui, "device", &[("Version", RichText::new(version).color(p.text2))]);
        }
    });
}

fn driver_section(ui: &mut Ui, status: &Status) {
    let p = theme::palette(ui);
    let driver = &status.driver;
    let yes_no = |b: bool| RichText::new(if b { "Yes" } else { "No" });
    let loaded = if driver.faulted {
        RichText::new("Failed: restart Core Audio").color(p.red_ink)
    } else if driver.connected {
        yes_no(true)
    } else {
        RichText::new("No").color(p.red_ink)
    };
    let audio = logic::audio_lamp(status);
    let audio_text = match audio.light {
        Light::Green | Light::Grey => RichText::new(&audio.word),
        light => RichText::new(&audio.word).color(p.ink(light)),
    };
    widgets::section(ui, "Core Audio driver", None, |ui| {
        widgets::rows(
            ui,
            "driver",
            &[
                ("Loaded by Core Audio", loaded),
                ("Used by an application", yes_no(driver.io_running)),
                ("Audio", audio_text),
            ],
        );
    });
}
