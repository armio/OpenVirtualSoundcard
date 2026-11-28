//! OpenVirtualSoundcard: the app that shows what the OpenVirtualSoundcard daemon is doing and
//! changes its settings, through the daemon's control socket
//! (`ovsc_control::SOCKET_PATH`, or `OVSC_CONTROL_SOCKET` for
//! testing; `OVSC_APP_THEME=light|dark` forces an appearance).

mod input;
mod logic;
mod model;
mod place;
mod settings;
mod status;
mod theme;
mod view;
mod widgets;
mod worker;

use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowOptions, px, size,
};

use crate::model::{APP_NAME, Leave};
use crate::view::{AppView, Cancel, CloseWindow, Minimize, Quit, ShowSettings, ShowStatus};

/// Fits a 13-inch MacBook beside a plug-in host; the window's size and
/// place are remembered after that.
const SIZE: (f32, f32) = (760.0, 720.0);
const MIN_SIZE: (f32, f32) = (520.0, 420.0);

fn main() {
    let socket = std::env::var_os("OVSC_CONTROL_SOCKET")
        .map_or_else(|| PathBuf::from(ovsc_control::SOCKET_PATH), PathBuf::from);
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-1", ShowStatus, None),
            KeyBinding::new("cmd-2", ShowSettings, None),
            KeyBinding::new("cmd-,", ShowSettings, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("escape", Cancel, None),
            // In a dialog, Return gives the safe answer: Cancel, Keep
            // editing.
            KeyBinding::new("enter", Cancel, Some("Modal")),
        ]);
        cx.bind_keys(input::key_bindings());
        cx.set_menus(menus());

        let bounds = place::load(cx)
            .unwrap_or_else(|| gpui::Bounds::centered(None, size(px(SIZE.0), px(SIZE.1)), cx));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(APP_NAME.into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
            app_id: Some("org.openvirtualsoundcard.app".into()),
            ..Default::default()
        };
        let window = cx
            .open_window(options, |window, cx| {
                cx.new(|cx| AppView::new(socket.clone(), window, cx))
            })
            .expect("cannot open the window");

        // Quit asks the window first about edits not applied; without the
        // window, it quits.
        cx.on_action(move |_: &Quit, cx| {
            if window.update(cx, |view, window, cx| view.leave(Leave::Quit, window, cx)).is_err() {
                cx.quit();
            }
        });
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        // One window: closing it ends the app.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

gpui::actions!(ovsc_app, [Hide, HideOthers, ShowAll]);

/// The menu bar, with the key equivalents bound above.
fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: APP_NAME.into(),
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide OpenVirtualSoundcard", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit OpenVirtualSoundcard", Quit),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Cut", input::Cut),
                MenuItem::action("Copy", input::Copy),
                MenuItem::action("Paste", input::Paste),
                MenuItem::action("Select All", input::SelectAll),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Status", ShowStatus),
                MenuItem::action("Settings", ShowSettings),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Close", CloseWindow),
            ],
        },
    ]
}
