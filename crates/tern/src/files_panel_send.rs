//! Send to…: the picked rows go to another connected pane's Shell folder (else its home), or to
//! this Mac's Downloads, copied or moved. Each copy runs through the shell's transfer queue,
//! which reads from one connection and writes to the other, so nothing lands on the Mac's disk
//! on the way. A target pane's SFTP channel is opened on demand, on that pane's own connection.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    Point, SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, px,
};
use tern_ssh::{CopyJob, FileEntry, FileSource, LocalFs, SftpFiles};

use crate::files_panel::FilesPanel;
use crate::files_panel_model::{Place, downloads_in};
use crate::files_tab_copy::copy_label;
use crate::files_tab_parts::tick_box;
use crate::hover::HoverFade as _;
use crate::icons::{self, icon};
use crate::split::PaneId;

/// Where Send to… puts files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendTo {
    /// A connected pane, at its Shell folder (else home).
    Pane(PaneId),
    /// This Mac's Downloads folder.
    Mac,
}

/// The open menu: where it was asked for, and whether choosing a place moves.
#[derive(Debug, Clone, Copy)]
pub struct SendMenu {
    pub position: Point<Pixels>,
    pub moving: bool,
}

/// The menu's width: the shell's context menu's least width.
const MENU_WIDTH: f32 = 216.0;

impl FilesPanel {
    /// Right-click on row `ix`, or its Send to… button: the row is picked first unless it is
    /// already among the picked ones, as in Finder.
    pub fn open_send_menu(&mut self, ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let row = self
            .listing
            .as_ref()
            .and_then(|l| l.row(ix))
            .map(|e| e.path.clone());
        let Some(row) = row else { return };
        if !self.model.is_picked(&row) {
            self.click_row(ix, false, false, cx);
        }
        self.send_menu = Some(SendMenu {
            position,
            moving: false,
        });
        cx.notify();
    }

