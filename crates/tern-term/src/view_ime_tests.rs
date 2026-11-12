#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The input method path: what reaches the remote, and how often, for each kind of key. Drawn
//! with GPUI's `TestApp`, whose simulated keystrokes follow the same route as the platform's:
//! the key handler first, then the input handler for what it left alone.

use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use gpui::{AppContext as _, Bounds, EntityInputHandler as _, TestApp, TestAppWindow};

struct Host {
    view: Entity<TerminalView>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(TerminalView::cached_child(&self.view))
    }
}

struct Rig {
    app: TestApp,
    window: TestAppWindow<Host>,
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
    /// Every `Typed` event, one entry each.
    typed: Rc<RefCell<Vec<Vec<u8>>>>,
    _tap: Subscription,
}

impl Rig {
    fn new() -> Self {
        let mut app = TestApp::new();
        let terminal = app.new_entity(|_| Terminal::new(80, 24));
        let typed = Rc::new(RefCell::new(Vec::new()));
        let tap = {
            let typed = typed.clone();
            app.update(|cx| {
                cx.subscribe(&terminal, move |_, event: &TerminalEvent, _| {
                    if let TerminalEvent::Typed(bytes) = event {
                        typed.borrow_mut().push(bytes.clone());
                    }
                })
            })
        };
        let mut view = None;
        let window = app.open_window(|window, cx| {
            let handle = cx.new(|cx| {
                TerminalView::new(terminal.clone(), TerminalTheme::default(), window, cx)
            });
            view = Some(handle.clone());
            Host { view: handle }
        });
        let mut rig = Self {
            app,
            window,
            terminal,
            view: view.unwrap(),
            typed,
            _tap: tap,
        };
        // The input handler is registered by painting.
        rig.window.draw();
        rig
    }

    /// A keystroke with the character the layout gives it, which `simulate_keystroke` cannot carry.
    fn dispatch(&mut self, keystroke: gpui::Keystroke) {
        let handle: gpui::AnyWindowHandle = self.window.handle().into();
        self.app.update(|cx| {
            cx.update_window(handle, |_, window, cx| {
                window.dispatch_keystroke(keystroke, cx);
            })
            .unwrap();
        });
    }

    fn typed(&self) -> Vec<Vec<u8>> {
        self.typed.borrow().clone()
    }

    /// What the platform's text input does: `f` calls the input handler's methods.
    fn input_method<R>(
        &mut self,
        f: impl FnOnce(&mut TerminalView, &mut Window, &mut Context<TerminalView>) -> R,
    ) -> R {
        let view = self.view.clone();
        self.window
            .update(|_, window, cx| view.update(cx, |v, cx| f(v, window, cx)))
    }

    fn commit(&mut self, text: &str) {
        self.input_method(|v, w, cx| v.replace_text_in_range(None, text, w, cx));
    }

    fn compose(&mut self, text: &str, selected: Option<std::ops::Range<usize>>) {
        self.input_method(|v, w, cx| v.replace_and_mark_text_in_range(None, text, selected, w, cx));
    }

    fn marked(&self) -> Option<String> {
        self.app
            .read_entity(&self.view, |v, _| v.marked_text().map(str::to_owned))
    }
}

#[test]
fn committed_text_reaches_the_remote_once_as_utf8() {
    let mut rig = Rig::new();
    rig.commit("\u{e9}");
    rig.commit("\u{4f60}\u{597d}");
    assert_eq!(
        rig.typed(),
        vec![
            "\u{e9}".as_bytes().to_vec(),
            "\u{4f60}\u{597d}".as_bytes().to_vec()
        ]
    );
}

#[test]
fn a_plain_key_is_typed_by_the_input_handler_only() {
    let mut rig = Rig::new();
    rig.window.simulate_keystroke("a");
    rig.window.simulate_keystroke("shift-b");
    rig.window.simulate_keystroke("space");
    assert_eq!(
        rig.typed(),
        vec![b"a".to_vec(), b"B".to_vec(), b" ".to_vec()]
    );
}

#[test]
fn typed_text_is_not_bracketed_even_when_the_remote_asked_for_paste_brackets() {
    let mut rig = Rig::new();
    let terminal = rig.terminal.clone();
    rig.app
        .update_entity(&terminal, |t, cx| t.feed(b"\x1b[?2004h", cx));
    rig.commit("\u{e9}");
    assert_eq!(rig.typed(), vec!["\u{e9}".as_bytes().to_vec()]);
}

#[test]
fn composing_text_is_shown_and_not_sent_until_it_is_committed() {
    let mut rig = Rig::new();
    rig.compose("\u{3053}\u{3093}", None);
    rig.window.draw();
    assert_eq!(rig.marked().as_deref(), Some("\u{3053}\u{3093}"));
    assert!(rig.typed().is_empty(), "marked text must not be sent");
    // The composition is the whole document the input method sees; the caret is at its end.
    rig.input_method(|v, w, cx| {
        assert_eq!(v.marked_text_range(w, cx), Some(0..2));
        let sel = v.selected_text_range(false, w, cx).unwrap();
        assert_eq!(sel.range, 2..2);
    });
    // Changing the composition replaces it, still sending nothing.
    rig.compose("\u{3053}", Some(0..1));
    assert_eq!(rig.marked().as_deref(), Some("\u{3053}"));
    assert!(rig.typed().is_empty());
    // Committing sends the final text once and clears the composition.
    rig.commit("\u{3053}\u{3093}");
    assert_eq!(rig.marked(), None);
    assert_eq!(rig.typed(), vec!["\u{3053}\u{3093}".as_bytes().to_vec()]);
}

