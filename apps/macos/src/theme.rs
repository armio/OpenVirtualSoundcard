//! The look: colours for each appearance, the type scale, spacing, and the
//! fonts. Every colour pair used for text or controls is checked for
//! contrast in the tests below.

use eframe::egui::{
    self, Color32, Context, CornerRadius, FontDefinitions, FontFamily, FontId, Margin, RichText,
    Stroke, Style, TextStyle, Theme, Ui, vec2,
};

use crate::logic::Light;

/// Spacing on a 4 pt grid.
pub const GUTTER: f32 = 16.0;
pub const CARD_PAD: i8 = 12;
pub const SECTION_GAP: f32 = 18.0;
pub const TITLE_GAP: f32 = 6.0;
pub const COLUMN_GAP: f32 = 16.0;

/// Corner radii.
pub const CARD_RADIUS: u8 = 10;
pub const CONTROL_RADIUS: u8 = 6;

/// The font family of the semibold weight: SF Pro's on macOS, the regular
/// font elsewhere.
pub const SEMIBOLD: &str = "semibold";

/// Named text styles, besides egui's own.
pub const HEADLINE: &str = "headline";
pub const STATUS: &str = "status";
pub const STATUS_LARGE: &str = "status-large";

/// The colours of one appearance.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// The window behind the cards.
    pub window: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    /// Primary, secondary and tertiary text.
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_stroke: Color32,
    /// The selected segment of a segmented control.
    pub segment: Color32,
    pub field: Color32,
    pub code: Color32,
    pub stripe: Color32,
    /// The default button's fill, under white text.
    pub accent: Color32,
    /// Focus rings, links and the selection.
    pub focus: Color32,
    /// Status colours for shapes: dots, icons, bars.
    pub green: Color32,
    pub amber: Color32,
    pub red: Color32,
    pub grey: Color32,
    /// Status colours for text.
    pub green_ink: Color32,
    pub amber_ink: Color32,
    pub red_ink: Color32,
    /// A solid red under white text: the down state.
    pub urgent: Color32,
    /// How much of a status colour tints a card.
    pub tint: f32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const DARK: Palette = Palette {
    window: rgb(0x1E1E1E),
    card: rgb(0x2A2A2C),
    card_stroke: rgb(0x3A3A3C),
    text: rgb(0xE5E5E7),
    text2: rgb(0x9C9CA1),
    text3: rgb(0x6C6C70),
    control: rgb(0x3A3A3C),
    control_hover: rgb(0x48484A),
    control_stroke: rgb(0x7C7C80),
    segment: rgb(0x636366),
    field: rgb(0x1C1C1E),
    code: rgb(0x1C1C1E),
    stripe: rgb(0x303032),
    accent: rgb(0x0071E3),
    focus: rgb(0x0A84FF),
    green: rgb(0x30D158),
    amber: rgb(0xFF9F0A),
    red: rgb(0xFF453A),
    grey: rgb(0x8E8E93),
    green_ink: rgb(0x30D158),
    amber_ink: rgb(0xFFB340),
    red_ink: rgb(0xFF6961),
    urgent: rgb(0xC4000F),
    tint: 0.20,
};

pub const LIGHT: Palette = Palette {
    window: rgb(0xEDEDF0),
    card: rgb(0xFFFFFF),
    card_stroke: rgb(0xD5D5DA),
    text: rgb(0x1D1D1F),
    text2: rgb(0x636368),
    text3: rgb(0xA1A1A6),
    control: rgb(0xFFFFFF),
    control_hover: rgb(0xF2F2F5),
    control_stroke: rgb(0x848489),
    segment: rgb(0xFFFFFF),
    field: rgb(0xFFFFFF),
    code: rgb(0xEFEFF2),
    stripe: rgb(0xF5F5F7),
    accent: rgb(0x0071E3),
    focus: rgb(0x0071E3),
    green: rgb(0x248A3D),
    amber: rgb(0xC27100),
    red: rgb(0xD70015),
    grey: rgb(0x848489),
    green_ink: rgb(0x1E7A34),
    amber_ink: rgb(0x9A4D00),
    red_ink: rgb(0xC9000F),
    urgent: rgb(0xC4000F),
    tint: 0.13,
};

