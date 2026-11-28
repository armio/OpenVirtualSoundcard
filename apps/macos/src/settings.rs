//! The Settings tab: the device's settings in cards, a bar with Apply that
//! stays in view, and the confirmation before a change that interrupts
//! audio or other devices' subscriptions.

use std::time::Instant;

use gpui::{
    AnyElement, Context, Corner, Div, IntoElement, ParentElement, SharedString, Stateful, Styled,
    Window, anchored, deferred, div, point, prelude::*, px,
};
use ovsc_control::Settings;

use crate::logic::{self, Light};
use crate::model::{Connection, Outcome};
use crate::theme::{self, CONTROL_HEIGHT, CONTROL_RADIUS, Palette, SEMIBOLD, WHITE};
use crate::view::{AppView, Menu};
use crate::widgets::{self, Kind};

/// The width of the menus and the name field.
const CONTROL_WIDTH: f32 = 200.0;

pub fn show(
    view: &AppView,
    p: &Palette,
    window: &mut Window,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let model = &view.model;
    let up = matches!(model.connection, Connection::Up(_));
    let Some(form) = &model.form else {
        let body = if up {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(widgets::spinner("reading", 20.0, p.text2))
                .child("Reading the settings…")
        } else {
            div().text_color(p.text2).child("The settings appear once the app reaches the daemon.")
        };
        return div().w_full().pt(px(40.0)).flex().justify_center().child(body).into_any_element();
    };
    let usable = model.usable();
    let (current, edited) = (&form.current, &form.edited);

    // Device.
    let name_problem = logic::edited_name_problem(&current.name, &edited.name);
    let help = match name_problem {
        Some(problem) => div()
            .text_size(px(theme::SMALL))
            .text_color(p.red_ink)
            .child(problem)
            .into_any_element(),
        None => widgets::caption(
            "Up to 31 letters, digits and hyphens, shown in Dante Controller. Renaming breaks \
             other devices' subscriptions to this one.",
            p,
        )
        .into_any_element(),
    };
    let name = widgets::setting_row(
        p,
        "Name",
        Some(help),
        edited.name.trim() != current.name,
        div().w(px(CONTROL_WIDTH)).child(view.name_input.clone()),
    );
    let device = widgets::section(p, "Device", None, name);

    // The settings that restart the daemon.
    let wide = (f32::from(window.viewport_size().width) * 0.55 - 40.0).clamp(CONTROL_WIDTH, 340.0);
    let interfaces = &model.interfaces;
    let interface_choices: Vec<(String, String)> =
        logic::interface_choices(interfaces, &[&current.interface, &edited.interface]);
    let interface = widgets::setting_row(
        p,
        "Network interface",
        logic::wifi_warning(interfaces, &edited.interface).map(|w| {
            div().text_size(px(theme::SMALL)).text_color(p.amber_ink).child(w).into_any_element()
        }),
        edited.interface != current.interface,
        select(
            view,
            Menu::Interface,
            wide,
            &edited.interface,
            interface_choices,
            |s, v| s.interface = v,
            usable,
            p,
            cx,
        ),
    );
    let secondary_choices: Vec<(String, String)> = logic::secondary_choices(
        interfaces,
        &[&current.secondary_interface, &edited.secondary_interface],
    );
    let secondary_help = match logic::secondary_problem(interfaces, edited) {
        Some(problem) => div()
            .text_size(px(theme::SMALL))
            .text_color(p.red_ink)
            .child(problem)
            .into_any_element(),
        None => widgets::caption(
            "Dante redundancy: a second Ethernet adapter on the secondary network. Flows with \
             redundant devices then run on both networks.",
            p,
        )
        .into_any_element(),
    };
    let secondary = widgets::setting_row(
        p,
        "Secondary network",
        Some(secondary_help),
        edited.secondary_interface != current.secondary_interface,
        select(
            view,
            Menu::Secondary,
            wide,
            &edited.secondary_interface,
            secondary_choices,
            |s, v| s.secondary_interface = v,
            usable,
            p,
            cx,
        ),
    );
    let rates = logic::rate_choices(current.sample_rate)
        .into_iter()
        .map(|r| (r, logic::rate_label(r)))
        .collect();
    let rate = with_was(
        select(
            view,
            Menu::Rate,
            CONTROL_WIDTH,
            &edited.sample_rate,
            rates,
            |s, v| s.sample_rate = v,
            usable,
            p,
            cx,
        ),
        (edited.sample_rate != current.sample_rate).then(|| logic::rate_label(current.sample_rate)),
        p,
    );
    let rate = widgets::setting_row(
        p,
        "Sample rate",
        None,
        edited.sample_rate != current.sample_rate,
        rate,
    );
    let bits_choices = logic::bits_choices(current.bits_per_sample)
        .into_iter()
        .map(|b| (b, logic::bits_label(b)))
        .collect();
    let bits = with_was(
        select(
            view,
            Menu::Bits,
            CONTROL_WIDTH,
            &edited.bits_per_sample,
            bits_choices,
            |s, v| s.bits_per_sample = v,
            usable,
            p,
            cx,
        ),
        (edited.bits_per_sample != current.bits_per_sample)
            .then(|| logic::bits_label(current.bits_per_sample)),
        p,
    );
    let bits = widgets::setting_row(
        p,
        "Bit depth",
        None,
        edited.bits_per_sample != current.bits_per_sample,
        bits,
    );
    let counts = |n: u16| -> Vec<(u16, String)> {
        logic::channel_choices(n).into_iter().map(|c| (c, c.to_string())).collect()
    };
    let rx = with_was(
        select(
            view,
            Menu::Rx,
            CONTROL_WIDTH,
            &edited.rx_channels,
            counts(current.rx_channels),
            |s, v| s.rx_channels = v,
            usable,
            p,
            cx,
        ),
        (edited.rx_channels != current.rx_channels).then(|| current.rx_channels.to_string()),
        p,
    );
    let rx = widgets::setting_row(
        p,
        "Receive channels",
        None,
        edited.rx_channels != current.rx_channels,
        rx,
    );
    let tx = with_was(
        select(
            view,
            Menu::Tx,
            CONTROL_WIDTH,
            &edited.tx_channels,
            counts(current.tx_channels),
            |s, v| s.tx_channels = v,
            usable,
            p,
            cx,
        ),
        (edited.tx_channels != current.tx_channels).then(|| current.tx_channels.to_string()),
        p,
    );
    let tx = widgets::setting_row(
        p,
        "Transmit channels",
        None,
        edited.tx_channels != current.tx_channels,
        tx,
    );
    let format = widgets::section(
        p,
        "Audio format and network",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(widgets::caption(
                "Changing any of these restarts OpenVirtualSoundcard: audio stops for 10–20 s in \
                 both directions.",
                p,
            ))
            .child(widgets::separator(p))
            .child(interface)
            .child(widgets::separator(p))
            .child(secondary)
            .child(widgets::separator(p))
            .child(rate)
            .child(widgets::separator(p))
            .child(bits)
            .child(widgets::separator(p))
            .child(rx)
            .child(widgets::separator(p))
            .child(tx),
    );

    // Latency.
    let latencies = logic::latency_choices(current.latency_ms)
        .into_iter()
        .map(|ms| (LatencyChoice(ms), logic::latency_label(ms)))
        .collect();
    let latency_changed = edited.latency_ms != current.latency_ms;
    let latency = with_was(
        select(
            view,
            Menu::Latency,
            CONTROL_WIDTH,
            &LatencyChoice(edited.latency_ms),
            latencies,
            |s, v| s.latency_ms = v.0,
            usable,
            p,
            cx,
        ),
        latency_changed.then(|| logic::latency_label(current.latency_ms)),
        p,
    );
    let latency = widgets::section(
        p,
        "Receive latency",
        None,
        widgets::setting_row(
            p,
            "Latency",
            Some(
                widgets::caption(
                    "How long received audio waits before it plays: more is safer on a busy \
                     network. Changing it stops the inputs for about a second; the returns carry \
                     on.",
                    p,
                )
                .into_any_element(),
            ),
            latency_changed,
            latency,
        ),
    );

    // The show lock applies at once, and stays usable while other edits
    // are pending.
    let on = edited.lock_remote_settings;
    let switch = if model.applying_lock() {
        widgets::spinner("lock-spinner", 18.0, p.text2).into_any_element()
    } else {
        widgets::toggle("show-lock", on, usable, p)
            .when(usable, |t| {
                t.on_click(cx.listener(move |view, _, window, cx| {
                    view.model.set_lock(!on);
                    view.after_edit(window, cx);
                }))
            })
            .into_any_element()
    };
    let lock = widgets::section(
        p,
        "Show lock",
        None,
        widgets::setting_row(
            p,
            "Refuse changes from Dante Controller",
            Some(
                widgets::caption(format!("{} Applies at once.", logic::LOCK_HELP), p)
                    .into_any_element(),
            ),
            false,
            switch,
        ),
    );

    div()
        .w_full()
        .flex()
        .flex_col()
        .child(device)
        .child(format)
        .child(latency)
        .child(lock)
        .into_any_element()
}

