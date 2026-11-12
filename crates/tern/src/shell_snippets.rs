// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card, overlay) (MIT).
//! Snippets: saved commands (`snippets.json`). Settings → Snippets edits them; ⇧⌘S opens a
//! fuzzy picker and Enter types the chosen command into the active session. The text is
//! written as typed: only a snippet that ends with a newline runs by itself.

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, actions, anchored, deferred, div,
    point, px,
};

use super::Shell;
use super::connections_ui::{button, note, row};
use super::settings_ui::Section;
use crate::empty_state::{self, Listing};
use crate::picker::{self, Key};
use crate::settings_widgets as w;
use crate::snippets::{self, Snippet};
use crate::text_input::TextInput;

actions!(tern, [ToggleSnippets]);

const HINT: &str = "↵ runs when picked; the rest are typed for you to finish.";

pub(super) struct SnippetsUi {
    pub items: Vec<Snippet>,
    /// Set when `snippets.json` exists but cannot be read; saving is refused.
    pub store_error: Option<String>,
    picker: Option<SnippetPicker>,
    form: Option<SnippetForm>,
    confirm_delete: Option<usize>,
}

impl SnippetsUi {
    pub fn load() -> Self {
        let (items, store_error) = match crate::settings::dir().map(|d| snippets::load(&d)) {
            Some(Ok(list)) => (list, None),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "snippets_json_unreadable");
                (Vec::new(), Some(e.to_string()))
            }
            None => (Vec::new(), None),
        };
        Self {
            items,
            store_error,
            picker: None,
            form: None,
            confirm_delete: None,
        }
    }
}

struct SnippetPicker {
    query: String,
    active: usize,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

struct SnippetForm {
    editing: Option<usize>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    error: Option<String>,
    _repaint: Vec<Subscription>,
}

impl Shell {
    // ---- picker -------------------------------------------------------------------------

    pub(super) fn toggle_snippet_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippets.picker.is_some() {
            return self.close_snippet_picker(window, cx);
        }
        let active_session = self.tabs.active_tab().is_some() && self.settings_page.is_none();
        if !active_session {
            return self.activity("Open a session to use a snippet", cx);
        }
        self.picker = None;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.snippets.picker = Some(SnippetPicker {
            query: String::new(),
            active: 0,
            focus,
            scroll: ScrollHandle::new(),
        });
        cx.notify();
    }

    fn close_snippet_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippets.picker.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    fn snippet_matches(&self) -> Vec<usize> {
        let query = self
            .snippets
            .picker
            .as_ref()
            .map(|p| p.query.clone())
            .unwrap_or_default();
        let labels: Vec<String> = self
            .snippets
            .items
            .iter()
            .map(|s| format!("{} {}", s.name, s.command))
            .collect();
        picker::rank_labels(&query, &labels)
    }

    /// Types snippet `ix` into the active session and returns focus to its terminal.
    fn insert_snippet(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_snippet_picker(window, cx);
        let Some(snippet) = self.snippets.items.get(ix).cloned() else {
            return;
        };
        let Some(tab) = self.tabs.active_tab() else {
            return;
        };
        let sent = tab
            .session()
            .is_some_and(|s| s.read(cx).insert_text(&snippet.command));
        if !sent {
            self.alert_failed("The session is not connected, so nothing was typed", cx);
        }
    }

