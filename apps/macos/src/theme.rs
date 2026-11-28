//! The look: sober and native, as macOS draws its own settings. Grouped
//! cards on a plain window, rounded-rect controls, the system font, and
//! colour only where it means something: the status, a warning, the accent.
//! Every colour pair used for text or controls is checked for contrast in
//! the tests below.

use gpui::{Background, Fill, FontWeight, Hsla, Rgba, SharedString, Window, WindowAppearance};

use crate::logic::Light;

/// Spacing on a 4 pt grid.
pub const GUTTER: f32 = 20.0;
pub const CARD_PAD: f32 = 14.0;
pub const SECTION_GAP: f32 = 20.0;
pub const TITLE_GAP: f32 = 8.0;
pub const COLUMN_GAP: f32 = 16.0;

/// Corner radii: cards and tiles, multi-line fields, and controls.
pub const CARD_RADIUS: f32 = 10.0;
pub const FIELD_RADIUS: f32 = 7.0;
pub const CONTROL_RADIUS: f32 = 6.0;

/// The height of buttons, menus, fields and table rows.
pub const CONTROL_HEIGHT: f32 = 28.0;

/// Text sizes: the body, captions, section titles and the device's name.
pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 11.0;
pub const TITLE: f32 = 15.0;
pub const HEADING: f32 = 22.0;

pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;

/// The system font, SF Pro on macOS, and a monospaced one for commands
/// and counters. Both come with the system; nothing is bundled.
#[cfg(target_os = "macos")]
pub const SANS: &str = ".SystemUIFont";
#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
pub const SANS: &str = "DejaVu Sans";
#[cfg(not(target_os = "macos"))]
pub const MONO: &str = "DejaVu Sans Mono";

pub fn sans() -> SharedString {
    SANS.into()
}

pub fn mono() -> SharedString {
    MONO.into()
}

/// A colour in 8-bit sRGB with alpha: exact, so the tests can check its
/// contrast as it is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

pub const fn rgb(hex: u32) -> Color {
    rgba(hex, 255)
}

/// `hex` at `alpha` (0–255) opacity.
pub const fn rgba(hex: u32, alpha: u8) -> Color {
    Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: alpha }
}

pub const WHITE: Color = rgb(0xFFFFFF);

impl Color {
    /// This colour at `alpha` (0–1) of its own opacity.
    pub fn fade(self, alpha: f32) -> Color {
        Color { a: (f32::from(self.a) * alpha.clamp(0.0, 1.0)).round() as u8, ..self }
    }

    /// This colour, translucent, laid over the opaque `bottom`.
    pub fn over(self, bottom: Color) -> Color {
        let a = f32::from(self.a) / 255.0;
        let mix = |t: u8, b: u8| (f32::from(t) * a + f32::from(b) * (1.0 - a)).round() as u8;
        Color {
            r: mix(self.r, bottom.r),
            g: mix(self.g, bottom.g),
            b: mix(self.b, bottom.b),
            a: 255,
        }
    }
}

impl From<Color> for Rgba {
    fn from(c: Color) -> Rgba {
        let f = |v: u8| f32::from(v) / 255.0;
        Rgba { r: f(c.r), g: f(c.g), b: f(c.b), a: f(c.a) }
    }
}

impl From<Color> for Hsla {
    fn from(c: Color) -> Hsla {
        Rgba::from(c).into()
    }
}

impl From<Color> for Background {
    fn from(c: Color) -> Background {
        Rgba::from(c).into()
    }
}

impl From<Color> for Fill {
    fn from(c: Color) -> Fill {
        Rgba::from(c).into()
    }
}

/// The colours of one appearance. Colours with alpha lie over a card, or
/// over the window.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub window: Color,
    /// A card, its edge, and the shadow of what floats (menus, dialogs).
    pub card: Color,
    pub card_edge: Color,
    pub shadow: Color,
    /// Menus and dialogs.
    pub popup: Color,
    /// The bar docked at the bottom of the Settings tab.
    pub footer: Color,
    /// Primary, secondary and tertiary text.
    pub text: Color,
    pub text2: Color,
    pub text3: Color,
    /// Controls: buttons, menus, the segmented control's track.
    pub control: Color,
    pub control_hover: Color,
    pub control_press: Color,
    /// The selected segment of the segmented control.
    pub segment: Color,
    /// The edge of a text field and of a switch that is off: 3:1 against
    /// the card around it.
    pub control_stroke: Color,
    pub field: Color,
    pub code: Color,
    pub stripe: Color,
    pub separator: Color,
    /// The default button, under white text.
    pub accent: Color,
    pub accent_hover: Color,
    /// Focus rings, links, the selection and the text cursor.
    pub focus: Color,
    pub selection: Color,
    /// A switch that is on.
    pub switch: Color,
    /// Status colours for shapes: marks, bars.
    pub green: Color,
    pub amber: Color,
    pub red: Color,
    pub grey: Color,
    /// Status colours for text.
    pub green_ink: Color,
    pub amber_ink: Color,
    pub red_ink: Color,
    /// Red under white text: the down state, and buttons that interrupt
    /// audio.
    pub urgent: Color,
    pub urgent_hover: Color,
    /// The dimming behind a dialog.
    pub scrim: Color,
    /// How much of a status colour tints a notice or a tile.
    pub tint: f32,
}

