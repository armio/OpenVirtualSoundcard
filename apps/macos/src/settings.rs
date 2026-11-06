//! The Settings tab: the device's settings in cards, a bar with Apply that
//! stays in view, and the confirmation before a change that interrupts
//! audio or other devices' subscriptions.

use eframe::egui::{self, Frame, Id, Key, Label, RichText, TextEdit, Ui, vec2};
use ovsc_control::{InterfaceInfo, SettingsChange};

use crate::app::{App, Connection, Form, Outcome};
use crate::logic::{self, Light};
use crate::theme;
use crate::widgets;

/// The width of the menus and the name field.
const CONTROL_WIDTH: f32 = 200.0;

pub fn show(app: &mut App, ui: &mut Ui) {
    let up = matches!(app.connection, Connection::Up(_));
    let usable = up && !app.applying;
    let interfaces = app.interfaces.clone();
    let applying_lock = app.applying && app.sent.lock_remote_settings.is_some();
    let Some(form) = &mut app.form else {
        ui.add_space(32.0);
        ui.vertical_centered(|ui| {
            if up {
                ui.spinner();
                ui.label("Reading the settings…");
            } else {
                ui.label(
                    RichText::new("The settings appear once the app reaches the daemon.").weak(),
                );
            }
        });
        return;
    };
    let before = form.edited.clone();
    let name_problem = logic::edited_name_problem(&form.current.name, &form.edited.name);
    let mut enter = false;
    let mut lock = None;
    ui.add_enabled_ui(usable, |ui| {
        widgets::section(ui, "Device", None, |ui| {
            enter = name_row(ui, form, name_problem);
        });
        widgets::section(ui, "Audio format and network", None, |ui| {
            ui.add(
                Label::new(theme::caption(
                    "Changing any of these restarts OpenVirtualSoundcard: audio stops for 10–20 s \
                     in both directions.",
                ))
                .wrap(),
            );
            widgets::separator(ui);
            format_rows(ui, form, &interfaces);
        });
        widgets::section(ui, "Receive latency", None, |ui| {
            let Form { current, edited } = form;
            let choices: Vec<(f64, String)> = logic::latency_choices(current.latency_ms)
                .into_iter()
                .map(|ms| (ms, logic::latency_label(ms)))
                .collect();
            let changed = edited.latency_ms != current.latency_ms;
            let help = theme::caption(
                "How long received audio waits before it plays: more is safer on a busy \
                 network. Changing it stops the inputs for about a second; the returns carry on.",
            );
            widgets::setting_row(ui, "Latency", Some(help), changed, |ui| {
                let r = menu(ui, "latency", CONTROL_WIDTH, &mut edited.latency_ms, &choices);
                was(ui, changed, logic::latency_label(current.latency_ms));
                r
            });
        });
    });
    // The show lock applies at once, and stays usable while other edits
    // are pending.
    ui.add_enabled_ui(usable || applying_lock, |ui| {
        widgets::section(ui, "Show lock", None, |ui| {
            let help = theme::caption(format!("{} Applies at once.", logic::LOCK_HELP));
            let mut on = form.edited.lock_remote_settings;
            widgets::setting_row(
                ui,
                "Refuse changes from Dante Controller",
                Some(help),
                false,
                |ui| {
                    if applying_lock {
                        ui.spinner()
                    } else {
                        let r = widgets::toggle(ui, &mut on);
                        if r.changed() {
                            lock = Some(on);
                        }
                        r
                    }
                },
            );
        });
    });
    if form.edited != before && !matches!(app.outcome, Some(Outcome::Restarting { .. })) {
        app.outcome = None;
    }
    if let Some(on) = lock {
        if let Some(form) = &mut app.form {
            form.edited.lock_remote_settings = on;
        }
        app.apply(SettingsChange { lock_remote_settings: Some(on), ..Default::default() });
    }
    if enter {
        request_apply(app);
    }
}

