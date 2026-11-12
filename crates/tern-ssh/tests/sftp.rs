//! SFTP against an in-process server backed by a temp directory.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Shell, serve, serve_sftp, spec, start};
use tern_ssh::SftpError;

struct Env {
    remote: tempfile::TempDir,
    sftp: tern_ssh::Sftp,
    _live: common::Live,
    _keys: tempfile::TempDir,
}

async fn env() -> Env {
    let remote = tempfile::tempdir().unwrap();
    std::fs::create_dir(remote.path().join("home")).unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(remote.path().to_path_buf()).await;
    let live = start(spec(server.port, &keys)).await.unwrap();
    let sftp = live.handle.open_sftp().await.unwrap();
    Env {
        remote,
        sftp,
        _live: live,
        _keys: keys,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn home_is_where_dot_resolves() {
    let e = env().await;
    assert_eq!(e.sftp.home().await.unwrap(), "/home");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_puts_folders_first_then_sorts_by_name() {
    let e = env().await;
    let home = e.remote.path().join("home");
    std::fs::write(home.join("b.txt"), b"12345").unwrap();
    std::fs::write(home.join("A.txt"), b"").unwrap();
    std::fs::create_dir(home.join("zeta")).unwrap();
    std::fs::create_dir(home.join("alpha")).unwrap();
    let got = e.sftp.list("/home").await.unwrap();
    let names: Vec<_> = got.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["alpha", "zeta", "A.txt", "b.txt"]);
    assert_eq!(
        got.iter().map(|x| x.is_dir).collect::<Vec<_>>(),
        [true, true, false, false]
    );
    let b = &got[3];
    assert_eq!((b.size, b.path.as_str()), (5, "/home/b.txt"));
    assert!(b.modified.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_a_missing_directory_says_which() {
    let e = env().await;
    assert_eq!(
        e.sftp.list("/nope").await.unwrap_err(),
        SftpError::NotFound("/nope".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_sftp_is_reported_as_such() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let live = start(spec(server.port, &dir)).await.unwrap();
    let err = live.handle.open_sftp().await.unwrap_err();
    assert_eq!(err, SftpError::NotOffered);
    assert_eq!(err.to_string(), "the host does not offer SFTP");
    live.handle.close();
}
