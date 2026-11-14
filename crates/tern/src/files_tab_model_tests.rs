//! One Files tab panel, driven as the window drives it: events in, what is shown and what to
//! ask next out.
#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;

fn entry(path: &str, is_dir: bool, size: u64) -> FileEntry {
    FileEntry {
        name: path.rsplit('/').next().unwrap().to_owned(),
        path: path.to_owned(),
        is_dir,
        is_symlink: false,
        size,
        modified: None,
    }
}

fn rows(paths: &[&str]) -> Vec<FileEntry> {
    paths.iter().map(|p| entry(p, false, 1)).collect()
}

fn shown(m: &PanelModel) -> Vec<&str> {
    m.entries.iter().map(|e| e.path.as_str()).collect()
}

fn list_ticket(ask: Ask) -> (Ticket, String) {
    match ask {
        Ask::List(t, path) => (t, path),
        other => panic!("not a listing: {other:?}"),
    }
}

/// A panel on a host, with files open, listing `path` (answered).
fn listing(path: &str, names: &[&str]) -> PanelModel {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let Ask::Home(t) = m.files_open() else {
        panic!("home first")
    };
    let (t, asked) = list_ticket(m.home(t, Ok(path.into())).unwrap());
    assert!(m.listed(t, &asked, Ok(rows(names))));
    m
}

#[test]
fn a_listing_that_arrives_after_another_folder_was_opened_is_ignored() {
    let mut m = listing("/a", &["/a/x"]);
    let (slow, _) = list_ticket(m.open("/b".into()));
    let (wanted, _) = list_ticket(m.open("/c".into()));
    assert!(!m.listed(slow, "/b", Ok(rows(&["/b/y"]))));
    assert_eq!((m.cwd.as_str(), shown(&m)), ("/a", vec!["/a/x"]));
    assert!(m.pending(), "still waiting for /c");
    assert!(m.listed(wanted, "/c", Ok(rows(&["/c/z"]))));
    assert_eq!((m.cwd.as_str(), shown(&m)), ("/c", vec!["/c/z"]));
}

#[test]
fn a_failed_listing_that_is_stale_changes_nothing() {
    let mut m = listing("/a", &["/a/x"]);
    let (slow, _) = list_ticket(m.open("/b".into()));
    m.open("/c".into());
    m.listed(slow, "/b", Err(Reason::Said("denied".into())));
    assert_eq!(m.notice, None);
    assert_eq!(m.state, State::Ready);
}

#[test]
fn the_old_rows_stay_while_a_new_folder_loads() {
    let mut m = listing("/a", &["/a/x"]);
    m.open("/b".into());
    assert_eq!(m.state, State::Ready);
    assert_eq!(shown(&m), vec!["/a/x"]);
}

#[test]
fn a_first_listing_shows_loading() {
    let mut m = PanelModel::new(Target::Mac);
    m.start();
    assert_eq!(m.state, State::Connecting);
    let Ask::Home(t) = m.files_open() else {
        panic!("home first")
    };
    m.home(t, Ok("/Users/me".into())).unwrap();
    assert_eq!(m.state, State::Loading);
}

#[test]
fn an_empty_folder_shown_is_loading_again_when_another_is_opened() {
    let mut m = listing("/a", &[]);
    m.open("/b".into());
    assert_eq!(m.state, State::Loading);
}

#[test]
fn a_failed_listing_with_a_folder_shown_stays_in_it_with_a_notice() {
    let mut m = listing("/a", &["/a/x"]);
    let (t, path) = list_ticket(m.open("/locked".into()));
    assert!(!m.listed(t, &path, Err(Reason::Said("permission denied".into()))));
    assert_eq!(m.state, State::Ready);
    assert_eq!((m.cwd.as_str(), shown(&m)), ("/a", vec!["/a/x"]));
    assert_eq!(m.notice.as_deref(), Some("permission denied"));
}

#[test]
fn a_failed_listing_with_no_folder_shown_is_a_failure_with_retry() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let (t, path) = list_ticket(m.open("/nope".into()));
    m.listed(t, &path, Err(Reason::Said("no such folder".into())));
    assert_eq!(
        m.state,
        State::Failed(Reason::Said("no such folder".into()))
    );
}

#[test]
fn a_listing_that_finds_the_host_gone_is_dropped_not_failed() {
    let mut m = listing("/a", &["/a/x"]);
    let (t, path) = list_ticket(m.open("/b".into()));
    m.listed(t, &path, Err(Reason::Gone));
    assert_eq!(m.state, State::Dropped(Reason::Gone));
    assert_eq!(m.notice.as_deref(), Some("The host stopped answering."));
}

#[test]
fn the_path_shown_is_the_folder_listed_not_the_one_asked_for() {
    let mut m = listing("/a", &["/a/x"]);
    let (t, path) = list_ticket(m.open("/b".into()));
    assert_eq!(m.cwd, "/a", "until the host answers");
    m.listed(t, &path, Ok(rows(&["/b/y"])));
    assert_eq!(m.cwd, "/b");
}

#[test]
fn moving_to_another_folder_clears_the_picked_rows_and_the_notice() {
    let mut m = listing("/a", &["/a/x", "/a/y"]);
    m.click_row(0, false, false);
    m.set_notice("something failed".into());
    let (t, path) = list_ticket(m.open("/b".into()));
    assert_eq!(m.notice, None);
    m.listed(t, &path, Ok(rows(&["/a/x", "/b/y"])));
    assert!(
        m.picked().is_empty(),
        "a path of the old folder is not picked here"
    );
}