/// The palette of the appearance `ui` shows.
pub fn palette(ui: &Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// `a` mixed with `t` of `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

impl Palette {
    /// A light's colour for shapes.
    pub fn fill(&self, light: Light) -> Color32 {
        match light {
            Light::Green => self.green,
            Light::Amber => self.amber,
            Light::Red => self.red,
            Light::Grey => self.grey,
        }
    }

    /// A light's colour for text.
    pub fn ink(&self, light: Light) -> Color32 {
        match light {
            Light::Green => self.green_ink,
            Light::Amber => self.amber_ink,
            Light::Red => self.red_ink,
            Light::Grey => self.text2,
        }
    }

    /// A card tinted with a light's colour, under primary text.
    pub fn tint(&self, light: Light) -> Color32 {
        mix(self.card, self.fill(light), self.tint)
    }
}

/// Installs the fonts and the style for both appearances.
pub fn install(ctx: &Context) {
    ctx.set_fonts(fonts());
    ctx.all_styles_mut(common);
    ctx.style_mut_of(Theme::Dark, |s| colours(s, &DARK));
    ctx.style_mut_of(Theme::Light, |s| colours(s, &LIGHT));
    // A testing hook, like OVSC_CONTROL_SOCKET: force an appearance.
    // Otherwise follow the system, also after a forced run: egui remembers
    // the preference across launches.
    match std::env::var("OVSC_APP_THEME").as_deref() {
        Ok("light") => ctx.set_theme(Theme::Light),
        Ok("dark") => ctx.set_theme(Theme::Dark),
        _ => ctx.set_theme(egui::ThemePreference::System),
    }
}

/// egui's fonts, with Hack as the last resort for symbols the others lack
/// (the daemon's `←`), and on macOS the system font in front: SF Pro,
/// regular and semibold, and SF Mono. They are read from the Mac at run
/// time, never bundled.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.push("Hack".to_owned());
    }
    #[cfg_attr(not(target_os = "macos"), expect(unused_mut))]
    let mut semibold = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    #[cfg(target_os = "macos")]
    system_fonts(&mut fonts, &mut semibold);
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    fonts
}

#[cfg(target_os = "macos")]
fn system_fonts(fonts: &mut FontDefinitions, semibold: &mut Vec<String>) {
    use std::sync::Arc;

    use eframe::egui::{FontData, FontTweak};

    let read = |path: &str| -> Option<&'static [u8]> {
        // Read once and kept for the app's life: two instances share it.
        std::fs::read(path).ok().map(|b| &*Box::leak(b.into_boxed_slice()))
    };
    if let Some(bytes) = read("/System/Library/Fonts/SFNS.ttf") {
        let axes: Vec<String> = FontData::from_static(bytes)
            .variation_axes()
            .iter()
            .map(|a| a.tag.to_string())
            .collect();
        let instance = |weight: f32| {
            let mut tweak = FontTweak::default();
            if axes.iter().any(|t| t == "wght") {
                tweak.coords.push("wght", weight);
            }
            if axes.iter().any(|t| t == "opsz") {
                // SF Text's optical size, for the app's 11–22 pt text.
                tweak.coords.push("opsz", 17.0);
            }
            Arc::new(FontData::from_static(bytes).tweak(tweak))
        };
        fonts.font_data.insert("sf".into(), instance(400.0));
        fonts.font_data.insert("sf-semibold".into(), instance(590.0));
        if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
            family.insert(0, "sf".into());
        }
        semibold.insert(0, "sf-semibold".into());
    }
    if let Some(bytes) = read("/System/Library/Fonts/SFNSMono.ttf") {
        fonts.font_data.insert("sf-mono".into(), Arc::new(FontData::from_static(bytes)));
        if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
            family.insert(0, "sf-mono".into());
        }
    }
}

