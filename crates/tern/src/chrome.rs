// Adapted from zeron crates/ui/src/app_menus.rs (MIT).
// Adapted from zeron crates/ui/src/shell.rs (traffic-light-aware titlebar layout) (MIT).
// Adapted from zeron crates/ui/src/shell.rs (WidthTween, sidebar resize handle, DragGhost) (MIT).
//! The window's chrome: the macOS menu bar and its app-wide shortcuts (without `set_menus` macOS
//! shows no menu and ⌘Q does nothing), the custom titlebar (app name, tabs, and an empty strip
//! that drags the window), and the sidebar width animation with the drag that resizes it.

use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, Empty, InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, WindowControlArea, actions, div, px,
};
#[cfg(target_os = "macos")]
use gpui::{Menu, MenuItem, OsAction, SystemMenuType};

use crate::hover::{Accessible as _, HoverFade as _};
use crate::settings::{SIDEBAR_MAX, SIDEBAR_MIN};
use crate::theme::{SPACE_SM, TITLEBAR_HEIGHT, TITLEBAR_TOP_PAD, Theme, titlebar_content_start};

actions!(
    tern,
    [
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        ToggleFullScreen,
        BringAllToFront,
        OpenHelp
    ]
);

const HELP_URL: &str = "https://github.com/your-moon/tern#readme";

pub fn init_menus(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |w| w.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |w| w.zoom_window()));
    cx.on_action(|_: &ToggleFullScreen, cx| with_active_window(cx, |w| w.toggle_fullscreen()));
    cx.on_action(|_: &BringAllToFront, cx| cx.activate(true));
    cx.on_action(|_: &OpenHelp, cx| cx.open_url(HELP_URL));
    // Only macOS has an application menu bar; elsewhere the chords below are all there is.
    #[cfg(target_os = "macos")]
    cx.set_menus(app_menus());
}

/// The menus every Mac app has (HIG: Menus), so each shortcut can be found where people look
/// for it. An item whose action nothing in focus handles is shown disabled.
#[cfg(target_os = "macos")]
fn app_menus() -> Vec<Menu> {
    use crate::shell::{
        DecreaseFontSize, IncreaseFontSize, NewConnection, OpenSettings, ResetFontSize, ShowAbout,
        SplitDown, SplitRight, ToggleSftp, ToggleSidebar, ToggleSnippets,
    };
    use crate::shell::{RedoHosts, UndoHosts};
    use crate::tabs::{CloseTab, NextTab, PrevTab};
    use crate::text_input::{Copy, Cut, Paste, SelectAll};
    let menu = |name: &str, items| Menu {
        name: name.to_owned().into(),
        items,
        disabled: false,
    };
    vec![
        menu(
            "tern",
            vec![
                MenuItem::action("About tern", ShowAbout),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide tern", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit tern", Quit),
            ],
        ),
        menu(
            "File",
            vec![
                MenuItem::action("New Connection…", NewConnection),
                MenuItem::action("New Tab…", crate::picker::ToggleHostPicker),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
            ],
        ),
        menu(
            "Edit",
            vec![
                MenuItem::os_action("Undo", UndoHosts, OsAction::Undo),
                MenuItem::os_action("Redo", RedoHosts, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
                MenuItem::separator(),
                MenuItem::action("Clear Scrollback", crate::shell::ClearTerminal),
                MenuItem::action("Find Host…", crate::shell::FocusHostSearch),
            ],
        ),
        menu(
            "View",
            vec![
                MenuItem::action("Show or Hide Sidebar", ToggleSidebar),
                MenuItem::action("Browse Files", ToggleSftp),
                MenuItem::separator(),
                MenuItem::action("Bigger Text", IncreaseFontSize),
                MenuItem::action("Smaller Text", DecreaseFontSize),
                MenuItem::action("Actual Size", ResetFontSize),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullScreen),
            ],
        ),
        menu(
            "Shell",
            vec![
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Snippets…", ToggleSnippets),
                MenuItem::action("Fill Password", crate::shell::FillPassword),
            ],
        ),
        menu(
            "Window",
            vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Show Next Tab", NextTab),
                MenuItem::action("Show Previous Tab", PrevTab),
                MenuItem::separator(),
                MenuItem::action("Bring All to Front", BringAllToFront),
            ],
        ),
        menu("Help", vec![MenuItem::action("tern Help", OpenHelp)]),
    ]
}

/// The menu's own chords; not rebindable, as in every macOS app. Windows and Linux have only
/// Quit (Ctrl+Shift+Q); there is no Hide, and the window manager minimises.
#[cfg(not(target_os = "macos"))]
pub fn menu_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("ctrl-shift-q", Quit, None)]
}

