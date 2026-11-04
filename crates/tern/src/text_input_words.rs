//! Word and line editing in a text field, as macOS fields do it: ⌥ moves and deletes by word,
//! ⌘ to the start or end, a double click selects a word. A masked field is one word, so a
//! secret's shape is never revealed by where the cursor stops.
use std::ops::Range;

use gpui::{Context, EntityInputHandler, InteractiveElement, KeyBinding, Window};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    CONTEXT, DeleteToStart, DeleteWordLeft, DeleteWordRight, SelectEnd, SelectHome, SelectWordLeft,
    SelectWordRight, TextInput, WordLeft, WordRight,
};

/// By word and to the ends of the line: ⌥ and ⌘ on macOS, Ctrl elsewhere (where Home and End
/// already go to the ends).
pub(super) fn bindings() -> Vec<KeyBinding> {
    #[cfg(target_os = "macos")]
    let word = "alt";
    #[cfg(not(target_os = "macos"))]
    let word = "ctrl";
    let ctx = Some(CONTEXT);
    #[allow(unused_mut)]
    let mut keys = vec![
        KeyBinding::new(&format!("{word}-left"), WordLeft, ctx),
        KeyBinding::new(&format!("{word}-right"), WordRight, ctx),
        KeyBinding::new(&format!("shift-{word}-left"), SelectWordLeft, ctx),
        KeyBinding::new(&format!("shift-{word}-right"), SelectWordRight, ctx),
        KeyBinding::new(&format!("{word}-backspace"), DeleteWordLeft, ctx),
        KeyBinding::new(&format!("{word}-delete"), DeleteWordRight, ctx),
    ];
    #[cfg(target_os = "macos")]
    keys.extend([
        KeyBinding::new("cmd-left", super::Home, ctx),
        KeyBinding::new("cmd-right", super::End, ctx),
        KeyBinding::new("shift-cmd-left", SelectHome, ctx),
        KeyBinding::new("shift-cmd-right", SelectEnd, ctx),
        KeyBinding::new("cmd-backspace", DeleteToStart, ctx),
    ]);
    keys
}

/// A word is a run with a letter or digit in it; spaces and punctuation are skipped over.
fn is_word(segment: &str) -> bool {
    segment.chars().any(char::is_alphanumeric)
}

/// Where ⌥← lands from `offset`: the start of the word before it, past any gap first.
pub(super) fn word_start_before(text: &str, offset: usize) -> usize {
    text.split_word_bound_indices()
        .take_while(|(i, _)| *i < offset)
        .filter(|(_, s)| is_word(s))
        .last()
        .map_or(0, |(i, _)| i)
}

/// Where ⌥→ lands from `offset`: the end of the word after it, past any gap first.
pub(super) fn word_end_after(text: &str, offset: usize) -> usize {
    text.split_word_bound_indices()
        .find(|(i, s)| i + s.len() > offset && is_word(s))
        .map_or(text.len(), |(i, s)| i + s.len())
}

/// What a double click at `offset` selects: the word there, or the gap when it is not on one.
pub(super) fn word_at(text: &str, offset: usize) -> Range<usize> {
    text.split_word_bound_indices()
        .find(|(i, s)| offset < i + s.len())
        .or_else(|| text.split_word_bound_indices().next_back())
        .map_or(0..0, |(i, s)| i..i + s.len())
}

