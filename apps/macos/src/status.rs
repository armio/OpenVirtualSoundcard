//! The Status tab: what is worth a look, the inputs and the returns, then
//! the clock and the counters, the device and the driver. Outages are
//! pinned above both tabs (see [`crate::view`]); this tab shows the rest.

use std::time::Instant;

use gpui::{
    AnyElement, Context, Div, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window,
    div, prelude::*, px,
};
use ovsc_control::{DeviceStatus, Redundancy, Status};

use crate::logic::{self, Light, PacketWindow, Problem};
use crate::model::Connection;
use crate::theme::{self, Color, Palette, SEMIBOLD};
use crate::view::AppView;
use crate::widgets::{self, Severity, Width};

pub fn show(
    view: &AppView,
    p: &Palette,
    now: Instant,
    wide: bool,
    window: &mut Window,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let model = &view.model;
    let Connection::Up(status) = &model.connection else {
        return empty_state(view, p, now, cx).into_any_element();
    };
    let mut page = div().w_full().flex().flex_col();
    let (_, other) = logic::split_warnings(status);
    let (problems, news) = logic::problems_and_news(other);
    if !problems.is_empty() {
        let heading = match problems.len() {
            1 => "Warning".to_owned(),
            n => format!("{n} warnings"),
        };
        let mut notice = widgets::notice("warnings", p, Severity::Warning, heading);
        for warning in &problems {
            notice = notice.child(div().pt(px(4.0)).child(view.warning(warning, p, cx)));
        }
        page = page.child(div().pt(px(theme::SECTION_GAP - 6.0)).child(notice));
    }
    if let Some(device) = &status.device {
        let (light, summary) = logic::rx_summary(&device.rx_channels);
        page = page.child(widgets::section(
            p,
            "Receive channels",
            Some((light, summary)),
            rx_table(device, p),
        ));
        let (light, summary) = logic::tx_summary(device);
        page = page.child(widgets::section(
            p,
            "Transmit channels",
            Some((light, summary)),
            tx_table(device, p),
        ));
        if let Some(r) = &device.redundancy {
            page = page.child(networks_section(device, r, p, window));
        }
    }
    let packets = match &status.device {
        Some(device) => {
            packets_section(status, device, &model.packets, p, window).into_any_element()
        }
        None => widgets::section(
            p,
            "Packets",
            None,
            div().text_color(p.text2).child("Not on the network."),
        )
        .into_any_element(),
    };
    page = page.child(widgets::pair(wide, clock_section(status, p, window), packets)).child(
        widgets::pair(wide, device_section(status, p, window), driver_section(status, p, window)),
    );
    if !news.is_empty() || !status.earlier.is_empty() {
        let body = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(news.iter().map(|item| div().child(item.to_string())))
            .when(!news.is_empty() && !status.earlier.is_empty(), |d| {
                d.child(widgets::separator(p))
            })
            .children(
                status.earlier.iter().map(|item| div().text_color(p.text2).child(item.clone())),
            );
        page = page.child(widgets::section(p, "Changes from controllers", None, body));
    }
    if !status.driver.detail.is_empty() {
        page = page.child(diagnostics(view, &status.driver.detail, p, cx));
    }
    page.into_any_element()
}

