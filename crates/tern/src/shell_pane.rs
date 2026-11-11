//! What a tab holds: a terminal on a connection, or the Files tab's two panels.

use gpui::{App, Entity, FocusHandle, Focusable, Subscription};

use crate::session::Session;
use crate::split::PaneId;
use crate::tab_set::Paned;

/// One pane in a tab: a terminal on a connection, or the Files tab's two panels.
pub(super) struct Pane {
    pub(super) id: PaneId,
    pub(super) content: Content,
}

pub(super) enum Content {
    Session(TermPane),
    Files(Entity<crate::files_tab::FilesView>),
}

/// A terminal: a connection.
pub(super) struct TermPane {
    pub(super) session: Entity<Session>,
    pub(super) _repaint: Subscription,
    pub(super) _bell: Subscription,
    pub(super) _notes: Subscription,
}

pub(super) type Tab = crate::tab_set::Tab<Pane>;

impl Paned for Pane {
    fn pane_id(&self) -> PaneId {
        self.id
    }

    fn is_session(&self) -> bool {
        matches!(self.content, Content::Session(_))
    }
}

impl Pane {
    /// The pane's session; a Files tab has none.
    pub(super) fn session(&self) -> Option<&Entity<Session>> {
        match &self.content {
            Content::Session(t) => Some(&t.session),
            Content::Files(_) => None,
        }
    }

    /// What the keyboard goes to when the pane is in front.
    pub(super) fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            Content::Session(t) => t.session.read(cx).view.focus_handle(cx),
            Content::Files(f) => f.read(cx).focus.clone(),
        }
    }
}

impl Tab {
    /// The focused pane's session: what the strip shows and what typing reaches. A Files tab
    /// has none.
    pub(super) fn session(&self) -> Option<&Entity<Session>> {
        self.front().session()
    }

    pub(super) fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.front().focus_handle(cx)
    }
}
