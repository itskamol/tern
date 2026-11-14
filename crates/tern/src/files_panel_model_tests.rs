//! The Files panel's per-pane state, driven as the window drives it.
use super::*;

fn folder(host: &str, path: &str) -> ShellFolder {
    ShellFolder {
        host: host.into(),
        path: path.into(),
    }
}

/// A panel with pane 1 on `web`, connected, in view.
fn one_pane() -> FilesPanelModel {
    let mut m = FilesPanelModel::default();
    m.pane(1, "web", Link::Connected, None);
    m.focus(Some(1));
    m
}

fn following(path: &str) -> Shown {
    Shown::Folder {
        place: Place::Path(path.into()),
        mode: Mode::Following,
        shell_reports: true,
        hidden: false,
    }
}

fn browsing(path: &str) -> Shown {
    Shown::Folder {
        place: Place::Path(path.into()),
        mode: Mode::Browsing,
        shell_reports: true,
        hidden: false,
    }
}

#[test]
fn a_shell_that_reports_no_folder_opens_at_home_and_says_so() {
    let m = one_pane();
    assert_eq!(
        m.shown(),
        Some(Shown::Folder {
            place: Place::Home,
            mode: Mode::AtHome,
            shell_reports: false,
            hidden: false,
        })
    );
    assert!(!m.shown().is_some_and(|s| s.can_return()));
    assert!(SNIPPET.contains(r"\e]7;file://") && !SNIPPET.contains('\n'));
}

#[test]
fn the_panel_starts_at_the_shell_folder_and_follows_it() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    assert_eq!(m.shown(), Some(following("/srv/app")));
    m.pane(
        1,
        "web",
        Link::Connected,
        Some(&folder("web", "/srv/app/logs")),
    );
    assert_eq!(m.shown(), Some(following("/srv/app/logs")));
}

#[test]
fn opening_a_folder_by_hand_stops_following_until_the_way_back_is_taken() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.open_folder("/etc");
    assert_eq!(m.shown(), Some(browsing("/etc")));
    assert!(m.shown().is_some_and(|s| s.can_return()));
    // The shell moves on; the panel stays where the person put it.
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/tmp")));
    assert_eq!(m.shown(), Some(browsing("/etc")));
    m.back_to_shell();
    assert_eq!(m.shown(), Some(following("/tmp")));
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/var")));
    assert_eq!(m.shown(), Some(following("/var")));
}

#[test]
fn opening_the_shell_folder_by_hand_is_following_it_again() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.open_folder("/etc");
    m.open_folder("/srv");
    assert_eq!(m.shown(), Some(following("/srv")));
}

#[test]
fn browsing_without_a_reporting_shell_has_no_way_back_but_still_says_so() {
    let mut m = one_pane();
    m.open_folder("/etc");
    let shown = m.shown();
    assert_eq!(
        shown,
        Some(Shown::Folder {
            place: Place::Path("/etc".into()),
            mode: Mode::Browsing,
            shell_reports: false,
            hidden: false,
        })
    );
    assert!(!shown.is_some_and(|s| s.can_return()));
    m.back_to_shell();
    assert!(matches!(
        m.shown(),
        Some(Shown::Folder {
            place: Place::Home,
            mode: Mode::AtHome,
            ..
        })
    ));
}

#[test]
fn each_pane_keeps_its_own_folder_selection_and_browsing_when_focus_moves() {
    let mut m = FilesPanelModel::default();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.pane(2, "db", Link::Connected, Some(&folder("db", "/var/lib")));
    m.focus(Some(1));
    m.open_folder("/etc");
    let rows = vec!["/etc/a".to_owned(), "/etc/b".to_owned()];
    m.click_row(&rows, 1, false, false);
    m.focus(Some(2));
    assert_eq!(m.shown(), Some(following("/var/lib")));
    assert!(m.picked().is_empty());
    m.focus(Some(1));
    assert_eq!(m.shown(), Some(browsing("/etc")));
    assert_eq!(m.picked(), ["/etc/b"]);
}

#[test]
fn a_pane_out_of_view_still_follows_its_shell() {
    let mut m = FilesPanelModel::default();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.pane(2, "db", Link::Connected, Some(&folder("db", "/var")));
    m.focus(Some(1));
    m.pane(2, "db", Link::Connected, Some(&folder("db", "/var/log")));
    m.focus(Some(2));
    assert_eq!(m.shown(), Some(following("/var/log")));
}

