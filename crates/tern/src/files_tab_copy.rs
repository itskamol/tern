//! The Files tab's copying: what Copy, a drag between panels and files dropped from Finder hand
//! to the shell's transfer queue, and the panels refreshing when a batch ends.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::Context;
use tern_ssh::{CopyJob, FileEntry, FileSource, LocalFs};

use crate::files_tab::{FilesView, State};
use crate::transfers::Finished;

impl FilesView {
    /// The Copy button: the picked items go to the other panel's folder.
    pub fn copy_picked(&mut self, side: usize, cx: &mut Context<Self>) {
        let entries = self.picked(side);
        self.copy(side, entries, Self::other(side), cx);
    }

    /// Copies `entries` of panel `from` into the folder panel `to` shows.
    pub fn copy(
        &mut self,
        from: usize,
        entries: Vec<FileEntry>,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        if entries.is_empty() {
            return;
        }
        let (Some(src), Some(dst)) = (
            self.panels[from].source.clone(),
            self.panels[to].source.clone(),
        ) else {
            self.panels[to].notice =
                Some("Both panels need a host or this Mac open to copy.".into());
            return cx.notify();
        };
        if self.panels[to].state != State::Ready {
            self.panels[to].notice = Some("Open a folder to copy into first.".into());
            return cx.notify();
        }
        let dir = self.panels[to].cwd.clone();
        let label = copy_label(&entries, &self.panels[to].target.label());
        let jobs = entries
            .into_iter()
            .map(|item| CopyJob {
                from: src.clone(),
                item,
                to: dst.clone(),
                dir: dir.clone(),
                rule: None,
                moving: false,
            })
            .collect();
        self.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
    }

    /// Files dropped from Finder upload into the folder panel `side` shows.
    pub fn drop_files(&mut self, side: usize, paths: &[PathBuf], cx: &mut Context<Self>) {
        let entries: Vec<FileEntry> = paths.iter().filter_map(|p| local_entry(p)).collect();
        if entries.is_empty() {
            return;
        }
        let Some(dst) = self.panels[side].source.clone() else {
            return;
        };
        if self.panels[side].state != State::Ready {
            return;
        }
        let dir = self.panels[side].cwd.clone();
        let label = copy_label(&entries, &self.panels[side].target.label());
        let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let jobs = entries
            .into_iter()
            .map(|item| CopyJob {
                from: mac.clone(),
                item,
                to: dst.clone(),
                dir: dir.clone(),
                rule: None,
                moving: false,
            })
            .collect();
        self.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
    }

    /// A batch ended: panels showing a folder it changed (where it landed, or where a moved item
    /// left) read it again.
    pub(crate) fn transfer_finished(&mut self, done: &Finished, cx: &mut Context<Self>) {
        for side in 0..2 {
            let p = &self.panels[side];
            if p.source
                .as_ref()
                .is_some_and(|s| done.touched(s.id(), &p.cwd))
            {
                self.refresh(side, cx);
            }
        }
    }
}

/// What a copy is called in the transfer list and its Activity.
pub fn copy_label(entries: &[FileEntry], to: &str) -> String {
    match entries {
        [one] => format!("{} to {to}", one.name),
        many => format!("{} items to {to}", many.len()),
    }
}

/// An item from Finder as a file source entry; `None` when it cannot be read.
pub(crate) fn local_entry(path: &std::path::Path) -> Option<FileEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let name = path.file_name()?.to_str()?.to_owned();
    Some(FileEntry {
        name,
        path: path.to_str()?.to_owned(),
        is_dir: meta.is_dir(),
        is_symlink: false,
        size: meta.len(),
        modified: meta.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: String::new(),
            is_dir: false,
            is_symlink: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn a_copy_is_named_for_one_item_or_the_count() {
        assert_eq!(copy_label(&[entry("a.txt")], "web-01"), "a.txt to web-01");
        assert_eq!(
            copy_label(&[entry("a"), entry("b"), entry("c")], "This Mac"),
            "3 items to This Mac"
        );
    }
}
