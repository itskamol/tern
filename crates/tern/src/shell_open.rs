//! Opening, switching and closing tabs: a host becomes a tab with one or more panes, each its
//! own SSH session.
//!
//! Splits: a tab holds one or more panes laid out as a tree (see `split.rs`). A split opens
//! another session like the focused one; ⌘W closes a pane before it closes the tab.

use crate::split::{Axis, Direction, Node, PaneId};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Focusable, InteractiveElement, IntoElement, ParentElement, Styled, Window,
    div, px, transparent_black,
};
use tern_ssh::{ConnectSpec, HostEntry};

use super::*;

impl Shell {
    /// Switches to the host's tab if it has one, reconnecting it when closed; otherwise opens
    /// a new tab.
    pub fn connect_host(&mut self, host: HostEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.record_recent(&host.alias);
        if let Some(ix) = self.tabs.iter().position(|tab| tab.alias == host.alias) {
            let session = self.tabs[ix].session().clone();
            session.update(cx, |s, cx| {
                if s.status().is_dormant() {
                    s.reconnect(cx);
                }
            });
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
        self.next_tab += 1;
        self.tabs.push(Tab {
            id: self.next_tab,
            alias,
            title: None,
            tree: split::Node::Leaf(pane.id),
            focused: pane.id,
            panes: vec![pane],
        });
        self.error = None;
        self.show_tab(self.tabs.len() - 1, window, cx);
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
                        .position(|t| t.panes.iter().any(|p| p.session == session));
                    if let Some(ix) =
                        ix.filter(|ix| *ix != shell.active || shell.settings_page.is_some())
                    {
                        let alias = shell.tabs[ix].alias.clone();
                        shell.toast(
                            Toast::new(ToastKind::Critical, format!("{alias} disconnected"))
                                .action("Show", move |s, window, cx| {
                                    if let Some(ix) = s.tabs.iter().position(|t| t.alias == alias) {
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
        self.next_pane += 1;
        Pane {
            id: self.next_pane,
            session,
            _repaint: repaint,
            _bell: bell,
            _notes: notes,
        }
    }

    /// Makes tab `ix` the active one. Settings, when open, stays open over it: this runs for
    /// reorders, closes and restores too, which are not a request to see the session.
    pub fn activate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        self.active = ix;
        self.persist_tabs();
        self.tab_scroll.reveal(ix);
        // Behind Settings the terminal is hidden, and keys sent to it would be typed blind.
        if self.settings_page.is_none() {
            let focus = tab.session().read(cx).view.focus_handle(cx);
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
        if ix >= self.tabs.len() {
            return;
        }
        self.renaming = None;
        self.tabs.remove(ix);
        self.sync_broadcast(cx);
        if self.tabs.is_empty() {
            self.active = 0;
            self.persist_tabs();
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        let next = if self.active > ix || self.active == self.tabs.len() {
            self.active - 1
        } else {
            self.active
        };
        self.activate_tab(next, window, cx);
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
    /// Splits the focused pane of the front tab; the new pane has the keyboard.
    pub(crate) fn split_pane(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let launch = tab.session().read(cx).launch_again();
        let alias = tab.alias.clone();
        let (target, ix) = (tab.focused, self.active);
        let pane = self.make_pane(launch, &alias, window, cx);
        let id = pane.id;
        let Some(tab) = self.tabs.get_mut(ix) else {
            return;
        };
        if !tab.tree.split(target, axis, id) {
            return;
        }
        tab.panes.push(pane);
        tab.focused = id;
        self.focus_front_pane(window, cx);
    }

    /// ⌘W: a split tab loses the focused pane, a tab with one pane closes.
    pub(crate) fn close_pane_or_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() > 1 {
            self.close_pane(window, cx);
        } else {
            self.close_tab_at(self.active, window, cx);
        }
    }

    /// Closes the focused pane (its session ends); the pane before it, or the one after for
    /// the first, takes the keyboard.
    fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let order = tab.tree.leaves();
        let Some(at) = order.iter().position(|id| *id == tab.focused) else {
            return;
        };
        let next = if at > 0 { order[at - 1] } else { order[1] };
        let tree = std::mem::replace(&mut tab.tree, Node::Leaf(tab.focused));
        let Some(rest) = tree.remove(tab.focused) else {
            return;
        };
        tab.tree = rest;
        tab.panes.retain(|p| p.id != tab.focused);
        tab.focused = next;
        self.focus_front_pane(window, cx);
    }

    pub(crate) fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.focused == id || !tab.tree.contains(id) {
            return;
        }
        tab.focused = id;
        self.focus_front_pane(window, cx);
    }

    pub(crate) fn move_pane_focus(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if let Some(to) = tab.tree.neighbour(tab.focused, dir) {
            self.focus_pane(to, window, cx);
        }
    }

    /// Gives the keyboard to the focused pane of the front tab, and points a broadcast at it.
    fn focus_front_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            let focus = tab.session().read(cx).view.focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.apply_broadcast(cx);
        cx.notify();
    }

    /// The tab's terminals, in their split layout.
    pub(super) fn render_panes(&self, tab: &Tab, cx: &mut Context<Self>) -> AnyElement {
        self.render_node(tab, &tab.tree, tab.panes.len() > 1, cx)
    }

    fn render_node(
        &self,
        tab: &Tab,
        node: &Node,
        split: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        match node {
            Node::Leaf(id) => {
                let Some(pane) = tab.panes.iter().find(|p| p.id == *id) else {
                    return div().into_any_element();
                };
                let id = *id;
                let focused = tab.focused == id;
                div()
                    .id(("pane", id))
                    .size_full()
                    // Only a split tab marks its focus; a lone pane would just wear a frame.
                    .when(split, |el| {
                        el.border_1().border_color(if focused {
                            t.accent.opacity(0.55)
                        } else {
                            transparent_black()
                        })
                    })
                    // Before the terminal sees the click, so selecting text in a pane also
                    // gives it the keyboard.
                    .capture_any_mouse_down(cx.listener(move |s, _, w, cx| s.focus_pane(id, w, cx)))
                    .child(tern_term::TerminalView::cached_child(
                        &pane.session.read(cx).view,
                    ))
                    .into_any_element()
            }
            Node::Split {
                axis,
                first,
                second,
            } => {
                let half = |el: gpui::Div| el.flex_1().min_w(px(0.)).min_h(px(0.));
                let a = self.render_node(tab, first, split, cx);
                let b = self.render_node(tab, second, split, cx);
                let divider = match axis {
                    Axis::Row => div().flex_none().bg(t.border).w(px(1.)).h_full(),
                    Axis::Column => div().flex_none().bg(t.border).h(px(1.)).w_full(),
                };
                div()
                    .size_full()
                    .flex()
                    .when(*axis == Axis::Column, |el| el.flex_col())
                    .child(half(div()).child(a))
                    .child(divider)
                    .child(half(div()).child(b))
                    .into_any_element()
            }
        }
    }
}
