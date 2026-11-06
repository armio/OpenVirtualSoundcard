//! The app's building blocks: status marks, cards, notices, warnings with
//! their commands, status tiles, the segmented control and settings rows.
//! Colours and sizes come from [`crate::theme`].

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Frame, Id, Layout, Margin, Painter,
    Pos2, Rect, Response, RichText, Sense, Shape, Stroke, TextStyle, Ui, Vec2, WidgetInfo,
    WidgetType, pos2, vec2,
};

use crate::logic::{self, Lamp, Light};
use crate::theme::{self, CARD_PAD, CARD_RADIUS, Palette};

/// Paints a light's mark: its shape says the state without the colour. A
/// filled circle with a check is fine, a triangle with a bar is worth a
/// look, an octagon with a bar is down, and a hollow ring is unknown or off.
pub fn paint_mark(painter: &Painter, center: Pos2, size: f32, light: Light, p: &Palette) {
    let r = size / 2.0;
    let color = p.fill(light);
    let on = Color32::WHITE;
    let thin = (size / 9.0).max(1.5);
    match light {
        Light::Green => {
            painter.circle_filled(center, r, color);
            if size >= 11.0 {
                let pts = [
                    center + vec2(-0.45, 0.02) * r,
                    center + vec2(-0.12, 0.33) * r,
                    center + vec2(0.45, -0.32) * r,
                ];
                painter.add(Shape::line(pts.to_vec(), Stroke::new(thin, on)));
            }
        }
        Light::Amber => {
            let h = r * 1.9;
            let top = center + vec2(0.0, -h * 0.55);
            let pts =
                vec![top, center + vec2(r * 1.05, h * 0.45), center + vec2(-r * 1.05, h * 0.45)];
            painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
            if size >= 11.0 {
                let dark = Color32::from_gray(25);
                painter.line_segment(
                    [center + vec2(0.0, -h * 0.18), center + vec2(0.0, h * 0.12)],
                    Stroke::new(thin, dark),
                );
                painter.circle_filled(center + vec2(0.0, h * 0.28), thin * 0.6, dark);
            }
        }
        Light::Red => {
            let pts = (0..8)
                .map(|i| {
                    let a = TAU * (i as f32 + 0.5) / 8.0;
                    center + vec2(a.cos(), a.sin()) * r * 1.04
                })
                .collect();
            painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
            if size >= 11.0 {
                painter.line_segment(
                    [center + vec2(-r * 0.5, 0.0), center + vec2(r * 0.5, 0.0)],
                    Stroke::new(thin * 1.2, on),
                );
            }
        }
        Light::Grey => {
            painter.circle_stroke(center, r - thin / 2.0, Stroke::new(thin, color));
        }
    }
}

/// A light's mark as a widget, `size` points wide.
pub fn mark(ui: &mut Ui, light: Light, size: f32) -> Response {
    let height = ui.text_style_height(&TextStyle::Body).max(size);
    let (rect, response) = ui.allocate_exact_size(vec2(size, height), Sense::hover());
    paint_mark(ui.painter(), rect.center(), size, light, theme::palette(ui));
    response
}

/// A titled card, with an optional one-line summary at the right of the
/// title: `Receive channels … 7 of 7 receiving`.
pub fn section<R>(
    ui: &mut Ui,
    title: &str,
    summary: Option<(Light, &str)>,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = theme::palette(ui);
    ui.add_space(theme::SECTION_GAP - ui.spacing().item_spacing.y);
    ui.horizontal(|ui| {
        ui.label(theme::headline(title));
        if let Some((light, text)) = summary {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let words = if light == Light::Grey { p.text2 } else { p.ink(light) };
                let marked = matches!(light, Light::Amber | Light::Red);
                // Leave the mark its room, or a long summary pushes it
                // over the title.
                let room = if marked { 11.0 + ui.spacing().item_spacing.x } else { 0.0 };
                ui.scope(|ui| {
                    ui.set_max_width((ui.available_width() - room).max(0.0));
                    ui.add(egui::Label::new(RichText::new(text).color(words)).truncate());
                });
                if marked {
                    mark(ui, light, 11.0);
                }
            });
        }
    });
    ui.add_space(theme::TITLE_GAP - ui.spacing().item_spacing.y);
    card(ui, add)
}

