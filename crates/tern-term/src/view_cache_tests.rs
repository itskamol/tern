#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `TerminalView::cached_child`: a parent that re-renders around an unchanged terminal must not
//! rebuild it, and everything that does change it must. Drawn with GPUI's `TestApp`, counting
//! the view's prepaints.

use super::*;
use gpui::{AppContext as _, FocusHandle, TestApp, TestAppWindow};

/// Stands in for the app's shell: a parent that re-renders around the cached terminal.
struct Host {
    view: Entity<TerminalView>,
    elsewhere: FocusHandle,
    renders: usize,
    /// How wide the terminal's slot is; the parent changes it to resize the terminal.
    width: Pixels,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("elsewhere")
                    .track_focus(&self.elsewhere)
                    .size(px(10.)),
            )
            .child(
                div().flex_1().min_h_0().child(
                    div()
                        .w(self.width)
                        .h_full()
                        .child(TerminalView::cached_child(&self.view)),
                ),
            )
    }
}

struct Rig {
    app: TestApp,
    window: TestAppWindow<Host>,
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
}

impl Rig {
    fn new() -> Self {
        let mut app = TestApp::new();
        let terminal = app.new_entity(|_| Terminal::new(80, 24));
        let mut view = None;
        let window = app.open_window(|window, cx| {
            let handle = cx.new(|cx| {
                TerminalView::new(terminal.clone(), TerminalTheme::default(), window, cx)
            });
            view = Some(handle.clone());
            Host {
                view: handle,
                elsewhere: cx.focus_handle(),
                renders: 0,
                width: px(900.),
            }
        });
        let mut rig = Self {
            app,
            window,
            terminal,
            view: view.unwrap(),
        };
        rig.window.draw();
        rig
    }

    fn prepaints(&self) -> usize {
        self.app.read_entity(&self.view, |v, _| v.prepaints)
    }

    fn host_renders(&self) -> usize {
        self.window.read(|host, _| host.renders)
    }

    /// Does `change`, lets the window draw (the test platform draws after an update, and
    /// `draw` makes sure), and says how many times the terminal was rebuilt meanwhile.
    fn rebuilds_after(&mut self, change: impl FnOnce(&mut Self)) -> usize {
        let before = self.prepaints();
        change(self);
        self.window.draw();
        self.prepaints() - before
    }

    /// The parent changes: it is rebuilt, the terminal is not.
    fn parent_changes(&mut self) {
        let renders = self.host_renders();
        let rebuilt = self.rebuilds_after(|rig| rig.window.update(|_, _, cx| cx.notify()));
        assert!(self.host_renders() > renders, "the parent was not rebuilt");
        assert_eq!(rebuilt, 0, "an unchanged terminal was rebuilt");
    }
}

#[test]
fn a_parent_re_render_replays_the_terminal() {
    let mut rig = Rig::new();
    assert_eq!(rig.prepaints(), 1, "built once on the first frame");
    assert_eq!(rig.rebuilds_after(|_| {}), 0, "nothing changed");
    for _ in 0..3 {
        rig.parent_changes();
    }
}

#[test]
fn output_rebuilds_it() {
    let mut rig = Rig::new();
    let terminal = rig.terminal.clone();
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.app
            .update_entity(&terminal, |t, cx| t.feed(b"hello\r\n", cx))
    });
    assert_eq!(rebuilt, 1);
    rig.parent_changes();
}

#[test]
fn a_new_theme_rebuilds_it() {
    let mut rig = Rig::new();
    let theme = TerminalTheme {
        font_size: 18.0,
        background_alpha: 0.6,
        ..TerminalTheme::default()
    };
    let view = rig.view.clone();
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.app.update_entity(&view, |v, cx| v.set_theme(theme, cx));
    });
    assert_eq!(rebuilt, 1);
    rig.parent_changes();
}

#[test]
fn changed_options_and_the_blink_phase_rebuild_it() {
    let mut rig = Rig::new();
    let view = rig.view.clone();
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.app
            .update_entity(&view, |v, cx| v.set_options(TerminalOptions::default(), cx));
    });
    assert_eq!(rebuilt, 1, "options");
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.app.update_entity(&view, |v, cx| {
            v.cursor_on = !v.cursor_on;
            cx.notify();
        });
    });
    assert_eq!(rebuilt, 1, "blink");
    rig.parent_changes();
}