/// A latency in a menu: equal when the labels would be.
#[derive(Clone, Copy)]
struct LatencyChoice(f64);

impl PartialEq for LatencyChoice {
    fn eq(&self, other: &Self) -> bool {
        (self.0 - other.0).abs() < 1e-9
    }
}

/// A control with the value before the edit beside it.
fn with_was(control: impl IntoElement, was: Option<String>, p: &Palette) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .when_some(was, |d, was| d.child(widgets::caption(format!("was {was}"), p)))
        .child(control)
}

/// A pop-up menu over `choices`, showing the label of `value`. Picking a
/// choice edits the form through `set`.
#[allow(clippy::too_many_arguments)]
fn select<T: Clone + PartialEq + 'static>(
    view: &AppView,
    menu: Menu,
    width: f32,
    value: &T,
    choices: Vec<(T, String)>,
    set: fn(&mut Settings, T),
    enabled: bool,
    p: &Palette,
    cx: &mut Context<AppView>,
) -> Div {
    let label = choices
        .iter()
        .find(|(v, _)| v == value)
        .map(|(_, label)| label.clone())
        .unwrap_or_default();
    let open = enabled && view.open_menu == Some(menu);
    let id = format!("{menu:?}");
    let button = div()
        .id(SharedString::from(format!("menu-{id}")))
        .w(px(width))
        .h(px(CONTROL_HEIGHT))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .pl(px(10.0))
        .pr(px(6.0))
        .rounded(px(CONTROL_RADIUS))
        .bg(if open { p.control_press } else { p.control })
        .child(div().min_w_0().truncate().child(label))
        .child(widgets::chevron(p.text2))
        .when(!enabled, |b| b.opacity(0.45))
        .when(enabled, |b| {
            b.cursor_pointer()
                .hover(|s| s.bg(p.control_hover))
                .on_click(cx.listener(move |view, _, _, cx| view.toggle_menu(menu, cx)))
        });
    let mut wrapper = div().relative().child(button);
    if open {
        let items = choices.into_iter().enumerate().map(|(i, (choice, label))| {
            let selected = &choice == value;
            div()
                .id(SharedString::from(format!("{id}-{i}")))
                .h(px(24.0))
                .mx(px(4.0))
                .pl(px(6.0))
                .pr(px(12.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(5.0))
                .cursor_pointer()
                .hover(|s| s.bg(p.accent).text_color(WHITE))
                .child(div().w(px(14.0)).flex_shrink_0().child(if selected { "✓" } else { "" }))
                .child(div().whitespace_nowrap().child(label))
                .on_click(cx.listener(move |view, _, window, cx| {
                    let choice = choice.clone();
                    view.model.edit(|s| set(s, choice));
                    view.open_menu = None;
                    view.after_edit(window, cx);
                }))
        });
        let list = div()
            .id(SharedString::from(format!("{id}-list")))
            .min_w(px(width))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .py(px(4.0))
            .flex()
            .flex_col()
            .bg(p.popup)
            .border_1()
            .border_color(p.card_edge)
            .rounded(px(8.0))
            .shadow(vec![gpui::BoxShadow {
                color: p.shadow.into(),
                offset: point(px(0.0), px(6.0)),
                blur_radius: px(18.0),
                spread_radius: px(0.0),
            }])
            .children(items)
            .on_mouse_down_out(cx.listener(|view, _, _, cx| view.close_menu(cx)));
        wrapper = wrapper.child(
            div().absolute().left_0().top(px(CONTROL_HEIGHT + 4.0)).child(
                deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.0))
                        .child(list),
                )
                .with_priority(1),
            ),
        );
    }
    wrapper
}