/// A plain card at the full width.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette(ui);
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(CARD_PAD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// How much a notice matters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// Audio is down now.
    Urgent,
    /// Worth a look.
    Warning,
    /// News, not a problem.
    Info,
    /// Something in progress, such as a restart.
    Progress,
}

impl Severity {
    fn light(self) -> Light {
        match self {
            Severity::Urgent => Light::Red,
            Severity::Warning | Severity::Progress => Light::Amber,
            Severity::Info => Light::Grey,
        }
    }
}

/// A notice: a tinted card with a coloured bar on its left edge, a mark,
/// and a heading that says in words how much it matters.
pub fn notice<R>(
    ui: &mut Ui,
    severity: Severity,
    heading: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = theme::palette(ui);
    let light = severity.light();
    let (fill, stroke) = match severity {
        Severity::Urgent => (theme::mix(p.card, p.red, p.tint + 0.06), Stroke::new(1.5, p.red)),
        Severity::Warning | Severity::Progress => (p.tint(Light::Amber), Stroke::new(1.0, p.amber)),
        Severity::Info => (p.card, Stroke::new(1.0, p.card_stroke)),
    };
    let inner = Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin { left: 16, right: CARD_PAD, top: 10, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if severity == Severity::Progress {
                    ui.spinner();
                } else {
                    mark(ui, light, 16.0);
                }
                ui.label(theme::semibold(heading, 14.0));
            });
            ui.add_space(2.0);
            add(ui)
        });
    // The bar on the left edge, inside the rounded corners.
    if severity != Severity::Info {
        let r = inner.response.rect;
        let bar = Rect::from_min_max(
            pos2(r.left() + 1.0, r.top() + 1.0),
            pos2(r.left() + 5.0, r.bottom() - 1.0),
        );
        let radius = CornerRadius { nw: CARD_RADIUS - 1, sw: CARD_RADIUS - 1, ne: 0, se: 0 };
        ui.painter().rect_filled(bar, radius, p.fill(light));
    }
    ui.add_space(4.0);
    inner.inner
}

/// A text with its backtick spans as code, laid out to wrap.
pub fn rich_text(ui: &Ui, text: &str, font: FontId, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let code_font = FontId::new(font.size - 1.0, FontFamily::Monospace);
    for (code, span) in logic::code_spans(text) {
        let format = if code {
            TextFormat {
                font_id: code_font.clone(),
                color,
                background: ui.visuals().code_bg_color,
                ..Default::default()
            }
        } else {
            TextFormat::simple(font.clone(), color)
        };
        job.append(span, 0.0, format);
    }
    job
}

/// What was copied last, and when: a Copy button says `Copied` for a moment.
#[derive(Default)]
pub struct Copied(Option<(String, Instant)>);

const COPIED_FOR: Duration = Duration::from_secs(2);

impl Copied {
    fn shows(&self, ui: &Ui, text: &str) -> bool {
        match &self.0 {
            Some((copied, at)) if copied == text => {
                let left = COPIED_FOR.saturating_sub(at.elapsed());
                if left.is_zero() {
                    false
                } else {
                    ui.ctx().request_repaint_after(left);
                    true
                }
            }
            _ => false,
        }
    }
}

/// A command in a framed box with a Copy button that confirms the copy.
pub fn command_row(ui: &mut Ui, command: &str, copied: &mut Copied) {
    let p = theme::palette(ui);
    Frame::new()
        .fill(p.code)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(command).monospace().color(p.text))
                            .wrap()
                            .selectable(true),
                    );
                },
                |ui| {
                    let done = copied.shows(ui, command);
                    let label = if done { "Copied" } else { "Copy" };
                    let button = ui
                        .add(egui::Button::new(label).min_size(vec2(72.0, 24.0)))
                        .on_hover_text("Copy the command, then paste it in Terminal.");
                    if button.clicked() {
                        ui.ctx().copy_text(command.to_owned());
                        copied.0 = Some((command.to_owned(), Instant::now()));
                        ui.ctx().request_repaint_after(COPIED_FOR);
                    }
                },
            );
        });
}

