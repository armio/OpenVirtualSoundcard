//! The window: the device and its status tiles at the top, alerts that
//! never scroll away under them, then the Status or the Settings tab, and
//! the dialogs. It wakes for each event from the worker and every second,
//! also while it is covered or minimised, so that its title and the Dock
//! say when audio is down.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, actions, div, prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::logic;
use crate::model::{Connection, Leave, Model, Outcome, Tab};
use crate::theme::{self, GUTTER, Palette, SEMIBOLD};
use crate::widgets::{self, Kind, Severity, Tooltip};
use crate::worker::Worker;
use crate::{place, settings, status};

actions!(
    ovsc,
    [
        /// Shows the Status tab (Cmd-1).
        ShowStatus,
        /// Shows the Settings tab (Cmd-2, Cmd-comma).
        ShowSettings,
        /// Closes the window (Cmd-W), asking first about edits not applied.
        CloseWindow,
        /// Minimises the window (Cmd-M).
        Minimize,
        /// Quits (Cmd-Q), asking first about edits not applied.
        Quit,
        /// Closes a menu or a dialog (Escape; Return in a dialog, whose
        /// default answer is the safe one).
        Cancel,
    ]
);

/// How long a Copy button says `Copied`.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// The pop-up menus of the Settings tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Menu {
    Interface,
    Secondary,
    Rate,
    Bits,
    Rx,
    Tx,
    Latency,
}

