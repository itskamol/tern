//! How the view tells the remote its size: the local grid follows every frame, the remote hears
//! after a pause, or once on release while a divider is held.

use super::*;

impl TerminalView {
    /// Called from element prepaint with the frame's measured grid. Resizes
    /// the local grid immediately and debounces the remote notification.
    pub(crate) fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            self.prepaints += 1;
        }
        self.geometry = Some(geometry);
        let changed = self.terminal.update(cx, |t, _| {
            t.resize(
                geometry.cols,
                geometry.rows,
                geometry.cell_w,
                geometry.line_h,
            )
        });
        if !changed {
            return;
        }
        if self.resize_held {
            self.resize_owed = true;
            return;
        }
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                .await;
            // Read the current size: later prepaints may have resized again
            // inside the debounce window.
            let _ = this.update(cx, |view, cx| {
                if view.resize_held {
                    view.resize_owed = true;
                } else {
                    view.terminal.update(cx, |t, cx| t.notify_remote_resize(cx));
                }
            });
        }));
        // No cx.notify(): this runs during prepaint, which already paints the
        // resized grid.
    }

    /// Holds back the remote's resize while the pane is being dragged to a new size; on
    /// release it is told once, if the size changed meanwhile.
    pub fn hold_remote_resize(&mut self, held: bool, cx: &mut Context<Self>) {
        self.resize_held = held;
        if !held && std::mem::take(&mut self.resize_owed) {
            self.resize_task = None;
            self.terminal.update(cx, |t, cx| t.notify_remote_resize(cx));
        }
    }
}