/// The menu's own chords; not rebindable, as in every macOS app.
#[cfg(target_os = "macos")]
pub fn menu_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        // A text field has no history of its own yet; ⌘Z there must not undo a host.
        KeyBinding::new("cmd-z", crate::shell::UndoHosts, Some("!TextInput")),
        KeyBinding::new("shift-cmd-z", crate::shell::RedoHosts, Some("!TextInput")),
    ]
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}

/// zeron's titlebar cluster: the sidebar toggle and + (find a host) at x = 88, 24 pt buttons,
/// 16 pt icons, then the tabs.
pub fn render_titlebar(
    t: &Theme,
    fullscreen: bool,
    sidebar_open: bool,
    tabs: impl IntoElement,
    tint: Option<gpui::Hsla>,
    cx: &mut gpui::Context<crate::shell::Shell>,
) -> impl IntoElement {
    div()
        .h(px(TITLEBAR_HEIGHT))
        .flex_none()
        .when_some(tint, |el, c| el.bg(c))
        .pt(px(TITLEBAR_TOP_PAD))
        .pl(px(titlebar_content_start(fullscreen)))
        .pr(px(SPACE_SM))
        .flex()
        .items_center()
        .gap(px(SPACE_SM))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    control("toggle-sidebar", t)
                        .icon_button("Toggle sidebar", t)
                        .on_click(cx.listener(|s, _, _, cx| s.toggle_sidebar(cx)))
                        .child(crate::icons::sidebar_glyph(sidebar_open, 16., t.muted)),
                )
                .child(
                    control("find-host", t)
                        .icon_button("New tab: find a host", t)
                        .on_click(cx.listener(|s, _, w, cx| s.toggle_picker(w, cx)))
                        .child(
                            crate::icons::icon(crate::icons::PLUS)
                                .size(px(16.))
                                .text_color(t.muted),
                        ),
                ),
        )
        .child(tabs)
        .child(
            div()
                // The drag strip keeps a grab area even when the tabs fill the bar.
                .flex_1()
                .min_w(px(48.))
                .h_full()
                .window_control_area(WindowControlArea::Drag)
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()),
        )
}

fn control(id: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover_fade(
            format!("titlebar-{id}"),
            gpui::transparent_black(),
            t.ink(0.11),
        )
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// zeron's pane transition: 200 ms ease-out.
pub const TWEEN: Duration = Duration::from_millis(200);
/// Half the width of the invisible grab strip centred on the sidebar edge.
pub const HANDLE_HALF_WIDTH: f32 = 10.0;

#[derive(Debug, Clone, Copy)]
pub struct WidthTween {
    pub from: f32,
    pub to: f32,
    pub started: Instant,
}

impl WidthTween {
    pub fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: Instant::now(),
        }
    }

    /// The width now, or `None` once the tween has finished.
    pub fn sample(&self, now: Instant) -> Option<f32> {
        let elapsed = now.saturating_duration_since(self.started);
        (elapsed < TWEEN).then(|| width_at(self.from, self.to, elapsed))
    }
}

/// Ease-out cubic from `from` to `to` over [`TWEEN`].
pub fn width_at(from: f32, to: f32, elapsed: Duration) -> f32 {
    let t = (elapsed.as_secs_f32() / TWEEN.as_secs_f32()).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    from + (to - from) * eased
}

/// Sidebar width for a pointer at window x `x`: the sidebar starts at the window's left edge.
pub fn dragged_width(x: f32) -> f32 {
    x.clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

/// Drag payload for the sidebar handle.
pub struct SidebarResize;

/// gpui needs a view to show under the pointer while dragging; the resize shows nothing.
pub struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tween_starts_at_from_ends_at_to_and_is_front_loaded() {
        assert_eq!(width_at(256.0, 0.0, Duration::ZERO), 256.0);
        assert_eq!(width_at(256.0, 0.0, TWEEN), 0.0);
        // Ease-out covers more than half the distance in the first half of the time.
        let half = width_at(0.0, 100.0, TWEEN / 2);
        assert!(half > 50.0 && half < 100.0, "{half}");
    }

    #[test]
    fn finished_tween_samples_none() {
        let tween = WidthTween::new(0.0, 256.0);
        assert!(tween.sample(tween.started).is_some());
        assert!(tween.sample(tween.started + TWEEN).is_none());
    }

    #[test]
    fn drag_is_clamped_to_the_sidebar_range() {
        assert_eq!(dragged_width(50.0), SIDEBAR_MIN);
        assert_eq!(dragged_width(300.0), 300.0);
        assert_eq!(dragged_width(1200.0), SIDEBAR_MAX);
    }
}
