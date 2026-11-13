//! The look: Raven's glass, in egui. Surfaces are translucent glass over a
//! sober grey wallpaper, controls are capsules, and text is regular
//! weight at larger sizes. Every colour pair used for text or controls is
//! checked for contrast in the tests below, over the wallpaper's darkest and
//! lightest points.
//!
//! The palettes, sizes and the wallpaper's blob layout follow the GlassDark
//! and GlassLight looks of Raven (<https://github.com/oscar-soirey/Raven>),
//! Copyright 2026 Oscar Soirey, licensed under the Apache License 2.0; some
//! colours are darker or more opaque here, for contrast.

use eframe::egui::{
    self, Color32, ColorImage, Context, CornerRadius, FontDefinitions, FontFamily, FontId, Margin,
    RichText, Shadow, Stroke, Style, TextStyle, Theme, Ui, vec2,
};

use crate::logic::Light;

/// Spacing on a 4 pt grid, with Raven's glass air.
pub const GUTTER: f32 = 20.0;
pub const CARD_PAD: i8 = 16;
pub const SECTION_GAP: f32 = 20.0;
pub const TITLE_GAP: f32 = 8.0;
pub const COLUMN_GAP: f32 = 16.0;

/// Corner radii: panels of glass, status tiles, multi-line fields, and
/// controls, which are capsules (egui clamps the radius to half the height).
pub const CARD_RADIUS: u8 = 20;
pub const TILE_RADIUS: u8 = 18;
pub const FIELD_RADIUS: u8 = 12;
pub const CONTROL_RADIUS: u8 = 255;

/// The height of buttons, menus, fields and table rows (Raven's 32, a
/// little denser).
pub const CONTROL_HEIGHT: f32 = 30.0;

/// The font family of the semibold weight: SF Pro's on macOS, the regular
/// font elsewhere.
pub const SEMIBOLD: &str = "semibold";

/// Named text styles, besides egui's own.
pub const HEADLINE: &str = "headline";
pub const TITLE: &str = "title";
pub const STATUS: &str = "status";
pub const STATUS_LARGE: &str = "status-large";

/// The colours of one appearance. Colours with alpha are glass: they lie
/// over the wallpaper, or over another surface.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// The wallpaper: a base colour, and the shades melted into it.
    pub window: Color32,
    pub blobs: &'static [Color32],
    /// Glass: the tint of panels, its rim (at the top and at the bottom),
    /// the sheen under the top edge, and the shadow of what floats.
    pub card: Color32,
    pub rim: Color32,
    pub rim_low: Color32,
    pub sheen: Color32,
    pub shadow: Color32,
    /// Menus and dialogs: nearly solid glass, so nothing behind shows
    /// through their text.
    pub popup: Color32,
    /// The bar docked at the bottom of the Settings tab: opaque, so that
    /// the page scrolls under it unseen.
    pub footer: Color32,
    /// Primary, secondary and tertiary text.
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    /// Capsule controls: buttons, menus, chips.
    pub control: Color32,
    pub control_hover: Color32,
    pub control_press: Color32,
    /// The edge of a text field and of a switch that is off: 3:1 against
    /// the glass around it.
    pub control_stroke: Color32,
    pub field: Color32,
    pub code: Color32,
    pub stripe: Color32,
    pub separator: Color32,
    /// The default button and the selected tab, under white text.
    pub accent: Color32,
    /// Focus rings, links and the selection.
    pub focus: Color32,
    /// A switch that is on.
    pub switch: Color32,
    /// Status colours for shapes: dots, icons, bars.
    pub green: Color32,
    pub amber: Color32,
    pub red: Color32,
    pub grey: Color32,
    /// Status colours for text.
    pub green_ink: Color32,
    pub amber_ink: Color32,
    pub red_ink: Color32,
    /// Red glass under white text: the down state.
    pub urgent: Color32,
    /// How much of a status colour tints the glass of a notice or a tile.
    pub tint: f32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `hex` at `alpha` (0–255) opacity.
const fn rgba(hex: u32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, alpha)
}

