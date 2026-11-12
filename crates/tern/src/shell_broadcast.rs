//! Broadcast input: what is typed or pasted in one tab is also written to the open sessions
//! picked from a popover. Every tab that takes part carries a badge; Escape or the tab menu
//! ends it. Input never goes to a closed session (see `Session::send_if_live`).

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    SharedString, StatefulInteractiveElement, Styled, anchored, deferred, div, px,
};

use super::Shell;
use crate::session::Status;

/// A tab's identity for as long as it is open; unlike its slot it survives reordering, and
/// unlike its session it survives a change of the focused pane.
pub(super) type TabId = u64;

/// Who is typing and who receives it, by tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Broadcast {
    pub source: TabId,
    pub targets: Vec<TabId>,
}

impl Broadcast {
    pub fn new(source: TabId) -> Self {
        Self {
            source,
            targets: Vec::new(),
        }
    }

    /// Adds the session, or removes it when it is already a target. The source is never its
    /// own target: it would receive every keystroke twice.
    pub fn toggle(&mut self, id: TabId) {
        if id == self.source {
            return;
        }
        match self.targets.iter().position(|t| *t == id) {
            Some(at) => {
                self.targets.remove(at);
            }
            None => self.targets.push(id),
        }
    }

    /// Takes part: types here, or receives.
    pub fn involves(&self, id: TabId) -> bool {
        self.source == id || self.targets.contains(&id)
    }

    /// Drops targets that are no longer open. False once the source is gone, or nothing is
    /// left to receive: the broadcast has ended.
    pub fn retain_open(&mut self, open: &[TabId]) -> bool {
        self.targets.retain(|t| open.contains(t));
        open.contains(&self.source) && !self.targets.is_empty()
    }
}

/// The popover that picks the receivers, and where it sits.
pub(super) struct BroadcastPicker {
    pub source: TabId,
    pub position: Point<Pixels>,
}

impl Shell {
    pub(crate) fn is_broadcasting(&self, id: TabId) -> bool {
        self.broadcast
            .as_ref()
            .is_some_and(|b| !b.targets.is_empty() && b.involves(id))
    }

    /// Opens the receiver list for the tab at `ix`, keeping the choice made earlier from the
    /// same tab.
    pub(crate) fn open_broadcast_picker(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.tabs.get(ix).map(|t| t.id) else {
            return;
        };
        if self.broadcast.as_ref().is_none_or(|b| b.source != source) {
            self.broadcast = Some(Broadcast::new(source));
            self.apply_broadcast(cx);
        }
        self.broadcast_picker = Some(BroadcastPicker { source, position });
        cx.notify();
    }

    pub(crate) fn toggle_broadcast_target(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(id) = self.tabs.get(ix).map(|t| t.id) else {
            return;
        };
        if let Some(broadcast) = self.broadcast.as_mut() {
            broadcast.toggle(id);
        }
        self.apply_broadcast(cx);
    }

    pub(crate) fn stop_broadcast(&mut self, cx: &mut Context<Self>) {
        self.broadcast = None;
        self.broadcast_picker = None;
        self.apply_broadcast(cx);
    }

    /// After tabs closed: forget the ones that are gone.
    pub(crate) fn sync_broadcast(&mut self, cx: &mut Context<Self>) {
        let open: Vec<TabId> = self.tabs.iter().map(|t| t.id).collect();
        let picking = self.broadcast_picker.is_some();
        if let Some(broadcast) = self.broadcast.as_mut() {
            // While the list is open an empty selection is still a broadcast in the making.
            let alive =
                broadcast.retain_open(&open) || (picking && open.contains(&broadcast.source));
            if !alive {
                self.broadcast = None;
                self.broadcast_picker = None;
            }
        }
        self.apply_broadcast(cx);
    }

