//! The Files tab entity over the Mac's own files in a temporary folder: open, list, and a
//! listing that fails.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
use std::time::{Duration, Instant};

use gpui::{AppContext, Entity, TestApp, WeakEntity};

use crate::files_tab::{FilesView, State};
use crate::platform::SshRuntime;
use crate::theme::Theme;
use crate::transfers::Transfers;

/// A Files tab with the Mac in both panels, and a way to run the app until a condition holds.
pub(crate) struct Tab {
    pub app: TestApp,
    pub view: Entity<FilesView>,
}

impl Tab {
    pub fn new() -> Self {
        let mut app = TestApp::new();
        let view = app.update(|cx| {
            SshRuntime::install(cx).unwrap();
            let rt = SshRuntime::handle(cx);
            let transfers = cx.new(|cx| Transfers::new(rt, cx));
            cx.new(|cx| {
                FilesView::new(
                    Vec::new(),
                    None,
                    transfers,
                    WeakEntity::new_invalid(),
                    Theme::zeron_dark(),
                    cx,
                )
            })
        });
        Self { app, view }
    }

    pub fn update(&mut self, f: impl FnOnce(&mut FilesView, &mut gpui::Context<FilesView>)) {
        self.app.update(|cx| self.view.update(cx, f));
    }

    pub fn read<T>(&mut self, f: impl FnOnce(&FilesView) -> T) -> T {
        self.app.read(|cx| f(self.view.read(cx)))
    }

    pub fn settle(&mut self, ready: impl Fn(&FilesView) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            self.app.run_until_parked();
            if self.read(&ready) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never ready");
    }
}

#[test]
fn a_panel_opens_in_the_home_folder_and_lists_a_folder_the_person_opens() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let mut tab = Tab::new();
    tab.settle(|v| v.panels[0].state == State::Ready);
    assert!(tab.read(|v| !v.panels[0].cwd.is_empty()), "home is listed");
    tab.update(|v, cx| v.go(0, cwd.clone(), cx));
    tab.settle(|v| v.panels[0].cwd == cwd);
    let names = tab.read(|v| {
        v.panels[0]
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(names, vec!["a.txt".to_owned()]);
    assert_eq!(tab.read(|v| v.panels[0].state.clone()), State::Ready);
}

#[test]
fn a_listing_that_fails_leaves_the_folder_shown_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let missing = dir.path().join("missing").to_string_lossy().into_owned();
    let mut tab = Tab::new();
    tab.update(|v, cx| v.go(0, cwd.clone(), cx));
    tab.settle(|v| v.panels[0].cwd == cwd);
    tab.update(|v, cx| v.go(0, missing, cx));
    tab.settle(|v| !v.panels[0].pending());
    assert_eq!(tab.read(|v| v.panels[0].cwd.clone()), cwd);
    assert_eq!(tab.read(|v| v.panels[0].state.clone()), State::Ready);
    assert!(tab.read(|v| v.panels[0].notice.is_some()));
}

#[test]
fn a_panel_whose_first_listing_fails_is_a_failure_with_retry() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing").to_string_lossy().into_owned();
    let mut tab = Tab::new();
    tab.update(|v, cx| {
        v.connect(0, cx);
        v.go(0, missing, cx);
    });
    tab.settle(|v| !v.panels[0].pending());
    assert!(matches!(
        tab.read(|v| v.panels[0].state.clone()),
        State::Failed(_)
    ));
}