impl TextInput {
    pub(super) fn word_actions<E: InteractiveElement>(el: E, cx: &mut Context<Self>) -> E {
        el.on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_start))
    }

    fn word_before(&self, offset: usize) -> usize {
        if self.masked {
            0
        } else {
            word_start_before(&self.content, offset)
        }
    }

    fn word_after(&self, offset: usize) -> usize {
        if self.masked {
            self.content.len()
        } else {
            word_end_after(&self.content, offset)
        }
    }

    /// A double click selects the word under it, a triple click (or any click in a masked
    /// field) everything.
    pub(super) fn select_by_clicks(
        &mut self,
        offset: usize,
        clicks: usize,
        cx: &mut Context<Self>,
    ) {
        self.selection_reversed = false;
        self.selected_range = if clicks >= 3 || self.masked {
            0..self.content.len()
        } else {
            word_at(&self.content, offset)
        };
        cx.notify();
    }

    pub(super) fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.start
        };
        self.move_to(self.word_before(from), cx);
    }

    pub(super) fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() {
            self.cursor_offset()
        } else {
            self.selected_range.end
        };
        self.move_to(self.word_after(from), cx);
    }

    pub(super) fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.word_before(self.cursor_offset()), cx);
    }

    pub(super) fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.word_after(self.cursor_offset()), cx);
    }

    pub(super) fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    pub(super) fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    pub(super) fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let to = self.word_before(self.cursor_offset());
        self.delete_back_to(to, window, cx);
    }

    pub(super) fn delete_to_start(
        &mut self,
        _: &DeleteToStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_back_to(0, window, cx);
    }

    pub(super) fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let to = self.word_after(self.cursor_offset());
            if to == self.cursor_offset() {
                return window.play_system_bell();
            }
            self.select_to(to, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Deletes the selection, or from the cursor back to `to` when nothing is selected.
    fn delete_back_to(&mut self, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if to == self.cursor_offset() {
                return window.play_system_bell();
            }
            self.select_to(to, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{word_at, word_end_after, word_start_before};

    #[cfg(target_os = "macos")]
    #[test]
    fn option_and_command_arrows_move_and_select_as_macos_fields() {
        use super::super::tests::resolves_to;
        use super::super::*;
        assert!(resolves_to::<WordLeft>("alt-left"));
        assert!(resolves_to::<WordRight>("alt-right"));
        assert!(resolves_to::<SelectWordLeft>("shift-alt-left"));
        assert!(resolves_to::<SelectWordRight>("shift-alt-right"));
        assert!(resolves_to::<Home>("cmd-left"));
        assert!(resolves_to::<End>("cmd-right"));
        assert!(resolves_to::<SelectHome>("shift-cmd-left"));
        assert!(resolves_to::<SelectEnd>("shift-cmd-right"));
        assert!(resolves_to::<DeleteWordLeft>("alt-backspace"));
        assert!(resolves_to::<DeleteToStart>("cmd-backspace"));
    }

    const TEXT: &str = "deploy@web-01.example.com  -p 22";

    #[test]
    fn option_left_stops_at_each_word_start_skipping_punctuation() {
        let mut at = TEXT.len();
        let mut stops = Vec::new();
        while at > 0 {
            at = word_start_before(TEXT, at);
            stops.push(&TEXT[at..]);
        }
        assert_eq!(
            stops,
            [
                "22",
                "p 22",
                "example.com  -p 22",
                "01.example.com  -p 22",
                "web-01.example.com  -p 22",
                TEXT,
            ]
        );
    }

    #[test]
    fn option_right_stops_at_each_word_end_skipping_punctuation() {
        let mut at = 0;
        let mut stops = Vec::new();
        while at < TEXT.len() {
            at = word_end_after(TEXT, at);
            stops.push(&TEXT[..at]);
        }
        assert_eq!(
            stops,
            [
                "deploy",
                "deploy@web",
                "deploy@web-01",
                "deploy@web-01.example.com",
                "deploy@web-01.example.com  -p",
                TEXT,
            ]
        );
        assert_eq!(word_end_after("a  ", 1), 3);
        assert_eq!(word_end_after("", 0), 0);
    }

    #[test]
    fn double_click_selects_the_word_or_the_gap_under_it() {
        assert_eq!(&TEXT[word_at(TEXT, 2)], "deploy");
        assert_eq!(&TEXT[word_at(TEXT, "deploy@we".len())], "web");
        assert_eq!(&TEXT[word_at(TEXT, "deploy".len())], "@");
        assert_eq!(&TEXT[word_at(TEXT, TEXT.len())], "22");
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn words_never_split_a_multibyte_character() {
        let text = "salom dunyo — o‘zbek";
        let mut at = text.len();
        while at > 0 {
            at = word_start_before(text, at);
            assert!(text.is_char_boundary(at));
        }
        assert!(text.is_char_boundary(word_end_after(text, 1)));
    }
}