/// What both appearances share: sizes, spacing, shapes.
fn common(s: &mut Style) {
    let proportional = |size| FontId::new(size, FontFamily::Proportional);
    let semibold = |size| FontId::new(size, FontFamily::Name(SEMIBOLD.into()));
    s.text_styles = [
        (TextStyle::Small, proportional(11.0)),
        (TextStyle::Body, proportional(13.0)),
        (TextStyle::Button, proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
        (TextStyle::Heading, semibold(22.0)),
        (TextStyle::Name(HEADLINE.into()), semibold(13.0)),
        (TextStyle::Name(STATUS.into()), semibold(16.0)),
        (TextStyle::Name(STATUS_LARGE.into()), semibold(20.0)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 6.0);
    s.spacing.button_padding = vec2(12.0, 4.0);
    s.spacing.interact_size.y = 24.0;
    // Room for every latency without scrolling the menu.
    s.spacing.combo_height = 360.0;
    s.spacing.menu_margin = Margin::same(6);
    // Overlay scroll bars, as on macOS: nothing moves when one appears.
    s.spacing.scroll = egui::style::ScrollStyle::floating();
    // Static text is not selectable, as in Mac apps; values worth copying
    // opt in.
    s.interaction.selectable_labels = false;
    let w = &mut s.visuals.widgets;
    for v in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        v.corner_radius = CornerRadius::same(CONTROL_RADIUS);
        v.expansion = 0.0;
    }
    s.visuals.window_corner_radius = CornerRadius::same(12);
    s.visuals.menu_corner_radius = CornerRadius::same(8);
    s.visuals.striped = false;
}

/// One appearance's colours.
fn colours(s: &mut Style, p: &Palette) {
    let v = &mut s.visuals;
    v.panel_fill = p.window;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.card_stroke);
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.code_bg_color = p.code;
    v.faint_bg_color = p.stripe;
    v.weak_text_color = Some(p.text2);
    v.hyperlink_color = p.focus;
    v.warn_fg_color = p.amber_ink;
    v.error_fg_color = p.red_ink;
    v.selection.bg_fill = mix(p.card, p.focus, 0.45);
    v.selection.stroke = Stroke::new(1.0, p.text);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.card_stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0, p.control_stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.bg_fill = p.control_hover;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.text2);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    // Pressed or focused: the focus ring.
    w.active.bg_fill = p.control_hover;
    w.active.weak_bg_fill = p.control_hover;
    w.active.bg_stroke = Stroke::new(2.0, p.focus);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.open.bg_fill = p.control_hover;
    w.open.weak_bg_fill = p.control_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.focus);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
}

/// Text in the semibold weight at `size`.
pub fn semibold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(FontId::new(size, FontFamily::Name(SEMIBOLD.into())))
}

/// A headline: 13 pt semibold.
pub fn headline(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(HEADLINE.into()))
}

/// A caption: 11 pt, secondary.
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).small().weak()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an sRGB colour.
    fn luminance(c: Color32) -> f64 {
        let ch = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[track_caller]
    fn at_least(ratio: f64, fg: Color32, bg: Color32, what: &str) {
        let c = contrast(fg, bg);
        assert!(c >= ratio, "{what}: {c:.2}:1 < {ratio}:1 ({fg:?} on {bg:?})");
    }

    #[test]
    fn text_and_controls_have_contrast() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for bg in [p.window, p.card, p.stripe] {
                at_least(4.5, p.text, bg, &format!("{name} text"));
                at_least(4.5, p.text2, bg, &format!("{name} secondary text"));
            }
            for light in [Light::Green, Light::Amber, Light::Red] {
                // Coloured words on the window and on cards.
                at_least(4.5, p.ink(light), p.card, &format!("{name} {light:?} ink"));
                at_least(4.5, p.ink(light), p.window, &format!("{name} {light:?} ink on window"));
                // Shapes against cards.
                at_least(3.0, p.fill(light), p.card, &format!("{name} {light:?} shape"));
                // Primary text on a tinted card.
                at_least(4.5, p.text, p.tint(light), &format!("{name} text on {light:?} tint"));
            }
            at_least(3.0, p.grey, p.card, &format!("{name} grey shape"));
            at_least(3.0, p.control_stroke, p.card, &format!("{name} control edge"));
            at_least(3.0, p.control_stroke, p.window, &format!("{name} control edge on window"));
            at_least(4.5, Color32::WHITE, p.accent, &format!("{name} default button"));
            at_least(4.5, Color32::WHITE, p.urgent, &format!("{name} urgent"));
            at_least(4.5, p.text, p.code, &format!("{name} code"));
            at_least(4.5, p.text, p.segment, &format!("{name} selected segment"));
        }
    }
}