pub struct AppView {
    pub model: Model,
    worker: Worker,
    focus: FocusHandle,
    /// Holds the focus while a dialog is open, for Escape and Return.
    modal_focus: FocusHandle,
    pub name_input: Entity<TextInput>,
    pub open_menu: Option<Menu>,
    /// The menu just closed by a click outside it: the same click on its
    /// button must not open it again.
    menu_closed: Option<(Menu, Instant)>,
    /// What was copied last, and when.
    copied: Option<(String, Instant)>,
    pub diagnostics_open: bool,
    scroll_status: ScrollHandle,
    scroll_settings: ScrollHandle,
    /// The window title set last, and whether the Dock was asked for
    /// attention for the current outage.
    title: String,
    alerted: bool,
    attention: Option<isize>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    pub fn new(socket: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> AppView {
        // The worker wakes the window through a channel: one task drains
        // it, another ticks every second.
        let (wake, mut woken) = futures::channel::mpsc::unbounded::<()>();
        let worker = Worker::start(socket, move || {
            let _ = wake.unbounded_send(());
        });
        let events = cx.spawn_in(window, async move |this, cx| {
            while woken.next().await.is_some() {
                while let Ok(Some(())) = woken.try_next() {}
                if this.update_in(cx, |view, window, cx| view.refresh(window, cx)).is_err() {
                    break;
                }
            }
        });
        let ticks = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update_in(cx, |view, window, cx| view.refresh(window, cx)).is_err() {
                    break;
                }
            }
        });

        let name_input = cx.new(|cx| TextInput::new(logic::MAX_NAME_LEN, cx));
        let subscriptions = vec![
            cx.subscribe_in(&name_input, window, |view, _, event, window, cx| match event {
                InputEvent::Changed(text) => {
                    let text = text.clone();
                    view.model.edit(|s| s.name = text);
                    view.after_edit(window, cx);
                }
                InputEvent::Submit => {
                    view.model.request_apply();
                    view.after_edit(window, cx);
                }
            }),
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
        ];

        // The close button and Cmd-W ask first about edits not applied.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |view, cx| view.may_close(Leave::Close, window, cx)).unwrap_or(true)
        });

        let focus = cx.focus_handle();
        window.focus(&focus);
        AppView {
            model: Model::new(),
            worker,
            focus,
            modal_focus: cx.focus_handle(),
            name_input,
            open_menu: None,
            menu_closed: None,
            copied: None,
            diagnostics_open: false,
            scroll_status: ScrollHandle::new(),
            scroll_settings: ScrollHandle::new(),
            title: String::new(),
            alerted: false,
            attention: None,
            _tasks: vec![events, ticks],
            _subscriptions: subscriptions,
        }
    }

    /// Takes in the worker's news, moves time on, and updates what is shown
    /// outside the window too.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let events: Vec<_> = self.worker.events().collect();
        for event in events {
            self.model.handle(event, now);
        }
        self.model.tick(now);
        if self.copied.as_ref().is_some_and(|(_, at)| at.elapsed() >= COPIED_FOR) {
            self.copied = None;
        }
        self.flush(cx);
        self.window_signals(now, window);
        cx.notify();
    }

    /// Sends the model's commands to the worker, and shows the model's
    /// name in the field when it changed there (a revert, a controller).
    fn flush(&mut self, cx: &mut Context<Self>) {
        for command in self.model.take_commands() {
            self.worker.send(command);
        }
        let (name, problem) = match &self.model.form {
            Some(form) => (
                form.edited.name.clone(),
                logic::edited_name_problem(&form.current.name, &form.edited.name).is_some(),
            ),
            None => (String::new(), false),
        };
        let enabled = self.model.usable();
        self.name_input.update(cx, |input, cx| {
            if input.text() != name {
                input.set_text(&name, cx);
            }
            input.set_problem(problem, cx);
            input.set_enabled(enabled, cx);
        });
    }

    /// After the user changed something: send what is to be sent, move the
    /// focus to a dialog that opened (or back from one that closed), and
    /// draw again.
    pub fn after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flush(cx);
        self.sync_focus(window);
        cx.notify();
    }

    /// The focus a dialog holds while it is open.
    pub fn modal_focus(&self) -> &FocusHandle {
        &self.modal_focus
    }

    fn modal_open(&self) -> bool {
        self.model.confirm.is_some() || self.model.ask_discard.is_some()
    }

    fn sync_focus(&mut self, window: &mut Window) {
        if self.modal_open() {
            self.open_menu = None;
            if !self.modal_focus.is_focused(window) {
                window.focus(&self.modal_focus);
            }
        } else if self.modal_focus.is_focused(window) {
            window.focus(&self.focus);
        }
    }

    /// The window's title, and the Dock while audio is down: its icon
    /// bounces once while the window is in the background, since macOS
    /// cannot be told to stop a critical request when audio comes back. No
    /// sound: the Mac's default output may be this very device.
    fn window_signals(&mut self, now: Instant, window: &mut Window) {
        let title = self.model.window_title(now);
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
        let down = self.model.audio_down(now);
        if down && !self.alerted {
            if !window.is_window_active() {
                self.attention = dock::request_attention();
            }
            self.alerted = true;
        } else if !down && self.alerted {
            if let Some(request) = self.attention.take() {
                dock::cancel_attention(request);
            }
            self.alerted = false;
        }
    }

    pub fn switch_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        self.model.switch_tab(tab);
        self.open_menu = None;
        self.after_edit(window, cx);
    }

    /// Whether the window may close (or the app quit) now; if not, the
    /// question whether to drop the edits is up.
    fn may_close(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.model.may_leave(how) {
            place::save(window);
            true
        } else {
            self.sync_focus(window);
            cx.notify();
            false
        }
    }

    /// Closes the window or quits, asking first about edits not applied.
    pub fn leave(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        if self.may_close(how, window, cx) {
            match how {
                Leave::Close => window.remove_window(),
                Leave::Quit => cx.quit(),
            }
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_menu.take().is_none() {
            if self.model.confirm.is_some() {
                self.model.answer_confirmation(false);
            } else if self.model.ask_discard.is_some() {
                self.model.answer_discard(false);
            }
        }
        self.after_edit(window, cx);
    }

    /// Opens or closes a pop-up menu.
    pub fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        if self
            .menu_closed
            .take()
            .is_some_and(|(m, at)| m == menu && at.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        self.open_menu = if self.open_menu == Some(menu) { None } else { Some(menu) };
        cx.notify();
    }

    /// A click outside the open menu closes it.
    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.open_menu.take() {
            self.menu_closed = Some((menu, Instant::now()));
            cx.notify();
        }
    }

    /// A Copy button for `text`: it says `Copied` for a moment.
    pub fn copy_button(
        &self,
        id: impl Into<ElementId>,
        text: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let done = self.copied.as_ref().is_some_and(|(copied, _)| copied == text);
        let text = text.to_owned();
        widgets::button(id, if done { "Copied" } else { "Copy" }, Kind::Plain, true, p)
            .min_w(px(72.0))
            .h(px(26.0))
            .tooltip(Tooltip::text("Copy, then paste in Terminal."))
            .on_click(cx.listener(move |view, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                view.copied = Some((text.clone(), Instant::now()));
                cx.notify();
            }))
    }

    /// One warning from the daemon: its first sentence in semibold, the rest
    /// below it, code as code, and each command it names in a row to copy.
    pub fn warning(&self, text: &str, p: &Palette, cx: &mut Context<Self>) -> Div {
        let (head, rest) = logic::warning_parts(text);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::rich_text(head, p.text, SEMIBOLD, p))
            .when(!rest.is_empty(), |d| {
                d.child(widgets::rich_text(rest, p.text, FontWeight::NORMAL, p))
            })
            .children(logic::commands(text).into_iter().map(|command| {
                let copy =
                    self.copy_button(SharedString::from(format!("copy {command}")), command, p, cx);
                widgets::command_row(p, command, copy)
            }))
    }

    /// A link that shows the Status tab.
    fn status_link(&self, p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("show-status")
            .text_color(p.focus)
            .cursor_pointer()
            .hover(|s| s.underline())
            .child("Show status and what to do")
            .on_click(cx.listener(|view, _, window, cx| view.switch_tab(Tab::Status, window, cx)))
    }

    fn header(&self, p: &Palette, now: Instant, width: f32, cx: &mut Context<Self>) -> Div {
        let model = &self.model;
        let locked = model.device().is_some_and(|d| d.remote_settings_locked);
        let tab = |id: &'static str, label, which: Tab, badge: crate::model::Badge| {
            widgets::segment(id, label, model.tab == which, badge.map(|(light, _)| light), p)
                .when(model.tab != which, |s| {
                    s.on_click(
                        cx.listener(move |view, _, window, cx| view.switch_tab(which, window, cx)),
                    )
                })
                .when_some(badge, |s, (_, meaning)| s.tooltip(Tooltip::text(meaning)))
        };
        let tabs = div()
            .flex_shrink_0()
            .flex()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(theme::CONTROL_RADIUS + 1.0))
            .bg(p.control)
            .child(tab("tab-status", "Status", Tab::Status, model.status_badge(now)))
            .child(tab("tab-settings", "Settings", Tab::Settings, model.settings_badge()));
        let name = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(theme::HEADING))
                    .font_weight(FontWeight::BOLD)
                    .child(model.device_name()),
            )
            .when(locked, |d| {
                d.child(
                    div()
                        .id("lock-badge")
                        .flex_shrink_0()
                        .h(px(22.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .bg(p.badge())
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child("Show lock on")
                        .tooltip(Tooltip::text(logic::LOCK_HELP)),
                )
            });
        let subtitle = div()
            .id("subtitle")
            .text_color(p.text2)
            .truncate()
            .child(model.subtitle(now))
            .tooltip(Tooltip::text(model.versions()));

        // Audio takes 40 % (180 pt at least) while the others keep 120 pt;
        // when even that does not fit, thirds.
        let gap = 8.0;
        let room = (width - 2.0 * GUTTER).max(0.0);
        let third = ((room - 2.0 * gap) / 3.0).max(0.0);
        let audio = (room * 0.40).max(180.0).min(room - 2.0 * (120.0 + gap)).max(third);
        let other = ((room - audio - 2.0 * gap) / 2.0).max(0.0);
        let [audio_lamp, clock_lamp, driver_lamp] = model.lamps(now);
        let tiles = div()
            .flex()
            .gap(px(gap))
            .child(widgets::tile("tile-audio", "Audio", &audio_lamp, audio, true, p))
            .child(widgets::tile("tile-clock", "Clock", &clock_lamp, other, false, p))
            .child(widgets::tile("tile-driver", "Driver", &driver_lamp, other, false, p));

        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(GUTTER))
            .pt(px(14.0))
            .pb(px(10.0))
            .child(div().flex().items_center().gap(px(16.0)).child(name).child(tabs))
            .child(div().pt(px(2.0)).child(subtitle))
            .child(div().pt(px(10.0)).child(tiles))
    }

    /// What is pinned under the header on both tabs: an outage, a restart,
    /// a stale status. It never scrolls away.
    fn alerts(&self, p: &Palette, now: Instant, cx: &mut Context<Self>) -> Div {
        let model = &self.model;
        let compact = model.tab == Tab::Settings;
        let list = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(GUTTER))
            .pt(px(4.0))
            .pb(px(4.0));
        if model.restarting(now) {
            // Expected: the user accepted it. Until audio is back, the
            // daemon's own alarms are the restart's.
            let mut text = "Audio returns in 10–20 s, in both directions.".to_owned();
            if let Some(Outcome::Restarting { since, .. }) = model.outcome {
                text = format!(
                    "{} so far. {text}",
                    logic::ago_label(now.saturating_duration_since(since))
                );
            }
            return list.child(
                widgets::notice(
                    "restart-spinner",
                    p,
                    Severity::Progress,
                    "OpenVirtualSoundcard is restarting",
                )
                .child(text),
            );
        }
        match &model.connection {
            Connection::Down(problem) => list.child(
                widgets::notice("down", p, Severity::Urgent, problem.title())
                    .when_some(problem.detail(), |n, detail| {
                        n.child(widgets::rich_text(&detail, p.text, FontWeight::NORMAL, p))
                    })
                    .child(self.status_link(p, cx)),
            ),
            Connection::Up(status) => {
                let mut list = list;
                if let Some(stale) = model.stale(now) {
                    list = list.child(
                        widgets::notice("stale", p, Severity::Warning, "No update from the daemon")
                            .child(format!(
                                "Nothing heard for {}: what is shown may be out of date. Audio \
                                 may still flow; check the console's meters.",
                                logic::ago_label(stale)
                            )),
                    );
                }
                let (urgent, _) = logic::split_warnings(status);
                if !status.engine_running || !urgent.is_empty() {
                    let mut n = widgets::notice("audio-down", p, Severity::Urgent, "Audio is down");
                    if !status.engine_running {
                        n = n
                            .child(widgets::headline("The network engine is not running."))
                            .when_some(status.engine_error.as_ref(), |n, error| {
                                n.child(logic::sentence_case(error))
                            })
                            .when(!compact, |n| {
                                n.child(
                                    "OpenVirtualSoundcard retries on its own. Check the cable and \
                                     the network interface in Settings.",
                                )
                            });
                    }
                    for warning in urgent {
                        n = if compact {
                            n.child(widgets::rich_text(
                                logic::headline(warning),
                                p.text,
                                SEMIBOLD,
                                p,
                            ))
                        } else {
                            n.child(self.warning(warning, p, cx))
                        };
                    }
                    if compact {
                        n = n.child(self.status_link(p, cx));
                    }
                    list = list.child(n);
                }
                list
            }
            Connection::Connecting => list,
        }
    }

    /// The question before edits not applied are dropped.
    fn discard_dialog(&self, p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let keep = widgets::button("keep-editing", "Keep editing", Kind::Plain, true, p)
            .min_w(px(112.0))
            .border_2()
            .border_color(p.focus)
            .on_click(cx.listener(|view, _, window, cx| {
                view.model.answer_discard(false);
                view.after_edit(window, cx);
            }));
        let discard = widgets::button("discard", "Discard", Kind::Destructive, true, p)
            .min_w(px(112.0))
            .on_click(cx.listener(|view, _, window, cx| {
                if let Some(how) = view.model.answer_discard(true) {
                    view.leave(how, window, cx);
                }
                view.after_edit(window, cx);
            }));
        widgets::scrim(p).track_focus(&self.modal_focus).key_context("Modal").child(
            widgets::dialog(p, 380.0)
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(SEMIBOLD)
                        .child("Discard the changes you have not applied?"),
                )
                .child(
                    div()
                        .text_color(p.text2)
                        .child("They are dropped. OpenVirtualSoundcard keeps running."),
                )
                .child(
                    div().pt(px(6.0)).flex().justify_end().gap(px(8.0)).child(keep).child(discard),
                ),
        )
    }
}