/// One warning from the daemon: its first sentence in semibold, the rest
/// below it, code as code, and each command it names in a row to copy.
pub fn warning(ui: &mut Ui, text: &str, copied: &mut Copied) {
    let p = theme::palette(ui);
    let (head, rest) = logic::warning_parts(text);
    let semibold = FontId::new(13.0, FontFamily::Name(theme::SEMIBOLD.into()));
    let body = FontId::new(13.0, FontFamily::Proportional);
    ui.add(egui::Label::new(rich_text(ui, head, semibold, p.text)).wrap());
    if !rest.is_empty() {
        ui.add(egui::Label::new(rich_text(ui, rest, body, p.text)).wrap());
    }
    for command in logic::commands(text) {
        command_row(ui, command, copied);
    }
}

/// A status tile: the mark, the title, the word, and what it means. Red
/// tiles are solid, so a fault changes the top of the window from across a
/// room; amber ones are tinted; the others stay quiet.
pub fn tile(ui: &mut Ui, title: &str, lamp: &Lamp, size: Vec2, large: bool) -> Response {
    let p = theme::palette(ui);
    let (fill, stroke, text, text2) = match lamp.light {
        Light::Red => (p.urgent, Stroke::NONE, Color32::WHITE, Color32::from_gray(235)),
        Light::Amber => (p.tint(Light::Amber), Stroke::new(1.0, p.amber), p.text, p.text),
        _ => (p.card, Stroke::new(1.0, p.card_stroke), p.text, p.text2),
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::focusable_noninteractive());
    let painter = ui.painter_at(rect);
    painter.rect(rect, CARD_RADIUS, fill, stroke, egui::StrokeKind::Inside);
    let pad = 12.0;
    let mark_size = if large { 22.0 } else { 18.0 };
    let mark_center =
        pos2(rect.left() + pad + mark_size / 2.0, rect.top() + pad + mark_size / 2.0 + 1.0);
    if lamp.light == Light::Red {
        // White on the solid red, so the shape still reads.
        let pal = Palette { red: Color32::WHITE, ..*p };
        paint_mark(&painter, mark_center, mark_size, lamp.light, &pal);
        // The bar inside the octagon, now in red.
        let r = mark_size / 2.0;
        painter.line_segment(
            [mark_center + vec2(-r * 0.5, 0.0), mark_center + vec2(r * 0.5, 0.0)],
            Stroke::new((mark_size / 9.0).max(1.5) * 1.2, p.urgent),
        );
    } else {
        paint_mark(&painter, mark_center, mark_size, lamp.light, p);
    }
    let left = rect.left() + pad + mark_size + 8.0;
    let width = (rect.right() - pad - left).max(10.0);
    let galley = |text: &str, font: FontId, color: Color32| {
        let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap.max_width = width;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        ui.painter().layout_job(job)
    };
    let title_g = galley(title, FontId::proportional(11.0), text2);
    let word_font =
        FontId::new(if large { 20.0 } else { 16.0 }, FontFamily::Name(theme::SEMIBOLD.into()));
    let word_g = galley(&lamp.word, word_font, text);
    let note_g = galley(&lamp.note, FontId::proportional(12.0), text2);
    let mut y = rect.top() + pad - 1.0;
    painter.galley(pos2(left, y), title_g.clone(), text2);
    y += title_g.size().y + 1.0;
    painter.galley(pos2(left, y), word_g.clone(), text);
    y += word_g.size().y + 2.0;
    if y + note_g.size().y <= rect.bottom() - 6.0 {
        painter.galley(pos2(left, y), note_g, text2);
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            CARD_RADIUS,
            Stroke::new(2.0, p.focus),
            egui::StrokeKind::Inside,
        );
    }
    let label = format!("{title}: {}. {}", lamp.word, lamp.detail);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    if lamp.detail.is_empty() { response } else { response.on_hover_text(&lamp.detail) }
}

