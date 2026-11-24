#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, TestApp};
use tokio::io::BufReader;

use super::*;
use crate::agents::protocol::{
    BoundedLineReader, BrokerMessage, BrokerRequest, BrokerResult, ListHostsResponse,
    MAX_FRAME_BYTES, write_json_line,
};
use crate::connections::Connection;
use crate::decisions::{DecisionQueue, DecisionRequest};
use crate::platform::SshRuntime;

fn conn(name: &str) -> Connection {
    Connection {
        name: name.into(),
        host: "10.0.0.1".into(),
        port: 22,
        user: "deploy".into(),
        ..Connection::default()
    }
}

struct TestShell {
    dir: PathBuf,
    connections: Vec<Connection>,
    agents: AgentsUi,
    decisions: DecisionQueue<DecisionTag>,
}

impl TestShell {
    fn new(dir: PathBuf, connections: Vec<Connection>) -> Self {
        Self {
            dir,
            connections,
            agents: AgentsUi::default(),
            decisions: DecisionQueue::new(),
        }
    }
}

impl AgentHost for TestShell {
    fn agent_snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot {
            dir: self.dir.clone(),
            known_hosts: None,
            config: self.agents.config.clone(),
            connections: self.connections.clone(),
        }
    }

    fn verify_agent_grant(
        &mut self,
        host: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) -> bool {
        self.agents.grants.verify(host, fingerprint)
    }

    fn record_agent_grant(
        &mut self,
        host: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) {
        self.agents
            .grants
            .insert(host, fingerprint, SystemTime::now());
    }

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        _cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        let mut req = DecisionRequest::new(DecisionTag::Agent, prompt);
        if let Some(d) = deadline {
            req = req.with_deadline(d);
        }
        self.decisions.enqueue(req, Instant::now())
    }
}

#[cfg(unix)]
fn send_broker_request(sock: &Path, request: BrokerRequest, app: &TestApp) -> BrokerResult {
    let sock = sock.to_path_buf();
    let rt = app.read(SshRuntime::handle);
    let (tx, rx) = async_channel::bounded(1);
    rt.spawn(async move {
        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        write_json_line(&mut w, &request).await.unwrap();
        let mut reader = BoundedLineReader::new(BufReader::new(r));
        loop {
            let line = reader
                .next_line(MAX_FRAME_BYTES)
                .await
                .unwrap()
                .expect("broker closed before response");
            match serde_json::from_str::<BrokerMessage>(&line).unwrap() {
                BrokerMessage::Progress { .. }
                | BrokerMessage::StdoutChunk { .. }
                | BrokerMessage::StderrChunk { .. } => {}
                BrokerMessage::Result(result) => {
                    let _ = tx.send(result).await;
                    break;
                }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.run_until_parked();
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        assert!(Instant::now() < deadline, "timed out waiting for broker");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn persist_config_refuses_when_store_error_set_and_persists_cleanly_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = AgentConfig {
        enabled: true,
        paused: true,
    };

    assert!(matches!(
        persist_config(Some(dir.path()), &cfg, Some("corrupt")),
        Err(StoreError::Unreadable(_))
    ));
    assert!(!dir.path().join(store::FILE_NAME).exists());

    persist_config(Some(dir.path()), &cfg, None).unwrap();
    assert_eq!(store::load(dir.path()).unwrap(), cfg);
}

#[cfg(unix)]
#[test]
fn list_hosts_exposes_all_saved_connections_and_reflects_edits_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());

    let shell = app.update(|cx| {
        cx.new(|_| {
            TestShell::new(
                dir.path().to_path_buf(),
                vec![conn("staging"), conn("prod")],
            )
        })
    });
    let sock = socket_path(dir.path());

    app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents.set_enabled(Some(&s.dir), true, weak, cx).unwrap();
        });
    });

    let res = send_broker_request(
        &sock,
        BrokerRequest::ListHosts {
            caller: None,
            query: None,
        },
        &app,
    );
    let BrokerResult::Ok { data } = res else {
        panic!("expected list_hosts Ok, got {res:?}");
    };
    let list: ListHostsResponse = serde_json::from_value(data).unwrap();
    let aliases: Vec<&str> = list.hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(aliases, vec!["staging", "prod"]);

    // Deleting a Saved connection immediately removes it from list_hosts and prunes its Grant.
    app.update(|cx| {
        shell.update(cx, |s, _| {
            let fp = GrantFingerprint::for_connection(&s.connections[0], &s.connections);
            s.agents.grants.insert("staging", fp, SystemTime::now());
            s.connections.remove(0);
            assert_eq!(s.agents.grants.prune_stale(&s.connections), 1);
            assert!(s.agents.grants.is_empty());
        });
    });

    let after = send_broker_request(
        &sock,
        BrokerRequest::ListHosts {
            caller: None,
            query: None,
        },
        &app,
    );
    let BrokerResult::Ok { data } = after else {
        panic!("expected list_hosts Ok, got {after:?}");
    };
    let list: ListHostsResponse = serde_json::from_value(data).unwrap();
    let aliases: Vec<&str> = list.hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(aliases, vec!["prod"]);
}

#[cfg(unix)]
#[test]
fn broker_start_failure_returns_error_rolls_back_enabled_and_surfaces_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());

    let sock = socket_path(dir.path());
    std::fs::create_dir(&sock).unwrap();

    let shell = app
        .update(|cx| cx.new(|_| TestShell::new(dir.path().to_path_buf(), vec![conn("staging")])));

    let err = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents.set_enabled(Some(&s.dir), true, weak, cx)
        })
    });
    let msg = err.expect_err("binding over a directory must fail, never silently succeed");
    assert!(
        msg.contains("Could not start local Agent broker"),
        "unexpected error message: {msg}"
    );
    app.update(|cx| {
        let s = shell.read(cx);
        assert!(!s.agents.config.enabled);
        assert!(s.agents.broker.is_none());
    });
    assert!(!store::load(dir.path()).unwrap().enabled);

    let enabled_cfg = AgentConfig {
        enabled: true,
        paused: false,
    };
    store::save(dir.path(), &enabled_cfg).unwrap();

    let startup_err = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents.load_and_start(&s.dir, weak, cx)
        })
    });
    let startup_msg =
        startup_err.expect_err("startup broker bind failure must surface an error/Alert");
    assert!(startup_msg.contains("Could not start local Agent broker"));
    app.update(|cx| {
        let s = shell.read(cx);
        assert!(!s.agents.config.enabled);
        assert!(s.agents.broker.is_none());
    });
    assert!(store::load(dir.path()).unwrap().enabled);

    std::fs::remove_dir(&sock).unwrap();
    let ok = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents.set_enabled(Some(&s.dir), true, weak, cx)
        })
    });
    assert_eq!(ok, Ok(false));
    app.update(|cx| {
        let s = shell.read(cx);
        assert!(s.agents.config.enabled);
        assert!(s.agents.broker.is_some());
    });
}
