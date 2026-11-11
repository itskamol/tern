//! Opening, switching and closing tabs: a host becomes a tab with one or more panes, each its
//! own SSH session.
//!
//! Splits: a tab holds one or more panes laid out as a tree (see `split.rs`; the verbs that make
//! them are in `shell_split.rs`). ⌘W closes a pane before it closes the tab.

use crate::split::{Direction, PaneId};
use gpui::{Context, Window};
use tern_ssh::{ConnectSpec, HostEntry};

use super::*;

impl Shell {
    /// Switches to the host's tab if it has one, reconnecting it when closed; otherwise opens
    /// a new tab.
    pub fn connect_host(&mut self, host: HostEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.record_recent(&host.alias);
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|tab| tab.is_session() && tab.alias == host.alias)
        {
            if let Some(session) = self.tabs[ix].session().cloned() {
                session.update(cx, |s, cx| {
                    if s.status().is_dormant() {
                        s.reconnect(cx);
                    }
                });
            }
            return self.show_tab(ix, window, cx);
        }
        match ConnectSpec::from_host_entry(&host) {
            Ok(spec) => self.open_tab(Launch::Ssh(spec), host.alias, window, cx),
            Err(e) => self.fail(e.to_string(), cx),
        }
    }

    /// `tern <target>`: a `~/.ssh/config` alias or `user@host:port`.
    pub fn connect_target(&mut self, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A saved connection's name wins, so `tern web-01` and the picker's typed name open the
        // same host (with its password command, jump host and group) as clicking it does.
        if let Some(host) = self.hosts.iter().find(|h| h.alias == target).cloned() {
            return self.connect_host(host, window, cx);
        }
        match ConnectSpec::parse(target) {
            Ok(spec) => self.open_tab(Launch::Ssh(spec), target.to_string(), window, cx),
            Err(e) => self.fail(e.to_string(), cx),
        }
    }

    pub(super) fn open_tab(
        &mut self,
        launch: Launch,
        alias: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.make_pane(launch, &alias, window, cx);
        self.tabs.open(alias, pane);
        self.error = None;
        self.settings_page = None;
        self.front_tab_changed(window, cx);
    }

    /// A session with the settings applied, and an eye on it for the day it drops.
    pub(super) fn make_pane(
        &mut self,
        launch: Launch,
        alias: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Pane {
        // Debug builds: keep scripted logins out of the real known_hosts file.
        #[cfg(debug_assertions)]
        let launch = match launch {
            Launch::Ssh(spec) => Launch::Ssh(with_dev_known_hosts(spec)),
            Launch::SshIdle(spec) => Launch::SshIdle(with_dev_known_hosts(spec)),
        };
        let theme = self.terminal_theme(alias);
        let auth = self.auth_for(alias);
        let session = Session::open(
            launch,
            auth,
            alias.to_owned(),
            self.settings.log_sessions,
            theme,
            window,
            cx,
        );
        let meta = self.settings.option_as_meta;
        let view = session.read(cx).view.clone();
        let options = self.terminal_options();
        view.update(cx, |v, cx| {
            v.set_option_as_meta(meta);
            v.set_options(options, cx);
        });
        let bell = self.on_bell(&session, window, cx);
        // A tab that is not in front can drop without anyone seeing its terminal; say so, with
        // a way to get to it.
        let mut last = Status::Connecting;
        let repaint =
            cx.observe(&session, move |shell, session, cx| {
                let status = session.read(cx).status();
                if status == Status::Closed && last != Status::Closed {
                    let ix = shell
                        .tabs
                        .iter()
                        .position(|t| t.panes().iter().any(|p| p.session() == Some(&session)));
                    if let Some(ix) =
                        ix.filter(|ix| *ix != shell.tabs.active() || shell.settings_page.is_some())
                    {
                        let alias = shell.tabs[ix].alias.clone();
                        shell.toast(
                            Toast::new(ToastKind::Critical, format!("{alias} disconnected"))
                                .action("Show", move |s, window, cx| {
                                    if let Some(ix) = s
                                        .tabs
                                        .iter()
                                        .position(|t| t.is_session() && t.alias == alias)
                                    {
                                        s.show_tab(ix, window, cx);
                                    }
                                }),
                            cx,
                        );
                    }
                }
                last = status;
                cx.notify();
            });
        let notes = cx.subscribe(
            &session,
            |shell, session, note: &crate::session::SessionNote, cx| {
                shell.on_session_note(&session, note, cx)
            },
        );
        Pane {
            id: self.tabs.new_pane_id(),
            host: alias.to_owned(),
            content: Content::Session(TermPane {
                session,
                _repaint: repaint,
                _bell: bell,
                _notes: notes,
            }),
        }
    }

    /// Makes tab `ix` the active one. Settings, when open, stays open over it: this runs for
    /// reorders, closes and restores too, which are not a request to see the session.
    pub fn activate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.activate(ix) {
            self.front_tab_changed(window, cx);
        }
    }

    /// The tab in front changed, or what it is: keep it in view in the strip, give its
    /// terminal the keyboard, and point a broadcast at it.
    pub(super) fn front_tab_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.active_tab() else {
            window.focus(&self.focus, cx);
            return cx.notify();
        };
        self.tab_scroll.reveal(self.tabs.active());
        // Behind Settings the terminal is hidden, and keys sent to it would be typed blind.
        if self.settings_page.is_none() {
            let focus = tab.focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.apply_broadcast(cx);
        cx.notify();
    }

    /// Shows tab `ix` because the user asked for that session, so Settings closes.
    pub fn show_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page = None;
        self.activate_tab(ix, window, cx);
    }

    /// Closing a tab drops its session, which ends the connection.
    pub fn close_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.close(ix).is_none() {
            return;
        }
        self.renaming = None;
        self.sync_broadcast(cx);
        self.front_tab_changed(window, cx);
    }
}