impl Focusable for AppView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(window);
        let now = Instant::now();
        let width = f32::from(window.viewport_size().width);
        let wide = width - 2.0 * GUTTER >= 600.0;
        let tab = self.model.tab;
        let content: AnyElement = match tab {
            Tab::Status => status::show(self, p, now, wide, window, cx),
            Tab::Settings => settings::show(self, p, window, cx),
        };
        let scroll = match tab {
            Tab::Status => &self.scroll_status,
            Tab::Settings => &self.scroll_settings,
        };
        let header = self.header(p, now, width, cx);
        let alerts = if self.model.has_alerts(now) { Some(self.alerts(p, now, cx)) } else { None };
        let footer = if tab == Tab::Settings && self.model.form.is_some() {
            Some(settings::action_bar(self, p, now, cx))
        } else {
            None
        };
        let confirm = if self.model.confirm.is_some() {
            Some(settings::confirm_dialog(self, p, cx))
        } else {
            None
        };
        let discard =
            if self.model.ask_discard.is_some() { Some(self.discard_dialog(p, cx)) } else { None };
        div()
            .id("window")
            .key_context("App")
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &ShowStatus, window, cx| {
                view.switch_tab(Tab::Status, window, cx)
            }))
            .on_action(cx.listener(|view, _: &ShowSettings, window, cx| {
                view.switch_tab(Tab::Settings, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &CloseWindow, window, cx| {
                    view.leave(Leave::Close, window, cx)
                }),
            )
            .on_action(cx.listener(|_, _: &Minimize, window, _| window.minimize_window()))
            .on_action(cx.listener(Self::cancel))
            // A click on the window's background takes the focus from the
            // name field.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|view, _, window, _| {
                    if !view.modal_open() {
                        window.focus(&view.focus);
                    }
                }),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.window)
            .text_color(p.text)
            .font_family(theme::sans())
            .text_size(px(theme::BODY))
            .line_height(px(18.0))
            .child(header)
            .children(alerts)
            .child(
                div()
                    .id(match tab {
                        Tab::Status => "status",
                        Tab::Settings => "settings",
                    })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(scroll)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .px(px(GUTTER))
                            .pb(px(GUTTER))
                            .child(content),
                    ),
            )
            .children(footer)
            .children(confirm)
            .children(discard)
    }
}

/// The Dock's attention while audio is down, on macOS.
#[cfg(target_os = "macos")]
mod dock {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    /// `NSInformationalRequest`: the icon bounces once.
    const INFORMATIONAL: isize = 10;

    pub fn request_attention() -> Option<isize> {
        // SAFETY: called on the main thread, where GPUI runs the app; both
        // are plain NSApplication messages.
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let request: isize = msg_send![app, requestUserAttention: INFORMATIONAL];
            Some(request)
        }
    }

    pub fn cancel_attention(request: isize) {
        // SAFETY: as above.
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, cancelUserAttentionRequest: request];
        }
    }
}

/// Elsewhere there is no Dock to ask.
#[cfg(not(target_os = "macos"))]
mod dock {
    pub fn request_attention() -> Option<isize> {
        None
    }

    pub fn cancel_attention(_: isize) {}
}
