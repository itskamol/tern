// Adapted from zeron crates/ui/src/lib.rs (window options) (MIT).
//! The main window: frosted shell, titlebar with tabs, host sidebar and the main panel.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, MouseUpEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowOptions, actions, div, point, px, size,
};
#[cfg(debug_assertions)]
use tern_ssh::ConnectSpec;
use tern_ssh::HostEntry;

use crate::chrome::{self, DragGhost, SidebarResize, WidthTween};
use crate::connections::{self, Connection};
use crate::picker::{self, Picker, ToggleHostPicker};
use crate::session::{Launch, Session, Status};
use crate::settings::{self, SIDEBAR_DEFAULT, Settings};
use crate::split::{self, PaneId};
use crate::tab_set::TabSet;
use crate::tabs::{self, ActivateTab, CloseTab, NextTab, PrevTab, TabInfo};
use crate::tabs_store::DiskTabs;
use crate::theme::{Theme, UI_FONT};
use crate::{sidebar, statusline};

actions!(
    tern,
    [
        ToggleSidebar,
        OpenFiles,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        NewConnection,
        SplitRight,
        SplitDown,
        SplitRightWithHost,
        SplitDownWithHost,
        ResizePaneLeft,
        ResizePaneRight,
        ResizePaneUp,
        ResizePaneDown,
        EqualizePanes,
        ZoomPane,
        ClosePane,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        OpenSettings,
        ShowAbout,
        CheckForUpdates
    ]
);

#[path = "shell_broadcast.rs"]
mod broadcast_ui;
#[path = "shell_files.rs"]
mod files_ui;
#[path = "shell_pane.rs"]
mod pane_ui;
use pane_ui::{Content, Pane, Tab, TermPane};
#[path = "shell_forwards.rs"]
mod forwards_ui;
pub(crate) use forwards_ui::{ClearTerminal, FillPassword};
pub(crate) use history_ui::ToggleCommandHistory;
#[path = "shell_history.rs"]
mod history_ui;
#[path = "shell_look.rs"]
mod look;
#[path = "shell_menu.rs"]
mod menu;
#[path = "shell_messages.rs"]
mod messages_ui;
#[path = "shell_open.rs"]
mod open;
#[path = "shell_sample.rs"]
mod sample_ui;
#[path = "shell_settings.rs"]
mod settings_ui;
#[path = "shell_split.rs"]
mod split_ui;
#[path = "shell_sync.rs"]
mod sync_ui;
#[path = "shell_tabs.rs"]
mod tabs_ui;
#[path = "shell_term_options.rs"]
mod term_options;
#[path = "shell_update.rs"]
mod update_ui;
#[path = "shell_vault_page.rs"]
mod vault_page_ui;
#[path = "shell_vault.rs"]
mod vault_ui;

pub(crate) use messages_ui::{Alert, CubicBezier};
#[path = "shell_themes.rs"]
mod themes_ui;

pub(crate) use themes_ui::ThemeTarget;

#[path = "shell_connections.rs"]
pub(crate) mod connections_ui;
#[path = "shell_hostlist.rs"]
mod hostlist;
#[path = "shell_import.rs"]
mod import_ui;
#[path = "shell_snippets.rs"]
mod snippets_ui;

pub(crate) use hostlist::{FocusHostSearch, RedoHosts, UndoHosts};
pub(crate) use snippets_ui::ToggleSnippets;

