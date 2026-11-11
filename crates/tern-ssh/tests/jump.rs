//! ProxyJump against three in-process servers: two jump hosts and the target.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{Shell, serve, spec, start};
use tern_ssh::{Disconnect, JumpHop, SessionEvent};

fn hop(port: u16) -> JumpHop {
    JumpHop {
        host: "127.0.0.1".into(),
        port,
        user: Some("hopper".into()),
        identity_files: Vec::new(),
    }
}

/// Types into the shell and reads until the echo comes back.
async fn echoes(live: &common::Live, text: &str) -> bool {
    live.handle.write(text.as_bytes().to_vec()).unwrap();
    let mut seen = Vec::new();
    let wait = async {
        while let Ok(ev) = live.events.recv().await {
            if let SessionEvent::Data(d) = ev {
                seen.extend(d);
                if String::from_utf8_lossy(&seen).contains(text) {
                    return true;
                }
            }
        }
        false
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_hop_chain_reaches_the_target_through_each_hop_in_order() {
    let (a, b, target) = (
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
    );
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![hop(a.port), hop(b.port)];
    let live = start(s).await.unwrap();

    // Each hop opened a channel to the next machine, and only to it.
    assert_eq!(*a.opened.lock().unwrap(), [format!("127.0.0.1:{}", b.port)]);
    assert_eq!(
        *b.opened.lock().unwrap(),
        [format!("127.0.0.1:{}", target.port)]
    );
    assert!(target.opened.lock().unwrap().is_empty());
    // Every machine's host key was put to the user, in connection order.
    let want: Vec<_> = [a.port, b.port, target.port]
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect();
    assert_eq!(live.host_keys, want);
    // The shell on the far end answers, and it is the target's (the jump hosts echo too, so
    // the bytes must have crossed both).
    assert!(echoes(&live, "through-two-hops").await);
    live.handle.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_hop_key_stops_the_chain_before_the_target() {
    let (a, target) = (serve(Shell::Echo).await, serve(Shell::Echo).await);
    let dir = tempfile::tempdir().unwrap();
    // known_hosts holds another key for hop A: the connection must be refused.
    let other = serve(Shell::Echo).await;
    let mut probe = spec(other.port, &dir);
    probe.proxy_jump = Vec::new();
    let learned = start(probe).await.unwrap();
    learned.handle.close();
    let known = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
    let key = known
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(
        dir.path().join("known_hosts"),
        format!("[127.0.0.1]:{} {key}\n", a.port),
    )
    .unwrap();

    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![hop(a.port)];
    let (error, reason) = start(s).await.err().unwrap();
    assert_eq!(reason, Disconnect::HostKeyChanged);
    let error = error.unwrap();
    assert!(error.contains("host key changed"), "{error}");
    assert!(
        error.contains(&format!("jump host 127.0.0.1:{}", a.port)),
        "{error}"
    );
    assert!(a.opened.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jump_host_that_cannot_reach_the_target_says_so() {
    let a = serve(Shell::Echo).await;
    let gone = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(gone, &dir);
    s.proxy_jump = vec![hop(a.port)];
    let (error, _) = start(s).await.err().unwrap();
    let error = error.unwrap();
    assert!(
        error.contains(&format!("could not open a connection to 127.0.0.1:{gone}")),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_first_hop_is_unreachable_not_a_login_failure() {
    let gone = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let target = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![hop(gone)];
    let (error, reason) = start(s).await.err().unwrap();
    assert_eq!(reason, Disconnect::Unreachable);
    assert!(error.unwrap().contains("refused the connection"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_hop_is_refused_by_name() {
    let target = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![JumpHop {
        host: "gw:abc".into(),
        port: 0,
        ..Default::default()
    }];
    let (error, reason) = start(s).await.err().unwrap();
    assert_eq!(reason, Disconnect::Failed);
    assert!(error.unwrap().contains("gw:abc"));
    assert!(target.opened.lock().unwrap().is_empty());
}
