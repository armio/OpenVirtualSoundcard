//! The window's size and place, remembered across launches in a small
//! file: `~/Library/Application Support/OpenVirtualSoundcard/window` on
//! macOS, `$XDG_CONFIG_HOME/openvirtualsoundcard/window` elsewhere.
//!
//! GPUI gives a window's place as the top-left corner of its frame,
//! relative to its display, and opens a window at such a place with the
//! size of its contents (without the title bar). So the file holds that
//! corner, the size of the contents, and the display.

use std::path::PathBuf;

use gpui::{App, Bounds, DisplayId, Pixels, Size, Window, point, px, size};

use crate::{MIN_SIZE, model::APP_NAME};

fn file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        Some(home?.join("Library/Application Support").join(APP_NAME).join("window"))
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".config")))?;
        Some(config.join("openvirtualsoundcard").join("window"))
    }
}

/// A saved place: the frame's corner on its display, the contents' size,
/// and the display's identity, if it has one.
#[derive(Debug, PartialEq)]
struct Place {
    bounds: Bounds<Pixels>,
    display: Option<String>,
}

/// The place saved last, and its display, if that display is still there
/// and the window would show on it.
pub fn load(cx: &App) -> Option<(Bounds<Pixels>, Option<DisplayId>)> {
    let text = std::fs::read_to_string(file()?).ok()?;
    let place = parse(&text)?;
    let displays = cx.displays();
    let display = match &place.display {
        Some(uuid) => Some(
            displays
                .iter()
                .find(|d| d.uuid().ok().is_some_and(|u| u.to_string() == *uuid))?
                .clone(),
        ),
        None => cx.primary_display(),
    }?;
    on_display(place.bounds, display.bounds().size).then_some((place.bounds, Some(display.id())))
}

/// Whether a window at `bounds` (relative to its display) shows enough of
/// its top to be grabbed on a display of `screen` size.
fn on_display(bounds: Bounds<Pixels>, screen: Size<Pixels>) -> bool {
    let (x, y) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let (w, sw, sh) =
        (f32::from(bounds.size.width), f32::from(screen.width), f32::from(screen.height));
    x + w > 80.0 && x < sw - 80.0 && y >= 0.0 && y < sh - 40.0
}

/// Saves the window's place (its place when not zoomed or full screen).
pub fn save(window: &Window, cx: &App) {
    let Some(path) = file() else { return };
    let frame = window.window_bounds().get_bounds();
    let contents = window.viewport_size();
    let display = window.display(cx).and_then(|d| d.uuid().ok()).map(|u| u.to_string());
    let place = Place { bounds: Bounds::new(frame.origin, contents), display };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Not worth an error: the window opens in the middle next time.
    let _ = std::fs::write(path, format(&place));
}

fn format(place: &Place) -> String {
    let b = place.bounds;
    let mut text = format!(
        "{} {} {} {}",
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height)
    );
    if let Some(display) = &place.display {
        text.push(' ');
        text.push_str(display);
    }
    text.push('\n');
    text
}

/// `x y width height [display]`, the size at least the window's minimum.
fn parse(text: &str) -> Option<Place> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let (numbers, display) = match words.len() {
        4 => (&words[..], None),
        5 => (&words[..4], Some(words[4].to_owned())),
        _ => return None,
    };
    let v: Vec<f32> = numbers.iter().map(|w| w.parse()).collect::<Result<_, _>>().ok()?;
    let [x, y, w, h] = v[..] else { return None };
    if !v.iter().all(|n| n.is_finite()) {
        return None;
    }
    let bounds =
        Bounds::new(point(px(x), px(y)), size(px(w.max(MIN_SIZE.0)), px(h.max(MIN_SIZE.1))));
    Some(Place { bounds, display })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_read_back() {
        let place = Place {
            bounds: Bounds::new(point(px(10.0), px(20.5)), size(px(800.0), px(600.0))),
            display: Some("37D8832A-2D66-02CA-B9F7-8F30A301B230".into()),
        };
        assert_eq!(parse(&format(&place)), Some(place));
        let old = parse("10 20 800 600\n").expect("a place without a display");
        assert_eq!(old.display, None);
        // Never smaller than the window may be.
        assert_eq!(parse("0 0 100 100").expect("a place").bounds.size, size(px(520.0), px(420.0)));
        for bad in ["", "1 2 3", "1 2 3 4 5 6", "a b c d", "NaN 0 800 600"] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_place_must_be_on_its_display() {
        let screen = size(px(1440.0), px(900.0));
        let at = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(760.0), px(720.0)));
        assert!(on_display(at(100.0, 40.0), screen));
        assert!(on_display(at(1300.0, 800.0), screen), "a little still shows");
        assert!(!on_display(at(1400.0, 40.0), screen));
        assert!(!on_display(at(-700.0, 40.0), screen));
        assert!(!on_display(at(100.0, -10.0), screen), "the title bar off the top");
        assert!(!on_display(at(100.0, 880.0), screen));
    }
}
