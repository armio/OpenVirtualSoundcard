//! Raven's glass (github.com/oscar-soirey/Raven), drawn with egui: a tint
//! that lets the wallpaper through, a sheen fading down from the top edge,
//! a rim that catches the light (brightest at the top), and a soft shadow
//! outside what floats. egui cannot blur what lies behind a surface; over
//! the wallpaper's soft blobs a tint looks the same as frosted glass.

use eframe::egui::epaint::tessellator::path::rounded_rectangle;
use eframe::egui::epaint::{CornerRadiusF32, PathShape, PathStroke, RectShape};
use eframe::egui::{
    Color32, Frame, InnerResponse, Margin, Mesh, Painter, Rect, Shape, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::theme::{self, Palette};

/// How far a surface floats above the wallpaper: how much shadow it casts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lift {
    /// Part of what it lies on: no shadow.
    Flat,
    /// A card or a tile.
    Raised,
    /// A bar that floats over the page, a dialog.
    Floating,
}

/// A surface of glass.
#[derive(Clone, Copy, Debug)]
pub struct Glass {
    /// The tint, and a second colour laid over it (a status), both
    /// translucent.
    pub tint: Color32,
    pub overlay: Color32,
    /// How bright the rim and the sheen are, 1 as designed.
    pub rim: f32,
    pub sheen: f32,
    pub lift: Lift,
}

impl Glass {
    /// A panel: a card, a tile.
    pub fn panel(p: &Palette) -> Glass {
        Glass {
            tint: p.card,
            overlay: Color32::TRANSPARENT,
            rim: 1.0,
            sheen: 1.0,
            lift: Lift::Raised,
        }
    }

    /// A panel tinted with a colour: a notice, a tile with a status.
    pub fn tinted(p: &Palette, overlay: Color32) -> Glass {
        Glass { overlay, ..Glass::panel(p) }
    }

    /// Tinted glass: the colour dominates, the light still plays on it.
    pub fn solid(color: Color32) -> Glass {
        Glass {
            tint: color,
            overlay: Color32::TRANSPARENT,
            rim: 0.75,
            sheen: 2.2,
            lift: Lift::Raised,
        }
    }

    /// A bar that floats over the page.
    pub fn bar(p: &Palette) -> Glass {
        Glass { rim: 0.8, lift: Lift::Floating, ..Glass::panel(p) }
    }

    /// A capsule control: a chip, a segmented control's track.
    pub fn control(p: &Palette) -> Glass {
        Glass {
            tint: p.control,
            overlay: Color32::TRANSPARENT,
            rim: 0.7,
            sheen: 0.8,
            lift: Lift::Flat,
        }
    }

    /// A field: darker or clearer glass, a quieter rim.
    pub fn field(p: &Palette) -> Glass {
        Glass {
            tint: p.field,
            overlay: Color32::TRANSPARENT,
            rim: 0.55,
            sheen: 0.0,
            lift: Lift::Flat,
        }
    }
}

/// The shapes of `glass` in `rect`, with corners of `radius` (clamped to a
/// capsule).
pub fn shapes(rect: Rect, radius: f32, glass: &Glass, p: &Palette) -> Vec<Shape> {
    let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let mut shapes = shadow(rect, radius, glass.lift, p.shadow);
    shapes.push(Shape::Rect(RectShape::filled(rect, radius, glass.tint)));
    if glass.overlay != Color32::TRANSPARENT {
        shapes.push(Shape::Rect(RectShape::filled(rect, radius, glass.overlay)));
    }
    if glass.sheen > 0.0 {
        shapes.push(sheen(rect, radius, scale(p.sheen, glass.sheen)));
    }
    shapes.push(rim(rect, radius, scale(p.rim, glass.rim), scale(p.rim_low, glass.rim)));
    shapes
}

/// Paints `glass` in `rect`.
pub fn paint(painter: &Painter, rect: Rect, radius: f32, glass: &Glass, p: &Palette) {
    painter.extend(shapes(rect, radius, glass, p));
}

/// Shows `add` on `glass`, inside `margin`, the glass sized to what it
/// holds.
pub fn frame<R>(
    ui: &mut Ui,
    radius: f32,
    margin: Margin,
    glass: Glass,
    add: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let p = theme::palette(ui);
    // The glass goes under the content, once its size is known.
    let below = ui.painter().add(Shape::Noop);
    let inner = Frame::new().inner_margin(margin).show(ui, add);
    ui.painter().set(below, Shape::Vec(shapes(inner.response.rect, radius, &glass, p)));
    inner
}

