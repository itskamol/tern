// Adapted from zeron crates/ui/src/shell.rs (settings route: nav column, section list,
// toggle_settings, Escape to close) (MIT).
//! The Settings page. ⌘, swaps the window body for it, as zeron does; Escape or Back returns.
//! Every control writes `settings.json` at once.

use crate::hover::Accessible as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

#[path = "shell_settings_nav.rs"]
mod nav;

use super::Shell;
use crate::keymap::{Keymap, Record, ShortcutId, badge, record};
use crate::settings::{self, AppearanceMode, FONT_DEFAULT, FONT_MAX, FONT_MIN, Settings};
use crate::settings_widgets as w;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Appearance,
    Terminal,
    Shortcuts,
    Connections,
    Snippets,
    Vault,
    Sync,
    About,
}

impl Section {
    const ALL: [Section; 8] = [
        Section::Appearance,
        Section::Terminal,
        Section::Shortcuts,
        Section::Connections,
        Section::Snippets,
        Section::Vault,
        Section::Sync,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Terminal => "Terminal",
            Section::Shortcuts => "Shortcuts",
            Section::Connections => "Connections",
            Section::Snippets => "Snippets",
            Section::Vault => "Vault",
            Section::Sync => "Sync",
            Section::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::Appearance => crate::icons::PALETTE,
            Section::Terminal => crate::icons::TERMINAL,
            Section::Shortcuts => crate::icons::KEYBOARD,
            Section::Connections => crate::icons::SERVER,
            Section::Snippets => crate::icons::FILE_CODE,
            Section::Vault => crate::icons::KEY,
            Section::Sync => crate::icons::CLOUD,
            Section::About => crate::icons::INFO,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Appearance => "nav-appearance",
            Section::Terminal => "nav-terminal",
            Section::Shortcuts => "nav-shortcuts",
            Section::Connections => "nav-connections",
            Section::Snippets => "nav-snippets",
            Section::Vault => "nav-vault",
            Section::Sync => "nav-sync",
            Section::About => "nav-about",
        }
    }
}

impl Shell {
    /// Changes, saves and applies settings: whatever the change needs re-applied to the running
    /// app (palette, open terminals, motion) is, here, so no caller has to remember it.
    pub fn update_settings(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        let old = self.settings.clone();
        change(&mut self.settings);
        self.settings = self.settings.clone().clamped();
        self.save_settings();
        self.reapply(old.reapply(&self.settings), cx);
        cx.notify();
    }

    pub(super) fn save_settings(&self) {
        if let Some(dir) = settings::dir()
            && let Err(e) = self.settings.save(&dir)
        {
            tracing::warn!(error = %e, "settings_save_failed");
        }
    }

    /// Applies what a settings change asks for: at launch everything, after a change or a sync
    /// reload what [`Settings::reapply`] found.
    pub(crate) fn reapply(&mut self, what: settings::Reapply, cx: &mut Context<Self>) {
        if what.motion {
            crate::platform::apply_reduce_motion(self.settings.reduce_motion, cx);
        }
        let restyled = what.palette && self.retheme(cx);
        if what.terminal_style && !restyled {
            self.restyle_tabs(cx);
        }
        if what.terminal_options {
            let options = self.terminal_options();
            for pane in self.tabs.iter().flat_map(|t| t.panes()) {
                let view = pane.session.read(cx).view.clone();
                view.update(cx, |v, cx| v.set_options(options.clone(), cx));
            }
        }
        if what.option_as_meta {
            self.apply_option_as_meta(cx);
        }
        if what.reopen_tabs {
            self.tabs.set_saving(self.settings.reopen_tabs);
        }
    }

    pub(super) fn apply_option_as_meta(&self, cx: &mut Context<Self>) {
        let on = self.settings.option_as_meta;
        for tab in &self.tabs {
            for pane in tab.panes() {
                let view = pane.session.read(cx).view.clone();
                view.update(cx, |v, _| v.set_option_as_meta(on));
            }
        }
    }
}