#[test]
fn a_pane_still_logging_in_is_waiting_and_a_dropped_one_offers_reconnect() {
    let mut m = FilesPanelModel::default();
    m.pane(1, "web", Link::Waiting, None);
    m.focus(Some(1));
    assert_eq!(m.shown(), Some(Shown::Waiting));
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.pane(1, "web", Link::Dropped, Some(&folder("web", "/srv")));
    assert_eq!(m.shown(), Some(Shown::Offline { idle: false }));
    m.pane(1, "web", Link::Idle, None);
    assert_eq!(m.shown(), Some(Shown::Offline { idle: true }));
}

#[test]
fn a_reconnect_starts_over_with_nothing_of_the_old_shell() {
    let mut m = one_pane();
    let before = m.epoch(1);
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.open_folder("/etc");
    let rows = vec!["/etc/a".to_owned()];
    m.click_row(&rows, 0, false, false);
    m.pane(1, "web", Link::Dropped, Some(&folder("web", "/srv")));
    m.pane(1, "web", Link::Waiting, None);
    assert_eq!(m.shown(), Some(Shown::Waiting));
    assert_eq!(m.epoch(1), before, "only a new connection is a new epoch");
    m.pane(1, "web", Link::Connected, None);
    assert_eq!(m.epoch(1), before + 1);
    assert_eq!(
        m.shown(),
        Some(Shown::Folder {
            place: Place::Home,
            mode: Mode::AtHome,
            shell_reports: false,
            hidden: false,
        })
    );
    assert!(m.picked().is_empty());
    assert_ne!(
        m.epoch(1),
        before,
        "the listing must reopen on the new connection"
    );
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/home/me")));
    assert_eq!(m.shown(), Some(following("/home/me")));
}

#[test]
fn a_folder_reported_by_a_nested_ssh_is_ignored() {
    let mut m = one_pane();
    // The shell names itself by its host name, not the address tern dialled.
    m.pane(
        1,
        "10.0.0.5",
        Link::Connected,
        Some(&folder("web01", "/srv")),
    );
    assert_eq!(m.shown(), Some(following("/srv")));
    m.pane(
        1,
        "10.0.0.5",
        Link::Connected,
        Some(&folder("db01", "/var/lib")),
    );
    assert_eq!(m.shown(), Some(following("/srv")));
    // Back in the outer shell.
    m.pane(
        1,
        "10.0.0.5",
        Link::Connected,
        Some(&folder("WEB01", "/home")),
    );
    assert_eq!(m.shown(), Some(following("/home")));
    // A folder with no host does not make the next nested one ours.
    m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("", "/opt")));
    m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("db01", "/x")));
    assert_eq!(m.shown(), Some(following("/opt")));
}

#[test]
fn a_folder_with_no_host_or_the_dialled_host_is_the_panes_own() {
    let mut m = one_pane();
    m.pane(
        1,
        "web.example.com",
        Link::Connected,
        Some(&folder("", "/a")),
    );
    assert_eq!(m.shown(), Some(following("/a")));
    m.pane(
        1,
        "web.example.com",
        Link::Connected,
        Some(&folder("web", "/b")),
    );
    assert_eq!(m.shown(), Some(following("/b")));
    m.pane(
        1,
        "web.example.com",
        Link::Connected,
        Some(&folder("db", "/c")),
    );
    assert_eq!(m.shown(), Some(following("/b")));
}

#[test]
fn the_dialled_name_stays_ours_even_when_the_shell_calls_itself_something_else() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web01", "/a")));
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/b")));
    assert_eq!(m.shown(), Some(following("/b")));
}

#[test]
fn a_shell_that_stops_reporting_goes_back_to_home() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    m.pane(1, "web", Link::Connected, None);
    assert!(matches!(
        m.shown(),
        Some(Shown::Folder {
            mode: Mode::AtHome,
            shell_reports: false,
            ..
        })
    ));
    m.pane(1, "web", Link::Connected, Some(&folder("other", "/z")));
    assert_eq!(m.shown(), Some(following("/z")));
}

#[test]
fn a_folder_without_a_host_does_not_use_up_the_shells_first_name() {
    let mut m = one_pane();
    m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("", "/a")));
    m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("web01", "/b")));
    assert_eq!(m.shown(), Some(following("/b")));
}

#[test]
fn two_addresses_are_the_same_host_only_when_equal() {
    assert!(!names_match("10.0.0.5", "10"));
    assert!(names_match("web", "WEB.example.com"));
    assert!(names_match("10.0.0.5", "10.0.0.5"));
    assert!(!names_match("10.0.0.5", "10.9.9.9"));
    assert!(!names_match("web", "db"));
}

