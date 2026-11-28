//! The app's building blocks, drawn with GPUI: status marks, cards,
//! notices, chips, status tiles, tables, name and value rows, settings rows,
//! buttons, the switch and dialogs. They hold no state: the views wire
//! their clicks. Colours and sizes come from [`crate::theme`].

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use gpui::{
    AnimationExt, AnyElement, AnyView, App, Bounds, Context, Div, ElementId, Font, FontWeight,
    Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Render, SharedString, Stateful, Styled,
    StyledText, TextRun, Window, canvas, div, fill, font, point, prelude::*, px, size,
};

use crate::logic::{self, Lamp, Light};
use crate::theme::{
    self, BODY, CARD_PAD, CARD_RADIUS, CONTROL_HEIGHT, CONTROL_RADIUS, Color, Palette, SEMIBOLD,
    SMALL, WHITE,
};

/// A light's mark, `size` points wide: its shape says the state without
/// the colour. A filled circle with a check is fine, a triangle with a bar
/// is worth a look, an octagon with a bar is down, and a hollow ring is
/// unknown or off.
pub fn mark(light: Light, size: f32, p: &Palette) -> impl IntoElement {
    mark_in(light, size, p.fill(light), WHITE)
}

/// A mark in `color`, its check or bar in `on` (the triangle's bar is
/// always dark).
pub fn mark_in(light: Light, size: f32, color: Color, on: Color) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, _, window, _| paint_mark(bounds, light, color, on, window))
        .size(px(size))
        .flex_shrink_0()
}

fn paint_mark(bounds: Bounds<Pixels>, light: Light, color: Color, on: Color, window: &mut Window) {
    let s = f32::from(bounds.size.width.min(bounds.size.height));
    let r = s / 2.0;
    let c = bounds.center();
    let (cx, cy) = (f32::from(c.x), f32::from(c.y));
    let at = |x: f32, y: f32| point(px(cx + x), px(cy + y));
    let thin = (s / 9.0).max(1.5);
    let details = s >= 11.0;
    match light {
        Light::Green => {
            window.paint_quad(fill(bounds, color).corner_radii(px(r)));
            if details {
                let mut check = PathBuilder::stroke(px(thin));
                check.move_to(at(-0.45 * r, 0.02 * r));
                check.line_to(at(-0.12 * r, 0.33 * r));
                check.line_to(at(0.45 * r, -0.32 * r));
                if let Ok(path) = check.build() {
                    window.paint_path(path, on);
                }
            }
        }
        Light::Amber => {
            let h = r * 1.9;
            let mut triangle = PathBuilder::fill();
            triangle.move_to(at(0.0, -h * 0.55));
            triangle.line_to(at(r * 1.05, h * 0.45));
            triangle.line_to(at(-r * 1.05, h * 0.45));
            triangle.close();
            if let Ok(path) = triangle.build() {
                window.paint_path(path, color);
            }
            if details {
                let dark = theme::rgb(0x191919);
                let bar = Bounds::new(at(-thin / 2.0, -h * 0.18), size(px(thin), px(h * 0.30)));
                window.paint_quad(fill(bar, dark).corner_radii(px(thin / 2.0)));
                let d = thin * 1.2;
                let dot = Bounds::new(at(-d / 2.0, h * 0.28 - d / 2.0), size(px(d), px(d)));
                window.paint_quad(fill(dot, dark).corner_radii(px(d / 2.0)));
            }
        }
        Light::Red => {
            let mut octagon = PathBuilder::fill();
            for i in 0..8 {
                let a = TAU * (i as f32 + 0.5) / 8.0;
                let p = at(a.cos() * r * 1.04, a.sin() * r * 1.04);
                if i == 0 { octagon.move_to(p) } else { octagon.line_to(p) }
            }
            octagon.close();
            if let Ok(path) = octagon.build() {
                window.paint_path(path, color);
            }
            if details {
                let w = thin * 1.2;
                let bar = Bounds::new(at(-r * 0.5, -w / 2.0), size(px(r), px(w)));
                window.paint_quad(fill(bar, on).corner_radii(px(w / 2.0)));
            }
        }
        Light::Grey => {
            let ring = gpui::outline(bounds, color, gpui::BorderStyle::Solid)
                .corner_radii(px(r))
                .border_widths(px(thin));
            window.paint_quad(ring);
        }
    }
}

