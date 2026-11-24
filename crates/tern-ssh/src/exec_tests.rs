#![allow(clippy::unwrap_used)]

use std::io::Error as IoError;
use std::io::ErrorKind;
use std::time::Duration;

use super::*;

#[test]
fn bounded_buf_preserves_exact_cap_and_truncates_on_first_excess_byte() {
    let mut buf = BoundedBuf::default();
    buf.push(&vec![b'a'; EXEC_OUTPUT_CAP - 1]);
    buf.push(b"b");
    buf.push(b"");
    assert_eq!(buf.bytes.len(), EXEC_OUTPUT_CAP);
    assert_eq!(buf.bytes[EXEC_OUTPUT_CAP - 1], b'b');
    assert!(!buf.truncated);

    buf.push(b"c");
    assert_eq!(buf.bytes.len(), EXEC_OUTPUT_CAP);
    assert_eq!(buf.bytes[EXEC_OUTPUT_CAP - 1], b'b');
    assert!(buf.truncated);

    let mut single = BoundedBuf::default();
    single.push(&vec![b'x'; EXEC_OUTPUT_CAP + 10]);
    assert_eq!(single.bytes.len(), EXEC_OUTPUT_CAP);
    assert!(single.truncated);
}

#[test]
fn timeout_clamping_and_duration_formatting_cover_boundaries() {
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_millis(999)).effective_timeout(),
        MIN_EXEC_TIMEOUT
    );
    assert_eq!(
        ExecOptions::with_timeout(Duration::from_secs(300) + Duration::from_nanos(1))
            .effective_timeout(),
        MAX_EXEC_TIMEOUT
    );
    assert_eq!(format_duration(Duration::from_secs(15)), "15s");
    assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
}

#[test]
fn exec_error_messages_are_lowercase_without_trailing_punctuation() {
    let hop = || Some(("bastion.example".to_owned(), 2222));
    let cases = [
        (ExecError::UnknownHostKey { hop: None }, "unknown host key"),
        (
            ExecError::UnknownHostKey { hop: hop() },
            "unknown host key for jump host bastion.example:2222",
        ),
        (
            ExecError::HostKeyChanged { line: 9, hop: None },
            "host key changed at known_hosts line 9",
        ),
        (
            ExecError::HostKeyChanged {
                line: 9,
                hop: hop(),
            },
            "host key changed for jump host bastion.example:2222 at known_hosts line 9",
        ),
        (
            ExecError::InteractiveAuthRequired { hop: None },
            "interactive authentication required",
        ),
        (
            ExecError::InteractiveAuthRequired { hop: hop() },
            "interactive authentication required for jump host bastion.example:2222",
        ),
        (ExecError::AuthFailed { hop: None }, "authentication failed"),
        (
            ExecError::AuthFailed { hop: hop() },
            "authentication failed for jump host bastion.example:2222",
        ),
        (
            ExecError::Timeout {
                timeout: Duration::from_secs(15),
                phase: ExecTimeoutPhase::Running,
                partial: ExecPartialOutput::default(),
            },
            "command timed out after 15s",
        ),
        (
            ExecError::Timeout {
                timeout: Duration::from_millis(1500),
                phase: ExecTimeoutPhase::NotDispatched,
                partial: ExecPartialOutput::default(),
            },
            "command timed out after 1.5s",
        ),
        (ExecError::Cancelled, "command cancelled"),
        (ExecError::Refused, "server refused exec request"),
        (ExecError::Ssh("boom".into()), "boom"),
    ];
    for (err, expected) in cases {
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn map_failure_preserves_security_auth_timeout_and_jump_hop_context() {
    let t10 = Duration::from_secs(10);
    let t30 = Duration::from_secs(30);
    let via = |f: Failure| Failure::Jump("gw.internal:2200".into(), Box::new(f));
    let h = || Some(("gw.internal".to_owned(), 2200));
    let check = |f, want| assert_eq!(map_failure(f, t10), want);

    check(
        Failure::HostKeyChanged { line: 3 },
        ExecError::HostKeyChanged { line: 3, hop: None },
    );
    check(
        via(Failure::HostKeyChanged { line: 4 }),
        ExecError::HostKeyChanged { line: 4, hop: h() },
    );
    check(
        Failure::HostKeyRejected,
        ExecError::UnknownHostKey { hop: None },
    );
    check(
        via(Failure::HostKeyRejected),
        ExecError::UnknownHostKey { hop: h() },
    );
    check(
        Failure::AuthCancelled,
        ExecError::InteractiveAuthRequired { hop: None },
    );
    check(
        via(Failure::AuthCancelled),
        ExecError::InteractiveAuthRequired { hop: h() },
    );
    check(Failure::AuthFailed, ExecError::AuthFailed { hop: None });
    check(via(Failure::AuthFailed), ExecError::AuthFailed { hop: h() });
    check(
        Failure::ConnectTimeout,
        ExecError::Timeout {
            timeout: t10,
            phase: ExecTimeoutPhase::NotDispatched,
            partial: ExecPartialOutput::default(),
        },
    );
    assert_eq!(
        map_failure(Failure::ConnectTimeout, t30),
        ExecError::Timeout {
            timeout: CONNECT_TIMEOUT,
            phase: ExecTimeoutPhase::NotDispatched,
            partial: ExecPartialOutput::default(),
        }
    );
    check(Failure::UiGone, ExecError::Cancelled);
    check(
        via(Failure::Unreachable("gw refused".into())),
        ExecError::Ssh("gw refused (while going through jump host gw.internal:2200)".into()),
    );
    let pipe = IoError::from(ErrorKind::BrokenPipe);
    check(
        Failure::Io(IoError::from(ErrorKind::BrokenPipe)),
        ExecError::Ssh(pipe.to_string()),
    );
    assert_eq!(parse_hop("[::1]:2222"), Some(("[::1]".to_owned(), 2222)));
    assert_eq!(parse_hop("no-port"), None);
}

#[test]
fn describe_drop_distinguishes_disconnect_causes() {
    assert_eq!(
        describe_drop(Some(&Disconnect::Timeout)),
        "connection timed out before command finished"
    );
    assert_eq!(
        describe_drop(Some(&Disconnect::Reset)),
        "connection reset before command finished"
    );
    assert_eq!(
        describe_drop(Some(&Disconnect::ServerClosed)),
        "server closed connection before command finished"
    );
    assert_eq!(
        describe_drop(None),
        "connection closed before command finished"
    );
}

#[tokio::test]
async fn exec_next_returns_none_after_finished_and_direct_cancel_signals() {
    let (etx, erx) = async_channel::bounded(2);
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let mut run = Exec {
        events: erx,
        cancel: ExecCancel { tx: cancel_tx },
        finished: false,
    };
    assert!(format!("{run:?}").contains("Exec"));
    run.cancel();
    assert!(*cancel_rx.borrow_and_update());
    etx.send(ExecEvent::Finished(Err(ExecError::Cancelled)))
        .await
        .unwrap();
    assert!(matches!(
        run.next().await,
        Some(ExecEvent::Finished(Err(ExecError::Cancelled)))
    ));
    assert!(run.next().await.is_none());
}
