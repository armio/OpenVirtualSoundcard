//! OpenVirtualSoundcard: the app that shows what the OpenVirtualSoundcard daemon is doing and
//! changes its settings, through the daemon's control socket
//! (`ovsc_control::SOCKET_PATH`, or `OVSC_CONTROL_SOCKET` for
//! testing).

mod app;
mod logic;
mod worker;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    let socket = std::env::var_os("OVSC_CONTROL_SOCKET")
        .map_or_else(|| PathBuf::from(ovsc_control::SOCKET_PATH), PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OpenVirtualSoundcard")
            .with_icon(icon())
            .with_app_id("org.openvirtualsoundcard.app")
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([520.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OpenVirtualSoundcard",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, socket)))),
    )
}

/// The window's and the Dock's icon: the same artwork as the bundle's
/// (`icon/`). Without it eframe would show egui's logo in the Dock.
fn icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../icon/OpenVirtualSoundcard-512.png"))
        .expect("the icon is a valid PNG")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_icon_loads() {
        let icon = super::icon();
        assert_eq!((icon.width, icon.height), (512, 512));
    }
}