/// The bar under the Settings tab: what is pending or what became of the
/// last change on the left, Revert and Apply on the right. It stays in
/// view at any window size.
pub fn action_bar(
    view: &AppView,
    p: &Palette,
    now: Instant,
    cx: &mut Context<AppView>,
) -> Stateful<Div> {
    let model = &view.model;
    let confirmation = model.pending_confirmation();
    let can_apply = model.can_apply();
    let can_revert = !model.applying && model.pending();
    let label = if confirmation.is_some() { "Apply…" } else { "Apply" };
    let apply = widgets::button("apply", label, Kind::Primary, can_apply, p).when(can_apply, |b| {
        b.on_click(cx.listener(|view, _, window, cx| {
            view.model.request_apply();
            view.after_edit(window, cx);
        }))
    });
    let revert =
        widgets::button("revert", "Revert", Kind::Plain, can_revert, p).when(can_revert, |b| {
            b.tooltip(widgets::Tooltip::text("Discard the edits.")).on_click(cx.listener(
                |view, _, window, cx| {
                    view.model.revert();
                    view.after_edit(window, cx);
                },
            ))
        });
    div()
        .id("action-bar")
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .px(px(theme::GUTTER))
        .pt(px(10.0))
        .pb(px(12.0))
        .min_h(px(56.0))
        .bg(p.footer)
        .border_t_1()
        .border_color(p.separator)
        .child(div().flex_1().min_w_0().child(status_line(view, confirmation.as_ref(), p, now)))
        .child(div().flex_shrink_0().flex().gap(px(8.0)).child(revert).child(apply))
}

