//! The shell's side of the Files panel: opening it, telling it what the panes are, and the tile
//! beside the terminal with the handle that sets its width. The panel sits in the window's row
//! next to the session, so the terminal narrows and reflows instead of being covered.

use gpui::{
    AnyElement, AppContext, Context, DragMoveEvent, InteractiveElement, IntoElement, MouseButton,
    MouseUpEvent, ParentElement, StatefulInteractiveElement, Styled, div, px,
};

use super::{Shell, look};
use crate::chrome::{self, DragGhost};
use crate::files_panel::{FilesPanel, PaneInfo};
use crate::settings::SIDEBAR_DEFAULT;

/// Drag payload for the panel's width handle.
pub(crate) struct FilesPanelResize;

/// Width of the grab strip on the panel's left edge, inside the panel so nothing clips it.
const HANDLE_WIDTH: f32 = 6.0;

impl Shell {
    /// ⇧⌘O. Closing drops the panel's channels and rows; opening starts from each pane's Shell
    /// folder again.
    pub(super) fn toggle_files_panel(&mut self, cx: &mut Context<Self>) {
        if self.panels.files.take().is_some() {
            return cx.notify();
        }
        let (transfers, shell, theme) = (self.transfers.clone(), cx.weak_entity(), self.theme);
        let width = self.settings.files_panel_width;
        self.panels.files = Some(cx.new(|cx| FilesPanel::new(transfers, shell, theme, width, cx)));
        cx.notify();
    }

    fn files_panel_width(&self) -> f32 {
        look::snap(self.settings.files_panel_width, self.frame.scale)
    }

    /// The panel's tile, when it is open and a terminal pane is in front. Tells the panel what
    /// the panes are as it goes, so it follows focus and each pane's folder.
    pub(super) fn files_panel_tile(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.panels.files.clone()?;
        let tab = self
            .tabs
            .active_tab()
            .filter(|t| t.is_session() && self.settings_page.is_none())?;
        let focused = tab.focused();
        let panes = self
            .tabs
            .iter()
            .flat_map(|t| t.panes())
            .filter_map(|p| {
                Some(PaneInfo {
                    id: p.id,
                    name: p.host.clone(),
                    session: p.session()?.clone(),
                })
            })
            .collect();
        let (width, t) = (self.files_panel_width(), self.theme);
        panel.update(cx, |p, cx| p.sync(panes, Some(focused), t, width, cx));
        let handle = div()
            .id("files-panel-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(px(HANDLE_WIDTH))
            .occlude()
            .cursor_col_resize()
            .on_drag(FilesPanelResize, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, e: &MouseUpEvent, _, cx| {
                    let reset = e.click_count == 2;
                    s.update_settings(
                        |st| {
                            if reset {
                                st.files_panel_width = SIDEBAR_DEFAULT;
                            }
                        },
                        cx,
                    );
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|s, _, _, _| s.save_settings()),
            );
        Some(
            div()
                .id("files-panel-tile")
                .flex_none()
                .relative()
                .h_full()
                .w(px(width))
                .overflow_hidden()
                .bg(self.panel_background())
                .border_l_1()
                .border_color(t.hairline)
                // The panel hugs the window's right edge, so its width is the distance from the
                // pointer to that edge.
                .on_drag_move(
                    cx.listener(|s, e: &DragMoveEvent<FilesPanelResize>, _, cx| {
                        let width = f32::from(e.bounds.right() - e.event.position.x);
                        s.settings.files_panel_width =
                            look::snap(chrome::dragged_width(width), s.frame.scale);
                        cx.notify();
                    }),
                )
                .child(panel)
                .child(handle)
                .into_any_element(),
        )
    }
}