/// Instead of the tab, when there is no status: what is wrong, what it
/// means for the audio, and the way back.
fn empty_state(view: &AppView, p: &Palette, now: Instant, cx: &mut Context<AppView>) -> Div {
    let waiting = |text: &'static str| {
        div()
            .w_full()
            .pt(px(48.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .child(widgets::spinner("waiting", 22.0, p.text2))
            .child(div().text_size(px(15.0)).font_weight(SEMIBOLD).child(text))
    };
    match &view.model.connection {
        Connection::Connecting => waiting("Connecting to OpenVirtualSoundcard…"),
        Connection::Down(_) if view.model.restarting(now) => {
            waiting("Waiting for OpenVirtualSoundcard to come back…")
        }
        Connection::Down(problem) => {
            let light = match problem {
                Problem::NotRunning | Problem::NoAnswer | Problem::Lost => Light::Red,
                _ => Light::Amber,
            };
            let body = div()
                .w_full()
                .max_w(px(600.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .child(widgets::mark(light, 36.0, p))
                .child(
                    div()
                        .pt(px(4.0))
                        .text_size(px(18.0))
                        .font_weight(SEMIBOLD)
                        .text_center()
                        .child(problem.title()),
                )
                .when_some(problem.detail(), |d, detail| {
                    d.child(div().w_full().text_center().child(widgets::rich_text(
                        &detail,
                        p.text,
                        FontWeight::NORMAL,
                        p,
                    )))
                })
                .child(div().w_full().text_center().text_color(p.text2).child(problem.hint()))
                .when_some(problem.command(), |d, command| {
                    let copy = view.copy_button("copy-start", command, p, cx);
                    d.child(
                        div().w_full().pt(px(6.0)).child(widgets::command_row(p, command, copy)),
                    )
                })
                .child(widgets::caption("The app tries again every second.", p).pt(px(8.0)));
            div().w_full().pt(px(40.0)).flex().flex_col().items_center().child(body)
        }
        Connection::Up(_) => div(),
    }
}

fn rx_table(device: &DeviceStatus, p: &Palette) -> AnyElement {
    if device.rx_channels.is_empty() {
        return div().text_color(p.text2).child("No receive channels.").into_any_element();
    }
    let columns = [
        ("", Width::Fixed(14.0)),
        ("#", Width::Fixed(22.0)),
        ("Channel", Width::Share(1.0)),
        ("Source", Width::Share(1.5)),
        ("Subscription", Width::Share(1.3)),
    ];
    let rows = device
        .rx_channels
        .iter()
        .enumerate()
        .map(|(i, channel)| {
            let light = logic::channel_light(channel);
            let state = logic::subscription_label(channel);
            let mark = if light == Light::Grey {
                div().text_color(p.text3).child(logic::NONE).into_any_element()
            } else {
                div()
                    .id(SharedString::from(format!("rx-mark-{i}")))
                    .child(widgets::mark(light, 12.0, p))
                    .tooltip(widgets::Tooltip::text(state.clone()))
                    .into_any_element()
            };
            let (source, source_color) = match &channel.source {
                Some(source) => (source.clone(), p.text),
                None => (logic::NONE.to_owned(), p.text3),
            };
            let state_color = match light {
                Light::Red | Light::Amber => p.ink(light),
                Light::Grey => p.text3,
                Light::Green => p.text,
            };
            vec![
                mark,
                cell((i + 1).to_string(), p.text2),
                cell(channel.name.clone(), p.text),
                cell(source, source_color),
                cell(state, state_color),
            ]
        })
        .collect();
    widgets::table(p, &columns, rows).into_any_element()
}

/// A table cell's text, cut short with an ellipsis.
fn cell(text: impl Into<SharedString>, color: Color) -> AnyElement {
    div().min_w_0().truncate().text_color(color).child(text.into()).into_any_element()
}

fn tx_table(device: &DeviceStatus, p: &Palette) -> AnyElement {
    if device.tx_flows.is_empty() {
        return div()
            .text_color(p.text2)
            .child("No device receives from this one: patch the returns in Dante Controller.")
            .into_any_element();
    }
    let columns = [
        ("Received by", Width::Share(1.4)),
        ("Address", Width::Share(1.0)),
        ("Channels", Width::Share(1.0)),
    ];
    let rows = device
        .tx_flows
        .iter()
        .map(|flow| {
            vec![
                cell(logic::flow_receiver(flow), p.text),
                cell(logic::flow_address_label(flow), p.text2),
                cell(logic::channels_label(&flow.channels), p.text),
            ]
        })
        .collect();
    widgets::table(p, &columns, rows).into_any_element()
}

/// A value in name and value rows.
fn value(text: impl Into<SharedString>, color: Color) -> AnyElement {
    div().text_color(color).child(text.into()).into_any_element()
}

/// A number in name and value rows: monospaced, so that it does not jump.
fn number(text: impl Into<SharedString>, color: Color) -> AnyElement {
    div()
        .font_family(theme::mono())
        .text_size(px(12.0))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// The two networks of a redundant Dante system: which carry the audio and
/// the clock.
fn networks_section(device: &DeviceStatus, r: &Redundancy, p: &Palette, window: &Window) -> Div {
    let (light, summary) = logic::networks_summary(r);
    let rows: Vec<(&'static str, AnyElement)> = logic::network_rows(device, r)
        .into_iter()
        .map(|(name, text, light)| {
            let v = match light {
                Light::Amber | Light::Red => value(text, p.ink(light)),
                Light::Grey => number(text, p.text2),
                Light::Green => value(text, p.text),
            };
            (name, v)
        })
        .collect();
    let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    let width = widgets::names_width(&names, window);
    widgets::section(p, "Networks", Some((light, summary)), widgets::rows(p, width, rows))
}

fn clock_section(status: &Status, p: &Palette, window: &Window) -> Div {
    let (light, _) = logic::clock_light(&status.clock);
    let rows: Vec<(&'static str, AnyElement)> = logic::clock_rows(&status.clock)
        .into_iter()
        .map(|(name, text)| {
            let v = match name {
                "State" if light != Light::Green => value(text, p.ink(light)),
                "Offset" | "Network delay" | "Drift" => number(text, p.text),
                _ => value(text, p.text),
            };
            (name, v)
        })
        .collect();
    let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    let width = widgets::names_width(&names, window);
    widgets::section(p, "Clock", None, widgets::rows(p, width, rows))
}

fn packets_section(
    status: &Status,
    device: &DeviceStatus,
    window_30s: &PacketWindow,
    p: &Palette,
    window: &Window,
) -> Div {
    let counts = device.packets;
    let (late, lost, without_audio) = window_30s.rises();
    let counter = |total: u64, rise: u64| {
        if rise > 0 {
            number(
                format!("{} · +{} in 30 s", logic::count_label(total), logic::count_label(rise)),
                p.amber_ink,
            )
        } else {
            number(logic::count_label(total), p.text)
        }
    };
    let underruns = if status.driver.io_running {
        counter(counts.tx_underruns, without_audio)
    } else {
        value(format!("{} (no application playing)", logic::NONE), p.text2)
    };
    let rows = vec![
        ("Received", number(logic::count_label(counts.rx), p.text)),
        ("Arrived too late", counter(counts.rx_late, late)),
        ("Lost", counter(counts.rx_lost, lost)),
        ("Sent", number(logic::count_label(counts.tx), p.text)),
        ("Sent without audio", underruns),
    ];
    let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    let width = widgets::names_width(&names, window);
    widgets::section(
        p,
        "Packets",
        Some((Light::Grey, "since the engine started".to_owned())),
        widgets::rows(p, width, rows),
    )
}

fn device_section(status: &Status, p: &Palette, window: &Window) -> Div {
    let version = format!("App {} · daemon {}", env!("CARGO_PKG_VERSION"), status.version);
    let body = match &status.device {
        Some(d) => {
            let interface = match (d.interface.is_empty(), d.ip.is_empty()) {
                (false, false) => format!("{} ({})", d.interface, d.ip),
                (false, true) => d.interface.clone(),
                (true, false) => d.ip.clone(),
                (true, true) => logic::NONE.to_owned(),
            };
            let rows = vec![
                ("Interface", value(interface, p.text)),
                ("Sample rate", value(logic::rate_label(d.sample_rate), p.text)),
                ("Bit depth", value(logic::bits_label(d.bits_per_sample), p.text)),
                ("Receive latency", value(logic::latency_label(d.latency_ms), p.text)),
                ("Show lock", value(logic::lock_label(d.remote_settings_locked), p.text)),
                ("Version", value(version, p.text2)),
            ];
            let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
            let width = widgets::names_width(&names, window);
            widgets::rows(p, width, rows)
        }
        None => {
            let width = widgets::names_width(&["Version"], window);
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().text_color(p.text2).child("Not on the network: see above."))
                .child(widgets::rows(p, width, vec![("Version", value(version, p.text2))]))
        }
    };
    widgets::section(p, "Device", None, body)
}

fn driver_section(status: &Status, p: &Palette, window: &Window) -> Div {
    let driver = &status.driver;
    let yes_no = |b: bool| value(if b { "Yes" } else { "No" }, p.text);
    let loaded = if driver.faulted {
        value("Failed: restart Core Audio", p.red_ink)
    } else if driver.connected {
        yes_no(true)
    } else {
        value("No", p.red_ink)
    };
    let audio = logic::audio_lamp(status);
    let audio_text = match audio.light {
        Light::Green | Light::Grey => value(audio.word.clone(), p.text),
        light => value(audio.word.clone(), p.ink(light)),
    };
    let rows = vec![
        ("Loaded by Core Audio", loaded),
        ("Used by an application", yes_no(driver.io_running)),
        ("Audio", audio_text),
    ];
    let names: Vec<&str> = rows.iter().map(|(n, _)| *n).collect();
    let width = widgets::names_width(&names, window);
    widgets::section(p, "Core Audio driver", None, widgets::rows(p, width, rows))
}

/// The driver's status line, folded away: for a bug report.
fn diagnostics(view: &AppView, detail: &str, p: &Palette, cx: &mut Context<AppView>) -> Div {
    let open = view.diagnostics_open;
    let toggle = div()
        .id("diagnostics")
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .child(div().w(px(10.0)).text_color(p.text2).child(if open { "▾" } else { "▸" }))
        .child(div().font_weight(SEMIBOLD).child("Diagnostics"))
        .on_click(cx.listener(|view, _, _, cx| {
            view.diagnostics_open = !view.diagnostics_open;
            cx.notify();
        }));
    div().w_full().flex().flex_col().gap(px(8.0)).pt(px(theme::SECTION_GAP)).child(toggle).when(
        open,
        |d| {
            d.child(widgets::caption("The driver's status, for a bug report.", p))
                .child(widgets::card(p).child(
                    div().font_family(theme::mono()).text_size(px(11.0)).child(detail.to_owned()),
                ))
                .child(div().flex().child(view.copy_button("copy-diagnostics", detail, p, cx)))
        },
    )
}