pub fn open_main_window(cx: &mut App) -> anyhow::Result<WindowHandle<Shell>> {
    let bounds = open::start_bounds(cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(900.), px(600.))),
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(14.), px(14.))),
        }),
        app_owns_titlebar_drag: true,
        window_background: if crate::theme::BLURS_BEHIND {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Opaque
        },
        app_id: Some("tern".into()),
        ..Default::default()
    };
    let recent = settings::dir()
        .map(|d| hostlist::Recent::load(&d))
        .unwrap_or_default();
    let (connections, store_error) = match settings::dir().map(|d| connections::load(&d)) {
        Some(Ok(list)) => (list, None),
        Some(Err(e)) => {
            tracing::warn!(error = %e, "hosts_json_unreadable");
            (Vec::new(), Some(e.to_string()))
        }
        None => (Vec::new(), None),
    };
    let window = cx.open_window(options, |_, cx| {
        cx.new(|cx| {
            let theme = Theme::zeron_dark();
            let settings = settings::dir()
                .map(|d| Settings::load(&d))
                .unwrap_or_default();
            let mut shell = Shell {
                focus: cx.focus_handle(),
                tabs: TabSet::new(Box::new(DiskTabs), settings.reopen_tabs),
                drop_hint: None,
                settings,
                theme,
                hosts: Vec::new(),
                connections,
                store_error,
                form: None,
                confirm_delete: None,
                renaming: None,
                pane_numbers: false,
                broadcast: None,
                broadcast_picker: None,
                panels: Default::default(),
                transfers: cx.new(|cx| {
                    crate::transfers::Transfers::new(crate::platform::SshRuntime::handle(cx), cx)
                }),
                quit_ask: false,
                conflict_all: false,
                _transfers_subs: Vec::new(),
                error: None,
                picker: None,
                history: Default::default(),
                sidebar_tween: None,
                settings_page: None,
                sample: None,
                theme_picker: None,
                recording: None,
                record_notice: None,
                record_interceptor: None,
                sync_ui: None,
                auto_sync: sync_ui::AutoSync::default(),
                vault_ui: None,
                last_input: std::time::Instant::now(),
                activity: messages_ui::ActivityUi::default(),
                tab_scroll: tabs::StripScroll::default(),
                context_menu: None,
                hostlist: hostlist::HostList::new(&theme, recent, cx),
                settings_search: settings_ui::SettingsSearch::new(&theme, cx),
                snippets: snippets_ui::SnippetsUi::load(),
                import: None,
                frame: look::FrameState::default(),
                system_light: false,
                update_ready: None,
                update_check: Default::default(),
                _clock: None,
            };
            shell.refresh_hosts();
            shell._transfers_subs = vec![
                cx.subscribe(
                    &shell.transfers,
                    |s, _, done: &crate::transfers::Finished, cx| s.on_transfer_finished(done, cx),
                ),
                cx.observe(&shell.transfers, |s, _, cx| s.on_transfers_changed(cx)),
            ];
            shell
        })
    })?;
    window.update(cx, |_, window, cx| {
        // The close button asks first when copies are running.
        let shell = cx.entity();
        window.on_window_should_close(cx, move |_, cx| {
            shell.update(cx, |s, cx| s.close_allowed(cx))
        });
    })?;
    window.update(cx, |shell, window, cx| {
        shell.reapply(settings::Reapply::all(), cx);
        shell.sync_display_preferences(cx);
        crate::drawables::apply(window);
        // Resize, full screen and display changes all move the bounds; re-apply is a no-op
        // while the count holds.
        cx.observe_window_bounds(window, |_, window, _| {
            crate::drawables::apply(window);
        })
        .detach();
        // macOS posts no notification gpui forwards, so re-read the preference whenever tern
        // comes to the front: the user changes it in System Settings, then switches back.
        cx.observe_window_activation(window, |shell, window, cx| {
            crate::platform::apply_reduce_motion(shell.settings.reduce_motion, cx);
            shell.sync_display_preferences(cx);
            if window.is_window_active() {
                for session in shell
                    .tabs
                    .iter()
                    .flat_map(|t| t.panes())
                    .filter_map(Pane::session)
                {
                    session.update(cx, |s, cx| s.nudge(cx));
                }
            }
        })
        .detach();
        shell.system_light = is_light(window.appearance());
        shell.retheme(cx);
        shell.install_input_colors(cx);
        shell.start_clock(cx);
        cx.observe_window_appearance(window, |shell, window, cx| {
            shell.system_light = is_light(window.appearance());
            shell.retheme(cx);
        })
        .detach();
        shell.check_for_updates(cx);
        shell.start_auto_sync(window, cx);
    })?;
    // With no tab open nothing else holds focus, and gpui only dispatches key bindings along
    // the focused element's path, so the shell itself must be focused for ⌘T to work.
    window.update(cx, |shell, window, cx| window.focus(&shell.focus, cx))?;
    window.update(cx, |shell, window, cx| shell.restore_tabs(window, cx))?;
    window.update(cx, |shell, _, cx| shell.watch_idle(cx))?;
    Ok(window)
}

/// macOS's light appearances; the dark ones and anything new count as dark.
fn is_light(appearance: gpui::WindowAppearance) -> bool {
    matches!(
        appearance,
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
    )
}