/// `color` with its opacity multiplied by `k` (above 1 brightens, to full).
fn scale(color: Color32, k: f32) -> Color32 {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    let a = (f32::from(a) * k).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// A soft shadow outside `rect`, slightly lower, in rings: drawn under the
/// whole surface it would show through the glass.
fn shadow(rect: Rect, radius: f32, lift: Lift, color: Color32) -> Vec<Shape> {
    let (size, drop, strength) = match lift {
        Lift::Flat => return Vec::new(),
        Lift::Raised => (10.0, 2.0, 0.55),
        Lift::Floating => (22.0, 6.0, 0.8),
    };
    let steps = 8;
    let width = size / steps as f32;
    let rect = rect.translate(vec2(0.0, drop));
    (0..steps)
        .map(|i| {
            let out = width * (i as f32 + 0.5);
            let k = 1.0 - (i as f32 + 0.5) / steps as f32;
            let c = color.gamma_multiply(strength * k * k);
            Shape::Rect(RectShape::stroke(
                rect.expand(out),
                radius + out,
                Stroke::new(width, c),
                StrokeKind::Middle,
            ))
        })
        .collect()
}

/// A sheen: `top` under the top edge, fading out by the middle, within the
/// rounded corners.
fn sheen(rect: Rect, radius: f32, top: Color32) -> Shape {
    let depth = (rect.height() * 0.55).min(48.0);
    let steps = 14;
    let mut mesh = Mesh::default();
    // How far the corners cut in at height `y`.
    let inset = |y: f32| {
        let from_top = y - rect.top();
        let from_bottom = rect.bottom() - y;
        let cut = |d: f32| {
            if d < radius {
                let k = radius - d;
                radius - (radius * radius - k * k).max(0.0).sqrt()
            } else {
                0.0
            }
        };
        cut(from_top).max(cut(from_bottom))
    };
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let y = rect.top() + 1.0 + t * depth;
        let x = inset(y) + 0.5;
        let c = top.gamma_multiply((1.0 - t) * (1.0 - t));
        mesh.colored_vertex(pos2(rect.left() + x, y), c);
        mesh.colored_vertex(pos2(rect.right() - x, y), c);
        if i > 0 {
            let k = 2 * i as u32;
            mesh.add_triangle(k - 2, k - 1, k);
            mesh.add_triangle(k - 1, k, k + 1);
        }
    }
    Shape::mesh(mesh)
}

/// The rim: a hairline round the edge, `top` at the top fading to
/// `bottom`.
fn rim(rect: Rect, radius: f32, top: Color32, bottom: Color32) -> Shape {
    let mut points = Vec::new();
    rounded_rectangle(
        &mut points,
        rect.shrink(0.5),
        CornerRadiusF32::same((radius - 0.5).max(0.0)),
    );
    let stroke = PathStroke::new_uv(1.0, move |r: Rect, at| {
        let t = ((at.y - r.top()) / r.height().max(1.0)).clamp(0.0, 1.0);
        // The light falls from above: bright for the top third.
        let t = (t * 1.5 - 0.25).clamp(0.0, 1.0);
        top.lerp_to_gamma(bottom, t)
    });
    Shape::Path(PathShape::closed_line(points, stroke))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_is_tint_sheen_rim_and_a_shadow_when_it_floats() {
        let p = &theme::DARK;
        let rect = Rect::from_min_size(pos2(10.0, 10.0), vec2(200.0, 80.0));
        let count = |g: &Glass| shapes(rect, 20.0, g, p).len();
        // A flat field: tint and rim.
        assert_eq!(count(&Glass::field(p)), 2);
        // A raised panel: eight rings of shadow, tint, sheen, rim.
        assert_eq!(count(&Glass::panel(p)), 11);
        // A tinted one adds its overlay.
        assert_eq!(count(&Glass::tinted(p, Color32::RED)), 12);
    }

    #[test]
    fn the_sheen_stays_inside_the_corners() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 40.0));
        let Shape::Mesh(mesh) = sheen(rect, 20.0, Color32::WHITE) else {
            panic!("a mesh");
        };
        for v in &mesh.vertices {
            assert!(rect.contains(v.pos), "{:?}", v.pos);
            // Inside the capsule: within the radius of the nearest end's
            // centre.
            let cx = v.pos.x.clamp(20.0, 80.0);
            assert!(v.pos.distance(pos2(cx, 20.0)) <= 20.0 + 0.01, "{:?}", v.pos);
        }
    }

    #[test]
    fn scaling_brightens_up_to_full() {
        let c = Color32::from_rgba_unmultiplied(255, 255, 255, 100);
        assert_eq!(scale(c, 2.0).a(), 200);
        assert_eq!(scale(c, 3.0).a(), 255);
        assert_eq!(scale(c, 0.0).a(), 0);
    }
}