/// A segment's badge: its light, and what it means, for VoiceOver.
pub type Badge<'a> = Option<(Light, &'a str)>;

/// A segmented control in the macOS style: one segment per choice, the
/// selected one raised. A segment can carry a badge (warnings on a tab the
/// user is not on).
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: Id,
    selected: &mut T,
    choices: &[(T, &str, Badge<'_>)],
) -> bool {
    let p = theme::palette(ui);
    let segment_w = 96.0;
    let size = vec2(segment_w * choices.len() as f32 + 4.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(
        rect,
        7,
        p.control,
        Stroke::new(1.0, p.card_stroke),
        egui::StrokeKind::Inside,
    );
    let mut changed = false;
    for (i, &(value, label, badge)) in choices.iter().enumerate() {
        let seg = Rect::from_min_size(
            pos2(rect.left() + 2.0 + segment_w * i as f32, rect.top() + 2.0),
            vec2(segment_w, rect.height() - 4.0),
        );
        let response = ui.interact(seg, id.with(i), Sense::click());
        let on = *selected == value;
        if on {
            ui.painter().rect(
                seg,
                5,
                p.segment,
                Stroke::new(1.0, p.card_stroke),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(seg.shrink(1.0), 5, p.control_hover);
        }
        let font = if on {
            FontId::new(13.0, FontFamily::Name(theme::SEMIBOLD.into()))
        } else {
            FontId::proportional(13.0)
        };
        ui.painter().text(seg.center(), egui::Align2::CENTER_CENTER, label, font, p.text);
        if let Some((light, _)) = badge {
            paint_mark(ui.painter(), pos2(seg.right() - 12.0, seg.center().y), 9.0, light, p);
        }
        if response.has_focus() {
            ui.painter().rect_stroke(seg, 5, Stroke::new(2.0, p.focus), egui::StrokeKind::Inside);
        }
        // VoiceOver hears the badge too: "Status, warnings".
        let spoken = match badge {
            Some((_, meaning)) => format!("{label}, {meaning}"),
            None => label.to_owned(),
        };
        response
            .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, on, &spoken));
        if response.clicked() && !on {
            *selected = value;
            changed = true;
        }
    }
    changed
}

/// Name and value rows in two columns, the values wrapping at the card's
/// edge instead of running past it.
pub fn rows(ui: &mut Ui, id: &str, rows: &[(&str, RichText)]) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        for (name, value) in rows {
            ui.label(RichText::new(*name).weak());
            ui.add(egui::Label::new(value.clone()).wrap());
            ui.end_row();
        }
    });
}

/// How wide a table column is.
#[derive(Clone, Copy)]
pub enum Width {
    Fixed(f32),
    /// A share of the width the fixed columns leave.
    Share(f32),
}

/// A table at the card's full width: column titles, then rows with stripes
/// that span the card. `cell` draws row `row`, column `col` in its cell;
/// labels in it should truncate, so a long value never pushes the others.
pub fn table(
    ui: &mut Ui,
    id: &str,
    columns: &[(&str, Width)],
    rows: usize,
    mut cell: impl FnMut(&mut Ui, usize, usize),
) {
    let p = theme::palette(ui);
    let full = ui.available_width();
    let gap = 10.0;
    let pad = 6.0;
    let fixed: f32 =
        columns.iter().map(|(_, w)| if let Width::Fixed(f) = w { *f } else { 0.0 }).sum();
    let shares: f32 =
        columns.iter().map(|(_, w)| if let Width::Share(s) = w { *s } else { 0.0 }).sum();
    let free = (full - 2.0 * pad - fixed - gap * (columns.len() as f32 - 1.0)).max(0.0);
    let widths: Vec<f32> = columns
        .iter()
        .map(|(_, w)| match w {
            Width::Fixed(f) => *f,
            Width::Share(s) => free * s / shares.max(f32::EPSILON),
        })
        .collect();
    let row_height = ui.spacing().interact_size.y;
    let mut draw_row = |ui: &mut Ui, row: Option<usize>, height: f32| {
        let (rect, _) = ui.allocate_exact_size(vec2(full, height), Sense::hover());
        if let Some(row) = row
            && row % 2 == 1
        {
            ui.painter().rect_filled(rect, 4, p.stripe);
        }
        let mut x = rect.left() + pad;
        for (col, width) in widths.iter().enumerate() {
            let cell_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(*width, rect.height()));
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, row, col))
                    .max_rect(cell_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            child.set_clip_rect(cell_rect.intersect(ui.clip_rect()));
            match row {
                None => {
                    child.add(egui::Label::new(theme::caption(columns[col].0)).truncate());
                }
                Some(row) => cell(&mut child, row, col),
            }
            x += width + gap;
        }
    };
    draw_row(ui, None, 18.0);
    for row in 0..rows {
        draw_row(ui, Some(row), row_height);
    }
}