#[test]
fn search_and_scrolling_rebuild_it() {
    let mut rig = Rig::new();
    let (terminal, view) = (rig.terminal.clone(), rig.view.clone());
    rig.app.update_entity(&terminal, |t, cx| {
        for i in 0..60 {
            t.feed(format!("line {i}\r\n").as_bytes(), cx);
        }
    });
    rig.window.draw();
    let rebuilt =
        rig.rebuilds_after(|rig| rig.app.update_entity(&view, |v, cx| v.find("line 7", cx)));
    assert_eq!(rebuilt, 1, "find");
    rig.parent_changes();
}

#[test]
fn a_resize_rebuilds_it() {
    let mut rig = Rig::new();
    let cols = rig.app.read_entity(&rig.terminal, |t, _| t.cols());
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.window.update(|host, _, cx| {
            host.width = px(640.);
            cx.notify();
        })
    });
    assert_eq!(rebuilt, 1);
    let after = rig.app.read_entity(&rig.terminal, |t, _| t.cols());
    assert!(
        after > 2 && after < cols,
        "the grid followed: {cols} -> {after}"
    );
    rig.parent_changes();
}

#[test]
fn a_focus_change_rebuilds_it() {
    let mut rig = Rig::new();
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.window
            .update(|host, window, cx| host.elsewhere.focus(window, cx))
    });
    assert_eq!(rebuilt, 1, "unfocused");
    rig.parent_changes();
    let view = rig.view.clone();
    let rebuilt = rig.rebuilds_after(|rig| {
        rig.window.update(|_, window, cx| {
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        })
    });
    assert_eq!(rebuilt, 1, "focused again");
    rig.parent_changes();
}

#[test]
fn a_bell_flash_keeps_asking_for_frames_while_it_fades() {
    let mut rig = Rig::new();
    let view = rig.view.clone();
    let rebuilt = rig.rebuilds_after(|rig| rig.app.update_entity(&view, |v, cx| v.flash(cx)));
    assert_eq!(rebuilt, 1, "the flash appears");
    for _ in 0..3 {
        let rebuilt = rig.rebuilds_after(|rig| {
            rig.window.update(|_, window, cx| {
                window.simulate_next_frame(cx);
            })
        });
        assert_eq!(rebuilt, 1, "the next frame of the fade");
    }
}

/// Not caching, but the same rig: OSC 52 reaches the clipboard only while the option allows.
#[test]
fn a_program_sets_the_clipboard_only_while_allowed() {
    let mut rig = Rig::new();
    let (terminal, view) = (rig.terminal.clone(), rig.view.clone());
    let copy = |rig: &mut Rig, text: &[u8]| {
        rig.app.update_entity(&terminal, |t, cx| t.feed(text, cx));
        rig.app.read_from_clipboard().and_then(|c| c.text())
    };
    // "hello" and "bye", base64.
    assert_eq!(
        copy(&mut rig, b"\x1b]52;c;aGVsbG8=\x07").as_deref(),
        Some("hello")
    );
    rig.app.update_entity(&view, |v, cx| {
        v.set_options(
            TerminalOptions {
                clipboard_write: false,
                ..TerminalOptions::default()
            },
            cx,
        )
    });
    assert_eq!(
        copy(&mut rig, b"\x1b]52;c;Ynll\x07").as_deref(),
        Some("hello")
    );
}

#[test]
fn a_held_resize_tells_the_remote_once_on_release() {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut rig = Rig::new();
    let heard = Rc::new(Cell::new(0));
    let _watch = rig.app.update(|cx| {
        let heard = heard.clone();
        cx.subscribe(&rig.terminal, move |_, event: &TerminalEvent, _| {
            if matches!(event, TerminalEvent::Resized { .. }) {
                heard.set(heard.get() + 1);
            }
        })
    });
    let at = |cols, rows| GridGeometry {
        origin: gpui::point(px(0.), px(0.)),
        cell_w: 8.0,
        line_h: 16.0,
        cols,
        rows,
    };
    let view = rig.view.clone();
    rig.app
        .update_entity(&view, |v, cx| v.hold_remote_resize(true, cx));
    // The panes pass through several sizes while the divider moves.
    for cols in [70, 60, 50] {
        rig.app
            .update_entity(&view, |v, cx| v.on_grid_metrics(at(cols, 24), cx));
    }
    rig.app.run_until_parked();
    assert_eq!(heard.get(), 0, "nothing is sent while the divider is held");
    rig.app
        .update_entity(&view, |v, cx| v.hold_remote_resize(false, cx));
    assert_eq!(heard.get(), 1, "one size is sent on release");
    rig.app
        .update_entity(&view, |v, cx| v.hold_remote_resize(false, cx));
    assert_eq!(heard.get(), 1, "a release with nothing owed sends nothing");
}