#[test]
fn unmarking_or_emptying_the_composition_clears_it_without_sending() {
    let mut rig = Rig::new();
    rig.compose("\u{b4}", None);
    rig.input_method(|v, w, cx| v.unmark_text(w, cx));
    assert_eq!(rig.marked(), None);
    rig.compose("ni", None);
    rig.compose("", None);
    assert_eq!(rig.marked(), None);
    assert!(rig.typed().is_empty());
}

#[test]
fn a_dead_key_composes_then_commits_one_character() {
    // Option+e marks the accent; e then replaces it with the finished letter.
    let mut rig = Rig::new();
    rig.compose("\u{b4}", None);
    rig.commit("\u{e9}");
    assert_eq!(rig.marked(), None);
    assert_eq!(rig.typed(), vec!["\u{e9}".as_bytes().to_vec()]);
}

#[test]
fn mapped_keys_are_written_by_the_key_handler_and_not_again() {
    let mut rig = Rig::new();
    rig.window.simulate_keystroke("ctrl-c");
    rig.window.simulate_keystroke("up");
    rig.window.simulate_keystroke("enter");
    rig.window.simulate_keystroke("alt-x");
    assert_eq!(
        rig.typed(),
        vec![
            vec![0x03],
            b"\x1b[A".to_vec(),
            b"\r".to_vec(),
            b"\x1bx".to_vec()
        ]
    );
}

#[test]
fn option_composes_instead_of_meta_when_the_setting_is_off() {
    let mut rig = Rig::new();
    let view = rig.view.clone();
    rig.app
        .update_entity(&view, |v, _| v.set_option_as_meta(false));
    // The key handler leaves Option+e alone, so GPUI offers it to the input method, which on
    // macOS starts the dead key. The simulated keystroke carries no character, so nothing is
    // typed, and above all no ESC prefix.
    rig.window.simulate_keystroke("alt-e");
    assert!(rig.typed().is_empty());
    // Keys that are mapped whatever the setting still are.
    rig.window.simulate_keystroke("alt-left");
    assert_eq!(rig.typed(), vec![b"\x1b[1;3D".to_vec()]);
}

#[test]
fn the_candidate_window_sits_on_the_cursor_cell() {
    let mut rig = Rig::new();
    let terminal = rig.terminal.clone();
    rig.app
        .update_entity(&terminal, |t, cx| t.feed(b"ab\r\ncde", cx));
    rig.window.draw();
    let geometry = rig.app.read_entity(&rig.view, |v, _| v.geometry.unwrap());
    let bounds = rig
        .input_method(|v, w, cx| v.bounds_for_range(0..0, Bounds::default(), w, cx))
        .unwrap();
    // Row 1, column 3.
    assert_eq!(
        bounds.origin.x,
        geometry.origin.x + px(geometry.cell_w * 3.0)
    );
    assert_eq!(bounds.origin.y, geometry.origin.y + px(geometry.line_h));
    assert_eq!(bounds.size.width, px(geometry.cell_w));
    assert_eq!(bounds.size.height, px(geometry.line_h));
}

#[test]
fn nothing_composes_under_the_find_bar() {
    let mut rig = Rig::new();
    let view = rig.view.clone();
    rig.app.update_entity(&view, |v, cx| v.find("x", cx));
    assert!(!rig.input_method(|v, w, cx| v.accepts_text_input(w, cx)));
    rig.window.simulate_keystroke("a");
    assert!(rig.typed().is_empty(), "the find bar took the key");
}

#[test]
fn a_long_composition_wraps_and_is_clipped_at_the_bottom_without_trouble() {
    let mut rig = Rig::new();
    // Mixed widths, longer than the 80 columns left on the cursor's row.
    let text = "a\u{4f60}\u{597d}".repeat(40);
    rig.compose(&text, None);
    rig.window.draw();
    assert_eq!(rig.marked().as_deref(), Some(text.as_str()));
    // Near the last row there is no room to wrap: it stops, it does not panic.
    let terminal = rig.terminal.clone();
    rig.app
        .update_entity(&terminal, |t, cx| t.feed(&b"\n".repeat(30), cx));
    rig.window.draw();
    assert!(rig.typed().is_empty());
}

#[test]
fn a_layout_character_on_option_is_typed_once_with_meta_on() {
    let mut rig = Rig::new();
    // German Option+L: the key handler leaves it alone and the input handler types the "@".
    let mut ks = gpui::Keystroke::parse("alt-l").unwrap();
    ks.key_char = Some("@".into());
    rig.dispatch(ks);
    assert_eq!(rig.typed(), vec![b"@".to_vec()]);
    // US Option+B ("\u{222b}") stays Meta.
    let mut ks = gpui::Keystroke::parse("alt-b").unwrap();
    ks.key_char = Some("\u{222b}".into());
    rig.dispatch(ks);
    assert_eq!(rig.typed(), vec![b"@".to_vec(), b"\x1bb".to_vec()]);
}
