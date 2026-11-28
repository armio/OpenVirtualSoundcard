//! The window's size and place, remembered across launches in a small
//! file: `~/Library/Application Support/OpenVirtualSoundcard/window` on
//! macOS, `$XDG_CONFIG_HOME/openvirtualsoundcard/window` elsewhere.

use std::path::PathBuf;

use gpui::{App, Bounds, Pixels, Window, point, px, size};

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

/// The place saved last, if it is still on a display.
pub fn load(cx: &App) -> Option<Bounds<Pixels>> {
    let text = std::fs::read_to_string(file()?).ok()?;
    let bounds = parse(&text)?;
    let on_screen = cx.displays().iter().any(|d| d.bounds().contains(&bounds.center()));
    on_screen.then_some(bounds)
}

/// Saves the window's place (its place when not zoomed or full screen).
pub fn save(window: &Window) {
    let Some(path) = file() else { return };
    let b = window.window_bounds().get_bounds();
    let text = format!(
        "{} {} {} {}\n",
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height)
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Not worth an error: the window opens in the middle next time.
    let _ = std::fs::write(path, text);
}

/// `x y width height`, the size at least the window's minimum.
fn parse(text: &str) -> Option<Bounds<Pixels>> {
    let v: Vec<f32> = text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let [x, y, w, h] = v[..] else { return None };
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return None;
    }
    Some(Bounds::new(point(px(x), px(y)), size(px(w.max(MIN_SIZE.0)), px(h.max(MIN_SIZE.1)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_read_back() {
        let b = parse("10 20.5 800 600\n").expect("a place");
        assert_eq!(b.origin, point(px(10.0), px(20.5)));
        assert_eq!(b.size, size(px(800.0), px(600.0)));
        // Never smaller than the window may be.
        assert_eq!(parse("0 0 100 100").expect("a place").size, size(px(520.0), px(420.0)));
        for bad in ["", "1 2 3", "1 2 3 4 5", "a b c d", "NaN 0 800 600"] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }
}