/// 1320×880; debug builds take `TERN_DEV_WINDOW=900x600` so a scripted check can open the
/// window at its minimum.
fn start_size() -> (f32, f32) {
    #[cfg(debug_assertions)]
    if let Some((w, h)) = std::env::var("TERN_DEV_WINDOW")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)))
    {
        return (w, h);
    }
    (1320., 880.)
}

/// Centred on the main display; debug builds take `TERN_DEV_WINDOW_AT=x,y` to place it, e.g. on
/// a 1× external display.
pub(super) fn start_bounds(cx: &gpui::App) -> gpui::Bounds<gpui::Pixels> {
    let (w, h) = start_size();
    #[allow(unused_mut)]
    let mut bounds = gpui::Bounds::centered(None, gpui::size(gpui::px(w), gpui::px(h)), cx);
    #[cfg(debug_assertions)]
    if let Some((x, y)) = std::env::var("TERN_DEV_WINDOW_AT")
        .ok()
        .and_then(|v| v.split_once(',').map(|(x, y)| (x.parse(), y.parse())))
        .and_then(|(x, y)| Some((x.ok()?, y.ok()?)))
    {
        bounds.origin = gpui::point(gpui::px(x), gpui::px(y));
    }
    bounds
}

impl Shell {
    /// ⌘W: a split tab loses the focused pane (its session ends, and the pane before it takes
    /// the keyboard); a tab with one pane closes.
    pub(crate) fn close_pane_or_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.close_pane().is_some() {
            self.focus_front_pane(window, cx);
        } else if !self.tabs.is_empty() {
            self.close_tab_at(self.tabs.active(), window, cx);
        }
    }

    pub(crate) fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.focus_pane(id) {
            self.focus_front_pane(window, cx);
        }
    }

    pub(crate) fn move_pane_focus(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(to) = self.tabs.neighbour(dir) {
            self.focus_pane(to, window, cx);
        }
    }

    /// Gives the keyboard to the focused pane of the front tab, and points a broadcast at it.
    pub(super) fn focus_front_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.active_tab() {
            let focus = tab.focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.apply_broadcast(cx);
        cx.notify();
    }
}
