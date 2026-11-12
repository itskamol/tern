//! The Settings search: its field, and the page of every section's matches. What matches is
//! `settings_search`'s; how a page hides what does not is `settings_widgets`'.

use gpui::{AppContext as _, Context, Entity, ParentElement, Styled, Subscription, Window, div};

use super::{Section, Shell};
use crate::settings_widgets as w;
use crate::text_input::TextInput;

/// The Settings search field; the page re-filters on every keystroke.
pub(crate) struct SettingsSearch {
    input: Entity<TextInput>,
    _repaint: Subscription,
}

impl SettingsSearch {
    pub(crate) fn new(t: &crate::theme::Theme, cx: &mut Context<Shell>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search settings", false, t.input_colors(), cx));
        let repaint = cx.observe(&input, |_, _, cx| cx.notify());
        Self {
            input,
            _repaint: repaint,
        }
    }
}

impl Shell {
    /// The Settings search's field and what is typed in it.
    pub(super) fn settings_search_input(&self) -> &Entity<TextInput> {
        &self.settings_search.input
    }

    /// ⌘F on the Settings page: the caret goes to the search.
    pub(super) fn focus_settings_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = gpui::Focusable::focus_handle(self.settings_search.input.read(cx), cx);
        window.focus(&focus, cx);
    }

    pub(super) fn clear_settings_search(&mut self, cx: &mut Context<Self>) {
        self.settings_search
            .input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
    }

    /// Every section's matching rows, each page in turn under its own header, or the one line
    /// that says nothing matched.
    pub(super) fn search_results(
        &self,
        searching: &w::Searching,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut pages = Vec::new();
        for section in Section::ALL {
            searching.page(section.label());
            let page = self.settings_page_of(section, cx);
            if searching.matched() > 0 {
                pages.push(page);
            }
        }
        if pages.is_empty() {
            // A page subtitle's look: a quiet line, not an error.
            return w::page_column().child(w::page_subtitle(&self.theme, "No setting matches"));
        }
        div().flex().flex_col().children(pages)
    }
}