/// GlassDark: Raven's dark glass, over graphite. The wallpaper only shades
/// from lighter to darker grey, so that colour in the window means
/// something: the status, a warning, the accent.
pub const DARK: Palette = Palette {
    dark: true,
    window: rgb(0x1D1D20),
    blobs: &[rgb(0x2C2D31), rgb(0x27272A), rgb(0x1F2023), rgb(0x2A2A2D)],
    card: rgba(0x3A3A3F, 128),
    rim: rgba(0xFFFFFF, 90),
    rim_low: rgba(0xFFFFFF, 26),
    sheen: rgba(0xFFFFFF, 18),
    shadow: rgba(0x000000, 110),
    popup: rgba(0x2E2E33, 247),
    footer: rgb(0x252528),
    text: rgb(0xF5F5F7),
    text2: rgb(0xCACAD2),
    text3: rgb(0x8A8A92),
    control: rgba(0xFFFFFF, 30),
    control_hover: rgba(0xFFFFFF, 48),
    control_press: rgba(0xFFFFFF, 68),
    control_stroke: rgb(0x8E8E96),
    field: rgba(0x141417, 120),
    code: rgba(0x141417, 130),
    stripe: rgba(0xFFFFFF, 12),
    separator: rgba(0xFFFFFF, 30),
    accent: rgba(0x0A6CE0, 240),
    focus: rgb(0x0A84FF),
    switch: rgb(0x30D158),
    green: rgb(0x30D158),
    amber: rgb(0xFFD60A),
    red: rgb(0xFF453A),
    grey: rgb(0x8E8E93),
    green_ink: rgb(0x5CE07E),
    amber_ink: rgb(0xFFD60A),
    red_ink: rgb(0xFF9F97),
    urgent: rgba(0xC4000F, 235),
    tint: 0.18,
};

/// GlassLight: Raven's light glass, over soft greys, as sober as the dark.
pub const LIGHT: Palette = Palette {
    dark: false,
    window: rgb(0xECECEF),
    blobs: &[rgb(0xF7F7F8), rgb(0xE3E3E6), rgb(0xF0F0F2), rgb(0xE6E6E9), rgb(0xF4F4F5)],
    card: rgba(0xFFFFFF, 160),
    rim: rgba(0xFFFFFF, 242),
    rim_low: rgba(0xFFFFFF, 110),
    sheen: rgba(0xFFFFFF, 90),
    shadow: rgba(0x1C1C22, 44),
    popup: rgba(0xFAFAFC, 247),
    footer: rgb(0xF7F7F8),
    text: rgb(0x1C1C1E),
    text2: rgb(0x45454C),
    text3: rgb(0x75757D),
    control: rgba(0xFFFFFF, 140),
    control_hover: rgba(0xFFFFFF, 190),
    control_press: rgba(0xFFFFFF, 230),
    control_stroke: rgb(0x85858D),
    field: rgba(0xFFFFFF, 180),
    code: rgba(0x3C3C43, 20),
    stripe: rgba(0x3C3C43, 12),
    separator: rgba(0x3C3C43, 36),
    accent: rgba(0x0062CC, 240),
    focus: rgb(0x007AFF),
    switch: rgb(0x1F8F40),
    green: rgb(0x248A3D),
    amber: rgb(0xB25000),
    red: rgb(0xD70015),
    grey: rgb(0x85858D),
    green_ink: rgb(0x1A6E31),
    amber_ink: rgb(0x8F4200),
    red_ink: rgb(0xC0000E),
    urgent: rgba(0xC4000F, 235),
    tint: 0.16,
};

/// The palette of the appearance `ui` shows.
pub fn palette(ui: &Ui) -> &'static Palette {
    palette_of(ui.visuals().dark_mode)
}