impl Shell {
    pub(crate) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_page.take().is_some() {
            self.restore_focus(window, cx);
        } else {
            self.picker = None;
            self.settings_page = Some(Section::Appearance);
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// tern → About tern: the Settings page that has the version and links.
    pub(crate) fn open_settings_at_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.select_section(Section::About, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn open_settings_at_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.settings_page = Some(Section::Sync);
        self.ensure_sync_ui(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Opens `section` and starts whatever it shows that loads lazily.
    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.settings_page = Some(section);
        if section == Section::Sync {
            self.ensure_sync_ui(cx);
        }
        if section == Section::Vault {
            self.ensure_vault_ui(cx);
            self.unlock_from_keychain(cx);
        }
        cx.notify();
    }

    pub(crate) fn render_settings(
        &self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let nav = nav::render(self, section, window, cx);
        let page = match section {
            Section::Appearance => self.appearance_page(cx),
            Section::Terminal => self.terminal_page(cx),
            Section::Shortcuts => self.shortcuts_page(cx),
            Section::Connections => self.connections_page(cx),
            Section::Snippets => self.snippets_page(cx),
            Section::Vault => self.vault_page(cx),
            Section::Sync => self.sync_page(cx),
            Section::About => self.about_page(cx),
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.side_tile(
                super::look::snap(self.settings.sidebar_width, self.frame.scale),
                nav,
            ))
            // The page is the body's main tile, so it carries the same tint as every other
            // view of it over a wallpaper. Its fill is the shell colour, not the active tab's
            // scheme: the page's text is in the app palette, and a dark scheme under the light
            // appearance would leave it unreadable.
            .child(
                self.main_tile(self.theme.shell).child(
                    div()
                        .id("settings-scroll")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .child(page),
                ),
            )
            .into_any_element()
    }

    fn appearance_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let size = self.settings.terminal_font_size;
        let (minus, value, plus) = w::stepper(&t, "font-size", format!("{size:.0} pt"));
        let font =
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(minus.on_click(
                    cx.listener(|s, _, _, cx| s.update_settings(|st| st.step_font(-1.0), cx)),
                ))
                .child(value)
                .child(plus.on_click(
                    cx.listener(|s, _, _, cx| s.update_settings(|st| st.step_font(1.0), cx)),
                ))
                .child(w::button(&t, "font-reset", "Reset").on_click(cx.listener(
                    |s, _, _, cx| s.update_settings(|st| st.terminal_font_size = FONT_DEFAULT, cx),
                )));
        let reduce = self.settings.reduce_motion;
        let show_status = self.settings.show_status_line;
        let sharp = self.settings.sharp_text;
        let mut appearance_modes = div().flex().gap(px(6.));
        for (id, label, mode) in [
            ("appearance-system", "System", AppearanceMode::System),
            ("appearance-light", "Light", AppearanceMode::Light),
            ("appearance-dark", "Dark", AppearanceMode::Dark),
        ] {
            let selected = self.settings.appearance == mode;
            appearance_modes = appearance_modes.child(
                w::button(&t, id, label)
                    .when(selected, |el| {
                        el.bg(t.ink(0.18)).font_weight(gpui::FontWeight::MEDIUM)
                    })
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.update_settings(|st| st.appearance = mode, cx);
                    })),
            );
        }
        w::page_column()
            .child(w::page_header(&t, "Appearance"))
            .child(w::section(
                &t,
                "Theme",
                w::card(&t)
                .child(w::row(
                    &t,
                    true,
                    "Appearance",
                    Some("System follows macOS; the default terminal colours follow it too".into()),
                    appearance_modes,
                ))
                .child(w::row(
                    &t,
                    false,
                    "Show status line",
                    Some("Host, connection state and session time under the terminal".into()),
                    div()
                        .id("toggle-status-line")
                        .switch("Show status line", show_status)
                        .cursor_pointer()
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.update_settings(|st| st.show_status_line = !st.show_status_line, cx)
                        }))
                        .child(w::toggle(&t, show_status, "status-line")),
                ))
                .child(w::row(
                    &t,
                    false,
                    "Sharper text on standard displays",
                    Some(
                        "On 1x monitors: whole-pixel sizes, thinner icons, an opaque window and stronger muted text"
                            .into(),
                    ),
                    div()
                        .id("toggle-sharp-text")
                        .switch("Sharper text on standard displays", sharp)
                        .cursor_pointer()
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.update_settings(|st| st.sharp_text = !st.sharp_text, cx)
                        }))
                        .child(w::toggle(&t, sharp, "sharp-text")),
                ))
                .child(w::row(
                    &t,
                    false,
                    "Terminal theme",
                    Some(
                        "For every host without its own; set one per host from the sidebar".into(),
                    ),
                    w::button(
                        &t,
                        "theme-default",
                        self.settings
                            .terminal_theme
                            .clone()
                            .unwrap_or_else(|| crate::theme::themes::default_name(t.light).to_owned()),
                    )
                    .on_click(cx.listener(|s, _, window, cx| {
                        s.open_theme_picker(super::ThemeTarget::Default, window, cx)
                    })),
                )),
            ))
            .child(w::section(
                &t,
                "Terminal text",
                w::card(&t).child(w::row(
                    &t,
                    true,
                    "Font size",
                    Some(
                        format!(
                            "{FONT_MIN:.0}–{FONT_MAX:.0} pt · {} {} {}",
                            crate::keymap::primary_hint("="),
                            crate::keymap::primary_hint("−"),
                            crate::keymap::primary_hint("0"),
                        )
                        .into(),
                    ),
                    font,
                )),
            ))
            .child(w::section(&t, "Wallpaper", self.wallpaper_card(cx)))
            .child(w::section(
                &t,
                "Motion",
                w::card(&t).child(w::row(
                    &t,
                    true,
                    "Reduce motion",
                    Some("Hold the connecting pulse and sidebar slide still. On by itself when macOS Reduce motion is on".into()),
                    div()
                        .id("toggle-reduce-motion")
                        .switch("Reduce motion", reduce)
                        .cursor_pointer()
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.update_settings(|st| st.reduce_motion = !st.reduce_motion, cx);
                        }))
                        .child(w::toggle(&t, reduce, "reduce-motion")),
                )),
            ))
    }

    fn shortcuts_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let keymap = cx.global::<Keymap>().clone();
        let mut card = w::card(&t);
        for (ix, id) in ShortcutId::ALL.into_iter().enumerate() {
            let recording = self.recording == Some(id);
            let combo = keymap.combo(id);
            let chip = w::button(
                &t,
                ("shortcut", ix),
                if recording {
                    "Press keys…".to_owned()
                } else {
                    badge(combo)
                },
            )
            .min_w(px(88.))
            .when(recording, |el| el.border_1().border_color(t.accent))
            .on_click(cx.listener(move |s, _, window, cx| s.start_recording(id, window, cx)));
            let mut control = div().flex().items_center().gap(px(6.)).child(chip);
            if !keymap.is_default(id) {
                control = control.child(w::button(&t, ("shortcut-reset", ix), "Reset").on_click(
                    cx.listener(move |s, _, _, cx| {
                        s.set_shortcut(id, id.default_combo(), cx);
                    }),
                ));
            }
            card = card.child(w::row(&t, ix == 0, id.label(), None, control));
        }
        w::page_column()
            .child(w::page_header(&t, "Shortcuts"))
            .child(w::page_subtitle(
                &t,
                if cfg!(target_os = "macos") {
                    "Click a shortcut, then press the new keys. Esc cancels, ⌫ unbinds. Keys without \
                     ⌘, ⌃ or ⌥ always go to the remote shell."
                } else {
                    "Click a shortcut, then press the new keys. Esc cancels, Backspace unbinds. \
                     Keys without Ctrl or Alt always go to the remote shell."
                },
            ))
            .when_some(self.record_notice.clone(), |el, notice| {
                el.child(w::page_subtitle(&t, notice).text_color(t.danger))
            })
            .child(w::section(&t, "App", card))
            .child(w::section(
                &t,
                "Fixed",
                w::card(&t)
                    .child(w::row(
                        &t,
                        true,
                        "Switch to tab 1–9",
                        None,
                        div().text_sm().text_color(t.muted).child(if cfg!(target_os = "macos") {
                            "⌘1 … ⌘9"
                        } else {
                            "Alt+1 … Alt+9"
                        }),
                    ))
                    .child(w::row(
                        &t,
                        false,
                        "Next / previous tab",
                        None,
                        div().text_sm().text_color(t.muted).child(if cfg!(target_os = "macos") {
                            "⌃Tab  ⌃⇧Tab"
                        } else {
                            "Ctrl+Tab  Ctrl+Shift+Tab"
                        }),
                    ))
                    .child(w::row(
                        &t,
                        false,
                        if cfg!(target_os = "macos") {
                            "Quit, hide, minimise"
                        } else {
                            "Quit"
                        },
                        None,
                        div().text_sm().text_color(t.muted).child(if cfg!(target_os = "macos") {
                            "⌘Q  ⌘H  ⌘M"
                        } else {
                            "Ctrl+Shift+Q"
                        }),
                    )),
            ))
    }

    fn start_recording(&mut self, id: ShortcutId, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = Some(id);
        self.record_notice = None;
        window.focus(&self.focus, cx);
        // Bound actions run before element key listeners, so intercept first: the chord being
        // recorded must not also fire whatever it is bound to now.
        let shell = cx.entity().downgrade();
        self.record_interceptor = Some(cx.intercept_keystrokes(move |event, _, cx| {
            let ks = &event.keystroke;
            let m = &ks.modifiers;
            let outcome = record(&ks.key, m.control, m.alt, m.shift, m.platform);
            let _ = shell.update(cx, |s, cx| s.on_recorded(outcome, cx));
            cx.stop_propagation();
        }));
        cx.notify();
    }

    fn on_recorded(&mut self, outcome: Record, cx: &mut Context<Self>) {
        let Some(id) = self.recording else {
            return;
        };
        match outcome {
            Record::Ignored => return,
            Record::Cancelled => {}
            Record::Cleared => self.set_shortcut(id, "", cx),
            Record::Set(combo) => match cx.global::<Keymap>().refusal(id, &combo) {
                Some(why) => self.record_notice = Some(why),
                None => self.set_shortcut(id, &combo, cx),
            },
        }
        self.recording = None;
        self.record_interceptor = None;
        cx.notify();
    }

    /// Saves `keymap.json` and re-installs every binding.
    fn set_shortcut(&mut self, id: ShortcutId, combo: &str, cx: &mut Context<Self>) {
        cx.global_mut::<Keymap>().set(id, combo);
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = cx.global::<Keymap>().save(&dir)
        {
            self.record_notice = Some(format!("Could not save keymap.json: {e}"));
        }
        crate::keymap::apply(cx);
        cx.notify();
    }

    fn connections_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let mut list = w::card(&t);
        if self.connections.is_empty() {
            list = list.child(w::row(
                &t,
                true,
                "No connections yet",
                Some("Hosts from ~/.ssh/config are listed in the sidebar".into()),
                div(),
            ));
        }
        for (ix, c) in self.connections.iter().enumerate() {
            let address = if c.port == 22 {
                format!("{}@{}", c.user, c.host)
            } else {
                format!("{}@{}:{}", c.user, c.host, c.port)
            };
            let confirming = self.confirm_delete == Some(ix);
            let actions = div()
                .flex()
                .gap(px(6.))
                .child(
                    w::button(&t, ("settings-edit", ix), "Edit").on_click(cx.listener(
                        move |s, _, window, cx| {
                            let draft = s.connection(ix);
                            s.open_form(Some(ix), draft, window, cx);
                        },
                    )),
                )
                .child(
                    w::button(
                        &t,
                        ("settings-delete", ix),
                        if confirming { "Delete?" } else { "Remove" },
                    )
                    .when(confirming, |el| el.text_color(t.danger))
                    .on_click(cx.listener(move |s, _, _, cx| s.delete_connection(ix, cx))),
                );
            list = list.child(w::row(
                &t,
                ix == 0,
                c.name.clone(),
                Some(SharedString::from(address)),
                actions,
            ));
        }
        w::page_column()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(w::page_header(&t, "Connections"))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                w::button(&t, "settings-import", "Import from ~/.ssh/config…")
                                    .on_click(
                                        cx.listener(|s, _, window, cx| s.open_import(window, cx)),
                                    ),
                            )
                            .child(w::button(&t, "settings-new", "New connection").on_click(
                                cx.listener(|s, _, window, cx| s.open_form(None, None, window, cx)),
                            )),
                    ),
            )
            .child(w::page_subtitle(
                &t,
                format!(
                    "{} saved in tern. Import copies hosts out of ~/.ssh/config once; \
                     it is never read or written afterwards.",
                    self.connections.len()
                ),
            ))
            .when_some(self.store_error.clone(), |el, e| {
                el.child(w::page_subtitle(&t, e).text_color(t.danger))
            })
            .child(w::section(&t, "Saved in tern", list))
    }
}

impl Shell {
    fn about_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let on = self.settings.check_for_updates;
        w::page_column()
            .child(w::page_header(&t, "About"))
            .child(w::section(
                &t,
                "tern",
                w::card(&t)
                    .child(w::row(
                        &t,
                        true,
                        "Version",
                        Some(env!("CARGO_PKG_VERSION").into()),
                        div(),
                    ))
                    .child(w::row(
                        &t,
                        true,
                        "Check for updates",
                        Some(
                            "Once a day, ask GitHub for a newer release; tern never installs one"
                                .into(),
                        ),
                        div()
                            .id("toggle-check-updates")
                            .switch("Check for updates", on)
                            .cursor_pointer()
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.update_settings(
                                    |st| st.check_for_updates = !st.check_for_updates,
                                    cx,
                                );
                            }))
                            .child(w::toggle(&t, on, "check-updates")),
                    ))
                    .child(w::row(
                        &t,
                        false,
                        "Logs",
                        Some(crate::settings::log_dir_label().into()),
                        div(),
                    )),
            ))
    }
}