    fn on_snippet_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = picker::classify(event);
        if key == Key::Ignore {
            return;
        }
        cx.stop_propagation();
        let matches = self.snippet_matches();
        let Some(p) = self.snippets.picker.as_mut() else {
            return;
        };
        match key {
            Key::Move(delta) => {
                if !matches.is_empty() {
                    p.active = crate::tabs::step(p.active, matches.len(), delta);
                    p.scroll.scroll_to_item(p.active);
                }
            }
            Key::Type(text) => {
                p.query.push_str(&text);
                p.active = 0;
            }
            Key::Erase => {
                p.query.pop();
                p.active = 0;
            }
            Key::Pick => {
                if let Some(&ix) = matches.get(p.active) {
                    self.insert_snippet(ix, window, cx);
                }
                return;
            }
            Key::Close => return self.close_snippet_picker(window, cx),
            Key::Ignore => {}
        }
        cx.notify();
    }

    pub(super) fn render_snippet_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = self.snippets.picker.as_ref()?;
        let t = &self.theme;
        let viewport = window.viewport_size();
        let matches = self.snippet_matches();
        let active = p.active.min(matches.len().saturating_sub(1));
        let rows = matches.iter().enumerate().map(|(n, &ix)| {
            let s = &self.snippets.items[ix];
            div()
                .id(("snippet-pick", n))
                .mx(px(4.))
                .px(px(12.))
                .py(px(4.))
                .min_h(px(30.))
                .rounded(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .cursor_pointer()
                .hover_fade(
                    format!("snippet-{n}"),
                    gpui::transparent_black(),
                    t.row_active,
                )
                .when(n == active, |el| el.bg(t.row_active))
                .on_click(
                    cx.listener(move |shell, _, window, cx| shell.insert_snippet(ix, window, cx)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(s.name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(t.muted)
                        .child(SharedString::from(snippets::preview(&s.command))),
                )
        });
        let query: SharedString = if p.query.is_empty() {
            "Insert a snippet…".into()
        } else {
            p.query.clone().into()
        };
        let card = div()
            .id("snippet-picker")
            .track_focus(&p.focus)
            .w(px(560.0_f32.min(f32::from(viewport.width) - 32.0)))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_snippet_key))
            .on_mouse_down_out(
                cx.listener(|shell, _, window, cx| shell.close_snippet_picker(window, cx)),
            )
            .child(
                div()
                    .min_h(px(44.))
                    .px(px(16.))
                    .py(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(t.hairline)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .when(p.query.is_empty(), |el| el.text_color(t.faint))
                            .child(query),
                    )
                    .child(picker::kbd(
                        SharedString::from(crate::keymap::badge(
                            cx.global::<crate::keymap::Keymap>()
                                .combo(crate::keymap::ShortcutId::Snippets),
                        )),
                        t,
                    )),
            )
            .child(
                div()
                    .id("snippet-results")
                    .py(px(8.))
                    .max_h(px(picker::results_height(viewport)))
                    .overflow_y_scroll()
                    .track_scroll(&p.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows)
                    .when(matches.is_empty(), |el| {
                        if empty_state::listing(self.snippets.items.len(), matches.len())
                            == Listing::Empty
                        {
                            return el.child(
                                div().py(px(24.)).child(
                                    empty_state::view(t, empty_state::SNIPPETS, "No snippets yet.")
                                        .gap(px(12.))
                                        .child(
                                            button(
                                                "snippet-empty-open",
                                                "Add in Settings",
                                                true,
                                                t,
                                            )
                                            .on_click(
                                                cx.listener(|s, _, w, cx| {
                                                    s.close_snippet_picker(w, cx);
                                                    s.settings_page = Some(Section::Snippets);
                                                    w.focus(&s.focus, cx);
                                                    cx.notify();
                                                }),
                                            ),
                                        ),
                                ),
                            );
                        }
                        el.child(
                            div()
                                .py(px(24.))
                                .flex()
                                .justify_center()
                                .text_sm()
                                .text_color(t.muted)
                                .child("No snippets match"),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(7.))
                    .border_t_1()
                    .border_color(t.hairline)
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(picker::hint("↑ ↓", "Navigate", t))
                    .child(picker::hint("↵", "Type it", t))
                    .child(picker::hint("Esc", "Close", t))
                    .child(div().text_size(px(10.)).text_color(t.faint).child(HINT)),
            );
        Some(overlay(card.into_any_element(), viewport, t))
    }

    // ---- Settings → Snippets --------------------------------------------------------------

    pub(super) fn snippets_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let mut list = w::card(&t);
        let empty = empty_state::listing(self.snippets.items.len(), self.snippets.items.len())
            == Listing::Empty;
        for (ix, s) in self.snippets.items.iter().enumerate() {
            let confirming = self.snippets.confirm_delete == Some(ix);
            let actions = div()
                .flex()
                .gap(px(6.))
                .child(w::button(&t, ("snippet-edit", ix), "Edit").on_click(
                    cx.listener(move |s, _, window, cx| s.open_snippet_form(Some(ix), window, cx)),
                ))
                .child(
                    w::button(
                        &t,
                        ("snippet-delete", ix),
                        if confirming { "Delete?" } else { "Remove" },
                    )
                    .when(confirming, |el| el.text_color(t.danger))
                    .on_click(cx.listener(move |s, _, _, cx| s.delete_snippet(ix, cx))),
                );
            list = list.child(w::row(
                &t,
                ix == 0,
                s.name.clone(),
                Some(SharedString::from(snippets::preview(&s.command))),
                actions,
            ));
        }
        let combo = crate::keymap::badge(
            cx.global::<crate::keymap::Keymap>()
                .combo(crate::keymap::ShortcutId::Snippets),
        );
        w::page_column()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(w::page_header(&t, "Snippets"))
                    .child(w::button(&t, "snippet-new", "New snippet").on_click(
                        cx.listener(|s, _, window, cx| s.open_snippet_form(None, window, cx)),
                    )),
            )
            .child(w::page_subtitle(
                &t,
                format!("{combo} opens the picker in a session. {HINT}"),
            ))
            .when_some(self.snippets.store_error.clone(), |el, e| {
                el.child(w::page_subtitle(&t, e).text_color(t.danger))
            })
            .child(if empty {
                div()
                    .py(px(48.))
                    .child(
                        empty_state::view(
                            &t,
                            empty_state::SNIPPETS,
                            "No snippets yet. Save a command you type often.",
                        )
                        .gap(px(12.))
                        .child(
                            button("snippet-empty-new", "New snippet", true, &t).on_click(
                                cx.listener(|s, _, window, cx| {
                                    s.open_snippet_form(None, window, cx)
                                }),
                            ),
                        ),
                    )
                    .into_any_element()
            } else {
                w::section(&t, "Saved snippets", list).into_any_element()
            })
    }

    fn open_snippet_form(
        &mut self,
        editing: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let t = self.theme;
        let colors = t.input_colors();
        let current = editing.and_then(|ix| self.snippets.items.get(ix).cloned());
        let mut field = |placeholder: &str, value: String| {
            cx.new(|cx| {
                let mut input = TextInput::new(placeholder.to_owned(), false, colors, cx);
                if !value.is_empty() {
                    input.set_text(value, cx);
                }
                input
            })
        };
        let name = field(
            "Tail the service log",
            current.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
        );
        let command = field(
            "journalctl -u tern -f",
            current
                .as_ref()
                .map(|s| snippets::to_field(&s.command))
                .unwrap_or_default(),
        );
        let first = name.focus_handle(cx);
        let repaint = vec![
            cx.observe(&name, |_, _, cx| cx.notify()),
            cx.observe(&command, |_, _, cx| cx.notify()),
        ];
        self.snippets.form = Some(SnippetForm {
            editing,
            name,
            command,
            error: None,
            _repaint: repaint,
        });
        window.focus(&first, cx);
        cx.notify();
    }

    fn close_snippet_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippets.form.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn on_snippet_form_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.snippets.form.as_ref() else {
            return;
        };
        match event.keystroke.key.as_str() {
            "escape" => self.close_snippet_form(window, cx),
            "enter" => self.submit_snippet_form(window, cx),
            "tab" => {
                let (a, b) = (form.name.focus_handle(cx), form.command.focus_handle(cx));
                window.focus(if a.is_focused(window) { &b } else { &a }, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn submit_snippet_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.snippets.form.as_mut() else {
            return;
        };
        if let Some(e) = &self.snippets.store_error {
            form.error = Some(e.clone());
            return cx.notify();
        }
        let editing = form.editing;
        let name = form.name.read(cx).text().to_owned();
        let command = snippets::from_field(form.command.read(cx).text());
        let others: Vec<&str> = self
            .snippets
            .items
            .iter()
            .enumerate()
            .filter(|(ix, _)| Some(*ix) != editing)
            .map(|(_, s)| s.name.as_str())
            .collect();
        let snippet = match snippets::validate(&name, &command, &others) {
            Ok(s) => s,
            Err(e) => {
                form.error = Some(e);
                return cx.notify();
            }
        };
        let mut next = self.snippets.items.clone();
        match editing.and_then(|ix| next.get_mut(ix)) {
            Some(slot) => *slot = snippet,
            None => next.push(snippet),
        }
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = snippets::save(&dir, &next)
        {
            if let Some(form) = self.snippets.form.as_mut() {
                form.error = Some(e.to_string());
            }
            return cx.notify();
        }
        self.snippets.items = next;
        self.close_snippet_form(window, cx);
    }

    /// First click asks, second removes.
    fn delete_snippet(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.snippets.confirm_delete != Some(ix) {
            self.snippets.confirm_delete = Some(ix);
            return cx.notify();
        }
        self.snippets.confirm_delete = None;
        let mut next = self.snippets.items.clone();
        if ix < next.len() {
            next.remove(ix);
        }
        let saved = match crate::settings::dir() {
            Some(dir) => snippets::save(&dir, &next).map_err(|e| e.to_string()),
            None => Ok(()),
        };
        match saved {
            Ok(()) => self.snippets.items = next,
            Err(e) => self.alert_failed(e, cx),
        }
        cx.notify();
    }

    pub(super) fn render_snippet_form(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.snippets.form.as_ref()?;
        let t = &self.theme;
        let viewport = window.viewport_size();
        let title = if form.editing.is_some() {
            "Edit snippet"
        } else {
            "New snippet"
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(16.))
            .py(px(12.))
            .child(row("Name", &form.name, t))
            .child(row("Command", &form.command, t))
            .child(note(HINT, t));
        if let Some(e) = &form.error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(t.danger)
                    .child(SharedString::from(e.clone())),
            );
        }
        let card = div()
            .id("snippet-form")
            .w(px(460.0_f32.min(f32::from(viewport.width) - 32.0)))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_snippet_form_key))
            .child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(t.hairline)
                    .text_size(px(14.))
                    .child(title),
            )
            .child(body)
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(t.hairline)
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(button("snippet-cancel", "Cancel", false, t).on_click(
                        cx.listener(|shell, _, window, cx| shell.close_snippet_form(window, cx)),
                    ))
                    .child(button("snippet-save", "Save", true, t).on_click(
                        cx.listener(|shell, _, window, cx| shell.submit_snippet_form(window, cx)),
                    )),
            );
        Some(overlay(card.into_any_element(), viewport, t))
    }
}

fn overlay(
    card: AnyElement,
    viewport: gpui::Size<gpui::Pixels>,
    t: &crate::theme::Theme,
) -> AnyElement {
    deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .occlude()
                .w(viewport.width)
                .h(viewport.height)
                .bg(t.scrim(0.35))
                .flex()
                .items_center()
                .justify_center()
                .child(card),
        ),
    )
    .priority(2)
    .into_any_element()
}