/// Dark: graphite, as macOS's own dark appearance.
pub const DARK: Palette = Palette {
    window: rgb(0x1E1E20),
    card: rgb(0x2A2A2D),
    card_edge: rgba(0xFFFFFF, 18),
    shadow: rgba(0x000000, 140),
    popup: rgb(0x323236),
    footer: rgb(0x252528),
    text: rgb(0xF5F5F7),
    text2: rgb(0xBDBDC4),
    text3: rgb(0x8A8A92),
    control: rgba(0x787880, 70),
    control_hover: rgba(0x787880, 88),
    control_press: rgba(0x787880, 112),
    segment: rgb(0x636366),
    control_stroke: rgb(0x8E8E96),
    field: rgb(0x1C1C1E),
    code: rgba(0x000000, 70),
    stripe: rgba(0xFFFFFF, 10),
    separator: rgba(0xFFFFFF, 26),
    accent: rgb(0x0A64D0),
    accent_hover: rgb(0x1270DC),
    focus: rgb(0x0A84FF),
    selection: rgba(0x0A84FF, 90),
    switch: rgb(0x30D158),
    green: rgb(0x30D158),
    amber: rgb(0xFFD60A),
    red: rgb(0xFF453A),
    grey: rgb(0x8E8E93),
    green_ink: rgb(0x5CE07E),
    amber_ink: rgb(0xFFD60A),
    red_ink: rgb(0xFF9F97),
    urgent: rgb(0xC4000F),
    urgent_hover: rgb(0xD0101E),
    scrim: rgba(0x000000, 110),
    tint: 0.18,
};

/// Light: soft greys and white cards.
pub const LIGHT: Palette = Palette {
    window: rgb(0xECECEF),
    card: rgb(0xFFFFFF),
    card_edge: rgba(0x000000, 20),
    shadow: rgba(0x1C1C22, 60),
    popup: rgb(0xFFFFFF),
    footer: rgb(0xF7F7F8),
    text: rgb(0x1C1C1E),
    text2: rgb(0x4A4A51),
    text3: rgb(0x75757D),
    control: rgba(0x787880, 36),
    control_hover: rgba(0x787880, 56),
    control_press: rgba(0x787880, 80),
    segment: rgb(0xFFFFFF),
    control_stroke: rgb(0x85858D),
    field: rgb(0xFFFFFF),
    code: rgba(0x3C3C43, 20),
    stripe: rgba(0x3C3C43, 11),
    separator: rgba(0x3C3C43, 36),
    accent: rgb(0x0062CC),
    accent_hover: rgb(0x0A6CD6),
    focus: rgb(0x007AFF),
    selection: rgba(0x007AFF, 70),
    switch: rgb(0x1F8F40),
    green: rgb(0x248A3D),
    amber: rgb(0xB25000),
    red: rgb(0xD70015),
    grey: rgb(0x85858D),
    green_ink: rgb(0x1A6E31),
    amber_ink: rgb(0x8F4200),
    red_ink: rgb(0xC0000E),
    urgent: rgb(0xC4000F),
    urgent_hover: rgb(0xD0101E),
    scrim: rgba(0x000000, 60),
    tint: 0.14,
};

/// Whether the window shows the dark appearance: the system's, unless
/// `OVSC_APP_THEME=light` or `dark` forces one (a testing hook, like
/// `OVSC_CONTROL_SOCKET`).
pub fn is_dark(window: &Window) -> bool {
    match std::env::var("OVSC_APP_THEME").as_deref() {
        Ok("light") => false,
        Ok("dark") => true,
        _ => matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark),
    }
}

/// The palette of the appearance `window` shows.
pub fn palette(window: &Window) -> &'static Palette {
    palette_of(is_dark(window))
}