    pub fn close_send_menu(&mut self, cx: &mut Context<Self>) {
        if self.send_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Sends the picked rows to `to`. A move removes each one from the pane in view once its
    /// copy landed whole.
    pub fn send(&mut self, to: SendTo, moving: bool, cx: &mut Context<Self>) {
        self.send_menu = None;
        let entries = self.picked_entries();
        let (Some(from), false) = (self.source(), entries.is_empty()) else {
            return;
        };
        match to {
            SendTo::Mac => self.send_to_mac(from, entries, moving, cx),
            SendTo::Pane(pane) => self.send_to_pane(pane, from, entries, moving, cx),
        }
        cx.notify();
    }

    fn send_to_mac(
        &mut self,
        from: Arc<dyn FileSource>,
        entries: Vec<FileEntry>,
        moving: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = std::env::home_dir().map(|h| downloads_in(&h)) else {
            return self.alert("tern cannot find your home folder", cx);
        };
        let Some(dir_text) = dir.to_str().map(str::to_owned) else {
            return self.alert("The Downloads folder's name is not text", cx);
        };
        let label = copy_label(&entries, "~/Downloads");
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let made = rt
                .spawn_blocking(move || std::fs::create_dir_all::<PathBuf>(dir))
                .await;
            let _ = this.update(cx, |this, cx| match made {
                Ok(Ok(())) => {
                    let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
                    let jobs = send_jobs(&from, entries, &mac, &dir_text, moving);
                    this.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
                }
                Ok(Err(e)) => this.alert(&format!("Could not open ~/Downloads: {e}"), cx),
                Err(_) => this.alert("Could not open ~/Downloads", cx),
            });
        })
        .detach();
    }

    /// Opens the target pane's channel when the panel has none for its connection, finds its
    /// home when its shell reports no folder, then queues the copies.
    fn send_to_pane(
        &mut self,
        pane: PaneId,
        from: Arc<dyn FileSource>,
        entries: Vec<FileEntry>,
        moving: bool,
        cx: &mut Context<Self>,
    ) {
        let here = self.listing.as_ref().map(|l| l.cwd.as_str());
        let Some(place) = self
            .model
            .send_targets(here)
            .into_iter()
            .find_map(|(id, place)| (id == pane).then_some(place))
        else {
            return self.alert("That pane is no longer connected", cx);
        };
        let Some(target) = self.panes.get(&pane) else {
            return;
        };
        let label = copy_label(&entries, &target.name);
        let epoch = self.model.epoch(pane);
        let open = self
            .sources
            .get(&pane)
            .filter(|(e, _)| *e == epoch)
            .map(|(_, s)| s.clone());
        let dial = match &open {
            Some(_) => None,
            None => {
                let session = target.session.upgrade();
                let Some(dial) = session.and_then(|s| {
                    let s = s.read(cx);
                    Some((s.ssh_handle()?, s.label()))
                }) else {
                    return self.alert("That pane is no longer connected", cx);
                };
                Some(dial)
            }
        };
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let found = rt
                .spawn(async move {
                    let to: Arc<dyn FileSource> = match (open, dial) {
                        (Some(to), _) => to,
                        (None, Some((handle, label))) => {
                            let sftp = handle.open_sftp().await.map_err(|e| e.to_string())?;
                            Arc::new(SftpFiles::new(label, sftp).keeping(handle))
                        }
                        (None, None) => return Err("That pane is no longer connected".into()),
                    };
                    let dir = match place {
                        Place::Path(path) => path,
                        Place::Home => to.home().await.map_err(|e| e.to_string())?,
                    };
                    Ok::<_, String>((to, dir))
                })
                .await;
            let _ = this.update(cx, |this, cx| match found {
                Ok(Ok((to, dir))) => {
                    // Kept for the pane's own listing, unless it reconnected meanwhile.
                    if this.model.epoch(pane) == epoch {
                        this.sources.entry(pane).or_insert((epoch, to.clone()));
                    }
                    let jobs = send_jobs(&from, entries, &to, &dir, moving);
                    this.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "files_panel_send_failed");
                    this.alert(&format!("Could not reach that pane: {e}"), cx);
                }
                Err(_) => this.alert("Could not reach that pane", cx),
            });
        })
        .detach();
    }

    /// The menu: every place Send to… can reach, this Mac last, and Move under them.
    pub(crate) fn render_send_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.send_menu?;
        let t = self.theme;
        let picked = self.picked_entries();
        let what = match picked.as_slice() {
            [one] => format!("\u{201c}{}\u{201d}", one.name),
            many => format!("{} items", many.len()),
        };
        let here = self.listing.as_ref().map(|l| l.cwd.as_str());
        let targets = self.model.send_targets(here);
        let item =
            |id: (&'static str, usize), glyph: &'static str, host: String, folder: String| {
                div()
                    .id(id)
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .hover_fade(
                        format!("{}-{}", id.0, id.1),
                        gpui::transparent_black(),
                        t.row_active,
                    )
                    .child(icon(glyph).size(px(16.)).flex_none().text_color(t.muted))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(host))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(t.faint)
                                    .truncate()
                                    .child(SharedString::from(folder)),
                            ),
                    )
            };
        let moving = menu.moving;
        let mut card = div()
            .id("files-send-menu")
            .occlude()
            .w(px(MENU_WIDTH))
            .max_h(px(480.))
            .overflow_y_scroll()
            .p(px(4.))
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
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_send_menu(cx)))
            .child(
                div()
                    .px(px(8.))
                    .pt(px(4.))
                    .pb(px(2.))
                    .text_size(px(11.))
                    .text_color(t.faint)
                    .truncate()
                    .child(SharedString::from(format!(
                        "{} {what} to",
                        if moving { "Move" } else { "Send" }
                    ))),
            );
        if targets.is_empty() {
            card = card.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child("No other pane is connected"),
            );
        }
        for (ix, (pane, place)) in targets.into_iter().enumerate() {
            let host = self
                .panes
                .get(&pane)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let folder = match place {
                Place::Path(path) => path,
                Place::Home => "~".to_owned(),
            };
            card = card.child(
                item(("files-send-pane", ix), icons::SERVER, host, folder).on_click(
                    cx.listener(move |this, _, _, cx| this.send(SendTo::Pane(pane), moving, cx)),
                ),
            );
        }
        card = card
            .child(
                item(
                    ("files-send-mac", 0),
                    icons::HOME,
                    "This Mac".into(),
                    "~/Downloads".into(),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.send(SendTo::Mac, moving, cx))),
            )
            .child(div().h(px(1.)).mx(px(-4.)).my(px(2.)).bg(t.hairline))
            .child(
                div()
                    .id("files-send-move")
                    .px(px(8.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(m) = this.send_menu.as_mut() {
                            m.moving = !m.moving;
                            cx.notify();
                        }
                    }))
                    .child(tick_box(moving, 12., &t))
                    // Wraps rather than run past the menu's edge on a long host name.
                    .child(div().flex_1().min_w_0().child(SharedString::from(format!(
                        "Move (remove from {} after)",
                        self.name()
                    )))),
            );
        Some(
            deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

/// The button that opens Send to…, over the row's end while the row is hovered or picked. It
/// floats, on the row's own colour, so Size and Modified stay under their titles.
pub(crate) fn send_button(
    ix: usize,
    selected: bool,
    t: &crate::theme::Theme,
    cx: &mut Context<FilesPanel>,
) -> gpui::Stateful<gpui::Div> {
    use crate::hover::Accessible as _;
    div()
        .id(("files-panel-send", ix))
        .absolute()
        .right(px(4.))
        .top(px(3.))
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .bg(if selected { t.row_active } else { t.row_hover })
        .hover(|s| s.bg(t.border))
        .icon_button("Send to\u{2026}", t)
        .child(
            icon(icons::ARROW_UP)
                .size(px(13.))
                .text_color(t.muted)
                // Solar's arrow-up, turned to point right: the arrow-right of the same set.
                .with_transformation(gpui::Transformation::rotate(gpui::radians(
                    std::f32::consts::FRAC_PI_2,
                ))),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(
            cx.listener(move |this, e: &gpui::ClickEvent, _: &mut Window, cx| {
                cx.stop_propagation();
                this.open_send_menu(ix, e.position(), cx);
            }),
        )
}

/// The copies Send to… hands to the queue: each item from `from` into `dir` on `to`. A name
/// already there is asked about (`rule: None`), not replaced.
pub fn send_jobs(
    from: &Arc<dyn FileSource>,
    items: Vec<FileEntry>,
    to: &Arc<dyn FileSource>,
    dir: &str,
    moving: bool,
) -> Vec<CopyJob> {
    items
        .into_iter()
        .map(|item| CopyJob {
            from: from.clone(),
            item,
            to: to.clone(),
            dir: dir.to_owned(),
            rule: None,
            moving,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/srv/{name}"),
            is_dir: dir,
            is_symlink: false,
            size: 1,
            modified: None,
        }
    }

    #[test]
    fn each_item_is_read_over_the_pane_s_source_and_asks_about_clashes() {
        let host: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let items = vec![entry("a.log", false), entry("logs", true)];
        let jobs = send_jobs(&host, items, &mac, "/Users/me/Downloads", true);
        assert_eq!(jobs.len(), 2);
        for job in &jobs {
            assert!(Arc::ptr_eq(&job.from, &host), "read over the pane's source");
            assert!(Arc::ptr_eq(&job.to, &mac));
            assert_eq!(job.dir, "/Users/me/Downloads");
            assert!(job.rule.is_none(), "a taken name is asked, not replaced");
            assert!(job.moving);
        }
        assert_eq!(jobs[1].item.name, "logs");
        let copies = send_jobs(&host, vec![entry("a", false)], &mac, "/", false);
        assert!(!copies[0].moving);
    }
}
