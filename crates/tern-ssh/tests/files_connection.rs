//! A connection made only for files, and one that outlives the terminal it was opened for.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{serve_sftp, spec, start};
use tern_ssh::{SessionEvent, connect_files};

#[tokio::test(flavor = "multi_thread")]
async fn a_files_connection_asks_for_no_pty_shell_or_agent_and_still_lists_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    std::fs::write(root.path().join("home/a.txt"), b"a").unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(root.path().to_path_buf()).await;
    let mut spec = spec(server.port, &keys);
    spec.forward_agent = true;
    let (handle, events) = connect_files(spec, &tokio::runtime::Handle::current());
    loop {
        match events.recv().await.unwrap() {
            SessionEvent::Prompt(tern_ssh::Prompt::UnknownHostKey { reply, .. }) => {
                let _ = reply.send(true);
            }
            SessionEvent::Connected => break,
            SessionEvent::Closed { error, .. } => panic!("closed: {error:?}"),
            _ => {}
        }
    }
    let sftp = handle.open_sftp().await.unwrap();
    let names: Vec<_> = sftp
        .list("/home")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a.txt"]);
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "{:?}",
        server.requests.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sftp_keeps_working_after_the_terminal_that_opened_the_connection_is_gone() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(root.path().to_path_buf()).await;
    let live = start(spec(server.port, &keys)).await.unwrap();
    let sftp = live.handle.open_sftp().await.unwrap();
    let keep = live.handle.clone();
    drop(live.events);
    // The shell echoes this, and there is nobody left to hear it.
    live.handle.write(b"x".to_vec()).unwrap();
    drop(live.handle);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(sftp.list("/home").await.is_ok(), "the connection survived");
    drop(keep);
}