/// A spinner: an arc that turns, `size` points wide.
pub fn spinner(id: impl Into<ElementId>, size: f32, color: Color) -> impl IntoElement {
    div().size(px(size)).flex_shrink_0().with_animation(
        id,
        gpui::Animation::new(Duration::from_millis(900)).repeat(),
        move |el, delta| {
            el.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let s = f32::from(bounds.size.width);
                        let w = (s / 8.0).max(1.5);
                        let r = s / 2.0 - w;
                        let c = bounds.center();
                        let at = |a: f32| point(c.x + px(r * a.cos()), c.y + px(r * a.sin()));
                        let start = delta * TAU;
                        let end = start + 1.5 * PI;
                        let mut arc = PathBuilder::stroke(px(w));
                        arc.move_to(at(start));
                        arc.arc_to(point(px(r), px(r)), px(0.0), true, true, at(end));
                        if let Ok(path) = arc.build() {
                            window.paint_path(path, color);
                        }
                    },
                )
                .size_full(),
            )
        },
    )
}

/// Text at the caption size, in the secondary colour.
pub fn caption(text: impl Into<SharedString>, p: &Palette) -> Div {
    div().text_size(px(SMALL)).text_color(p.text2).child(text.into())
}

/// A headline: the body size, semibold.
pub fn headline(text: impl Into<SharedString>) -> Div {
    div().font_weight(SEMIBOLD).child(text.into())
}

/// A card at the full width, its contents in a column.
pub fn card(p: &Palette) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(CARD_PAD))
        .bg(p.card)
        .border_1()
        .border_color(p.card_edge)
        .rounded(px(CARD_RADIUS))
}

/// A titled card, with an optional one-line summary in a chip at the
/// right of the title: `Receive channels … (7 of 7 receiving)`.
pub fn section(
    p: &Palette,
    title: impl Into<SharedString>,
    summary: Option<(Light, String)>,
    body: impl IntoElement,
) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .pt(px(theme::SECTION_GAP))
        .gap(px(theme::TITLE_GAP))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(theme::TITLE))
                        .font_weight(SEMIBOLD)
                        .child(title.into()),
                )
                .when_some(summary, |row, (light, text)| row.child(chip(p, light, text))),
        )
        .child(card(p).child(body))
}

/// A chip: a capsule with a few words in a light's colour, and its mark
/// when the light is amber or red. It truncates to the room it has.
pub fn chip(p: &Palette, light: Light, text: impl Into<SharedString>) -> Div {
    let color = if light == Light::Grey { p.text2 } else { p.ink(light) };
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(22.0))
        .px(px(10.0))
        .rounded_full()
        .bg(p.card)
        .border_1()
        .border_color(p.card_edge)
        .when(matches!(light, Light::Amber | Light::Red), |chip| chip.child(mark(light, 11.0, p)))
        .child(div().min_w_0().truncate().text_color(color).child(text.into()))
}

/// How much a notice matters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// Audio is down now.
    Urgent,
    /// Worth a look.
    Warning,
    /// Something in progress, such as a restart.
    Progress,
}