/// The left of the action bar, in one or two lines.
fn status_line(
    view: &AppView,
    confirmation: Option<&logic::Confirmation>,
    p: &Palette,
    now: Instant,
) -> AnyElement {
    let model = &view.model;
    let line = |light: Option<Light>, text: String, color: theme::Color| {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .when_some(light, |d, light| d.child(widgets::mark(light, 14.0, p)))
            .child(div().flex_1().min_w_0().text_color(color).child(text))
            .into_any_element()
    };
    if model.applying {
        return div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(widgets::spinner("applying", 14.0, p.text2))
            .child("Applying…")
            .into_any_element();
    }
    let Some(form) = &model.form else { return div().into_any_element() };
    let (change, name_problem) = model.pending_change().unwrap_or_default();
    let lines = logic::change_lines(&form.current, &change, &model.interfaces);
    // A refusal explains why the edit is still pending: it comes first.
    if let Some(Outcome::Failed { text, kept }) = &model.outcome {
        return div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(line(Some(Light::Red), text.clone(), p.red_ink))
            .when(*kept && !lines.is_empty(), |d| {
                d.child(widgets::caption("The edit stays here: change it, or Revert.", p))
            })
            .into_any_element();
    }
    if !lines.is_empty() {
        if let Some(problem) = name_problem {
            return line(Some(Light::Red), format!("{problem} Fix the name to apply."), p.red_ink);
        }
        let fields: Vec<&str> = lines.iter().filter_map(|l| l.split(':').next()).collect();
        let n = lines.len();
        let what = format!(
            "{n} change{} not applied: {}.",
            if n == 1 { "" } else { "s" },
            fields.join(", ")
        );
        return div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(widgets::headline(what))
            .when_some(confirmation, |d, c| {
                d.child(div().text_color(p.amber_ink).child(c.cost.clone()))
            })
            .into_any_element();
    }
    match &model.outcome {
        Some(Outcome::Saved { text, .. }) => line(Some(Light::Green), text.clone(), p.text),
        Some(Outcome::ToDo(text)) => line(Some(Light::Amber), text.clone(), p.text),
        Some(Outcome::Restarting { since, .. }) if model.restarting(now) => div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(widgets::spinner("restarting", 14.0, p.text2))
            .child(format!(
                "Restarting OpenVirtualSoundcard · {}",
                logic::ago_label(now.saturating_duration_since(*since))
            ))
            .into_any_element(),
        // The restart's time is up and audio is still down.
        Some(Outcome::Restarting { since, .. }) => line(
            Some(Light::Red),
            format!(
                "Audio is not back {} after the restart: the Status tab says why.",
                logic::ago_label(now.saturating_duration_since(*since))
            ),
            p.red_ink,
        ),
        Some(Outcome::Back { after, .. }) => line(
            Some(Light::Green),
            format!(
                "Back after {}. Check that the console receives the returns.",
                logic::ago_label(*after)
            ),
            p.text,
        ),
        Some(Outcome::Failed { text, .. }) => line(Some(Light::Red), text.clone(), p.red_ink),
        None => {
            widgets::caption("No changes. Settings are kept across restarts.", p).into_any_element()
        }
    }
}

/// The confirmation before a change that interrupts audio or other
/// devices' subscriptions: what changes, what it costs, and Cancel as the
/// default (Return and Escape).
pub fn confirm_dialog(view: &AppView, p: &Palette, cx: &mut Context<AppView>) -> Stateful<Div> {
    let c = view.model.confirm.as_ref().map(|(_, c)| c.clone()).expect("a confirmation is up");
    let cancel = widgets::button("confirm-cancel", "Cancel", Kind::Plain, true, p)
        .min_w(px(96.0))
        .border_2()
        .border_color(p.focus)
        .on_click(cx.listener(|view, _, window, cx| {
            view.model.answer_confirmation(false);
            view.after_edit(window, cx);
        }));
    let go = widgets::button("confirm-go", c.button, Kind::Destructive, true, p)
        .min_w(px(96.0))
        .on_click(cx.listener(|view, _, window, cx| {
            view.model.answer_confirmation(true);
            view.after_edit(window, cx);
        }));
    widgets::scrim(p).track_focus(view.modal_focus()).key_context("Modal").child(
        widgets::dialog(p, 440.0)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::mark(Light::Amber, 22.0, p))
                    .child(div().text_size(px(16.0)).font_weight(SEMIBOLD).child(c.title)),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(theme::FIELD_RADIUS))
                    .bg(p.code)
                    .children(c.changes.iter().map(|line| widgets::headline(line.clone()))),
            )
            .child(widgets::headline(c.cost.clone()))
            .child(div().text_color(p.text2).child(c.text.clone()))
            .child(div().pt(px(6.0)).flex().justify_end().gap(px(8.0)).child(cancel).child(go)),
    )
}
