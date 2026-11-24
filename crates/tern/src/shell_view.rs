//! Main window rendering: titlebar, sidebar/main tiles, empty state and top-level action wiring.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::connections_ui;
use super::forwards_ui::{ClearTerminal, FillPassword};
use super::history_ui::ToggleCommandHistory;
use super::hostlist::{FocusHostSearch, RedoHosts, UndoHosts};
use super::snippets_ui::ToggleSnippets;
use super::split_ui;
use super::{
    CheckForUpdates, DecreaseFontSize, IncreaseFontSize, NewConnection, OpenFiles, OpenSettings,
    ResetFontSize, Shell, ShowAbout, ToggleFilesPanel, ToggleSidebar,
};
use crate::chrome::{self, SidebarResize};
use crate::picker::{self, ToggleHostPicker};
use crate::settings;
use crate::sidebar;
use crate::tabs::{self, ActivateTab, CloseTab, NextTab, PrevTab, TabInfo};
use crate::theme::UI_FONT;

impl Shell {
    pub(super) fn tab_infos(&self, cx: &App) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let panes = tab.in_reading_order();
                (ix, tab, panes)
            })
            .map(|(ix, tab, panes)| TabInfo {
                alias: tab.alias.clone(),
                heading: tab.heading(),
                logging: tab.session().is_some_and(|s| s.read(cx).is_logging()),
                broadcast: self.is_broadcasting(tab.id),
                dots: tabs::tab_dots(panes.iter().map(|p| p.session().map(|s| s.read(cx).dot()))),
                front: tab.focus_in_reading_order(),
                zoomed: tab.zoomed().is_some(),
                rename: self
                    .renaming
                    .as_ref()
                    .filter(|r| r.ix == ix)
                    .map(|r| r.input.clone()),
            })
            .collect()
    }

    /// The main panel's content: the active tab's panes, or the empty view.
    fn panel_content(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(tab) = self.tabs.active_tab() {
            return self.render_panes(tab, cx);
        }
        let t = self.theme;
        let keymap = cx.global::<crate::keymap::Keymap>();
        let hint = |id: crate::keymap::ShortcutId, label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .min_w(px(44.))
                        .flex()
                        .justify_end()
                        .text_sm()
                        .text_color(t.text)
                        .child(SharedString::from(crate::keymap::badge(keymap.combo(id)))),
                )
                .child(div().text_sm().text_color(t.muted).child(label))
        };
        let (title, detail): (SharedString, Option<SharedString>) = match &self.error {
            Some(e) => ("Could not open that".into(), Some(e.clone().into())),
            None if self.hosts.is_empty() => (
                "No hosts yet".into(),
                Some("Add a connection, or copy your hosts over from ~/.ssh/config".into()),
            ),
            None => ("No session open".into(), None),
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .child(div().text_size(px(15.)).text_color(t.text).child(title))
                    .when_some(detail, |el, d| {
                        el.child(div().text_sm().text_color(t.muted).child(d))
                    }),
            )
            .when(self.hosts.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            connections_ui::button("empty-main-new", "New connection", true, &t)
                                .on_click(
                                    cx.listener(|s, _, w, cx| s.open_form(None, None, w, cx)),
                                ),
                        )
                        .child(
                            connections_ui::button(
                                "empty-main-import",
                                "Import from ~/.ssh/config…",
                                false,
                                &t,
                            )
                            .on_click(cx.listener(|s, _, w, cx| s.open_import(w, cx))),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(hint(crate::keymap::ShortcutId::HostPicker, "Find a host"))
                    .child(hint(
                        crate::keymap::ShortcutId::NewConnection,
                        "New connection",
                    ))
                    .child(hint(crate::keymap::ShortcutId::Settings, "Settings")),
            )
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _timed = tern_term::frame_stats::start(tern_term::frame_stats::Span::ShellRender);
        // Whatever held focus can disappear (a field hidden after sync, a closed form or tab);
        // with nothing focused gpui dispatches no keys at all, so the window takes it back.
        if window.focused(cx).is_none() {
            window.focus(&self.focus, cx);
        }
        self.sync_density(window);
        // ⌥⌘ released in another app never reaches this window: no labels while it is behind.
        self.pane_numbers &= window.is_window_active();
        self.sync_sample(window, cx);
        self.sync_frame(window);
        let t = self.theme;
        let infos = self.tab_infos(cx);
        let active_alias = infos.get(self.tabs.active()).map(|i| i.alias.clone());
        let strip = tabs::strip(&infos, self.tabs.active(), &self.tab_scroll, &t, cx);
        let query = self.search_query(cx);
        let sidebar = sidebar::render(
            &sidebar::SidebarState {
                hosts: &self.hosts,
                connections: &self.connections,
                open: &infos,
                active_alias: active_alias.as_deref(),
                width: self.settings.sidebar_width,
                collapsed_groups: &self.hostlist.collapsed_groups,
                query: &query,
                search: &self.hostlist.search,
                selected: self.sidebar_selection(window, cx),
            },
            &t,
            cx,
        );
        let form = self.render_form(window, cx);
        let import = self.render_import(window, cx);
        let snippet_picker = self.render_snippet_picker(window, cx);
        let snippet_form = self.render_snippet_form(window, cx);
        let panel_bg = self.panel_background();
        let (line_activity, bar_activity) = self.activity_slots();
        let status_line = self.render_status_line(panel_bg, line_activity, cx);
        let theme_picker = self.render_theme_picker(window, cx);
        let toast = self.render_top_right(window, cx);
        let activity_list = self.render_activity_list(window, cx);
        let context_menu = self.render_menu(cx);
        let sidebar_now = self.sidebar_now();
        if self.sidebar_tween.is_some() {
            if sidebar_now == self.sidebar_target() {
                self.sidebar_tween = None;
            } else {
                window.request_animation_frame();
            }
        }
        let collapsed = self.settings.sidebar_collapsed;
        let handle = (!collapsed && self.sidebar_tween.is_none()).then(|| self.resize_handle(cx));
        let root = div()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|s, _, _, _| s.last_input = std::time::Instant::now()))
            .capture_any_mouse_down(
                cx.listener(|s, _, _, _| s.last_input = std::time::Instant::now()),
            )
            .on_mouse_move(cx.listener(|s, _, _, _| s.last_input = std::time::Instant::now()))
            .on_scroll_wheel(cx.listener(|s, _, _, _| s.last_input = std::time::Instant::now()))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(self.window_base())
            .font_family(UI_FONT)
            .text_color(t.text)
            .on_action(cx.listener(|s, _: &CloseTab, w, cx| s.close_pane_or_tab(w, cx)))
            .map(|el| split_ui::pane_actions(el, cx))
            .on_action(cx.listener(|s, _: &NextTab, w, cx| {
                s.show_tab(tabs::step(s.tabs.active(), s.tabs.len(), 1), w, cx)
            }))
            .on_action(cx.listener(|s, _: &PrevTab, w, cx| {
                s.show_tab(tabs::step(s.tabs.active(), s.tabs.len(), -1), w, cx)
            }))
            .on_action(cx.listener(|s, a: &ActivateTab, w, cx| s.show_tab(a.0, w, cx)))
            .on_modifiers_changed(cx.listener(|s, e: &gpui::ModifiersChangedEvent, _, cx| {
                s.hold_pane_numbers(e.modifiers, cx)
            }))
            .on_action(cx.listener(|s, _: &ToggleHostPicker, w, cx| s.toggle_picker(w, cx)))
            .on_action(cx.listener(|s, _: &FocusHostSearch, w, cx| s.focus_search(w, cx)))
            .on_action(cx.listener(|s, _: &ToggleSnippets, w, cx| s.toggle_snippet_picker(w, cx)))
            .on_action(
                cx.listener(|s, _: &ToggleCommandHistory, w, cx| s.toggle_command_history(w, cx)),
            )
            .on_action(cx.listener(|s, _: &ToggleSidebar, _, cx| s.toggle_sidebar(cx)))
            .on_action(cx.listener(|s, _: &OpenFiles, w, cx| s.open_files_tab(None, w, cx)))
            .on_action(cx.listener(|s, _: &ToggleFilesPanel, _, cx| s.toggle_files_panel(cx)))
            .on_action(cx.listener(|s, _: &FillPassword, w, cx| s.fill_password(w, cx)))
            .on_action(cx.listener(|s, _: &ClearTerminal, _, cx| s.clear_terminal(cx)))
            .on_action(cx.listener(|s, _: &NewConnection, w, cx| s.open_form(None, None, w, cx)))
            .on_action(cx.listener(|s, _: &OpenSettings, w, cx| s.toggle_settings(w, cx)))
            .on_action(cx.listener(|s, _: &ShowAbout, w, cx| s.open_settings_at_about(w, cx)))
            .when(!self.update_check_running(), |d| {
                d.on_action(cx.listener(|s, _: &CheckForUpdates, _, cx| s.check_now(cx)))
            })
            .on_action(cx.listener(|s, _: &UndoHosts, w, cx| s.undo_hosts(w, cx)))
            .on_action(cx.listener(|s, _: &RedoHosts, w, cx| s.redo_hosts(w, cx)))
            // Capture phase: the terminal handles Escape itself, and ending a broadcast must
            // not depend on which pane has focus.
            .capture_key_down(cx.listener(|s, e: &gpui::KeyDownEvent, _, cx| {
                // The Activity list too: a focused terminal would take its Escape otherwise.
                if e.keystroke.key == "escape"
                    && e.keystroke.modifiers == gpui::Modifiers::default()
                    && (s.on_broadcast_escape(cx) || s.close_activity_list(cx))
                {
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|s, e: &gpui::KeyDownEvent, w, cx| {
                if s.on_rename_key(e, w, cx) {
                    cx.stop_propagation();
                    return;
                }
                if e.keystroke.key == "escape" && s.context_menu.is_some() {
                    s.context_menu = None;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if s.settings_page.is_some() && s.settings_find_key(e, w, cx) {
                    cx.stop_propagation();
                    return;
                }
                if e.keystroke.key == "escape" && s.settings_page.is_some() && s.form.is_none() {
                    s.toggle_settings(w, cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|s, _: &IncreaseFontSize, _, cx| {
                s.update_settings(|st| st.step_font(1.0), cx)
            }))
            .on_action(cx.listener(|s, _: &DecreaseFontSize, _, cx| {
                s.update_settings(|st| st.step_font(-1.0), cx)
            }))
            .on_action(cx.listener(|s, _: &ResetFontSize, _, cx| {
                s.update_settings(|st| st.terminal_font_size = settings::FONT_DEFAULT, cx)
            }))
            .on_drag_move(
                cx.listener(|s, e: &gpui::DragMoveEvent<SidebarResize>, _, cx| {
                    s.on_sidebar_drag(f32::from(e.event.position.x), cx)
                }),
            )
            .child(chrome::render_titlebar(
                &t,
                window.is_fullscreen(),
                !self.settings.sidebar_collapsed,
                strip,
                chrome::Activity {
                    text: bar_activity,
                    open: self.activity_list_open(),
                },
                cx,
            ))
            .child(match self.settings_page {
                Some(section) => self.render_settings(section, window, cx),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .relative()
                    .child(self.side_tile(sidebar_now, sidebar))
                    .child(
                        self.main_tile(panel_bg)
                            .child(div().flex_1().min_h_0().child(self.panel_content(cx)))
                            .when_some(status_line, |el, line| el.child(line)),
                    )
                    .children(self.files_panel_tile(cx))
                    .children(handle)
                    .into_any_element(),
            })
            .when_some(form, |el, form| el.child(form))
            .when_some(import, |el, i| el.child(i))
            .when_some(snippet_picker, |el, p| el.child(p))
            .when_some(self.render_command_history(window, cx), |el, p| el.child(p))
            .when_some(snippet_form, |el, f| el.child(f))
            .when_some(theme_picker, |el, p| el.child(p))
            .when_some(context_menu, |el, m| el.child(m))
            .when_some(self.render_broadcast_picker(cx), |el, p| el.child(p))
            .children(self.render_panels(cx))
            .children(self.render_conflict_sheet(window, cx))
            .children(self.render_quit_sheet(window, cx))
            .children(activity_list)
            .when_some(toast, |el, t| el.child(t))
            .when_some(self.picker.as_ref(), |el, p| {
                el.child(picker::render(
                    p,
                    &self.hosts,
                    &self.picker_matches(),
                    &self.recent_aliases(),
                    window.viewport_size(),
                    &t,
                    cx,
                ))
            });
        // Another frame while any hover fade is mid-flight (zeron drives it the same way).
        if crate::hover::fades_active() {
            window.request_animation_frame();
        }
        root
    }
}
