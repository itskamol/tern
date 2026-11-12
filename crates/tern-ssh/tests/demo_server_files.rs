//! The Files panel's path against the demo server the by-eye checks use (`password_server
//! --demo --sftp`): connect with no shell, answer the host key and the password, open SFTP, list.
//! The demo shell used to answer every channel's bytes as typing, so the SFTP channel got a
//! prompt back instead of a protocol reply and opening it failed with "sender dropped".
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use tern_ssh::{ConnectSpec, Prompt, SessionEvent, connect_files};

/// Stops with the test: the child is killed when this drops.
struct Server(#[allow(dead_code)] tokio::process::Child);

async fn demo_server(root: &std::path::Path, port: u16) -> Server {
    let exe = std::env::current_exe().unwrap();
    let examples = exe.parent().unwrap().parent().unwrap().join("examples");
    let child = tokio::process::Command::new(examples.join("password_server"))
        .args(["--demo", "--sftp"])
        .arg(root)
        .arg(port.to_string())
        .arg("hunter2")
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the password_server example is built for the tests");
    let server = Server(child);
    let until = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < until, "the demo server never listened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_files_connection_to_the_demo_server_opens_sftp_and_lists_the_home_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(root.join("home/notes.txt"), b"n").unwrap();
    let port = free_port();
    let _server = demo_server(&root, port).await;
    let spec = ConnectSpec {
        host: "127.0.0.1".into(),
        port,
        user: "demo".into(),
        known_hosts: Some(dir.path().join("known_hosts")),
        ..Default::default()
    };
    let (handle, events) = connect_files(spec, &tokio::runtime::Handle::current());
    loop {
        match events.recv().await.unwrap() {
            SessionEvent::Prompt(Prompt::UnknownHostKey { reply, .. }) => {
                let _ = reply.send(true);
            }
            SessionEvent::Prompt(Prompt::Password { reply, .. }) => {
                let _ = reply.send(Some("hunter2".to_owned().into()));
            }
            SessionEvent::Connected => break,
            SessionEvent::Closed { error, .. } => panic!("closed: {error:?}"),
            _ => {}
        }
    }
    let sftp = handle.open_sftp().await.unwrap();
    let home = sftp.home().await.unwrap();
    let names: Vec<_> = sftp
        .list(&home)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["notes.txt"]);
}