/// A notice: a tinted card with a coloured bar down its left edge, a mark
/// and a heading that says in words how much it matters. The body goes in
/// with `.child()` under the heading.
pub fn notice(
    id: impl Into<ElementId>,
    p: &Palette,
    severity: Severity,
    heading: impl Into<SharedString>,
) -> Div {
    let (light, bg) = match severity {
        Severity::Urgent => (Light::Red, p.alarm()),
        Severity::Warning | Severity::Progress => (Light::Amber, p.tinted(Light::Amber)),
    };
    let icon = match severity {
        Severity::Progress => spinner(id, 15.0, p.text2).into_any_element(),
        _ => mark(light, 16.0, p).into_any_element(),
    };
    div()
        .relative()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .py(px(12.0))
        .pl(px(24.0))
        .pr(px(CARD_PAD))
        .bg(bg)
        .border_1()
        .border_color(p.card_edge)
        .rounded(px(CARD_RADIUS))
        .child(
            div()
                .absolute()
                .left(px(9.0))
                .top(px(12.0))
                .bottom(px(12.0))
                .w(px(3.0))
                .rounded(px(1.5))
                .bg(p.fill(light)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(icon)
                .child(div().font_weight(SEMIBOLD).text_size(px(14.0)).child(heading.into())),
        )
}

/// A text with its backtick spans as code, wrapping at its container's
/// edge.
pub fn rich_text(text: &str, color: Color, weight: FontWeight, p: &Palette) -> StyledText {
    let base = Font { weight, ..font(theme::SANS) };
    let code = Font { weight, ..font(theme::MONO) };
    let mut all = String::new();
    let mut runs = Vec::new();
    for (is_code, span) in logic::code_spans(text) {
        if span.is_empty() {
            continue;
        }
        all.push_str(span);
        runs.push(TextRun {
            len: span.len(),
            font: if is_code { code.clone() } else { base.clone() },
            color: color.into(),
            background_color: is_code.then(|| Hsla::from(p.code)),
            underline: None,
            strikethrough: None,
        });
    }
    StyledText::new(all).with_runs(runs)
}

/// What a button looks like.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Plain,
    /// The default action: the accent, white words.
    Primary,
    /// An action that interrupts audio: red, white words.
    Destructive,
}

/// A button. The caller adds `.on_click` while it is enabled.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: Kind,
    enabled: bool,
    p: &Palette,
) -> Stateful<Div> {
    let (bg, hover, text) = match (kind, enabled) {
        (Kind::Primary, true) => (p.accent, p.accent_hover, WHITE),
        (Kind::Destructive, true) => (p.urgent, p.urgent_hover, WHITE),
        _ => (p.control, p.control_hover, p.text),
    };
    let press = if kind == Kind::Plain { p.control_press } else { hover };
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .h(px(CONTROL_HEIGHT))
        .min_w(px(80.0))
        .px(px(14.0))
        .rounded(px(CONTROL_RADIUS))
        .bg(bg)
        .text_color(text)
        .when(kind != Kind::Plain && enabled, |b| b.font_weight(FontWeight::MEDIUM))
        .when(!enabled, |b| b.opacity(0.45))
        .when(enabled, |b| {
            b.cursor_pointer().hover(move |s| s.bg(hover)).active(move |s| s.bg(press))
        })
        .child(label.into())
}

/// A status tile: the mark, the title, the word, and what it means. Red
/// tiles are solid, so a fault changes the top of the window from across a
/// room; amber ones are tinted; the others stay quiet. Hovering shows the
/// lamp's detail.
pub fn tile(
    id: impl Into<ElementId>,
    title: &'static str,
    lamp: &Lamp,
    width: f32,
    large: bool,
    p: &Palette,
) -> Stateful<Div> {
    let (bg, edge, text, text2) = match lamp.light {
        Light::Red => (p.urgent, p.urgent, WHITE, theme::rgb(0xEBEBEB)),
        Light::Amber => (p.tinted(Light::Amber), p.card_edge, p.text, p.text),
        _ => (p.card, p.card_edge, p.text, p.text2),
    };
    let mark_size = if large { 22.0 } else { 18.0 };
    let icon = match lamp.light {
        // White on the solid red, its bar red, so the shape still reads.
        Light::Red => mark_in(Light::Red, mark_size, WHITE, p.urgent).into_any_element(),
        light => mark(light, mark_size, p).into_any_element(),
    };
    let detail = lamp.detail.clone();
    div()
        .id(id)
        .flex_shrink_0()
        .w(px(width))
        .h(px(84.0))
        .flex()
        .gap(px(9.0))
        .p(px(14.0))
        .bg(bg)
        .border_1()
        .border_color(edge)
        .rounded(px(CARD_RADIUS))
        .overflow_hidden()
        .child(div().pt(px(1.0)).child(icon))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(SMALL)).text_color(text2).truncate().child(title))
                .child(
                    div()
                        .text_size(px(if large { 22.0 } else { 19.0 }))
                        .line_height(px(if large { 27.0 } else { 24.0 }))
                        .font_weight(SEMIBOLD)
                        .text_color(text)
                        .truncate()
                        .child(lamp.word.clone()),
                )
                .child(
                    div().text_size(px(12.0)).text_color(text2).truncate().child(lamp.note.clone()),
                ),
        )
        .when(!detail.is_empty(), |t| t.tooltip(Tooltip::text(detail)))
}