pub fn palette_of(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// `color` at `alpha` (0–1) of its own opacity.
pub fn fade(color: Color32, alpha: f32) -> Color32 {
    color.gamma_multiply(alpha)
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

    /// A light's colour as a tint laid over glass.
    pub fn tint(&self, light: Light) -> Color32 {
        fade(self.fill(light), self.tint)
    }

    /// The red that tints the notice that audio is down: a little more
    /// than a tint.
    pub fn alarm(&self) -> Color32 {
        fade(self.red, self.tint + 0.08)
    }
}

/// The wallpaper's size in texels: it is stretched over the window, and
/// its blobs are soft enough not to show it.
pub const WALLPAPER_SIZE: usize = 256;

/// Where the blobs lie and how large they are, as fractions of the window
/// (Raven's places).
const BLOB_PLACES: [(f32, f32, f32); 5] = [
    (0.15, 0.20, 0.55),
    (0.88, 0.12, 0.50),
    (0.80, 0.86, 0.58),
    (0.12, 0.92, 0.46),
    (0.50, 0.50, 0.34),
];

/// The wallpaper of `p`: its colours melted into its base, as Raven makes
/// the wallpaper of its glass looks.
pub fn wallpaper(p: &Palette) -> ColorImage {
    let n = WALLPAPER_SIZE;
    let rgb = |c: Color32| [f32::from(c.r()), f32::from(c.g()), f32::from(c.b())];
    let base = rgb(p.window);
    let mut pixels = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            let (u, v) = ((x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32);
            let mut c = base;
            for (blob, (bx, by, radius)) in p.blobs.iter().zip(BLOB_PLACES) {
                let (dx, dy) = (u - bx, v - by);
                let weight = (-(dx * dx + dy * dy) / (radius * radius * 0.5)).exp() * 0.9;
                let b = rgb(*blob);
                for i in 0..3 {
                    c[i] += (b[i] - c[i]) * weight;
                }
            }
            let ch = |v: f32| v.round().clamp(0.0, 255.0) as u8;
            pixels.push(Color32::from_rgb(ch(c[0]), ch(c[1]), ch(c[2])));
        }
    }
    ColorImage::new([n, n], pixels)
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
                // SF Text's optical size, for the app's 11–24 pt text.
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

/// What both appearances share: sizes, spacing, shapes. Titles and status
/// words are regular weight, larger, as Raven sets them; semibold is kept
/// for the first sentence of a warning and other words to find at once.
fn common(s: &mut Style) {
    let proportional = |size| FontId::new(size, FontFamily::Proportional);
    let semibold = |size| FontId::new(size, FontFamily::Name(SEMIBOLD.into()));
    s.text_styles = [
        (TextStyle::Small, proportional(11.0)),
        (TextStyle::Body, proportional(13.0)),
        (TextStyle::Button, proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
        (TextStyle::Heading, proportional(26.0)),
        (TextStyle::Name(HEADLINE.into()), semibold(13.0)),
        (TextStyle::Name(TITLE.into()), proportional(17.0)),
        (TextStyle::Name(STATUS.into()), proportional(19.0)),
        (TextStyle::Name(STATUS_LARGE.into()), proportional(24.0)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 8.0);
    s.spacing.button_padding = vec2(15.0, 6.0);
    s.spacing.interact_size.y = CONTROL_HEIGHT;
    // Room for every latency without scrolling the menu.
    s.spacing.combo_height = 360.0;
    s.spacing.menu_margin = Margin::same(8);
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
    s.visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    s.visuals.menu_corner_radius = CornerRadius::same(16);
    s.visuals.striped = false;
}

/// One appearance's colours.
fn colours(s: &mut Style, p: &Palette) {
    let v = &mut s.visuals;
    v.panel_fill = p.window;
    v.window_fill = p.popup;
    v.window_stroke = Stroke::new(1.0, p.rim);
    v.window_shadow = Shadow { offset: [0, 12], blur: 40, spread: 0, color: p.shadow };
    v.popup_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: p.shadow };
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.code_bg_color = p.code;
    v.faint_bg_color = p.stripe;
    v.weak_text_color = Some(p.text2);
    v.hyperlink_color = p.focus;
    v.warn_fg_color = p.amber_ink;
    v.error_fg_color = p.red_ink;
    v.selection.bg_fill = fade(p.focus, 0.45);
    v.selection.stroke = Stroke::new(1.0, p.text);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    // Capsules of glass: a tint and its rim.
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0, p.rim);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.bg_fill = p.control_hover;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.rim);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    // Pressed or focused: the focus ring.
    w.active.bg_fill = p.control_press;
    w.active.weak_bg_fill = p.control_press;
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

/// A headline: 13 pt semibold, for words to find at once.
pub fn headline(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(HEADLINE.into()))
}

