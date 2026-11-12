//! Activity and Alert on the window (ADR 0002). [`crate::messages`] decides where a message
//! goes; this file raises messages, wakes the view when an Activity's time is up, and draws the
//! Activity list the titlebar button opens.

use std::time::{Instant, SystemTime};

use gpui::prelude::FluentBuilder;
use gpui::{
    Anchor, AnyElement, App, Context, InteractiveElement, IntoElement, MouseDownEvent,
    ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled, Task, Window,
    anchored, deferred, div, point, px,
};

use std::collections::VecDeque;

use gpui::{Entity, WeakEntity};

use super::Shell;
use super::toast::Toast;
use crate::clipboard::Answer;
use crate::messages::{Kind, Messages, Place, SHOWN_FOR, Work};
use crate::session::Session;
use crate::theme::{SPACE_SM, TITLEBAR_HEIGHT};

/// Whether the Activity list is open. A press outside it closes it before the titlebar button
/// hears the same press, so the button is told which press that was and does not reopen it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum ListState {
    #[default]
    Closed,
    Open,
    ClosedBy(Point<Pixels>),
    /// The button's own press closed it; the click that follows must not reopen it.
    Swallow,
}

impl ListState {
    fn pressed_outside(self, at: Point<Pixels>) -> Self {
        match self {
            Self::Open => Self::ClosedBy(at),
            other => other,
        }
    }

    fn button_down(self, at: Point<Pixels>) -> Self {
        match self {
            Self::ClosedBy(by) if by == at => Self::Swallow,
            other => other,
        }
    }

    fn button_click(self) -> Self {
        match self {
            Self::Open | Self::Swallow => Self::Closed,
            Self::Closed | Self::ClosedBy(_) => Self::Open,
        }
    }
}

/// A Decision waiting for its answer: a session holding a clipboard write.
#[derive(Debug)]
struct Decision {
    session: WeakEntity<Session>,
    who: String,
}

#[derive(Debug, Default)]
pub(super) struct ActivityUi {
    /// Questions tern cannot go on without, oldest first; the first one's card shows.
    decisions: VecDeque<Decision>,
    messages: Messages,
    list_state: ListState,
    /// The one wake-up for when the newest Activity's time is up; a newer one replaces it.
    wake: Option<Task<()>>,
}