/// A segment of the segmented control: the selected one raised, the others
/// on the track. A badge flags warnings on a tab the user is not on.
pub fn segment(
    id: impl Into<ElementId>,
    label: &'static str,
    on: bool,
    badge: Option<Light>,
    p: &Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .w(px(104.0))
        .h(px(CONTROL_HEIGHT - 4.0))
        .rounded(px(CONTROL_RADIUS - 1.0))
        .when(on, |s| {
            s.bg(p.segment).font_weight(FontWeight::MEDIUM).shadow(vec![gpui::BoxShadow {
                color: p.shadow.fade(0.5).into(),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            }])
        })
        .when(!on, |s| s.cursor_pointer().hover(|s| s.bg(p.control_hover)))
        .child(label)
        .when_some(badge, |s, light| s.child(mark(light, 9.0, p)))
}

/// Name and value rows in two columns, the values wrapping at the card's
/// edge. `name_width` fits the longest name.
pub fn rows(p: &Palette, name_width: Pixels, rows: Vec<(&'static str, AnyElement)>) -> Div {
    div().flex().flex_col().gap(px(6.0)).children(rows.into_iter().map(|(name, value)| {
        div()
            .flex()
            .gap(px(16.0))
            .child(div().w(name_width).flex_shrink_0().text_color(p.text2).child(name))
            .child(div().flex_1().min_w_0().child(value))
    }))
}

/// The width of the longest of `names` at the body size, for [`rows`].
pub fn names_width(names: &[&str], window: &Window) -> Pixels {
    let font_size = px(BODY);
    names
        .iter()
        .map(|name| {
            let run = TextRun {
                len: name.len(),
                font: font(theme::SANS),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line((*name).to_owned().into(), font_size, &[run], None)
                .width
        })
        .fold(px(40.0), |a, b| a.max(b))
        + px(4.0)
}

/// How wide a table column is.
#[derive(Clone, Copy)]
pub enum Width {
    Fixed(f32),
    /// A share of the width the fixed columns leave.
    Share(f32),
}

/// A table at the card's full width: column titles, then rows with stripes
/// across the card. Cells should truncate, so that a long value never
/// pushes the others.
pub fn table(p: &Palette, columns: &[(&'static str, Width)], rows: Vec<Vec<AnyElement>>) -> Div {
    let cell = |width: Width| {
        let mut cell = div().h_full().flex().items_center().overflow_hidden();
        match width {
            Width::Fixed(w) => cell = cell.w(px(w)).flex_shrink_0(),
            Width::Share(s) => {
                cell = cell.min_w_0().flex_basis(px(0.0));
                cell.style().flex_grow = Some(s);
                cell.style().flex_shrink = Some(1.0);
            }
        }
        cell
    };
    let row = || div().flex().gap(px(10.0)).px(px(6.0)).w_full();
    let header =
        row().h(px(20.0)).children(columns.iter().map(|(title, width)| {
            cell(*width).text_size(px(SMALL)).text_color(p.text2).child(*title)
        }));
    div().flex().flex_col().w_full().child(header).children(rows.into_iter().enumerate().map(
        |(i, cells)| {
            row()
                .h(px(CONTROL_HEIGHT))
                .rounded(px(6.0))
                .when(i % 2 == 1, |r| r.bg(p.stripe))
                .children(
                    cells
                        .into_iter()
                        .zip(columns)
                        .map(|(content, (_, width))| cell(*width).child(content)),
                )
        },
    ))
}

/// Two cards side by side in a wide window, or one under the other.
pub fn pair(wide: bool, left: impl IntoElement, right: impl IntoElement) -> Div {
    if wide {
        div()
            .w_full()
            .flex()
            .items_start()
            .gap(px(theme::COLUMN_GAP))
            .child(div().flex_1().min_w_0().child(left))
            .child(div().flex_1().min_w_0().child(right))
    } else {
        div().w_full().flex().flex_col().child(left).child(right)
    }
}

/// A row of a settings card, as macOS lays them out: the label and its
/// help on the left, the control on the right. A changed setting gets a dot
/// before its label.
pub fn setting_row(
    p: &Palette,
    label: &'static str,
    help: Option<AnyElement>,
    changed: bool,
    control: impl IntoElement,
) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .when(changed, |l| l.child(div().size(px(7.0)).rounded_full().bg(p.focus)))
                        .child(label),
                )
                .when_some(help, |l, help| l.child(help)),
        )
        .child(div().flex_shrink_0().flex().items_center().gap(px(8.0)).child(control))
}

/// A switch: a capsule track, green when on, and a white knob.
pub fn toggle(id: impl Into<ElementId>, on: bool, enabled: bool, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .w(px(42.0))
        .h(px(24.0))
        .p(px(2.0))
        .rounded_full()
        .flex()
        .when(on, |t| t.bg(p.switch).justify_end())
        .when(!on, |t| t.bg(p.control).border_1().border_color(p.control_stroke).justify_start())
        .when(!enabled, |t| t.opacity(0.45))
        .when(enabled, |t| t.cursor_pointer())
        .child(div().size(px(if on { 20.0 } else { 18.0 })).rounded_full().bg(WHITE).shadow(vec![
            gpui::BoxShadow {
                color: theme::rgba(0x000000, 70).into(),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            },
        ]))
}

/// A hairline between rows of a card.
pub fn separator(p: &Palette) -> Div {
    div().w_full().h(px(1.0)).my(px(2.0)).bg(p.separator)
}

/// A command in a box, with a Copy button that says `Copied` for a
/// moment. The caller wires the button's click.
pub fn command_row(p: &Palette, command: &str, copy: Stateful<Div>) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.0))
        .py(px(6.0))
        .pl(px(12.0))
        .pr(px(6.0))
        .rounded(px(theme::FIELD_RADIUS))
        .bg(p.code)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(theme::mono())
                .text_size(px(12.0))
                .child(command.to_owned()),
        )
        .child(copy)
}

