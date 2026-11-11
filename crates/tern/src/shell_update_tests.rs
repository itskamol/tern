//! Asking for a check the way a person does, against a local test feed: what comes back, and
//! that the attempt counts as the day's check.
use super::*;

/// Serves `body` with `status` to the next request, once; the `Feed` that points at it.
fn serve_once(status: u16, body: &'static str) -> Feed {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    Feed::custom(&url)
}

const RELEASE_021: &str =
    r#"{"tag_name":"v0.2.1","html_url":"https://github.com/x/tern/releases/tag/v0.2.1"}"#;
const RELEASE_022: &str =
    r#"{"tag_name":"v0.2.2","html_url":"https://github.com/x/tern/releases/tag/v0.2.2"}"#;

#[test]
fn a_feed_with_nothing_newer_finds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let feed = serve_once(200, RELEASE_021);
    assert_eq!(run_check(&feed, Some(dir.path()), 100, "0.2.1"), Ok(None));
}

#[test]
fn a_feed_with_a_newer_release_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let feed = serve_once(200, RELEASE_022);
    let found = run_check(&feed, Some(dir.path()), 100, "0.2.1")
        .unwrap()
        .unwrap();
    assert_eq!(found.version, "0.2.2");
    assert_eq!(found.url, "https://github.com/x/tern/releases/tag/v0.2.2");
}

#[test]
fn a_feed_that_is_not_there_fails_as_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    // A port nothing listens on: bind, note the port, drop.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let feed = Feed::custom(&format!("http://127.0.0.1:{port}"));
    assert_eq!(
        run_check(&feed, Some(dir.path()), 100, "0.2.1"),
        Err(Trouble::Unreachable)
    );
}

#[test]
fn a_feed_that_answers_badly_fails_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_check(&serve_once(503, "{}"), Some(dir.path()), 100, "0.2.1"),
        Err(Trouble::Status(503))
    );
    assert_eq!(
        run_check(&serve_once(200, "<html>"), Some(dir.path()), 100, "0.2.1"),
        Err(Trouble::Malformed)
    );
}

#[test]
fn a_check_counts_as_the_day_s_check_whatever_it_finds() {
    let dir = tempfile::tempdir().unwrap();
    let found = run_check(
        &serve_once(200, RELEASE_022),
        Some(dir.path()),
        5_000,
        "0.2.1",
    );
    assert!(found.is_ok());
    assert_eq!(last_check(dir.path()), Some(5_000));
    // Also when it fails: being offline must not retry on every launch.
    let failed = run_check(&serve_once(503, "{}"), Some(dir.path()), 9_000, "0.2.1");
    assert!(failed.is_err());
    assert_eq!(last_check(dir.path()), Some(9_000));
    // So the daily timer does not fire an hour later.
    assert!(!due(last_check(dir.path()), 9_000 + 3_600));
}