impl Shell {
    /// An Activity: "Copied 3 files", "Synced". Quiet, in the status line or the titlebar.
    pub(crate) fn activity(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.activity.messages.raise(
            Kind::Activity,
            text,
            false,
            false,
            SystemTime::now(),
            Instant::now(),
        );
        // Only the newest Activity shows, so only its time needs a repaint: replacing the task
        // drops the older wait.
        self.activity.wake = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOWN_FOR).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.notify();
    }

    /// Work in progress shown as an Activity ("Downloading\u{2026}"), replaced in place until
    /// [`Self::end_progress_activity`]. Repaints only when what shows changed.
    pub(crate) fn progress_activity(
        &mut self,
        work: Work,
        text: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if self.activity.messages.set_progress(work, text) {
            cx.notify();
        }
    }

    pub(crate) fn end_progress_activity(&mut self, work: Work, cx: &mut Context<Self>) {
        if self.activity.messages.end_progress(work) {
            cx.notify();
        }
    }

    /// An Alert for something that did not work, with no action.
    pub(crate) fn alert_failed(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.alert(Toast::new(super::ToastKind::Critical, text), cx);
    }

    /// An Alert: a failure, or a message with one action. The snackbar, kept in the list too.
    pub(crate) fn alert(&mut self, toast: Toast, cx: &mut Context<Self>) {
        self.activity.messages.raise(
            Kind::Alert,
            toast.text(),
            toast.has_action(),
            toast.failed(),
            SystemTime::now(),
            Instant::now(),
        );
        self.toast(toast, cx);
    }

    /// The status line is drawn: the setting is on, Settings is closed and a session tab is in
    /// front.
    pub(super) fn status_line_showing(&self) -> bool {
        self.settings.show_status_line
            && self.settings_page.is_none()
            && self.tabs.active_tab().and_then(|t| t.session()).is_some()
    }

    /// The Activity on screen now and where it goes.
    fn activity_now(&self) -> Option<(Place, SharedString)> {
        let status_line = self.status_line_showing();
        self.activity
            .messages
            .showing(Instant::now(), status_line)
            .map(|(place, text)| (place, SharedString::from(text.to_owned())))
    }

    /// A session holds a clipboard write (the setting is Ask): its Decision waits in line. A
    /// second write from the same session replaces the held text, not the question.
    pub(super) fn ask_clipboard(
        &mut self,
        session: &Entity<Session>,
        chars: usize,
        cx: &mut Context<Self>,
    ) {
        let weak = session.downgrade();
        if !self.activity.decisions.iter().any(|d| d.session == weak) {
            let who = session.read(cx).label();
            self.activity.messages.raise(
                Kind::Decision,
                crate::clipboard::question(&who, chars),
                false,
                false,
                SystemTime::now(),
                Instant::now(),
            );
            self.activity
                .decisions
                .push_back(Decision { session: weak, who });
        }
        cx.notify();
    }

    /// The answer to the Decision in front.
    fn answer_decision(&mut self, answer: Answer, cx: &mut Context<Self>) {
        let Some(front) = self.activity.decisions.pop_front() else {
            return;
        };
        if let Some(session) = front.session.upgrade() {
            session.update(cx, |s, cx| s.answer_clipboard(answer, cx));
        }
        cx.notify();
    }

    /// Drops the Decisions of `alias`'s sessions: their writes went through or are gone.
    pub(super) fn forget_decisions_of(&mut self, alias: &str, cx: &App) {
        self.activity.decisions.retain(|d| {
            d.session
                .upgrade()
                .is_some_and(|s| s.read(cx).alias() != alias)
        });
    }

    /// The front Decision's card, while its session still holds the write. Card as the
    /// Activity list's (tern's port forwards popover: 340 wide, 12 radius, 13 pt), inset and
    /// gap `SPACE_SM`; buttons are the connection form's, 6 apart as in that popover's input
    /// row.
    fn decision_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        while let Some(front) = self.activity.decisions.front() {
            let waiting = front
                .session
                .upgrade()
                .and_then(|s| s.read(cx).clipboard_waiting());
            if waiting.is_some() {
                break;
            }
            self.activity.decisions.pop_front();
        }
        let front = self.activity.decisions.front()?;
        let chars = front
            .session
            .upgrade()
            .and_then(|s| s.read(cx).clipboard_waiting())?;
        let t = self.theme;
        let button = |id: &'static str, label: &'static str, primary: bool, answer: Answer| {
            super::connections_ui::button(id, label, primary, &t)
                .on_click(cx.listener(move |s, _, _, cx| s.answer_decision(answer, cx)))
        };
        Some(
            div()
                .id("decision")
                .occlude()
                .w(px(340.))
                .p(px(SPACE_SM))
                .flex()
                .flex_col()
                .gap(px(SPACE_SM))
                .rounded(px(12.))
                .border_1()
                .border_color(t.border)
                .bg(t.popup)
                .shadow_lg()
                .text_size(px(13.))
                .text_color(t.text)
                .child(SharedString::from(crate::clipboard::question(
                    &front.who, chars,
                )))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(button("decision-once", "Allow once", true, Answer::Once))
                        .child(button(
                            "decision-always",
                            "Always for this host",
                            false,
                            Answer::Always,
                        ))
                        .child(button("decision-deny", "Deny", false, Answer::Deny)),
                )
                .into_any_element(),
        )
    }

    /// The top right under the titlebar: the Decision card, then the snackbar under it, so
    /// neither covers the other.
    pub(super) fn render_top_right(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let decision = self.decision_card(cx);
        let snackbar = self.render_toast(window, cx);
        if decision.is_none() && snackbar.is_none() {
            return None;
        }
        Some(
            deferred(
                anchored()
                    .position(point(px(0.), px(TITLEBAR_HEIGHT)))
                    .child(
                        div()
                            .w(window.viewport_size().width)
                            .p(px(SPACE_SM))
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(SPACE_SM))
                            .children(decision)
                            .children(snackbar),
                    ),
            )
            .priority(3)
            .into_any_element(),
        )
    }

    /// The Activity on screen now, for the status line and for the titlebar: at most one is set.
    pub(super) fn activity_slots(&self) -> (Option<SharedString>, Option<SharedString>) {
        match self.activity_now() {
            Some((Place::StatusLine, text)) => (Some(text), None),
            Some((Place::Titlebar, text)) => (None, Some(text)),
            Some((Place::Snackbar | Place::Card, _)) | None => (None, None),
        }
    }

    pub(super) fn activity_list_open(&self) -> bool {
        self.activity.list_state == ListState::Open
    }

    /// The titlebar button's press, heard after the list's own outside-press handler.
    pub(crate) fn activity_button_down(&mut self, e: &MouseDownEvent) {
        self.activity.list_state = self.activity.list_state.button_down(e.position);
    }

    pub(crate) fn toggle_activity_list(&mut self, cx: &mut Context<Self>) {
        self.activity.list_state = self.activity.list_state.button_click();
        cx.notify();
    }

    /// Escape closes the list; returns whether it was open.
    pub(super) fn close_activity_list(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.activity_list_open();
        if open {
            self.activity.list_state = ListState::Closed;
            cx.notify();
        }
        open
    }

    /// The Activity list under the titlebar's right end, newest first. Card, rows and header are
    /// tern's port forwards popover's (340 wide, 6 inset, 12 radius, 13 pt; rows 8 × 6 with a
    /// 7 radius and 10 gap; an 11 pt muted header); its height is the Files tab's source
    /// menu's (320). The time is the status line's 11 pt.
    pub(super) fn render_activity_list(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.activity_list_open() {
            return None;
        }
        let t = self.theme;
        let now = SystemTime::now();
        let entries = self.activity.messages.list();
        let empty = entries.len() == 0;
        let rows = entries.rev().enumerate().map(|(ix, e)| {
            div()
                .id(("activity-row", ix))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(7.))
                .flex()
                .items_start()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(e.failed, |el| el.text_color(t.danger))
                        .child(SharedString::from(e.text.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(t.muted)
                        .child(SharedString::from(crate::files_text::modified(e.at, now))),
                )
        });
        let card = div()
            .id("activity-list")
            .occlude()
            .w(px(340.))
            .max_h(px(320.))
            .overflow_y_scroll()
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
            .on_mouse_down_out(cx.listener(|s, e: &MouseDownEvent, _, cx| {
                s.activity.list_state = s.activity.list_state.pressed_outside(e.position);
                cx.notify();
            }))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .child("Activity"),
            )
            .when(empty, |el| {
                el.child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_color(t.muted)
                        .child("Nothing yet"),
                )
            })
            .children(rows);
        let right = window.viewport_size().width - px(SPACE_SM);
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(point(right, px(TITLEBAR_HEIGHT)))
                    .snap_to_window_with_margin(px(SPACE_SM))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click on the titlebar button: its press, then the click.
    fn button(list: ListState, at: Point<Pixels>) -> ListState {
        list.button_down(at).button_click()
    }

    #[test]
    fn the_button_opens_the_list_and_closes_it_again() {
        let at = point(px(880.), px(20.));
        let open = button(ListState::Closed, at);
        assert_eq!(open, ListState::Open);
        // The press on the button is outside the list: the list hears it first.
        assert_eq!(button(open.pressed_outside(at), at), ListState::Closed);
    }

    #[test]
    fn a_press_elsewhere_closes_the_list_and_the_next_button_click_opens_it() {
        let closed = ListState::Open.pressed_outside(point(px(300.), px(300.)));
        assert_eq!(button(closed, point(px(880.), px(20.))), ListState::Open);
    }

    #[test]
    fn a_press_outside_a_closed_list_changes_nothing() {
        let at = point(px(300.), px(300.));
        assert_eq!(ListState::Closed.pressed_outside(at), ListState::Closed);
    }
}