pub struct Shell {
    focus: FocusHandle,
    settings: Settings,
    theme: Theme,
    /// What the sidebar and picker list: tern's own connections, in `hosts.json` order.
    hosts: Vec<HostEntry>,
    connections: Vec<Connection>,
    /// Set when `hosts.json` exists but cannot be read; saving is refused so it is not lost.
    store_error: Option<String>,
    form: Option<connections_ui::ConnectionForm>,
    confirm_delete: Option<usize>,
    tabs: TabSet<Pane>,
    /// A host being dragged over a pane: the pane and the side it would take.
    drop_hint: Option<(PaneId, split::Direction)>,
    /// The tab whose title is being edited in the strip.
    renaming: Option<tabs_ui::Rename>,
    /// ⌥⌘ is held: each pane of a split tab shows its number and host.
    pane_numbers: bool,
    /// Input typed in one tab also going to others, and the list that picks them.
    broadcast: Option<broadcast_ui::Broadcast>,
    broadcast_picker: Option<broadcast_ui::BroadcastPicker>,
    panels: forwards_ui::Panels,
    /// The copies in progress: the queue is the shell's, so closing a Files tab does not stop
    /// one.
    transfers: Entity<crate::transfers::Transfers>,
    /// Quitting with copies running is being asked about.
    quit_ask: bool,
    /// The conflict sheet's "apply to all" box.
    conflict_all: bool,
    _transfers_subs: Vec<Subscription>,
    error: Option<String>,
    picker: Option<Picker>,
    history: history_ui::HistoryUi,
    sidebar_tween: Option<WidthTween>,
    settings_page: Option<settings_ui::Section>,
    /// The Settings sample terminal, alive only while a page showing it is open.
    sample: Option<Entity<tern_term::TerminalView>>,
    theme_picker: Option<themes_ui::ThemePicker>,
    /// The shortcut being recorded in Settings → Shortcuts, and why the last one was refused.
    recording: Option<crate::keymap::ShortcutId>,
    record_notice: Option<String>,
    record_interceptor: Option<Subscription>,
    sync_ui: Option<sync_ui::SyncUi>,
    auto_sync: sync_ui::AutoSync,
    vault_ui: Option<vault_ui::VaultUi>,
    /// The last key or mouse input to the window, for the vault's idle lock.
    last_input: std::time::Instant,
    /// Activity and Alerts raised so far, and the Activity list.
    activity: messages_ui::ActivityUi,
    tab_scroll: tabs::StripScroll,
    context_menu: Option<menu::ContextMenu>,
    hostlist: hostlist::HostList,
    settings_search: settings_ui::SettingsSearch,
    snippets: snippets_ui::SnippetsUi,
    import: Option<import_ui::ImportSheet>,
    frame: look::FrameState,
    /// Whether macOS is in its light appearance; what `Appearance: System` follows.
    system_light: bool,
    /// A downloaded, checked update; it is installed when tern quits, or on "Restart now".
    update_ready: Option<std::rc::Rc<update_ui::Staged>>,
    /// Whether an update check runs, and how the last one ended.
    update_check: crate::update_outcome::CheckState,
    _clock: Option<gpui::Task<()>>,
}

/// Debug builds: keep scripted logins out of the real ~/.ssh/known_hosts.
#[cfg(debug_assertions)]
pub(crate) fn with_dev_known_hosts(spec: ConnectSpec) -> ConnectSpec {
    ConnectSpec {
        known_hosts: std::env::var_os("TERN_KNOWN_HOSTS")
            .map(Into::into)
            .or(spec.known_hosts),
        ..spec
    }
}

impl Shell {
    pub fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::debug!(open = self.picker.is_some(), "picker_toggle");
        if self.picker.is_some() {
            return self.close_picker(window, cx);
        }
        let picker = Picker::new(cx);
        window.focus(picker.focus(), cx);
        self.picker = Some(picker);
        cx.notify();
    }

    /// Closing hands focus back to the active terminal, so typing resumes where it was.
    pub fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    /// Applies a settings change and writes it out; a failed write is logged, not fatal.
    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            look::snap(self.settings.sidebar_width, self.frame.scale)
        }
    }

    /// The sidebar's width this frame: mid-tween while collapsing or expanding.
    fn sidebar_now(&self) -> f32 {
        self.sidebar_tween
            .and_then(|tween| tween.sample(std::time::Instant::now()))
            .map_or_else(
                || self.sidebar_target(),
                |w| look::snap(w, self.frame.scale),
            )
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_now();
        self.update_settings(|s| s.sidebar_collapsed = !s.sidebar_collapsed, cx);
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
    }

    /// Live drag: follows the pointer without writing the file on every move.
    fn on_sidebar_drag(&mut self, x: f32, cx: &mut Context<Self>) {
        self.settings.sidebar_width = look::snap(chrome::dragged_width(x), self.frame.scale);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None;
        cx.notify();
    }

    fn resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("sidebar-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(self.sidebar_now() - chrome::HANDLE_HALF_WIDTH))
            .w(px(chrome::HANDLE_HALF_WIDTH * 2.0))
            .occlude()
            .cursor_col_resize()
            .on_drag(SidebarResize, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|shell, event: &MouseUpEvent, _, cx| {
                    let reset = event.click_count == 2;
                    shell.update_settings(
                        |s| {
                            if reset {
                                s.sidebar_width = SIDEBAR_DEFAULT;
                            }
                        },
                        cx,
                    );
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|shell, _, _, _| shell.save_settings()),
            )
    }

    fn refresh_hosts(&mut self) {
        self.hosts = self
            .connections
            .iter()
            .map(|c| c.entry_in(&self.connections))
            .collect();
    }

    /// Focus back to the active terminal, or the window when there is none.
    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match self.tabs.active_tab() {
            Some(tab) => tab.focus_handle(cx),
            None => self.focus.clone(),
        };
        window.focus(&focus, cx);
        cx.notify();
    }

    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    pub fn picker_query(&self) -> String {
        self.picker
            .as_ref()
            .map(|p| p.query().to_string())
            .unwrap_or_default()
    }

    pub fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        tracing::warn!(error = %error, "connect_target_invalid");
        self.error = Some(error);
        cx.notify();
    }

    fn tab_infos(&self, cx: &App) -> Vec<TabInfo> {
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