pub fn palette_of(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

impl Palette {
    /// A light's colour for shapes.
    pub fn fill(&self, light: Light) -> Color {
        match light {
            Light::Green => self.green,
            Light::Amber => self.amber,
            Light::Red => self.red,
            Light::Grey => self.grey,
        }
    }

    /// A light's colour for text.
    pub fn ink(&self, light: Light) -> Color {
        match light {
            Light::Green => self.green_ink,
            Light::Amber => self.amber_ink,
            Light::Red => self.red_ink,
            Light::Grey => self.text2,
        }
    }

    /// A card tinted with a light's colour: notices and tiles.
    pub fn tinted(&self, light: Light) -> Color {
        self.fill(light).fade(self.tint).over(self.card)
    }

    /// The card of the notice that audio is down: a little more red than a
    /// tint.
    pub fn alarm(&self) -> Color {
        self.red.fade(self.tint + 0.08).over(self.card)
    }

    /// The show lock's badge: the focus colour over a control.
    pub fn badge(&self) -> Color {
        self.focus.fade(0.30).over(self.control.over(self.window))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an opaque sRGB colour.
    fn luminance(c: Color) -> f64 {
        let ch = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[track_caller]
    fn at_least(ratio: f64, fg: Color, bg: Color, what: &str) {
        assert_eq!(bg.a, 255, "{what}: the background must be opaque");
        let c = contrast(fg, bg);
        assert!(c >= ratio, "{what}: {c:.2}:1 < {ratio}:1 ({fg:?} on {bg:?})");
    }

    #[test]
    fn colours_blend_as_drawn() {
        assert_eq!(rgba(0xFFFFFF, 0).over(rgb(0x102030)), rgb(0x102030));
        assert_eq!(rgb(0xABCDEF).over(rgb(0x102030)), rgb(0xABCDEF));
        assert_eq!(rgba(0xFFFFFF, 128).over(rgb(0x000000)), rgb(0x808080));
        assert_eq!(rgb(0x123456).fade(0.5).a, 128);
        let h: Hsla = rgb(0xFF0000).into();
        assert!((h.s - 1.0).abs() < 1e-6 && h.h.abs() < 1e-6);
    }

    #[test]
    fn text_and_controls_have_contrast() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for opaque in [p.window, p.card, p.popup, p.footer, p.field, p.segment, p.urgent] {
                assert_eq!(opaque.a, 255, "{name}: {opaque:?} is drawn opaque");
            }
            // Text straight on the window: the header, titles, the empty
            // state.
            at_least(4.5, p.text, p.window, &format!("{name}: text on the window"));
            at_least(4.5, p.text2, p.window, &format!("{name}: secondary text on the window"));
            for (bg, on) in [
                (p.card, "cards"),
                (p.stripe.over(p.card), "stripes"),
                (p.control.over(p.card), "buttons on cards"),
                (p.control_press.over(p.card), "pressed buttons"),
                (p.field, "fields"),
                (p.code.over(p.card), "code"),
                (p.popup, "menus"),
                (p.control_hover.over(p.popup), "the hovered item of a menu"),
                (p.footer, "the bottom bar"),
                (p.control.over(p.footer), "buttons on the bottom bar"),
                (p.control.over(p.window), "the tabs' track"),
            ] {
                at_least(4.5, p.text, bg, &format!("{name}: text on {on}"));
                at_least(4.5, p.text2, bg, &format!("{name}: secondary text on {on}"));
            }
            // Only primary text lies on these.
            at_least(4.5, p.text, p.segment, &format!("{name}: the selected tab"));
            at_least(4.5, p.text, p.badge(), &format!("{name}: the show lock's badge"));
            for light in [Light::Green, Light::Amber, Light::Red] {
                // Coloured words in cards, and on the bottom bar.
                at_least(4.5, p.ink(light), p.card, &format!("{name}: {light:?} ink on cards"));
                at_least(4.5, p.ink(light), p.footer, &format!("{name}: {light:?} ink on the bar"));
                // Shapes against cards and the window.
                at_least(3.0, p.fill(light), p.card, &format!("{name}: {light:?} shape"));
                at_least(3.0, p.fill(light), p.window, &format!("{name}: {light:?} on the window"));
                // Text on tinted cards: notices, tiles.
                let tinted = p.tinted(light);
                at_least(4.5, p.text, tinted, &format!("{name}: text on {light:?} cards"));
                at_least(4.5, p.text2, tinted, &format!("{name}: secondary on {light:?}"));
                at_least(
                    3.0,
                    p.fill(light),
                    tinted,
                    &format!("{name}: {light:?} mark on its tint"),
                );
            }
            at_least(4.5, p.text, p.alarm(), &format!("{name}: text on the alarm"));
            at_least(4.5, p.text2, p.alarm(), &format!("{name}: secondary text on the alarm"));
            at_least(3.0, p.grey, p.card, &format!("{name}: grey shape"));
            // The edges of fields and of a switch that is off.
            at_least(3.0, p.control_stroke, p.card, &format!("{name}: field edge"));
            at_least(3.0, p.focus, p.card, &format!("{name}: focus ring"));
            at_least(3.0, p.focus, p.footer, &format!("{name}: focus ring on the bar"));
            at_least(3.0, p.switch, p.card, &format!("{name}: switch on"));
            // White on the accent and on red, also when hovered.
            for bg in [p.accent, p.accent_hover, p.urgent, p.urgent_hover] {
                at_least(4.5, WHITE, bg, &format!("{name}: white on {bg:?}"));
            }
            // The secondary words of a red tile.
            at_least(4.5, rgb(0xEBEBEB), p.urgent, &format!("{name}: words on a red tile"));
        }
    }
}