/// The device name, with its rule up front and its error in its place.
/// True when Return was pressed in it.
fn name_row(ui: &mut Ui, form: &mut Form, problem: Option<&str>) -> bool {
    let p = theme::palette(ui);
    let changed = form.edited.name.trim() != form.current.name;
    let help = match problem {
        Some(problem) => RichText::new(problem).small().color(p.red_ink),
        None => theme::caption(
            "Up to 31 letters, digits and hyphens, shown in Dante Controller. Renaming breaks \
             other devices' subscriptions to this one.",
        ),
    };
    let mut enter = false;
    widgets::setting_row(ui, "Name", Some(help), changed, |ui| {
        let r = ui.add(
            TextEdit::singleline(&mut form.edited.name)
                .desired_width(CONTROL_WIDTH)
                .char_limit(logic::MAX_NAME_LEN),
        );
        enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if problem.is_some() {
            let red = p.red;
            ui.painter().rect_stroke(
                r.rect,
                theme::CONTROL_RADIUS,
                egui::Stroke::new(1.5, red),
                egui::StrokeKind::Outside,
            );
        }
        r
    });
    enter
}

/// The settings that restart the daemon.
fn format_rows(ui: &mut Ui, form: &mut Form, interfaces: &[InterfaceInfo]) {
    let p = theme::palette(ui);
    let Form { current, edited } = form;
    let choices = logic::interface_choices(interfaces, &[&current.interface, &edited.interface]);
    let changed = edited.interface != current.interface;
    let help = logic::wifi_warning(interfaces, &edited.interface)
        .map(|w| RichText::new(w).small().color(p.amber_ink));
    let width = (ui.available_width() * 0.55).clamp(CONTROL_WIDTH, 340.0);
    widgets::setting_row(ui, "Network interface", help, changed, |ui| {
        menu(ui, "interface", width, &mut edited.interface, &choices)
    });
    widgets::separator(ui);

    let rates: Vec<(u32, String)> = logic::rate_choices(current.sample_rate)
        .into_iter()
        .map(|r| (r, logic::rate_label(r)))
        .collect();
    let changed = edited.sample_rate != current.sample_rate;
    widgets::setting_row(ui, "Sample rate", None, changed, |ui| {
        let r = menu(ui, "rate", CONTROL_WIDTH, &mut edited.sample_rate, &rates);
        was(ui, changed, logic::rate_label(current.sample_rate));
        r
    });
    widgets::separator(ui);

    let bits: Vec<(u16, String)> = logic::bits_choices(current.bits_per_sample)
        .into_iter()
        .map(|b| (b, logic::bits_label(b)))
        .collect();
    let changed = edited.bits_per_sample != current.bits_per_sample;
    widgets::setting_row(ui, "Bit depth", None, changed, |ui| {
        let r = menu(ui, "bits", CONTROL_WIDTH, &mut edited.bits_per_sample, &bits);
        was(ui, changed, logic::bits_label(current.bits_per_sample));
        r
    });
    widgets::separator(ui);

    let counts = |n: u16| -> Vec<(u16, String)> {
        logic::channel_choices(n).into_iter().map(|c| (c, c.to_string())).collect()
    };
    let changed = edited.rx_channels != current.rx_channels;
    let rx = counts(current.rx_channels);
    widgets::setting_row(ui, "Receive channels", None, changed, |ui| {
        let r = menu(ui, "rx", CONTROL_WIDTH, &mut edited.rx_channels, &rx);
        was(ui, changed, current.rx_channels.to_string());
        r
    });
    widgets::separator(ui);
    let changed = edited.tx_channels != current.tx_channels;
    let tx = counts(current.tx_channels);
    widgets::setting_row(ui, "Transmit channels", None, changed, |ui| {
        let r = menu(ui, "tx", CONTROL_WIDTH, &mut edited.tx_channels, &tx);
        was(ui, changed, current.tx_channels.to_string());
        r
    });
}