#[test]
fn a_closed_pane_is_forgotten_and_cannot_be_in_view() {
    let mut m = one_pane();
    assert_eq!(m.focused(), Some(1));
    m.keep_only(&[]);
    m.focus(Some(1));
    assert_eq!(m.shown(), None);
    assert_eq!(m.focused(), None);
}

#[test]
fn picking_goes_with_the_folder_and_a_new_listing_drops_it() {
    let mut m = one_pane();
    let rows: Vec<String> = ["a", "b"].map(|n| format!("/x/{n}")).to_vec();
    m.click_row(&rows, 0, false, false);
    m.click_row(&rows, 1, true, false);
    assert_eq!(m.picked(), ["/x/a", "/x/b"]);
    m.clear_picked();
    assert!(m.picked().is_empty());
    m.click_row(&rows, 0, false, false);
    assert!(m.is_picked("/x/a"));
    m.open_folder("/y");
    assert!(m.picked().is_empty());
}

#[test]
fn showing_dotfiles_is_kept_per_pane_and_drops_what_was_picked() {
    let mut m = FilesPanelModel::default();
    m.pane(1, "web", Link::Connected, None);
    m.pane(2, "db", Link::Connected, None);
    m.focus(Some(1));
    let rows = vec!["/a".to_owned()];
    m.click_row(&rows, 0, false, false);
    m.toggle_hidden();
    assert!(m.picked().is_empty());
    assert!(matches!(
        m.shown(),
        Some(Shown::Folder { hidden: true, .. })
    ));
    m.focus(Some(2));
    assert!(matches!(
        m.shown(),
        Some(Shown::Folder { hidden: false, .. })
    ));
}

#[test]
fn the_breadcrumb_names_every_folder_up_to_the_root() {
    let labels = |p: &str| {
        crumbs(p)
            .into_iter()
            .map(|c| (c.label, c.path))
            .collect::<Vec<_>>()
    };
    assert_eq!(labels("/"), [("/".into(), "/".into())]);
    assert_eq!(
        labels("/home/me//logs/"),
        [
            ("/".into(), "/".into()),
            ("home".into(), "/home".into()),
            ("me".into(), "/home/me".into()),
            ("logs".into(), "/home/me/logs".into()),
        ]
    );
}

#[test]
fn downloads_go_to_the_downloads_folder_of_the_mac_home() {
    let home = std::path::Path::new("/Users/me");
    assert_eq!(
        downloads_in(home),
        std::path::Path::new("/Users/me/Downloads")
    );
}

/// Pane 1 in `/srv`, with `rows` listed and `picked` (indexes) picked.
fn picking(rows: &[&str], picked: &[usize]) -> (FilesPanelModel, Vec<String>) {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
    let rows: Vec<String> = rows.iter().map(|r| format!("/srv/{r}")).collect();
    for (n, ix) in picked.iter().enumerate() {
        m.click_row(&rows, *ix, n > 0, false);
    }
    (m, rows)
}

#[test]
fn rename_is_for_exactly_one_picked_row() {
    let (mut m, _) = picking(&["a", "b"], &[]);
    assert_eq!(m.start_rename(), None);
    let (mut m, _) = picking(&["a", "b"], &[0, 1]);
    assert_eq!(m.start_rename(), None, "two rows have no one name");
    assert_eq!(m.edit(), None);
    let (mut m, _) = picking(&["a", "b"], &[1]);
    assert_eq!(m.start_rename().as_deref(), Some("/srv/b"));
    assert_eq!(m.edit(), Some(&Edit::Rename("/srv/b".into())));
}

#[test]
fn delete_asks_about_what_is_picked_and_cancel_leaves_it_all_alone() {
    let (mut m, _) = picking(&["a", "b", "c"], &[]);
    assert!(!m.start_delete(), "nothing picked, nothing to ask");
    let (mut m, _) = picking(&["a", "b", "c"], &[0, 2]);
    assert!(m.start_delete());
    assert_eq!(
        m.edit(),
        Some(&Edit::Delete(vec!["/srv/a".into(), "/srv/c".into()]))
    );
    m.cancel_edit();
    assert_eq!(m.edit(), None);
    assert_eq!(m.take_edit(), None, "a cancelled delete deletes nothing");
    assert_eq!(m.picked(), ["/srv/a", "/srv/c"]);
}

