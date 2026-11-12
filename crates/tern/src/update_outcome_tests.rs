//! What a person sees for each result of an update check, and the one-at-a-time rule.
use super::*;
use crate::messages::Kind;

fn available() -> Found {
    Found {
        version: "0.2.2".into(),
        url: "https://github.com/x/tern/releases/tag/v0.2.2".into(),
    }
}

#[test]
fn each_result_has_its_message_kind_and_version_row() {
    // (outcome, message, kind, version row)
    let cases = [
        (
            Outcome::Checking,
            "Checking for updates…",
            Kind::Activity,
            "0.2.1; checking for updates…",
        ),
        (
            Outcome::UpToDate,
            "tern is up to date (0.2.1)",
            Kind::Activity,
            "0.2.1; up to date",
        ),
        (
            Outcome::Downloading("0.2.2".into()),
            "tern 0.2.2 is available; downloading it…",
            Kind::Activity,
            "0.2.1; 0.2.2 is downloading",
        ),
        (
            Outcome::Ready("0.2.2".into()),
            "tern 0.2.2 is ready; it installs when tern quits",
            Kind::Alert,
            "0.2.1; 0.2.2 is downloaded and installs when tern quits",
        ),
        (
            Outcome::NotInstallable(available()),
            "tern 0.2.2 is available",
            Kind::Alert,
            "0.2.1; 0.2.2 is available to download",
        ),
        (
            Outcome::Failed("could not reach the update feed".into()),
            "Could not check for updates: could not reach the update feed",
            Kind::Alert,
            "0.2.1; the last check failed: could not reach the update feed",
        ),
    ];
    for (outcome, message, kind, row) in cases {
        assert_eq!(outcome.message("0.2.1"), message, "{outcome:?}");
        assert_eq!(outcome.kind(), kind, "{outcome:?}");
        assert_eq!(
            version_row("0.2.1", Some(&outcome), None),
            row,
            "{outcome:?}"
        );
    }
}

#[test]
fn the_version_row_alone_is_the_version_until_a_check_has_run() {
    assert_eq!(version_row("0.2.1", None, None), "0.2.1");
}

#[test]
fn a_waiting_update_stays_on_the_row_whatever_the_last_check_said() {
    let waiting = "0.2.1; 0.2.3 is downloaded and installs when tern quits";
    assert_eq!(version_row("0.2.1", None, Some("0.2.3")), waiting);
    // A later check that found nothing new, or failed, does not hide it.
    for later in [Outcome::UpToDate, Outcome::Failed("x".into())] {
        assert_eq!(version_row("0.2.1", Some(&later), Some("0.2.3")), waiting);
    }
    // While a check runs, the row says so.
    assert_eq!(
        version_row("0.2.1", Some(&Outcome::Checking), Some("0.2.3")),
        "0.2.1; checking for updates…"
    );
}

#[test]
fn only_a_ready_update_offers_restart_and_only_a_found_one_offers_download() {
    assert!(Outcome::Ready("0.2.2".into()).restarts());
    assert!(!Outcome::UpToDate.restarts());
    assert_eq!(
        Outcome::NotInstallable(available()).download_url(),
        Some("https://github.com/x/tern/releases/tag/v0.2.2")
    );
    assert_eq!(Outcome::UpToDate.download_url(), None);
}

#[test]
fn a_request_while_a_check_runs_is_dropped() {
    let mut state = CheckState::default();
    assert!(!state.running());
    assert!(state.begin());
    assert!(state.running());
    assert!(!state.begin(), "the second request does nothing new");
    state.finish(Outcome::UpToDate);
    assert!(!state.running());
    assert_eq!(state.shown(), Some(&Outcome::UpToDate));
    assert!(state.begin(), "a finished check lets the next one start");
}

#[test]
fn a_running_check_shows_as_checking_on_the_row() {
    let mut state = CheckState::default();
    state.begin();
    assert_eq!(state.shown(), Some(&Outcome::Checking));
}

#[test]
fn the_row_follows_the_check_into_the_download() {
    let mut state = CheckState::default();
    state.begin();
    state.progress(Outcome::Downloading("0.2.2".into()));
    assert!(state.running());
    assert_eq!(state.shown(), Some(&Outcome::Downloading("0.2.2".into())));
    state.finish(Outcome::Ready("0.2.2".into()));
    assert_eq!(state.shown(), Some(&Outcome::Ready("0.2.2".into())));
}

#[test]
fn a_new_check_does_not_show_the_last_result_while_it_runs() {
    let mut state = CheckState::default();
    state.begin();
    state.finish(Outcome::Failed("x".into()));
    state.begin();
    assert_eq!(state.shown(), Some(&Outcome::Checking));
}

#[test]
fn an_error_is_put_in_words_a_person_can_act_on() {
    assert_eq!(
        reason(&Trouble::Status(503)),
        "GitHub answered 503; try again later"
    );
    assert_eq!(
        reason(&Trouble::Unreachable),
        "could not reach the update feed; are you offline?"
    );
    assert_eq!(
        reason(&Trouble::Malformed),
        "the update feed's reply was not understood"
    );
}