/// A pop-up menu over `choices`, showing the label of `value`.
fn menu<T: PartialEq + Clone>(
    ui: &mut Ui,
    id: &str,
    width: f32,
    value: &mut T,
    choices: &[(T, String)],
) -> egui::Response {
    let selected = choices
        .iter()
        .find(|(v, _)| v == value)
        .map(|(_, label)| label.clone())
        .unwrap_or_default();
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (v, label) in choices {
                ui.selectable_value(value, v.clone(), label);
            }
        })
        .response
}

/// The value before the edit, beside a changed control.
fn was(ui: &mut Ui, changed: bool, old: String) {
    if changed {
        ui.label(theme::caption(format!("was {old}")));
    }
}

/// Applies the edit: at once, or after the confirmation when it interrupts
/// audio or other devices' subscriptions.
pub fn request_apply(app: &mut App) {
    let up = matches!(app.connection, Connection::Up(_));
    let Some(form) = &app.form else { return };
    let change = logic::change(&form.current, &form.edited);
    if !up
        || app.applying
        || change.is_empty()
        || logic::edited_name_problem(&form.current.name, &form.edited.name).is_some()
    {
        return;
    }
    let receivers = logic::receivers(app.device());
    match logic::confirmation(&change, &form.current, &receivers, &app.interfaces) {
        Some(confirmation) => {
            app.confirm = Some((change, confirmation));
            app.confirm_opened = true;
        }
        None => app.apply(change),
    }
}

/// The bar under the Settings tab: what is pending or what became of the
/// last change on the left, Revert and Apply on the right. It stays in
/// view at any window size.
pub fn action_bar(app: &mut App, ui: &mut Ui) {
    let Some(form) = &app.form else { return };
    let up = matches!(app.connection, Connection::Up(_));
    let change = logic::change(&form.current, &form.edited);
    let name_problem = logic::edited_name_problem(&form.current.name, &form.edited.name);
    let receivers = logic::receivers(app.device());
    let confirmation = if change.is_empty() || name_problem.is_some() {
        None
    } else {
        logic::confirmation(&change, &form.current, &receivers, &app.interfaces)
    };
    let lines = logic::change_lines(&form.current, &change, &app.interfaces);
    let can_apply = up && !app.applying && !change.is_empty() && name_problem.is_none();
    let can_revert = !app.applying && !change.is_empty();
    let (mut apply, mut revert) = (false, false);
    egui::Sides::new().shrink_left().spacing(16.0).show(
        ui,
        |ui| status_line(ui, app, &lines, name_problem, confirmation.as_ref()),
        |ui| {
            let label = if confirmation.is_some() { "Apply…" } else { "Apply" };
            let button = widgets::primary_button(ui, label, can_apply);
            if ui.add_enabled(can_apply, button).clicked() {
                apply = true;
            }
            let button = egui::Button::new("Revert").min_size(vec2(84.0, 26.0));
            if ui.add_enabled(can_revert, button).on_hover_text("Discard the edits.").clicked() {
                revert = true;
            }
        },
    );
    if apply {
        request_apply(app);
    }
    if revert {
        if let Some(form) = &mut app.form {
            form.edited = form.current.clone();
        }
        app.outcome = None;
        app.worker.send(crate::worker::Command::Reload);
    }
}