#[test]
fn listing_the_same_folder_again_keeps_the_rows_still_there_picked() {
    let mut m = listing("/a", &["/a/x", "/a/y"]);
    m.click_row(0, false, false);
    m.click_row(1, true, false);
    let (t, path) = list_ticket(m.open("/a".into()));
    m.listed(t, &path, Ok(rows(&["/a/y", "/a/z"])));
    let picked: Vec<_> = m.picked().into_iter().map(|e| e.path).collect();
    assert_eq!(picked, vec!["/a/y".to_owned()]);
}

#[test]
fn the_chosen_sort_applies_to_every_new_listing() {
    let mut m = listing("/a", &["/a/b", "/a/c"]);
    m.sort_by(SortKey::Name);
    let (t, path) = list_ticket(m.open("/d".into()));
    m.listed(t, &path, Ok(rows(&["/d/a", "/d/b", "/d/c"])));
    assert_eq!(shown(&m), vec!["/d/c", "/d/b", "/d/a"]);
}

#[test]
fn a_host_that_went_away_while_connecting_shows_as_dropped_with_what_it_said() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    m.dropped("exit status 1".into());
    assert_eq!(
        m.state,
        State::Dropped(Reason::Lost("exit status 1".into()))
    );
}

#[test]
fn a_listing_after_the_host_dropped_does_not_bring_the_panel_back() {
    let mut m = listing("/a", &["/a/x"]);
    let (t, path) = list_ticket(m.open("/b".into()));
    m.dropped(String::new());
    assert!(!m.listed(t, &path, Ok(rows(&["/b/y"]))));
    assert_eq!(m.state, State::Dropped(Reason::Lost(String::new())));
}

#[test]
fn a_panel_starts_in_the_hosts_home_folder_once_its_files_are_open() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let t = m.opening();
    assert_eq!(m.state, State::Loading);
    let Some(Ask::Home(home)) = m.opened(t, Ok(())) else {
        panic!("home is asked for once files are open")
    };
    let (t, path) = list_ticket(m.home(home, Ok("/home/me".into())).unwrap());
    assert_eq!(path, "/home/me");
    m.listed(t, &path, Ok(rows(&["/home/me/a"])));
    assert_eq!(
        (m.state.clone(), m.cwd.as_str()),
        (State::Ready, "/home/me")
    );
}

#[test]
fn a_home_answer_that_arrives_late_is_ignored() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let t = m.opening();
    let Some(Ask::Home(home)) = m.opened(t, Ok(())) else {
        panic!("home is asked for")
    };
    m.open("/typed".into());
    assert_eq!(m.home(home, Ok("/home/me".into())), None);
    assert!(m.pending(), "the typed folder is still wanted");
}

#[test]
fn a_home_that_cannot_be_found_is_a_failure_with_retry() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let Ask::Home(t) = m.files_open() else {
        panic!("home first")
    };
    assert_eq!(m.home(t, Err(Reason::Said("no home".into()))), None);
    assert_eq!(m.state, State::Failed(Reason::Said("no home".into())));
    assert!(!m.pending());
}

#[test]
fn files_that_failed_to_open_are_a_failure_with_retry() {
    let mut m = PanelModel::new(Target::Host("web".into()));
    m.start();
    let t = m.opening();
    assert_eq!(m.opened(t, Err(Reason::Said("no sftp".into()))), None);
    assert_eq!(m.state, State::Failed(Reason::Said("no sftp".into())));
}

#[test]
fn an_unknown_host_is_a_failure_naming_it() {
    let mut m = PanelModel::new(Target::Host("nope".into()));
    m.start();
    m.unknown_host("nope");
    assert_eq!(m.state, State::Failed(Reason::Unknown("nope".into())));
}

#[test]
fn switching_to_another_host_drops_everything_pending_for_the_old_one() {
    let mut m = listing("/a", &["/a/x"]);
    m.click_row(0, false, false);
    m.set_notice("old".into());
    let (slow, path) = list_ticket(m.open("/b".into()));
    let old_open = m.opening();
    m.switch_to(Target::Host("db".into()));
    assert_eq!(m.state, State::Connecting);
    assert_eq!(m.target, Target::Host("db".into()));
    assert!((m.cwd.is_empty() && m.entries.is_empty() && m.notice.is_none()));
    assert!(m.picked().is_empty() && !m.pending());
    assert!(!m.listed(slow, &path, Ok(rows(&["/b/y"]))));
    assert_eq!(m.opened(old_open, Ok(())), None);
    assert_eq!(m.state, State::Connecting);
}

#[test]
fn a_failure_on_one_side_leaves_the_other_side_alone() {
    let mut left = listing("/a", &["/a/x"]);
    let mut right = listing("/r", &["/r/y"]);
    let (t, path) = list_ticket(right.open("/gone".into()));
    right.listed(t, &path, Err(Reason::Gone));
    assert_eq!(right.state, State::Dropped(Reason::Gone));
    assert_eq!(
        (left.state.clone(), shown(&left)),
        (State::Ready, vec!["/a/x"])
    );
    assert_eq!(left.notice, None);
    left.start();
    assert_eq!(right.state, State::Dropped(Reason::Gone));
}

#[test]
fn a_reason_is_worded_in_the_hosts_terms() {
    assert_eq!(Reason::Gone.text(), "The host stopped answering.");
    assert_eq!(Reason::from(&FileError::Closed).text(), Reason::Gone.text());
    assert_eq!(
        Reason::Unknown("web".into()).text(),
        "web: not a host tern knows"
    );
    assert_eq!(
        Reason::Lost("exit status 1".into()).text(),
        "The host went away: exit status 1"
    );
    assert_eq!(Reason::Lost(String::new()).text(), "The host went away.");
}