/// Two cards side by side, or one under the other in a narrow window.
pub fn pair(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    if ui.available_width() >= 600.0 {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = theme::COLUMN_GAP;
            ui.columns(2, |columns| {
                left(&mut columns[0]);
                right(&mut columns[1]);
            });
        });
    } else {
        left(ui);
        right(ui);
    }
}

/// A row of a settings card in the System Settings style: the label and
/// its help on the left, the control on the right. A changed setting gets a
/// dot before its label.
pub fn setting_row(
    ui: &mut Ui,
    label: &str,
    help: Option<RichText>,
    changed: bool,
    control: impl FnOnce(&mut Ui) -> Response,
) -> Response {
    let p = theme::palette(ui);
    let mut label_id = None;
    let mut control_response = None;
    egui::Sides::new().shrink_left().spacing(16.0).show(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if changed {
                        let (rect, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 3.5, p.focus);
                    }
                    label_id = Some(ui.label(label).id);
                });
                if let Some(help) = help {
                    ui.add(egui::Label::new(help).wrap());
                }
            });
        },
        |ui| {
            control_response = Some(control(ui));
        },
    );
    let response = control_response.expect("the control was drawn");
    // VoiceOver reads the control by its label.
    match label_id {
        Some(id) => response.labelled_by(id),
        None => response,
    }
}

/// A switch in the macOS style: on is the accent colour.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = theme::palette(ui);
    let size = vec2(38.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    let value = *on;
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, value, ""));
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let mut fill = theme::mix(p.control, p.accent, t);
    let mut knob = Color32::WHITE;
    if !enabled {
        fill = fill.gamma_multiply(0.5);
        knob = knob.gamma_multiply(0.7);
    }
    let radius = rect.height() / 2.0;
    let painter = ui.painter();
    painter.rect(rect, radius, fill, Stroke::new(1.0, p.control_stroke), egui::StrokeKind::Inside);
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    painter.circle(
        pos2(x, rect.center().y),
        radius - 3.0,
        knob,
        Stroke::new(0.5, p.control_stroke),
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            radius + 2.0,
            Stroke::new(2.0, p.focus),
            egui::StrokeKind::Outside,
        );
    }
    response
}

/// A hairline between rows of a card.
pub fn separator(ui: &mut Ui) {
    let p = theme::palette(ui);
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.card_stroke));
    ui.add_space(4.0);
}

/// The default button: accent fill and white text.
pub fn primary_button(ui: &Ui, text: &str, enabled: bool) -> egui::Button<'static> {
    let p = theme::palette(ui);
    if enabled {
        egui::Button::new(RichText::new(text.to_owned()).color(Color32::WHITE))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .min_size(vec2(84.0, 26.0))
    } else {
        egui::Button::new(text.to_owned()).min_size(vec2(84.0, 26.0))
    }
}

/// A button for an action that interrupts audio: solid red, white text.
pub fn destructive_button(ui: &Ui, text: &str) -> egui::Button<'static> {
    let p = theme::palette(ui);
    egui::Button::new(RichText::new(text.to_owned()).color(Color32::WHITE))
        .fill(p.urgent)
        .stroke(Stroke::NONE)
        .min_size(vec2(84.0, 26.0))
}