    /// Points the source at its receivers, and every other session at none.
    pub(super) fn apply_broadcast(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let id = tab.id;
            let mirrors = match &self.broadcast {
                Some(b) if b.source == id => self
                    .tabs
                    .iter()
                    .filter(|t| b.targets.contains(&t.id))
                    .filter_map(|t| t.session().map(|s| s.downgrade()))
                    .collect(),
                _ => Vec::new(),
            };
            if let Some(session) = tab.session() {
                session.update(cx, |s, _| s.set_mirrors(mirrors));
            }
        }
        cx.notify();
    }

    /// Escape ends a broadcast (or just closes the list); true when the key was used.
    pub(super) fn on_broadcast_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.broadcast_picker.take().is_some() {
            if self
                .broadcast
                .as_ref()
                .is_some_and(|b| b.targets.is_empty())
            {
                self.broadcast = None;
            }
            cx.notify();
            return true;
        }
        if self.broadcast.is_some() {
            self.stop_broadcast(cx);
            return true;
        }
        false
    }

    pub(super) fn render_broadcast_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.broadcast_picker.as_ref()?;
        let t = self.theme;
        let source_label = self
            .tabs
            .iter()
            .find(|tab| tab.id == picker.source)
            .map(|tab| tab.heading().text)
            .unwrap_or_default();
        let mut card = div()
            .id("broadcast-picker")
            .occlude()
            .w(px(264.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(12.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.text)
            .on_mouse_down_out(cx.listener(|s, _, _, cx| {
                s.on_broadcast_escape(cx);
            }))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .child(SharedString::from(format!(
                        "What you type in {source_label} also goes to:"
                    ))),
            );
        let mut any = false;
        for (ix, tab) in self.tabs.iter().enumerate() {
            let id = tab.id;
            if id == picker.source {
                continue;
            }
            any = true;
            let live = tab
                .session()
                .is_some_and(|s| s.read(cx).status() == Status::Connected);
            let on = self
                .broadcast
                .as_ref()
                .is_some_and(|b| b.targets.contains(&id));
            let label = tab.heading().text;
            card = card.child(
                div()
                    .id(("broadcast-target", ix))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .when(live, |el| {
                        el.cursor_pointer().hover(|s| s.bg(t.row_active)).on_click(
                            cx.listener(move |s, _, _, cx| s.toggle_broadcast_target(ix, cx)),
                        )
                    })
                    .when(!live, |el| el.opacity(0.5))
                    .child(
                        div()
                            .flex_none()
                            .size(px(14.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(if on { t.accent } else { t.muted })
                            .when(on, |el| el.bg(t.accent))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.))
                            .text_color(t.text)
                            .child(if on { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(label)),
                    )
                    .when(!live, |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(t.faint)
                                .child("not connected"),
                        )
                    }),
            );
        }
        if !any {
            card = card.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_color(t.muted)
                    .child("No other tab is open"),
            );
        }
        if self
            .broadcast
            .as_ref()
            .is_some_and(|b| !b.targets.is_empty())
        {
            card = card.child(
                div()
                    .id("broadcast-stop")
                    .mt(px(4.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_color(t.danger)
                    .hover(|s| s.bg(t.row_active))
                    .on_click(cx.listener(|s, _, _, cx| s.stop_broadcast(cx)))
                    .child("Stop broadcasting"),
            );
        }
        Some(
            deferred(
                anchored()
                    .position(picker.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Broadcast, TabId};

    fn id(n: u64) -> TabId {
        n
    }

    #[test]
    fn toggling_adds_then_removes_and_never_adds_the_source() {
        let mut b = Broadcast::new(id(1));
        b.toggle(id(2));
        b.toggle(id(3));
        assert_eq!(b.targets, [id(2), id(3)]);
        b.toggle(id(2));
        assert_eq!(b.targets, [id(3)]);
        b.toggle(id(1));
        assert_eq!(b.targets, [id(3)], "the source is not its own receiver");
    }

    #[test]
    fn source_and_receivers_take_part_and_others_do_not() {
        let mut b = Broadcast::new(id(1));
        b.toggle(id(2));
        assert!(b.involves(id(1)));
        assert!(b.involves(id(2)));
        assert!(!b.involves(id(3)));
    }

    #[test]
    fn closed_tabs_drop_out_and_the_broadcast_ends_without_a_source_or_receivers() {
        let mut b = Broadcast::new(id(1));
        b.toggle(id(2));
        b.toggle(id(3));
        assert!(b.retain_open(&[id(1), id(3)]));
        assert_eq!(b.targets, [id(3)]);
        assert!(!b.retain_open(&[id(1)]), "no receiver left");
        let mut b = Broadcast::new(id(1));
        b.toggle(id(2));
        assert!(!b.retain_open(&[id(2)]), "the source closed");
    }
}