/// A section's title: 17 pt regular.
pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(TITLE.into()))
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

    /// `top`, translucent, laid over the opaque `bottom`, as egui blends.
    fn over(top: Color32, bottom: Color32) -> Color32 {
        bottom.blend(top)
    }

    /// The wallpaper's darkest and lightest points.
    fn extremes(p: &Palette) -> [Color32; 2] {
        let image = wallpaper(p);
        let by = |a: &&Color32, b: &&Color32| luminance(**a).total_cmp(&luminance(**b));
        let darkest = *image.pixels.iter().min_by(by).expect("pixels");
        let lightest = *image.pixels.iter().max_by(by).expect("pixels");
        [darkest, lightest]
    }

    #[test]
    fn the_wallpaper_is_soft_and_opaque() {
        for p in [&DARK, &LIGHT] {
            let image = wallpaper(p);
            assert_eq!(image.size, [WALLPAPER_SIZE, WALLPAPER_SIZE]);
            assert!(image.pixels.iter().all(|c| c.a() == 255));
            // Neighbours differ by little: no edges for the stretch to show.
            let n = WALLPAPER_SIZE;
            for y in 0..n {
                for x in 1..n {
                    let (a, b) = (image.pixels[y * n + x - 1], image.pixels[y * n + x]);
                    let d = |u: u8, v: u8| u.abs_diff(v);
                    assert!(d(a.r(), b.r()).max(d(a.g(), b.g())).max(d(a.b(), b.b())) <= 3);
                }
            }
        }
    }

    #[test]
    fn text_and_controls_have_contrast() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for wall in extremes(p) {
                let name = &format!("{name} over {wall:?}");
                let card = over(p.card, wall);
                // Chips are card glass too; the show lock's badge is
                // control glass tinted with the focus colour.
                let badge = over(fade(p.focus, 0.30), over(p.control, wall));
                let popup = over(p.popup, card);
                // Text straight on the wallpaper: the header, titles, the
                // empty state.
                at_least(4.5, p.text, wall, &format!("{name}: text on the wallpaper"));
                at_least(4.5, p.text2, wall, &format!("{name}: secondary text on the wallpaper"));
                for (bg, on) in [
                    (card, "cards"),
                    (over(p.stripe, card), "stripes"),
                    (over(p.control, card), "buttons on cards"),
                    (over(p.field, card), "fields"),
                    (over(p.code, card), "code"),
                    (popup, "menus"),
                    (p.footer, "the bottom bar"),
                    (over(p.control, p.footer), "buttons on the bottom bar"),
                ] {
                    at_least(4.5, p.text, bg, &format!("{name}: text on {on}"));
                    at_least(4.5, p.text2, bg, &format!("{name}: secondary text on {on}"));
                }
                for light in [Light::Green, Light::Amber, Light::Red] {
                    // Coloured words in cards and in chips.
                    at_least(4.5, p.ink(light), card, &format!("{name}: {light:?} ink on cards"));
                    // Shapes against glass.
                    at_least(3.0, p.fill(light), card, &format!("{name}: {light:?} shape"));
                    // Text on glass tinted with a status: notices, tiles.
                    let tinted = over(p.tint(light), card);
                    at_least(4.5, p.text, tinted, &format!("{name}: text on {light:?} glass"));
                    at_least(4.5, p.text2, tinted, &format!("{name}: secondary on {light:?}"));
                }
                let alarm = over(p.alarm(), card);
                at_least(4.5, p.text, alarm, &format!("{name}: text on the alarm"));
                at_least(4.5, p.text2, alarm, &format!("{name}: secondary text on the alarm"));
                at_least(4.5, p.text, badge, &format!("{name}: the show lock's badge"));
                at_least(3.0, p.grey, card, &format!("{name}: grey shape"));
                // The edges of fields and of a switch that is off.
                at_least(3.0, p.control_stroke, card, &format!("{name}: field edge"));
                at_least(3.0, p.focus, card, &format!("{name}: focus ring"));
                at_least(3.0, p.switch, card, &format!("{name}: switch on"));
                // White on the accent and on red glass, wherever they lie.
                for bg in [wall, card] {
                    at_least(4.5, Color32::WHITE, over(p.accent, bg), &format!("{name}: accent"));
                    at_least(4.5, Color32::WHITE, over(p.urgent, bg), &format!("{name}: urgent"));
                }
            }
        }
    }
}
