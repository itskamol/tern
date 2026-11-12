// Adapted from zed crates/terminal_view/src/terminal_view.rs (GPL-3.0-or-later), its
// `TerminalInputHandler`.
//
//! Input methods for [`TerminalView`]: composed text (Chinese, Japanese, Korean), dead keys
//! (Option+e, then e), the character picker and press-and-hold accents.
//!
//! Two paths carry a key, never both. A key the mapping owns (special keys, ctrl combos,
//! Option-as-Meta) is written by `on_key_down` and stops there, so GPUI never offers it to the
//! input method. Every other key falls through to the platform's text input, which calls back
//! here: finished text arrives in `replace_text_in_range` and is written like typing, text still
//! being composed arrives in `replace_and_mark_text_in_range` and is only shown at the cursor.

use std::ops::Range;

use gpui::{Bounds, EntityInputHandler, UTF16Selection, point, size};

use super::*;

/// Text an input method is composing. It lives in the view, not the grid, and is sent only once
/// it is committed.
pub(super) struct Ime {
    marked: Option<Marked>,
    _focus_out: Subscription,
}

struct Marked {
    text: String,
    /// The selection inside `text`, in UTF-16 units as the platform counts them.
    selected: Range<usize>,
}

impl Ime {
    /// Composition cannot survive the view losing focus: the platform drops its input handler
    /// and the text would hang at the cursor for good.
    pub(super) fn new(
        focus_handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) -> Self {
        let focus_out = cx.on_focus_out(focus_handle, window, |this, _, _, cx| {
            if this.ime.marked.take().is_some() {
                cx.notify();
            }
        });
        Self {
            marked: None,
            _focus_out: focus_out,
        }
    }
}

impl TerminalView {
    /// The text being composed, to draw at the cursor.
    pub(crate) fn marked_text(&self) -> Option<&str> {
        self.ime.marked.as_ref().map(|m| m.text.as_str())
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

impl EntityInputHandler for TerminalView {
    // The grid is not an editable document, so there is no text to hand back for reconversion.
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    // The composition is the whole document the input method sees, starting at 0.
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let range = self
            .ime
            .marked
            .as_ref()
            .map_or(0..0, |marked| marked.selected.clone());
        Some(UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime
            .marked
            .as_ref()
            .map(|marked| 0..utf16_len(&marked.text))
    }

    // Composition abandoned: the text was never sent, so there is nothing to take back.
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.ime.marked.take().is_some() {
            cx.notify();
        }
    }

    /// Finished text, from a commit, a dead key, the character picker or a plain key. It is
    /// typing, so it is not bracketed like a paste.
    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ime.marked.take().is_some() {
            cx.notify();
        }
        if !text.is_empty() {
            self.send_input(text.as_bytes().to_vec(), cx);
        }
    }

    // The edit menu's Paste reaches the platform text input too; it is a paste, so it is
    // bracketed when the remote asked for that.
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let bracketed = self.mode(cx).contains(TermMode::BRACKETED_PASTE);
            self.send_input(paste_bytes(&text, bracketed), cx);
        }
    }

    /// Composition in progress: show it, send nothing. An empty string ends it.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime.marked = (!new_text.is_empty()).then(|| {
            let len = utf16_len(new_text);
            Marked {
                text: new_text.to_owned(),
                selected: new_selected_range.unwrap_or(len..len),
            }
        });
        if self.ime.marked.is_some() {
            // The cursor is where the text appears, so it has to be on screen.
            self.terminal.update(cx, |t, _| {
                if t.display_offset() > 0 {
                    t.scroll_to_bottom();
                }
            });
            self.note_input();
        }
        cx.notify();
    }

    /// The cursor cell, which is where the candidate window should sit.
    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let g = self.geometry?;
        let (row, col) = self.terminal.read(cx).cursor_cell()?;
        Some(Bounds::new(
            point(
                g.origin.x + gpui::px(g.cell_w * col as f32),
                g.origin.y + gpui::px(g.line_h * row as f32),
            ),
            size(gpui::px(g.cell_w), gpui::px(g.line_h)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    // The find bar takes every key itself, so nothing may compose underneath it.
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.find.is_none()
    }
}