/// A dialog over the window: the window dimmed behind it, which takes the
/// clicks. Its contents go in with `.child()`.
pub fn dialog(p: &Palette, width: f32) -> Div {
    div()
        .w(px(width))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(22.0))
        .bg(p.popup)
        .border_1()
        .border_color(p.card_edge)
        .rounded(px(12.0))
        .shadow(vec![gpui::BoxShadow {
            color: p.shadow.into(),
            offset: point(px(0.0), px(10.0)),
            blur_radius: px(30.0),
            spread_radius: px(0.0),
        }])
}

/// The layer that holds a dialog in the middle of the window.
pub fn scrim(p: &Palette) -> Stateful<Div> {
    div()
        .id("scrim")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(p.scrim)
        .occlude()
}

/// A tooltip: a few words in a small popup.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    /// A builder for `.tooltip()`.
    pub fn text(
        text: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip { text: text.clone() }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(window);
        // A little below and to the right of the pointer.
        div().pl(px(10.0)).pt(px(16.0)).child(
            div()
                .max_w(px(320.0))
                .px(px(9.0))
                .py(px(6.0))
                .bg(p.popup)
                .border_1()
                .border_color(p.card_edge)
                .rounded(px(6.0))
                .shadow(vec![gpui::BoxShadow {
                    color: p.shadow.fade(0.6).into(),
                    offset: point(px(0.0), px(3.0)),
                    blur_radius: px(10.0),
                    spread_radius: px(0.0),
                }])
                .font_family(theme::sans())
                .text_size(px(12.0))
                .text_color(p.text)
                .child(self.text.clone()),
        )
    }
}

/// The small chevron of a pop-up menu's button.
pub fn chevron(color: Color) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let c = bounds.center();
            let mut path = PathBuilder::stroke(px(1.5));
            path.move_to(point(c.x - px(3.5), c.y - px(1.5)));
            path.line_to(point(c.x, c.y + px(2.0)));
            path.line_to(point(c.x + px(3.5), c.y - px(1.5)));
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size(px(12.0))
    .flex_shrink_0()
}
