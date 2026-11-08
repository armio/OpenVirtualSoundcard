//! Raven's glass (github.com/oscar-soirey/Raven), drawn with egui: a tint
//! that lets the wallpaper through, a sheen fading down from the top edge,
//! a rim that catches the light (brightest at the top), and a soft shadow
//! outside what floats. egui cannot blur what lies behind a surface; over
//! the wallpaper's soft blobs a tint looks the same as frosted glass.

use eframe::egui::epaint::tessellator::path::rounded_rectangle;
use eframe::egui::epaint::{CornerRadiusF32, PathShape, PathStroke, RectShape};
use eframe::egui::{
    Color32, Frame, InnerResponse, Margin, Mesh, Painter, Pos2, Rect, Shape, Ui, pos2, vec2,
};

use crate::theme::{self, Palette};

/// How far a surface floats above the wallpaper: how much shadow it casts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lift {
    /// Part of what it lies on: no shadow.
    Flat,
    /// A card or a tile.
    Raised,
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
    let mut shapes: Vec<Shape> = shadow(rect, radius, glass.lift, p.shadow).into_iter().collect();
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

/// A soft shadow outside `rect`, deeper below it than above: one mesh that
/// fades from the edge outwards, which the GPU shades without steps. It
/// lies outside the surface only, so that the glass, which lets what is
/// under it through, does not show it.
fn shadow(rect: Rect, radius: f32, lift: Lift, color: Color32) -> Option<Shape> {
    let (size, drop, strength) = match lift {
        Lift::Flat => return None,
        Lift::Raised => (12.0, 3.0, 0.5),
    };
    let edge = outline(rect, radius);
    // Rings from the edge (k = 0) out to nothing (k = 1), the light falling
    // off as (1 - k)²: each ring moves out along the edge's normal, and down.
    const RINGS: usize = 5;
    let mut mesh = Mesh::default();
    for ring in 0..RINGS {
        let k = ring as f32 / (RINGS - 1) as f32;
        let c = color.gamma_multiply(strength * (1.0 - k) * (1.0 - k));
        for &(at, normal) in &edge {
            mesh.colored_vertex(at + normal * (size * k) + vec2(0.0, drop * k), c);
        }
    }
    let n = edge.len() as u32;
    for ring in 0..RINGS as u32 - 1 {
        for i in 0..n {
            let (a, b) = (ring * n + i, ring * n + (i + 1) % n);
            mesh.add_triangle(a, b, a + n);
            mesh.add_triangle(b, b + n, a + n);
        }
    }
    Some(Shape::mesh(mesh))
}

/// The edge of `rect` with corners of `radius`, clockwise from the top
/// left, each point with its outward normal: a fixed number of points per
/// corner, also when the radius is 0.
fn outline(rect: Rect, radius: f32) -> Vec<(Pos2, eframe::egui::Vec2)> {
    const SEGMENTS: usize = 8;
    let r = radius.max(0.0);
    let corners = [
        (pos2(rect.left() + r, rect.top() + r), 180.0_f32),
        (pos2(rect.right() - r, rect.top() + r), 270.0),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
    ];
    let mut points = Vec::with_capacity(4 * (SEGMENTS + 1));
    for (centre, from) in corners {
        for i in 0..=SEGMENTS {
            let angle = (from + 90.0 * i as f32 / SEGMENTS as f32).to_radians();
            let normal = vec2(angle.cos(), angle.sin());
            points.push((centre + normal * r, normal));
        }
    }
    points
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
        // A raised panel: shadow, tint, sheen, rim.
        assert_eq!(count(&Glass::panel(p)), 4);
        // A tinted one adds its overlay.
        assert_eq!(count(&Glass::tinted(p, Color32::RED)), 5);
    }

    #[test]
    fn the_shadow_lies_outside_the_surface_and_fades_out() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 80.0));
        let Some(Shape::Mesh(mesh)) = shadow(rect, 20.0, Lift::Raised, Color32::BLACK) else {
            panic!("a mesh");
        };
        // Outside the rounded rectangle, or on its edge.
        for v in &mesh.vertices {
            let inner = rect.shrink(20.0);
            let nearest = pos2(
                v.pos.x.clamp(inner.left(), inner.right()),
                v.pos.y.clamp(inner.top(), inner.bottom()),
            );
            assert!(v.pos.distance(nearest) >= 20.0 - 0.01, "{:?} inside", v.pos);
        }
        // Darkest at the edge, nothing at the outer ring.
        let n = mesh.vertices.len() / 5;
        assert!(mesh.vertices[..n].iter().all(|v| v.color.a() > 0));
        assert!(mesh.vertices[4 * n..].iter().all(|v| v.color.a() == 0));
        // Deeper below than above.
        let bottom = mesh.vertices.iter().map(|v| v.pos.y).fold(f32::MIN, f32::max);
        let top = mesh.vertices.iter().map(|v| v.pos.y).fold(f32::MAX, f32::min);
        assert!(bottom - 80.0 > 0.0 - top);
        // No shadow for what lies flat.
        assert!(shadow(rect, 20.0, Lift::Flat, Color32::BLACK).is_none());
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