#[test]
fn confirming_hands_the_edit_over_once() {
    let (mut m, _) = picking(&["a"], &[0]);
    m.start_delete();
    assert_eq!(m.take_edit(), Some(Edit::Delete(vec!["/srv/a".into()])));
    assert_eq!(m.take_edit(), None);
}

#[test]
fn a_new_folder_a_click_elsewhere_or_a_new_connection_ends_the_edit() {
    let (mut m, _) = picking(&["a", "b"], &[0]);
    m.start_rename();
    m.open_folder("/tmp");
    assert_eq!(m.edit(), None);

    let (mut m, rows) = picking(&["a", "b"], &[0]);
    m.start_delete();
    m.click_row(&rows, 1, false, false);
    assert_eq!(m.edit(), None);

    let (mut m, _) = picking(&["a", "b"], &[0]);
    m.start_rename();
    m.pane(1, "web", Link::Dropped, None);
    m.pane(1, "web", Link::Connected, None);
    assert_eq!(m.edit(), None);
}

#[test]
fn each_pane_keeps_its_own_edit_when_focus_moves() {
    let (mut m, _) = picking(&["a"], &[0]);
    m.pane(2, "db", Link::Connected, None);
    m.start_rename();
    m.focus(Some(2));
    assert_eq!(m.edit(), None);
    m.focus(Some(1));
    assert_eq!(m.edit(), Some(&Edit::Rename("/srv/a".into())));
}

#[test]
fn send_targets_are_every_connected_pane_at_its_shell_folder_else_home() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.pane(
        2,
        "db",
        Link::Connected,
        Some(&folder("db", "/var/backups")),
    );
    m.pane(3, "cache", Link::Connected, None);
    m.pane(4, "old", Link::Dropped, Some(&folder("old", "/tmp")));
    m.pane(5, "new", Link::Waiting, None);
    assert_eq!(
        m.send_targets(Some("/srv/app")),
        [(2, Place::Path("/var/backups".into())), (3, Place::Home),],
        "not the folder in view, nor a pane that is not connected"
    );
}

#[test]
fn the_pane_in_view_is_a_target_when_its_shell_is_elsewhere_and_a_place_is_listed_once() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.open_folder("/srv/app/logs");
    m.pane(2, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.pane(3, "WEB", Link::Connected, Some(&folder("web", "/srv/app")));
    m.pane(4, "db", Link::Connected, Some(&folder("db", "/srv/app")));
    assert_eq!(
        m.send_targets(Some("/srv/app/logs")),
        [
            (1, Place::Path("/srv/app".into())),
            (4, Place::Path("/srv/app".into())),
        ]
    );
}

#[test]
fn the_pane_in_view_is_no_target_for_the_folder_it_shows_even_at_home() {
    let mut m = one_pane();
    m.pane(2, "db", Link::Connected, None);
    assert_eq!(
        m.send_targets(Some("/home/test")),
        [(2, Place::Home)],
        "its home is the folder on screen"
    );
    m.open_folder("/srv");
    assert_eq!(
        m.send_targets(Some("/srv")),
        [(1, Place::Home), (2, Place::Home)],
        "browsing elsewhere, its home is somewhere else"
    );
}

#[test]
fn a_dropped_row_goes_to_the_pane_s_folder_unless_that_is_the_folder_it_came_from() {
    let mut m = one_pane();
    m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.pane(2, "db", Link::Connected, None);
    m.pane(3, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    m.pane(4, "old", Link::Dropped, Some(&folder("old", "/tmp")));
    m.pane(5, "web", Link::Connected, Some(&folder("web", "/srv/app")));
    assert_eq!(m.target_for(2, Some("/srv/app")), Some(Place::Home));
    assert_eq!(m.target_for(1, Some("/srv/app")), None, "onto itself");
    assert_eq!(
        m.target_for(3, Some("/srv/app")),
        None,
        "another pane on the same host and folder is the same place"
    );
    assert_eq!(m.target_for(4, Some("/srv/app")), None, "not connected");
    assert!(m.connected(2) && !m.connected(4) && !m.connected(9));
    assert_eq!(m.target_for(9, Some("/srv/app")), None, "no such pane");
    m.open_folder("/srv/app/logs");
    assert_eq!(
        m.target_for(1, Some("/srv/app/logs")),
        Some(Place::Path("/srv/app".into()))
    );
    assert_eq!(
        m.send_targets(Some("/srv/app/logs")),
        [(1, Place::Path("/srv/app".into())), (2, Place::Home)],
        "the menu lists a place once"
    );
}
