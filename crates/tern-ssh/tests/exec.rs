//! Isolated non-PTY SSH `exec` integration tests against in-process loopback `russh` servers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    AgentProbe, ExecCtx, Shell, serve, serve_exec, serve_exec_with, spec, start, wait_active,
    write_known_host,
};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Response, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, Sig};
use tern_ssh::{
    DEFAULT_EXEC_TIMEOUT, EXEC_OUTPUT_CAP, ExecError, ExecOptions, Forward, JumpHop,
    MAX_EXEC_TIMEOUT, MIN_EXEC_TIMEOUT, MemoryKey, Prompt, SecretString, SessionEvent, exec,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;

fn rt() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

async fn wait_notify(notify: &Notify, label: &str) {
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

fn known_spec(server: &common::Server, dir: &tempfile::TempDir) -> tern_ssh::ConnectSpec {
    let s = spec(server.port, dir);
    write_known_host(
        s.known_hosts.as_deref().unwrap(),
        server.port,
        &server.host_key,
    );
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_captures_stdout_stderr_exit_status_sends_stdin_eof_and_disconnects() {
    let server = serve_exec(|ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        // Verify the client closes stdin immediately after starting the command.
        let eof = tokio::time::timeout(Duration::from_secs(3), ctx.stdin_eof)
            .await
            .expect("client must send immediate stdin EOF");
        assert!(eof.is_ok(), "stdin EOF channel closed without EOF");

        let code = if ctx.command == "fail" {
            let _ = ctx
                .handle
                .data(ctx.channel, format!("cmd={}\n", ctx.command).into_bytes())
                .await;
            let _ = ctx
                .handle
                .extended_data(ctx.channel, 1, b"err-body\n".to_vec())
                .await;
            7
        } else {
            let _ = ctx
                .handle
                .data(ctx.channel, format!("ok:{}\n", ctx.command).into_bytes())
                .await;
            let _ = ctx
                .handle
                .extended_data(ctx.channel, 1, b"warn\n".to_vec())
                .await;
            0
        };
        let _ = ctx.handle.eof(ctx.channel).await;
        let _ = ctx.handle.exit_status_request(ctx.channel, code).await;
        let _ = ctx.handle.close(ctx.channel).await;
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir);

    let ok = exec(
        s.clone(),
        "status api.service",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap();
    assert_eq!(ok.exit_status, Some(0));
    assert_eq!(ok.exit_signal, None);
    assert_eq!(ok.stdout, b"ok:status api.service\n");
    assert_eq!(ok.stderr, b"warn\n");
    assert!(!ok.stdout_truncated);
    assert!(!ok.stderr_truncated);
    wait_active(&server.active, 0).await;

    let fail = exec(s, "fail", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(fail.exit_status, Some(7));
    assert_eq!(fail.stdout, b"cmd=fail\n");
    assert_eq!(fail.stderr, b"err-body\n");
    assert!(!fail.stdout_truncated);
    assert!(!fail.stderr_truncated);
    wait_active(&server.active, 0).await;

    // Never requests pty, shell, or agent forwarding — strictly exec only.
    assert_eq!(*server.requests.lock().unwrap(), ["exec", "exec"]);
}

fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 251) as u8))
        .collect()
}

async fn send_chunked(
    handle: &russh::server::Handle,
    ch: russh::ChannelId,
    ext: Option<u32>,
    data: &[u8],
) {
    // Split into 16 KiB SSH packets to exercise multi-chunk boundary accumulation.
    for chunk in data.chunks(16 * 1024) {
        match ext {
            None => {
                let _ = handle.data(ch, chunk.to_vec()).await;
            }
            Some(code) => {
                let _ = handle.extended_data(ch, code, chunk.to_vec()).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn output_caps_enforce_exact_64kib_boundary_independently_on_stdout_and_stderr() {
    assert_eq!(EXEC_OUTPUT_CAP, 65_536);

    let server = serve_exec(|ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        let _ = ctx.stdin_eof.await;
        match ctx.command.as_str() {
            "exact" => {
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    None,
                    &patterned_bytes(EXEC_OUTPUT_CAP, 1),
                )
                .await;
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    Some(1),
                    &patterned_bytes(EXEC_OUTPUT_CAP, 2),
                )
                .await;
                // Non-stderr extended data (ext != 1) must be ignored.
                let _ = ctx
                    .handle
                    .extended_data(ctx.channel, 2, b"ignored".to_vec())
                    .await;
            }
            "stdout_over_by_one" => {
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    None,
                    &patterned_bytes(EXEC_OUTPUT_CAP + 1, 3),
                )
                .await;
                let _ = ctx
                    .handle
                    .extended_data(ctx.channel, 1, b"small-err".to_vec())
                    .await;
            }
            "stderr_over_by_one" => {
                let _ = ctx.handle.data(ctx.channel, b"small-out".to_vec()).await;
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    Some(1),
                    &patterned_bytes(EXEC_OUTPUT_CAP + 1, 4),
                )
                .await;
            }
            "flood" => {
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    None,
                    &patterned_bytes(512 * 1024, 5),
                )
                .await;
                send_chunked(
                    &ctx.handle,
                    ctx.channel,
                    Some(1),
                    &patterned_bytes(512 * 1024, 6),
                )
                .await;
            }
            _ => {}
        }
        let _ = ctx.handle.eof(ctx.channel).await;
        let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
        let _ = ctx.handle.close(ctx.channel).await;
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir);

    // Exactly 64 KiB on both streams: neither is truncated, all bytes preserved.
    let exact = exec(s.clone(), "exact", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(exact.stdout, patterned_bytes(EXEC_OUTPUT_CAP, 1));
    assert_eq!(exact.stderr, patterned_bytes(EXEC_OUTPUT_CAP, 2));
    assert!(!exact.stdout_truncated);
    assert!(!exact.stderr_truncated);

    // 64 KiB + 1 on stdout only: stdout truncated at 64 KiB prefix, stderr unaffected.
    let out_over = exec(
        s.clone(),
        "stdout_over_by_one",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap();
    assert_eq!(out_over.stdout.len(), EXEC_OUTPUT_CAP);
    assert_eq!(out_over.stdout, patterned_bytes(EXEC_OUTPUT_CAP, 3));
    assert!(out_over.stdout_truncated);
    assert_eq!(out_over.stderr, b"small-err");
    assert!(!out_over.stderr_truncated);

    // 64 KiB + 1 on stderr only: stderr truncated at 64 KiB prefix, stdout unaffected.
    let err_over = exec(
        s.clone(),
        "stderr_over_by_one",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap();
    assert_eq!(err_over.stdout, b"small-out");
    assert!(!err_over.stdout_truncated);
    assert_eq!(err_over.stderr.len(), EXEC_OUTPUT_CAP);
    assert_eq!(err_over.stderr, patterned_bytes(EXEC_OUTPUT_CAP, 4));
    assert!(err_over.stderr_truncated);

    // 512 KiB flood on both streams: both capped at 64 KiB, excess discarded cleanly.
    let flood = exec(s, "flood", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(flood.stdout.len(), EXEC_OUTPUT_CAP);
    assert_eq!(flood.stdout, patterned_bytes(EXEC_OUTPUT_CAP, 5));
    assert!(flood.stdout_truncated);
    assert_eq!(flood.stderr.len(), EXEC_OUTPUT_CAP);
    assert_eq!(flood.stderr, patterned_bytes(EXEC_OUTPUT_CAP, 6));
    assert!(flood.stderr_truncated);

    wait_active(&server.active, 0).await;
}

#[test]
fn timeout_options_default_to_15s_and_clamp_to_1_to_60_seconds() {
    assert_eq!(DEFAULT_EXEC_TIMEOUT, Duration::from_secs(15));
    assert_eq!(MIN_EXEC_TIMEOUT, Duration::from_secs(1));
    assert_eq!(MAX_EXEC_TIMEOUT, Duration::from_secs(60));
    assert_eq!(
        ExecOptions::default().effective_timeout(),
        Duration::from_secs(15)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::ZERO).effective_timeout(),
        Duration::from_secs(1)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_millis(500)).effective_timeout(),
        Duration::from_secs(1)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_secs(1)).effective_timeout(),
        Duration::from_secs(1)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_secs(25)).effective_timeout(),
        Duration::from_secs(25)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_secs(60)).effective_timeout(),
        Duration::from_secs(60)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_secs(61)).effective_timeout(),
        Duration::from_secs(60)
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::MAX).effective_timeout(),
        Duration::from_secs(60)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_aborts_hanging_command_and_tears_down_connection() {
    let server = serve_exec(|ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        let _ = ctx.stdin_eof.await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir);

    // Duration::ZERO must clamp to 1 second before aborting the command.
    let started = Instant::now();
    let err = exec(
        s,
        "sleep 30",
        ExecOptions::with_timeout(Duration::ZERO),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap_err();
    let elapsed = started.elapsed();

    assert_eq!(
        err,
        ExecError::Timeout {
            timeout: Duration::from_secs(1),
        }
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "0s timeout must clamp to 1s, finished too fast in {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "1s timeout took too long: {elapsed:?}"
    );
    wait_active(&server.active, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_aborts_stalled_connection_setup() {
    // Accepts TCP connections and stays silent forever so the SSH handshake never completes.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let blackhole = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(port, &dir);
    s.server_alive_interval = Duration::ZERO;

    let started = Instant::now();
    let err = exec(
        s,
        "true",
        ExecOptions::with_timeout(Duration::from_secs(1)),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    blackhole.abort();

    assert_eq!(
        err,
        ExecError::Timeout {
            timeout: Duration::from_secs(1),
        }
    );
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(5),
        "setup timeout took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_cancellation_via_cancel_handle_and_drop_tears_down_connection() {
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let server = serve_exec(move |ctx: ExecCtx| {
        let notify = notify.clone();
        async move {
            let _ = ctx.handle.channel_success(ctx.channel).await;
            let _ = ctx.stdin_eof.await;
            notify.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir);

    // Explicit cancellation via ExecCancel returns ExecError::Cancelled and closes the session.
    let run = exec(
        s.clone(),
        "sleep 30",
        ExecOptions::with_timeout(Duration::from_secs(30)),
        &rt(),
    );
    let cancel = run.cancel_handle();
    let task = tokio::spawn(run.run_headless());
    wait_notify(&started, "cancel_handle exec start").await;
    wait_active(&server.active, 1).await;
    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancelled exec did not return")
        .unwrap()
        .unwrap_err();
    assert_eq!(err, ExecError::Cancelled);
    wait_active(&server.active, 0).await;

    // Dropping Exec cancels and tears down even when a cloned ExecCancel handle is still held.
    let run = exec(
        s,
        "sleep 30",
        ExecOptions::with_timeout(Duration::from_secs(30)),
        &rt(),
    );
    let _kept_cancel = run.cancel_handle();
    wait_notify(&started, "drop exec start").await;
    wait_active(&server.active, 1).await;
    drop(run);
    wait_active(&server.active, 0).await;
}

#[cfg(unix)]
fn fake_agent(path: &std::path::Path) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_enforces_forward_agent_false_and_zero_port_forwards_even_when_spec_asks() {
    // Allocate a free local port that must NOT be bound by exec even though ConnectSpec lists it.
    let unused_local_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let forwards = vec![
        Forward::Local {
            bind_host: "127.0.0.1".into(),
            bind_port: unused_local_port,
            dest_host: "127.0.0.1".into(),
            dest_port: 9,
        },
        Forward::Remote {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            dest_host: "127.0.0.1".into(),
            dest_port: 9,
        },
        Forward::Dynamic {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
        },
    ];

    // 1. Polite server (AgentProbe::WhenAsked): exec never asks for agent forwarding or forwards.
    let polite = serve_exec_with(AgentProbe::WhenAsked, |ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        let _ = ctx.stdin_eof.await;
        let _ = ctx.handle.data(ctx.channel, b"isolated".to_vec()).await;
        let _ = ctx.handle.eof(ctx.channel).await;
        let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
        let _ = ctx.handle.close(ctx.channel).await;
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let mut s = known_spec(&polite, &dir);
    s.forward_agent = true;
    s.forwards = forwards.clone();
    #[cfg(unix)]
    {
        let sock = dir.path().join("agent.sock");
        fake_agent(&sock);
        s.agent_socket = Some(sock);
    }

    let out = exec(s, "check", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"isolated");
    assert_eq!(*polite.requests.lock().unwrap(), ["exec"]);
    assert!(polite.agent_log.lock().unwrap().is_empty());
    assert!(polite.opened.lock().unwrap().is_empty());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", unused_local_port))
            .await
            .is_err(),
        "local forward from ConnectSpec must never be started by exec"
    );

    // 2. Hostile server (AgentProbe::Always): server-initiated agent channel is refused.
    let probe_done = Arc::new(Notify::new());
    let done = probe_done.clone();
    let hostile = serve_exec_with(AgentProbe::Always, move |ctx: ExecCtx| {
        let done = done.clone();
        async move {
            let _ = ctx.handle.channel_success(ctx.channel).await;
            let _ = ctx.stdin_eof.await;
            // Give the hostile agent probe task time to receive the refusal before closing.
            done.notified().await;
            let _ = ctx.handle.eof(ctx.channel).await;
            let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
            let _ = ctx.handle.close(ctx.channel).await;
        }
    })
    .await;

    let dir2 = tempfile::tempdir().unwrap();
    let mut s2 = known_spec(&hostile, &dir2);
    s2.forward_agent = true;
    s2.forwards = forwards;
    #[cfg(unix)]
    {
        let sock = dir2.path().join("hostile-agent.sock");
        fake_agent(&sock);
        s2.agent_socket = Some(sock);
    }

    let agent_log = hostile.agent_log.clone();
    let task = tokio::spawn(exec(s2, "check", ExecOptions::default(), &rt()).run_headless());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while agent_log.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "hostile agent probe never settled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(*agent_log.lock().unwrap(), ["refused"]);
    probe_done.notify_one();

    let out = task.await.unwrap().unwrap();
    assert_eq!(out.exit_status, Some(0));
    assert_eq!(*hostile.requests.lock().unwrap(), ["exec"]);
    wait_active(&hostile.active, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_dials_its_own_connection_without_touching_an_open_terminal_session() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (e, r) = (entered.clone(), release.clone());
    let server = serve_exec(move |ctx: ExecCtx| {
        let (e, r) = (e.clone(), r.clone());
        async move {
            let _ = ctx.handle.channel_success(ctx.channel).await;
            let _ = ctx.stdin_eof.await;
            e.notify_one();
            r.notified().await;
            let _ = ctx.handle.data(ctx.channel, b"isolated-ok".to_vec()).await;
            let _ = ctx.handle.eof(ctx.channel).await;
            let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
            let _ = ctx.handle.close(ctx.channel).await;
        }
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir);

    // Open an interactive terminal Session first.
    let live = start(s.clone()).await.unwrap();
    wait_active(&server.active, 1).await;

    // Run isolated exec concurrently: must open a second independent SSH connection.
    let task = tokio::spawn(exec(s, "probe", ExecOptions::default(), &rt()).run_headless());
    wait_notify(&entered, "concurrent exec start").await;
    wait_active(&server.active, 2).await;
    release.notify_one();
    let out = task.await.unwrap().unwrap();
    assert_eq!(out.stdout, b"isolated-ok");
    wait_active(&server.active, 1).await;

    // Interactive Session is completely untouched and still echoes.
    live.handle.write(b"still-alive".to_vec()).unwrap();
    let mut seen = Vec::new();
    let echoed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(ev) = live.events.recv().await {
            if let SessionEvent::Data(d) = ev {
                seen.extend(d);
                if String::from_utf8_lossy(&seen).contains("still-alive") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(echoed);

    live.handle.close();
    let _ = live.closed().await;
    wait_active(&server.active, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_key_verification_rejects_unknown_headlessly_and_hard_stops_on_changed() {
    let server = serve_exec(|ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        let _ = ctx.stdin_eof.await;
        let _ = ctx.handle.data(ctx.channel, b"verified".to_vec()).await;
        let _ = ctx.handle.eof(ctx.channel).await;
        let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
        let _ = ctx.handle.close(ctx.channel).await;
    })
    .await;

    // 1. Unknown host key fails closed in headless mode without writing known_hosts.
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let err = exec(spec(server.port, &dir), "id", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(err, ExecError::UnknownHostKey { hop: None });
    assert!(!kh.exists(), "rejected host key must never be learned");

    // 2. Caller can inspect and accept UnknownHostKey via the prompt seam.
    let mut saw_prompt = false;
    let out = exec(spec(server.port, &dir), "id", ExecOptions::default(), &rt())
        .run(|prompt| {
            let port = server.port;
            let matched = matches!(
                &prompt,
                Prompt::UnknownHostKey {
                    host,
                    port: p,
                    algorithm,
                    fingerprint_sha256,
                    ..
                } if host == "127.0.0.1"
                    && *p == port
                    && algorithm == "ssh-ed25519"
                    && fingerprint_sha256.starts_with("SHA256:")
            );
            saw_prompt |= matched;
            async move {
                if let Prompt::UnknownHostKey { reply, .. } = prompt {
                    let _ = reply.send(true);
                }
            }
        })
        .await
        .unwrap();
    assert!(saw_prompt);
    assert_eq!(out.stdout, b"verified");
    assert!(kh.exists(), "accepted host key should be persisted");

    // 3. Changed host key is a hard stop with zero prompts emitted.
    let dir_changed = tempfile::tempdir().unwrap();
    let wrong = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    write_known_host(
        &dir_changed.path().join("known_hosts"),
        server.port,
        wrong.public_key(),
    );
    let mut prompted = false;
    let err = exec(
        spec(server.port, &dir_changed),
        "id",
        ExecOptions::default(),
        &rt(),
    )
    .run(|_| {
        prompted = true;
        async {}
    })
    .await
    .unwrap_err();
    assert!(
        !prompted,
        "changed host key must hard-stop without prompting"
    );
    assert_eq!(err, ExecError::HostKeyChanged { line: 1, hop: None });
    wait_active(&server.active, 0).await;
}

#[derive(Clone)]
enum AuthWant {
    Password(&'static str),
    Code(&'static str),
    Key(PublicKey),
    KeyOrPassword(PublicKey, &'static str),
}

#[derive(Clone)]
struct AuthExecHandler(AuthWant);

impl server::Handler for AuthExecHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        let matched = match &self.0 {
            AuthWant::Password(p) | AuthWant::KeyOrPassword(_, p) => *p == password,
            _ => false,
        };
        Ok(if matched {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }
        })
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let matched = match &self.0 {
            AuthWant::Key(k) | AuthWant::KeyOrPassword(k, _) => {
                k.key_data() == key.public_key.key_data()
            }
            _ => false,
        };
        Ok(if matched {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let AuthWant::Code(code) = self.0 else {
            return Ok(Auth::reject());
        };
        Ok(match response {
            None => Auth::Partial {
                name: Cow::Borrowed(""),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("2FA code: "), false)]),
            },
            Some(mut r) => match r.next() {
                Some(answer) if answer.as_ref() == code.as_bytes() => Auth::Accept,
                _ => Auth::reject(),
            },
        })
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        let cmd = String::from_utf8_lossy(data);
        s.data(ch, format!("authed:{cmd}").into_bytes())?;
        s.eof(ch)?;
        s.exit_status_request(ch, 0)?;
        s.close(ch)?;
        Ok(())
    }
}

async fn serve_auth_exec(want: AuthWant, methods: &[MethodKind]) -> (u16, PublicKey) {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_key = key.public_key().clone();
    let config = Arc::new(server::Config {
        methods: MethodSet::from(methods),
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![key],
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (config, handler) = (config.clone(), AuthExecHandler(want.clone()));
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, stream, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    (port, host_key)
}

fn unused_identity(dir: &tempfile::TempDir) -> Vec<PathBuf> {
    let path = dir.path().join("id_unused");
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .write_openssh_file(&path, LineEnding::LF)
        .unwrap();
    vec![path]
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_prompt_seam_supports_headless_ladder_and_distinguishes_interactive_auth_required() {
    // 1. MemoryKey (from open vault) and unencrypted identity file authenticate headlessly.
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let (key_port, key_host) = serve_auth_exec(
        AuthWant::Key(key.public_key().clone()),
        &[MethodKind::PublicKey],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut mem_spec = spec(key_port, &dir);
    write_known_host(
        mem_spec.known_hosts.as_deref().unwrap(),
        key_port,
        &key_host,
    );
    mem_spec.identity_files = unused_identity(&dir);
    mem_spec.memory_keys = vec![MemoryKey {
        name: "vault-key".into(),
        openssh: SecretString::from(key.to_openssh(LineEnding::LF).unwrap().to_string()),
    }];
    let out = exec(mem_spec, "whoami", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"authed:whoami");

    // 2. Password server: headless rejects as InteractiveAuthRequired; ladder supplying password
    //    succeeds; wrong password 3 times returns AuthFailed.
    let (pw_port, pw_host) =
        serve_auth_exec(AuthWant::Password("hunter2"), &[MethodKind::Password]).await;
    let dir_pw = tempfile::tempdir().unwrap();
    let mut pw_spec = spec(pw_port, &dir_pw);
    write_known_host(pw_spec.known_hosts.as_deref().unwrap(), pw_port, &pw_host);
    pw_spec.identity_files = unused_identity(&dir_pw);

    let err = exec(pw_spec.clone(), "check", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(err, ExecError::InteractiveAuthRequired { hop: None });

    let out = exec(pw_spec.clone(), "check", ExecOptions::default(), &rt())
        .run(|prompt| async move {
            if let Prompt::Password { reply, .. } = prompt {
                let _ = reply.send(Some(SecretString::from("hunter2")));
            }
        })
        .await
        .unwrap();
    assert_eq!(out.stdout, b"authed:check");

    let err = exec(pw_spec, "check", ExecOptions::default(), &rt())
        .run(|prompt| async move {
            if let Prompt::Password { reply, .. } = prompt {
                let _ = reply.send(Some(SecretString::from("wrong")));
            }
        })
        .await
        .unwrap_err();
    assert_eq!(err, ExecError::AuthFailed { hop: None });

    // 3. Encrypted identity file on key-only server: headless rejects as InteractiveAuthRequired;
    //    supplying passphrase via prompt seam succeeds.
    let dir_enc = tempfile::tempdir().unwrap();
    let enc_path = dir_enc.path().join("id_locked");
    key.encrypt(&mut rand::rng(), "open-sesame")
        .unwrap()
        .write_openssh_file(&enc_path, LineEnding::LF)
        .unwrap();
    let mut enc_spec = spec(key_port, &dir_enc);
    write_known_host(
        enc_spec.known_hosts.as_deref().unwrap(),
        key_port,
        &key_host,
    );
    enc_spec.identity_files = vec![enc_path.clone()];

    let err = exec(enc_spec.clone(), "enc", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(err, ExecError::InteractiveAuthRequired { hop: None });

    let out = exec(enc_spec, "enc", ExecOptions::default(), &rt())
        .run(|prompt| async move {
            if let Prompt::KeyPassphrase { reply, .. } = prompt {
                let _ = reply.send(Some(SecretString::from("open-sesame")));
            }
        })
        .await
        .unwrap();
    assert_eq!(out.stdout, b"authed:enc");

    // 4. Declining an encrypted key's passphrase still lets the ladder fall through to password.
    let (both_port, both_host) = serve_auth_exec(
        AuthWant::KeyOrPassword(key.public_key().clone(), "vault-pw"),
        &[MethodKind::PublicKey, MethodKind::Password],
    )
    .await;
    let dir_both = tempfile::tempdir().unwrap();
    let mut both_spec = spec(both_port, &dir_both);
    write_known_host(
        both_spec.known_hosts.as_deref().unwrap(),
        both_port,
        &both_host,
    );
    both_spec.identity_files = vec![enc_path];
    let out = exec(both_spec, "fallback", ExecOptions::default(), &rt())
        .run(|prompt| async move {
            match prompt {
                Prompt::KeyPassphrase { reply, .. } => {
                    let _ = reply.send(None);
                }
                Prompt::Password { reply, .. } => {
                    let _ = reply.send(Some(SecretString::from("vault-pw")));
                }
                _ => {}
            }
        })
        .await
        .unwrap();
    assert_eq!(out.stdout, b"authed:fallback");

    // 5. Keyboard-interactive / 2FA challenge fails closed headlessly as InteractiveAuthRequired.
    let (kbd_port, kbd_host) =
        serve_auth_exec(AuthWant::Code("123456"), &[MethodKind::KeyboardInteractive]).await;
    let dir_kbd = tempfile::tempdir().unwrap();
    let mut kbd_spec = spec(kbd_port, &dir_kbd);
    write_known_host(
        kbd_spec.known_hosts.as_deref().unwrap(),
        kbd_port,
        &kbd_host,
    );
    kbd_spec.identity_files = unused_identity(&dir_kbd);
    let err = exec(kbd_spec, "2fa", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(err, ExecError::InteractiveAuthRequired { hop: None });
}

fn jump_hop(port: u16) -> JumpHop {
    JumpHop {
        host: "127.0.0.1".into(),
        port,
        user: Some("hopper".into()),
        identity_files: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_jump_executes_checks_hop_keys_and_tears_down_on_complete_timeout_and_cancel() {
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let hop_a = serve(Shell::Echo).await;
    let hop_b = serve(Shell::Echo).await;
    let target = serve_exec(move |ctx: ExecCtx| {
        let notify = notify.clone();
        async move {
            let _ = ctx.handle.channel_success(ctx.channel).await;
            let _ = ctx.stdin_eof.await;
            if ctx.command == "hang" {
                notify.notify_one();
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            let _ = ctx
                .handle
                .data(
                    ctx.channel,
                    format!("via-jump:{}", ctx.command).into_bytes(),
                )
                .await;
            let _ = ctx.handle.eof(ctx.channel).await;
            let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
            let _ = ctx.handle.close(ctx.channel).await;
        }
    })
    .await;

    // 1. Two-hop ProxyJump succeeds and disconnects every hop cleanly.
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    write_known_host(&kh, hop_a.port, &hop_a.host_key);
    write_known_host(&kh, hop_b.port, &hop_b.host_key);
    write_known_host(&kh, target.port, &target.host_key);
    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![jump_hop(hop_a.port), jump_hop(hop_b.port)];

    let out = exec(s.clone(), "status", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(out.exit_status, Some(0));
    assert_eq!(out.stdout, b"via-jump:status");
    assert_eq!(
        *hop_a.opened.lock().unwrap(),
        [format!("127.0.0.1:{}", hop_b.port)]
    );
    assert_eq!(
        *hop_b.opened.lock().unwrap(),
        [format!("127.0.0.1:{}", target.port)]
    );
    assert_eq!(*target.requests.lock().unwrap(), ["exec"]);
    wait_active(&target.active, 0).await;
    wait_active(&hop_b.active, 0).await;
    wait_active(&hop_a.active, 0).await;

    // 2. Timeout tears down both jump hops and target cleanly.
    let err = exec(
        s.clone(),
        "hang",
        ExecOptions::with_timeout(Duration::from_secs(1)),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap_err();
    assert_eq!(
        err,
        ExecError::Timeout {
            timeout: Duration::from_secs(1),
        }
    );
    wait_active(&target.active, 0).await;
    wait_active(&hop_b.active, 0).await;
    wait_active(&hop_a.active, 0).await;

    // 3. Dropping Exec mid-command tears down jump hops and target cleanly.
    let run = exec(
        s,
        "hang",
        ExecOptions::with_timeout(Duration::from_secs(30)),
        &rt(),
    );
    let _kept_cancel = run.cancel_handle();
    wait_notify(&started, "proxy_jump hang start").await;
    wait_active(&hop_a.active, 1).await;
    wait_active(&hop_b.active, 1).await;
    wait_active(&target.active, 1).await;
    drop(run);
    wait_active(&target.active, 0).await;
    wait_active(&hop_b.active, 0).await;
    wait_active(&hop_a.active, 0).await;

    // 4. Unknown host key on a jump hop reports that hop.
    let dir_unk = tempfile::tempdir().unwrap();
    let mut s_unk = spec(target.port, &dir_unk);
    s_unk.proxy_jump = vec![jump_hop(hop_a.port)];
    let err = exec(s_unk, "status", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ExecError::UnknownHostKey {
            hop: Some(("127.0.0.1".into(), hop_a.port)),
        }
    );
    wait_active(&hop_a.active, 0).await;

    // 5. Changed host key on second hop stops hard, names that hop, and tears down hop A.
    let dir_chg = tempfile::tempdir().unwrap();
    let kh_chg = dir_chg.path().join("known_hosts");
    let wrong = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    write_known_host(&kh_chg, hop_a.port, &hop_a.host_key);
    write_known_host(&kh_chg, hop_b.port, wrong.public_key());
    let mut s_chg = spec(target.port, &dir_chg);
    s_chg.proxy_jump = vec![jump_hop(hop_a.port), jump_hop(hop_b.port)];
    let err = exec(s_chg, "status", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ExecError::HostKeyChanged {
            line: 2,
            hop: Some(("127.0.0.1".into(), hop_b.port)),
        }
    );
    wait_active(&hop_a.active, 0).await;
    wait_active(&hop_b.active, 0).await;
}

#[cfg(unix)]
const RELAY_PY: &str = r#"import os, select, socket, sys, time
host, port, pidfile, metafile, user = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
hang = "--hang-on-eof" in sys.argv[6:]
tmp = pidfile + ".tmp"
with open(tmp, "w") as f:
    f.write(str(os.getpid()))
os.replace(tmp, pidfile)
with open(metafile, "w") as f:
    f.write(f"{host}:{port}:{user}")
sock = socket.create_connection((host, port))
in_fd, out_fd = sys.stdin.fileno(), sys.stdout.fileno()
try:
    while True:
        r, _, _ = select.select([in_fd, sock], [], [])
        if in_fd in r:
            data = os.read(in_fd, 16384)
            if not data:
                if hang:
                    time.sleep(60)
                break
            sock.sendall(data)
        if sock in r:
            data = sock.recv(16384)
            if not data:
                if hang:
                    time.sleep(60)
                break
            view = memoryview(data)
            while view:
                n = os.write(out_fd, view)
                view = view[n:]
except (BrokenPipeError, ConnectionResetError, OSError):
    if hang:
        time.sleep(60)
finally:
    sock.close()
    if not hang:
        with open(pidfile + ".done", "w") as f:
            f.write("clean")
"#;

#[cfg(unix)]
async fn read_pid(pidfile: &std::path::Path) -> u32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "proxy never wrote pidfile {}",
            pidfile.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
async fn wait_pid_dead(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let alive = tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "proxy process {pid} still alive after exec ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_command_executes_and_kills_child_on_complete_timeout_and_cancel() {
    let started = Arc::new(Notify::new());
    let notify = started.clone();
    let server = serve_exec(move |ctx: ExecCtx| {
        let notify = notify.clone();
        async move {
            let _ = ctx.handle.channel_success(ctx.channel).await;
            let _ = ctx.stdin_eof.await;
            if ctx.command == "hang" {
                notify.notify_one();
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            let _ = ctx.handle.data(ctx.channel, b"via-proxy".to_vec()).await;
            let _ = ctx.handle.eof(ctx.channel).await;
            let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
            let _ = ctx.handle.close(ctx.channel).await;
        }
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("relay.py");
    std::fs::write(&script, RELAY_PY).unwrap();

    let make_proxy_spec = |name: &str, extra: &str| {
        let pidfile = dir.path().join(format!("{name}.pid"));
        let metafile = dir.path().join(format!("{name}.meta"));
        let mut s = known_spec(&server, &dir);
        s.proxy_command = Some(format!(
            "exec python3 '{}' %h %p '{}' '{}' %r{extra}",
            script.display(),
            pidfile.display(),
            metafile.display(),
        ));
        (s, pidfile, metafile)
    };

    // 1. Clean completion over ProxyCommand exits child cleanly.
    let (s_ok, pidfile, metafile) = make_proxy_spec("ok", "");
    let out = exec(s_ok, "check", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"via-proxy");
    assert_eq!(
        std::fs::read_to_string(&metafile).unwrap(),
        format!("127.0.0.1:{}:tester", server.port)
    );
    let pid = read_pid(&pidfile).await;
    wait_pid_dead(pid).await;
    assert!(dir.path().join("ok.pid.done").exists());
    wait_active(&server.active, 0).await;

    // 2. Timeout kills even a lingering (--hang-on-eof) ProxyCommand child.
    let (s_to, pidfile_to, _) = make_proxy_spec("timeout", " --hang-on-eof");
    let err = exec(
        s_to,
        "hang",
        ExecOptions::with_timeout(Duration::from_secs(1)),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap_err();
    assert_eq!(
        err,
        ExecError::Timeout {
            timeout: Duration::from_secs(1),
        }
    );
    let pid_to = read_pid(&pidfile_to).await;
    wait_pid_dead(pid_to).await;
    wait_active(&server.active, 0).await;

    // 3. Dropping Exec mid-command kills a lingering ProxyCommand child.
    let (s_cancel, pidfile_cancel, _) = make_proxy_spec("cancel", " --hang-on-eof");
    let run = exec(
        s_cancel,
        "hang",
        ExecOptions::with_timeout(Duration::from_secs(30)),
        &rt(),
    );
    let _kept_cancel = run.cancel_handle();
    wait_notify(&started, "proxy_command hang start").await;
    let pid_cancel = read_pid(&pidfile_cancel).await;
    drop(run);
    wait_pid_dead(pid_cancel).await;
    wait_active(&server.active, 0).await;

    // 4. Failing ProxyCommand surfaces its stderr line in ExecError::Ssh.
    let mut s_fail = known_spec(&server, &dir);
    s_fail.proxy_command = Some("printf 'bastion denied\\n' >&2; exit 1".into());
    let err = exec(s_fail, "check", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Ssh(msg) if msg.contains("bastion denied")),
        "unexpected error: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_reports_refusal_exit_signal_and_mid_flight_transport_drop() {
    // 1. Server with no exec handler refuses the exec request.
    let no_exec = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let err = exec(
        known_spec(&no_exec, &dir),
        "nope",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap_err();
    assert_eq!(err, ExecError::Refused);
    wait_active(&no_exec.active, 0).await;

    // 2. ExitSignal, trailing output after exit, clean close without exit status, and
    //    abrupt mid-flight disconnect.
    let server = serve_exec(|ctx: ExecCtx| async move {
        let _ = ctx.handle.channel_success(ctx.channel).await;
        let _ = ctx.stdin_eof.await;
        match ctx.command.as_str() {
            "signal" => {
                let _ = ctx
                    .handle
                    .exit_signal_request(ctx.channel, Sig::KILL, false, String::new(), "en".into())
                    .await;
                // Trailing output arriving after exit-signal must still be captured within EXIT_GRACE.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = ctx.handle.data(ctx.channel, b"after-sig".to_vec()).await;
                let _ = ctx.handle.eof(ctx.channel).await;
                let _ = ctx.handle.close(ctx.channel).await;
            }
            "late_after_exit" => {
                let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
                // Trailing output arriving after exit-status must still be captured within EXIT_GRACE.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = ctx.handle.data(ctx.channel, b"after-exit".to_vec()).await;
                let _ = ctx.handle.eof(ctx.channel).await;
                let _ = ctx.handle.close(ctx.channel).await;
            }
            "clean_close_no_status" => {
                let _ = ctx.handle.data(ctx.channel, b"clean-close".to_vec()).await;
                let _ = ctx.handle.eof(ctx.channel).await;
                let _ = ctx.handle.close(ctx.channel).await;
            }
            "exit_then_disconnect" => {
                let _ = ctx.handle.data(ctx.channel, b"done".to_vec()).await;
                let _ = ctx.handle.exit_status_request(ctx.channel, 0).await;
                let _ = ctx
                    .handle
                    .disconnect(russh::Disconnect::ByApplication, "bye".into(), "en".into())
                    .await;
            }
            "drop" => {
                let _ = ctx.handle.data(ctx.channel, b"partial".to_vec()).await;
                let _ = ctx
                    .handle
                    .disconnect(russh::Disconnect::ByApplication, "bye".into(), "en".into())
                    .await;
            }
            _ => {}
        }
    })
    .await;

    let dir2 = tempfile::tempdir().unwrap();
    let s = known_spec(&server, &dir2);

    let sig = exec(s.clone(), "signal", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(sig.exit_status, None);
    assert_eq!(sig.exit_signal.as_deref(), Some("KILL"));
    assert_eq!(sig.stdout, b"after-sig");
    wait_active(&server.active, 0).await;

    let late = exec(s.clone(), "late_after_exit", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap();
    assert_eq!(late.exit_status, Some(0));
    assert_eq!(late.stdout, b"after-exit");
    wait_active(&server.active, 0).await;

    let clean = exec(
        s.clone(),
        "clean_close_no_status",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap();
    assert_eq!(clean.exit_status, None);
    assert_eq!(clean.exit_signal, None);
    assert_eq!(clean.stdout, b"clean-close");
    wait_active(&server.active, 0).await;

    let done = exec(
        s.clone(),
        "exit_then_disconnect",
        ExecOptions::default(),
        &rt(),
    )
    .run_headless()
    .await
    .unwrap();
    assert_eq!(done.exit_status, Some(0));
    assert_eq!(done.stdout, b"done");
    wait_active(&server.active, 0).await;

    let err = exec(s, "drop", ExecOptions::default(), &rt())
        .run_headless()
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Ssh(msg) if msg.contains("closed")),
        "mid-flight disconnect must return ExecError::Ssh, got {err:?}"
    );
    wait_active(&server.active, 0).await;
}