/// The left of the action bar, in one or two lines.
fn status_line(
    ui: &mut Ui,
    app: &App,
    lines: &[String],
    name_problem: Option<&str>,
    confirmation: Option<&logic::Confirmation>,
) {
    let p = theme::palette(ui);
    let line = |ui: &mut Ui, light: Option<Light>, text: RichText| {
        ui.horizontal(|ui| {
            if let Some(light) = light {
                widgets::mark(ui, light, 14.0);
            }
            ui.add(Label::new(text).wrap());
        });
    };
    if app.applying {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Applying…");
        });
        return;
    }
    // A refusal explains why the edit is still pending: it comes first.
    if let Some(Outcome::Failed(text)) = &app.outcome {
        ui.vertical(|ui| {
            line(ui, Some(Light::Red), RichText::new(text).color(p.red_ink));
            if !lines.is_empty() {
                ui.label(theme::caption("The edit stays here: change it, or Revert."));
            }
        });
        return;
    }
    if !lines.is_empty() {
        if let Some(problem) = name_problem {
            line(
                ui,
                Some(Light::Red),
                RichText::new(format!("{problem} Fix the name to apply.")).color(p.red_ink),
            );
            return;
        }
        let fields: Vec<&str> = lines.iter().filter_map(|l| l.split(':').next()).collect();
        let n = lines.len();
        let what = format!(
            "{n} change{} not applied: {}.",
            if n == 1 { "" } else { "s" },
            fields.join(", ")
        );
        ui.vertical(|ui| {
            ui.add(Label::new(theme::headline(what)).wrap());
            if let Some(c) = confirmation {
                ui.add(Label::new(RichText::new(&c.cost).color(p.amber_ink)).wrap());
            }
        });
        return;
    }
    match &app.outcome {
        Some(Outcome::Saved { text, .. }) => line(ui, Some(Light::Green), RichText::new(text)),
        Some(Outcome::ToDo(text)) => line(ui, Some(Light::Amber), RichText::new(text)),
        Some(Outcome::Restarting { since }) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add(
                    Label::new(format!(
                        "Restarting OpenVirtualSoundcard · {}",
                        logic::ago_label(since.elapsed())
                    ))
                    .wrap(),
                );
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
        Some(Outcome::Back { after, .. }) => line(
            ui,
            Some(Light::Green),
            RichText::new(format!(
                "Back after {}. Check that the console receives the returns.",
                logic::ago_label(*after)
            )),
        ),
        Some(Outcome::Failed(text)) => {
            line(ui, Some(Light::Red), RichText::new(text).color(p.red_ink))
        }
        None => {
            ui.label(theme::caption("No changes. Settings are kept across restarts."));
        }
    }
}

/// The confirmation before a change that interrupts audio or other
/// devices' subscriptions: what changes, what it costs, and Cancel first.
pub fn confirm_modal(app: &mut App, ctx: &egui::Context) {
    let Some((change, c)) = &app.confirm else { return };
    let (change, c) = (change.clone(), c.clone());
    let focus_cancel = std::mem::take(&mut app.confirm_opened);
    let mut answer = None;
    let modal = egui::Modal::new(Id::new("confirm-change"))
        .frame(Frame::popup(&ctx.global_style()).corner_radius(12).inner_margin(20))
        .show(ctx, |ui| {
            let p = theme::palette(ui);
            ui.set_width(420.0);
            ui.horizontal(|ui| {
                widgets::mark(ui, Light::Amber, 22.0);
                ui.label(theme::semibold(c.title, 16.0));
            });
            ui.add_space(8.0);
            Frame::new()
                .fill(p.code)
                .corner_radius(theme::CONTROL_RADIUS)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in &c.changes {
                        ui.add(Label::new(theme::headline(line)).wrap());
                    }
                });
            ui.add_space(8.0);
            ui.add(Label::new(theme::headline(&c.cost)).wrap());
            ui.add(Label::new(RichText::new(&c.text).color(p.text2)).wrap());
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let w = 96.0;
                ui.add_space(
                    (ui.available_width() - 2.0 * w - ui.spacing().item_spacing.x).max(0.0),
                );
                let cancel = ui.add(egui::Button::new("Cancel").min_size(vec2(w, 26.0)));
                if focus_cancel {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    answer = Some(false);
                }
                let action = widgets::destructive_button(ui, c.button).min_size(vec2(w, 26.0));
                if ui.add(action).clicked() {
                    answer = Some(true);
                }
            });
        });
    if answer.is_none() && modal.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            app.confirm = None;
            app.apply(change);
        }
        Some(false) => app.confirm = None,
        None => {}
    }
}
