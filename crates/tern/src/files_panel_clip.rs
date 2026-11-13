//! Rows carried somewhere else: dragged onto a pane or a tab (⌥ moves), or put on tern's
//! clipboard with ⌘C and pasted with ⌘V into the folder another pane's panel shows. Both end in
//! [`FilesPanel::send_entries`], as Send to… does. tern's clipboard is its own, not the Mac's: it
//! holds the entries and the connection they are read over, and forgets them when that pane
//! closes or reconnects.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use tern_ssh::{FileEntry, FileSource};

use crate::files_panel::FilesPanel;
use crate::files_panel_send::{SendTo, already_in};
use crate::files_tab_parts::{action, drag_ghost};
use crate::hover::Accessible as _;
use crate::icons::{self, icon};
use crate::split::PaneId;

/// Entries of one pane, with the channel to read them over.
#[derive(Debug)]
pub struct Carried {
    pub pane: PaneId,
    /// The pane's connection when they were taken: a new one ends the clipboard.
    pub epoch: u64,
    pub source: Arc<dyn FileSource>,
    pub entries: Vec<FileEntry>,
    /// The pane's host as the panel names it, and the folder they are in, for the banner.
    pub host: String,
    pub dir: String,
}

/// A drag of panel rows: the row it started on, by path, so a listing that changes under the
/// drag cannot point it at another row. The rows are read from the panel at the drop.
#[derive(Debug, Clone)]
pub struct DraggedRows {
    pub pane: PaneId,
    pub path: String,
    /// What the ghost says: the name, or how many.
    pub label: String,
}

/// The ghost that follows the pointer: the Files tab's, saying whether a drop copies or moves.
impl Render for DraggedRows {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let verb = if window.modifiers().alt {
            "Move"
        } else {
            "Copy"
        };
        drag_ghost(format!("{verb} {}", self.label))
    }
}

impl FilesPanel {
    /// What a drag starting on row `ix` carries: the picked rows when it is one of them (as in
    /// Finder), else the row alone.
    pub(crate) fn dragged_rows(&self, ix: usize) -> Option<DraggedRows> {
        let pane = self.model.focused()?;
        let row = self.listing.as_ref()?.row(ix)?;
        let picked = self.model.picked();
        let label = if self.model.is_picked(&row.path) && picked.len() > 1 {
            format!("{} items", picked.len())
        } else {
            row.name.clone()
        };
        Some(DraggedRows {
            pane,
            path: row.path.clone(),
            label,
        })
    }

    /// The entries a drag carries, read at the drop.
    fn carried(&self, rows: &DraggedRows) -> Option<Carried> {
        if self.model.focused() != Some(rows.pane) {
            return None;
        }
        let entries = if self.model.is_picked(&rows.path) {
            self.picked_entries()
        } else {
            let listing = self.listing.as_ref()?;
            let one = listing.entries.iter().find(|e| e.path == rows.path)?;
            vec![one.clone()]
        };
        self.carry(entries)
    }

    /// `entries` of the pane in view, with its channel and where they are.
    fn carry(&self, entries: Vec<FileEntry>) -> Option<Carried> {
        let pane = self.model.focused()?;
        let dir = self.folder_shown()?;
        if entries.is_empty() {
            return None;
        }
        Some(Carried {
            pane,
            epoch: self.model.epoch(pane),
            source: self.source()?,
            entries,
            host: self.name(),
            dir,
        })
    }

    /// Rows dropped on pane `target` (or on its tab): into its Shell folder, else its home.
    pub fn drop_rows(
        &mut self,
        rows: &DraggedRows,
        target: PaneId,
        moving: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.carried(rows) else { return };
        self.send_entries(c.source, c.entries, SendTo::Pane(target), moving, cx);
        cx.notify();
    }

    /// ⌘C: the picked rows go on tern's clipboard, to paste in another pane's panel.
    pub fn copy_rows(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.carry(self.picked_entries()) else {
            return;
        };
        let what = what(&c.entries);
        self.clip = Some(Arc::new(c));
        self.activity(
            format!("Copied {what}: \u{2318}V in another pane's Files panel pastes it"),
            cx,
        );
        cx.notify();
    }

    /// ⌘V: what is on tern's clipboard is copied into the folder shown, if its pane is still
    /// on the connection it was taken over. A paste empties the clipboard, as pasting a cut
    /// does: left there, the banner would only offer the same paste again. Where the rows
    /// already are nothing is pasted, so they stay held.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        self.keep_clip();
        let Some(c) = self.clip.clone() else { return };
        let (Some(to), Some(dir)) = (self.source(), self.folder_shown()) else {
            return;
        };
        if already_in(c.source.id(), &c.entries, to.id(), &dir) {
            return;
        }
        self.clip = None;
        self.send_entries(c.source.clone(), c.entries.clone(), SendTo::Here, false, cx);
        cx.notify();
    }

    /// The banner's Forget: tern's clipboard is emptied.
    pub fn forget_clip(&mut self, cx: &mut Context<Self>) {
        if self.clip.take().is_some() {
            cx.notify();
        }
    }

    /// The clipboard outlives neither its pane nor that pane's connection.
    pub(crate) fn keep_clip(&mut self) {
        let alive = self
            .clip
            .as_ref()
            .is_some_and(|c| self.model.connected(c.pane) && self.model.epoch(c.pane) == c.epoch);
        if !alive {
            self.clip = None;
        }
    }

    /// What is on tern's clipboard and where ⌘V would put it. In the folder it came from it
    /// only says what is held: there is nowhere new to paste.
    pub(crate) fn render_clip_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = self.clip.as_ref()?;
        let to = self.source()?;
        let dir = self.folder_shown()?;
        let pasteable = !already_in(c.source.id(), &c.entries, to.id(), &dir);
        let t = self.theme;
        let route = if pasteable {
            format!("{}:{} \u{2192} {}:{dir}", c.host, c.dir, self.name())
        } else {
            format!("from {}:{}, paste it in another folder", c.host, c.dir)
        };
        Some(
            div()
                .id("files-panel-clip")
                .flex_none()
                .mx(px(8.))
                .my(px(4.))
                .px(px(8.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(7.))
                .bg(t.accent.opacity(0.08))
                .child(
                    icon(icons::COPY)
                        .size(px(14.))
                        .flex_none()
                        .text_color(t.muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .child(div().truncate().child(SharedString::from(what(&c.entries))))
                        .child(
                            div()
                                .id("files-panel-clip-route")
                                .truncate()
                                .text_size(px(11.))
                                .text_color(t.faint)
                                .hint(route.clone(), &t)
                                .child(SharedString::from(route)),
                        ),
                )
                .when(pasteable, |el| {
                    el.child(
                        action("files-panel-paste", "Paste \u{2318}V", true, &t)
                            .on_click(cx.listener(|this, _, _, cx| this.paste(cx))),
                    )
                })
                .child(
                    div()
                        .id("files-panel-clip-forget")
                        .size(px(20.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.bg(t.row_hover))
                        .icon_button("Forget", &t)
                        .child(icon(icons::CLOSE).size(px(12.)).text_color(t.muted))
                        .on_click(cx.listener(|this, _, _, cx| this.forget_clip(cx))),
                )
                .into_any_element(),
        )
    }
}

/// `"notes.txt"` for one entry, `"3 items"` for more.
fn what(entries: &[FileEntry]) -> String {
    match entries {
        [one] => format!("\u{201c}{}\u{201d}", one.name),
        many => format!("{} items", many.len()),
    }
}
