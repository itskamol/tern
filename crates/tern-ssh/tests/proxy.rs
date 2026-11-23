//! Connection setup lifecycle across ProxyCommand, ProxyJump, terminal sessions, and files
//! connections.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::{Shell, serve, serve_sftp, spec, start, wait_active};
use russh::keys::{Algorithm, PrivateKey};
use tern_ssh::{ConnectSpec, Disconnect, JumpHop, Prompt, SessionEvent, connect_files};

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
fn write_relay(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("relay.py");
    std::fs::write(&script, RELAY_PY).unwrap();
    script
}

#[cfg(unix)]
fn proxy_cmd(script: &Path, pidfile: &Path, metafile: &Path, extra: &str) -> String {
    format!(
        "exec python3 '{}' %h %p '{}' '{}' %r{extra}",
        script.display(),
        pidfile.display(),
        metafile.display(),
    )
}

#[cfg(unix)]
async fn read_pid(pidfile: &Path) -> u32 {
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
async fn pid_alive(pid: u32) -> bool {
    tokio::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
async fn wait_pid_dead(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pid_alive(pid).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "proxy process {pid} is still alive after connection ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

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

fn write_wrong_known_host(known_hosts: &Path, port: u16) {
    let wrong = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pub_text = wrong.public_key().to_openssh().unwrap();
    std::fs::write(known_hosts, format!("[127.0.0.1]:{port} {pub_text}\n")).unwrap();
}

async fn connect_files_accepting_host_key(
    spec: ConnectSpec,
) -> Result<
    (
        tern_ssh::SessionHandle,
        async_channel::Receiver<SessionEvent>,
    ),
    (Option<String>, Disconnect),
> {
    let (handle, events) = connect_files(spec, &tokio::runtime::Handle::current());
    let wait = async {
        loop {
            match events
                .recv()
                .await
                .expect("files session ended without Closed")
            {
                SessionEvent::Prompt(Prompt::UnknownHostKey { reply, .. }) => {
                    let _ = reply.send(true);
                }
                SessionEvent::Prompt(_) => {}
                SessionEvent::Connected => return Ok(()),
                SessionEvent::Data(_) | SessionEvent::Latency(_) => {}
                SessionEvent::Closed { error, reason, .. } => return Err((error, reason)),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("files login timed out")?;
    Ok((handle, events))
}

async fn wait_closed(
    events: &async_channel::Receiver<SessionEvent>,
) -> (Option<String>, Disconnect) {
    let wait = async {
        loop {
            if let SessionEvent::Closed { error, reason, .. } =
                events.recv().await.expect("no Closed event")
            {
                return (error, reason);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("session did not close")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_command_relays_session_and_terminates_child_on_close() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let script = write_relay(dir.path());
    let pidfile = dir.path().join("proxy.pid");
    let metafile = dir.path().join("proxy.meta");

    let mut s = spec(server.port, &dir);
    s.proxy_command = Some(proxy_cmd(&script, &pidfile, &metafile, ""));
    let live = start(s).await.unwrap();

    let pid = read_pid(&pidfile).await;
    assert!(
        pid_alive(pid).await,
        "proxy child must stay alive during session"
    );
    assert_eq!(
        std::fs::read_to_string(&metafile).unwrap(),
        format!("127.0.0.1:{}:tester", server.port)
    );
    assert!(echoes(&live, "via-proxy-command").await);

    live.handle.close();
    let (_, reason) = live.closed().await;
    assert_eq!(reason, Disconnect::Local);
    wait_pid_dead(pid).await;
    assert!(
        dir.path().join("proxy.pid.done").exists(),
        "proxy child should exit cleanly on EOF before SIGKILL"
    );
    wait_active(&server.active, 0).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_command_kills_a_lingering_child_on_close() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let script = write_relay(dir.path());
    let pidfile = dir.path().join("proxy.pid");
    let metafile = dir.path().join("proxy.meta");

    let mut s = spec(server.port, &dir);
    s.proxy_command = Some(proxy_cmd(&script, &pidfile, &metafile, " --hang-on-eof"));
    let live = start(s).await.unwrap();
    let pid = read_pid(&pidfile).await;
    assert!(pid_alive(pid).await);

    live.handle.close();
    let (_, reason) = live.closed().await;
    assert_eq!(reason, Disconnect::Local);
    wait_pid_dead(pid).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_host_key_over_proxy_command_is_still_a_hard_stop() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let script = write_relay(dir.path());
    let pidfile = dir.path().join("proxy.pid");
    let metafile = dir.path().join("proxy.meta");
    write_wrong_known_host(&dir.path().join("known_hosts"), server.port);

    let mut s = spec(server.port, &dir);
    s.proxy_command = Some(proxy_cmd(&script, &pidfile, &metafile, ""));
    let (error, reason) = start(s).await.err().unwrap();
    assert_eq!(reason, Disconnect::HostKeyChanged { hop: None });
    let error = error.unwrap();
    assert!(error.contains("host key changed"), "{error}");

    let pid = read_pid(&pidfile).await;
    wait_pid_dead(pid).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_command_failure_reports_first_non_empty_stderr_line_or_exit_status() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();

    let mut with_stderr = spec(server.port, &dir);
    with_stderr.proxy_command =
        Some("printf '\\n  bastion refused  \\nusage: proxy\\n' >&2; exit 1".into());
    let (error, reason) = start(with_stderr).await.err().unwrap();
    assert_eq!(reason, Disconnect::Failed);
    let error = error.unwrap();
    assert!(
        error.contains("ProxyCommand failed: bastion refused"),
        "{error}"
    );

    let mut silent = spec(server.port, &dir);
    silent.proxy_command = Some("exit 7".into());
    let (error, reason) = start(silent).await.err().unwrap();
    assert_eq!(reason, Disconnect::Failed);
    let error = error.unwrap();
    assert!(error.contains("ProxyCommand failed:"), "{error}");
    assert!(error.contains('7'), "{error}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_jump_takes_precedence_over_proxy_command_when_both_are_set() {
    let (hop_server, target) = (serve(Shell::Echo).await, serve(Shell::Echo).await);
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("proxy_command_ran");

    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![JumpHop {
        host: "127.0.0.1".into(),
        port: hop_server.port,
        user: Some("hopper".into()),
        identity_files: Vec::new(),
    }];
    s.proxy_command = Some(format!("touch '{}'; exit 1", marker.display()));

    let live = start(s).await.unwrap();
    assert!(
        !marker.exists(),
        "proxy_command must not run when proxy_jump is set"
    );
    assert_eq!(
        *hop_server.opened.lock().unwrap(),
        [format!("127.0.0.1:{}", target.port)]
    );
    assert!(echoes(&live, "jump-wins").await);
    live.handle.close();
    let (_, reason) = live.closed().await;
    assert_eq!(reason, Disconnect::Local);
    wait_active(&hop_server.active, 0).await;
    wait_active(&target.active, 0).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn files_connection_works_through_proxy_command_and_closes_child() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    std::fs::write(root.path().join("home/hello.txt"), b"hi").unwrap();
    let target = serve_sftp(root.path().to_path_buf()).await;

    let dir_proxy = tempfile::tempdir().unwrap();
    let script = write_relay(dir_proxy.path());
    let pidfile = dir_proxy.path().join("files-proxy.pid");
    let metafile = dir_proxy.path().join("files-proxy.meta");
    let mut s_proxy = spec(target.port, &dir_proxy);
    s_proxy.forward_agent = true;
    s_proxy.proxy_command = Some(proxy_cmd(&script, &pidfile, &metafile, ""));
    let (handle, events) = connect_files_accepting_host_key(s_proxy).await.unwrap();
    let pid = read_pid(&pidfile).await;
    assert!(pid_alive(pid).await);
    let sftp = handle.open_sftp().await.unwrap();
    let names: Vec<_> = sftp
        .list("/home")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["hello.txt"]);
    assert!(target.requests.lock().unwrap().is_empty());
    handle.close();
    let (_, reason) = wait_closed(&events).await;
    assert_eq!(reason, Disconnect::Local);
    wait_pid_dead(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_connection_shares_proxy_jump_and_host_key_checks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    std::fs::write(root.path().join("home/hello.txt"), b"hi").unwrap();
    let target = serve_sftp(root.path().to_path_buf()).await;

    // Files over ProxyJump: hop stays alive until close, then closes cleanly.
    let hop_server = serve(Shell::Echo).await;
    let dir_jump = tempfile::tempdir().unwrap();
    let mut s_jump = spec(target.port, &dir_jump);
    s_jump.proxy_jump = vec![JumpHop {
        host: "127.0.0.1".into(),
        port: hop_server.port,
        user: Some("hopper".into()),
        identity_files: Vec::new(),
    }];
    let (handle, events) = connect_files_accepting_host_key(s_jump).await.unwrap();
    wait_active(&hop_server.active, 1).await;
    let sftp = handle.open_sftp().await.unwrap();
    assert_eq!(sftp.list("/home").await.unwrap().len(), 1);
    handle.close();
    let (_, reason) = wait_closed(&events).await;
    assert_eq!(reason, Disconnect::Local);
    wait_active(&hop_server.active, 0).await;

    // Files with changed host key: hard stop.
    let dir_changed = tempfile::tempdir().unwrap();
    write_wrong_known_host(&dir_changed.path().join("known_hosts"), target.port);
    let (error, reason) = connect_files_accepting_host_key(spec(target.port, &dir_changed))
        .await
        .err()
        .unwrap();
    assert_eq!(reason, Disconnect::HostKeyChanged { hop: None });
    assert!(error.unwrap().contains("host key changed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn jump_hops_stay_alive_during_session_and_close_cleanly_on_disconnect() {
    let (a, b, target) = (
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
    );
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![
        JumpHop {
            host: "127.0.0.1".into(),
            port: a.port,
            user: Some("hopper".into()),
            identity_files: Vec::new(),
        },
        JumpHop {
            host: "127.0.0.1".into(),
            port: b.port,
            user: Some("hopper".into()),
            identity_files: Vec::new(),
        },
    ];
    let live = start(s).await.unwrap();
    wait_active(&a.active, 1).await;
    wait_active(&b.active, 1).await;
    wait_active(&target.active, 1).await;
    assert!(echoes(&live, "hops-alive").await);

    live.handle.close();
    let (_, reason) = live.closed().await;
    assert_eq!(reason, Disconnect::Local);
    wait_active(&target.active, 0).await;
    wait_active(&b.active, 0).await;
    wait_active(&a.active, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_later_jump_hop_tears_down_earlier_hops_cleanly() {
    let (a, b, target) = (
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
        serve(Shell::Echo).await,
    );
    let dir = tempfile::tempdir().unwrap();
    write_wrong_known_host(&dir.path().join("known_hosts"), b.port);

    let mut s = spec(target.port, &dir);
    s.proxy_jump = vec![
        JumpHop {
            host: "127.0.0.1".into(),
            port: a.port,
            user: Some("hopper".into()),
            identity_files: Vec::new(),
        },
        JumpHop {
            host: "127.0.0.1".into(),
            port: b.port,
            user: Some("hopper".into()),
            identity_files: Vec::new(),
        },
    ];
    let (error, reason) = start(s).await.err().unwrap();
    assert_eq!(
        reason,
        Disconnect::HostKeyChanged {
            hop: Some(("127.0.0.1".into(), b.port))
        }
    );
    assert!(error.unwrap().contains("host key changed"));
    wait_active(&a.active, 0).await;
    wait_active(&b.active, 0).await;
    wait_active(&target.active, 0).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proxy_command_child_is_killed_when_host_key_is_rejected() {
    let server = serve(Shell::Echo).await;
    let dir = tempfile::tempdir().unwrap();
    let script = write_relay(dir.path());
    let pidfile = dir.path().join("reject.pid");
    let metafile = dir.path().join("reject.meta");

    let mut s = spec(server.port, &dir);
    s.proxy_command = Some(proxy_cmd(&script, &pidfile, &metafile, " --hang-on-eof"));
    let (_handle, events) =
        tern_ssh::connect(s, common::size(), &tokio::runtime::Handle::current());
    let (error, reason) = loop {
        match events.recv().await.unwrap() {
            SessionEvent::Prompt(Prompt::UnknownHostKey { reply, .. }) => {
                let _ = reply.send(false);
            }
            SessionEvent::Closed { error, reason, .. } => break (error, reason),
            _ => {}
        }
    };
    assert_eq!(reason, Disconnect::Failed);
    let error = error.unwrap();
    assert!(error.contains("host key rejected"), "{error}");

    let pid = read_pid(&pidfile).await;
    wait_pid_dead(pid).await;
}
