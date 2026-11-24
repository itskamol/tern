#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, TestApp};
use tokio::io::BufReader;

use super::*;
use crate::agents::protocol::{
    AgentErrorCode, BoundedLineReader, BrokerMessage, BrokerRequest, BrokerResult,
    ListHostsResponse, MAX_FRAME_BYTES, write_json_line,
};
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
        service: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) -> bool {
        self.agents.grants.verify(host, service, fingerprint)
    }

    fn record_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) {
        self.agents
            .grants
            .insert(host, service, fingerprint, SystemTime::now());
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
                BrokerMessage::Progress { .. } => {}
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
fn persist_config_returns_pruned_config_and_refuses_when_store_error_set() {
    let dir = tempfile::tempdir().unwrap();
    let connections = vec![conn("staging")];
    let mut cfg = AgentConfig {
        enabled: true,
        paused: true,
        hosts: BTreeMap::new(),
    };
    cfg.hosts
        .insert("staging".into(), vec!["api.service".into()]);
    cfg.hosts
        .insert("deleted-host".into(), vec!["old.service".into()]);

    // Refuses to clobber disk when store_error is present.
    assert!(matches!(
        persist_config(Some(dir.path()), &cfg, &connections, Some("corrupt")),
        Err(StoreError::Unreadable(_))
    ));
    assert!(!dir.path().join(store::FILE_NAME).exists());

    // Returns the pruned config so callers never keep deleted hosts in memory.
    let clean = persist_config(Some(dir.path()), &cfg, &connections, None).unwrap();
    assert!(!clean.hosts.contains_key("deleted-host"));
    assert_eq!(
        clean.hosts.get("staging").map(Vec::as_slice),
        Some(&["api.service".to_owned()][..])
    );

    let loaded = store::load(dir.path()).unwrap();
    assert_eq!(loaded, clean);
}

#[cfg(unix)]
#[test]
fn deleted_saved_connection_prunes_allowlist_so_recreated_alias_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());

    let shell = app
        .update(|cx| cx.new(|_| TestShell::new(dir.path().to_path_buf(), vec![conn("staging")])));
    let sock = socket_path(dir.path());

    // 1. Enable MCP and allow api.service on Saved connection "staging".
    app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents
                .set_enabled(Some(&s.dir), true, &s.connections, weak, cx)
                .unwrap();
            s.agents
                .set_allowed_services(
                    Some(&s.dir),
                    "staging",
                    vec!["api.service".into()],
                    &s.connections,
                )
                .unwrap();
        });
    });

    let before = send_broker_request(&sock, BrokerRequest::ListHosts { caller: None }, &app);
    let BrokerResult::Ok { data } = before else {
        panic!("expected list_hosts Ok, got {before:?}");
    };
    let list: ListHostsResponse = serde_json::from_value(data).unwrap();
    assert_eq!(list.hosts.len(), 1);
    assert_eq!(list.hosts[0].alias, "staging");
    assert_eq!(list.hosts[0].services, vec!["api.service"]);

    // 2. Delete Saved connection "staging" (same path as Shell::delete_connection -> refresh_hosts).
    app.update(|cx| {
        shell.update(cx, |s, _| {
            s.connections.clear();
            let removed = s
                .agents
                .prune_orphaned_hosts(Some(&s.dir), &s.connections, false);
            assert_eq!(removed, 1);
        });
    });

    let disk_after_delete = store::load(dir.path()).unwrap();
    assert!(
        disk_after_delete.hosts.is_empty(),
        "deleting staging must remove its allowlist from agents.json on disk"
    );

    // 3. Create a brand-new Saved connection under the reused alias "staging" with NO allowed services.
    app.update(|cx| {
        shell.update(cx, |s, _| {
            s.connections.push(conn("staging"));
            s.agents
                .prune_orphaned_hosts(Some(&s.dir), &s.connections, false);
        });
    });

    // 4. list_hosts must NOT include "staging", and service_status("staging", "api.service") must be denied.
    let after = send_broker_request(&sock, BrokerRequest::ListHosts { caller: None }, &app);
    let BrokerResult::Ok { data } = after else {
        panic!("expected list_hosts Ok, got {after:?}");
    };
    let list: ListHostsResponse = serde_json::from_value(data).unwrap();
    assert!(
        list.hosts.is_empty(),
        "recreated staging must not inherit deleted host's allowlist: {list:?}"
    );

    let status = send_broker_request(
        &sock,
        BrokerRequest::ServiceStatus {
            caller: Some("TestAgent".into()),
            host: "staging".into(),
            service: "api.service".into(),
            timeout_secs: None,
            wants_progress: false,
        },
        &app,
    );
    assert!(
        matches!(
            status,
            BrokerResult::Error(ref err) if err.error == AgentErrorCode::Denied
        ),
        "recreated staging must deny previously allowlisted service; got {status:?}"
    );
    assert!(store::load(dir.path()).unwrap().hosts.is_empty());

    // 5. Toggling paused also assigns the pruned config in memory, never retaining orphaned hosts.
    app.update(|cx| {
        shell.update(cx, |s, _| {
            s.agents
                .config
                .hosts
                .insert("ghost".into(), vec!["old.service".into()]);
            s.agents
                .set_paused(Some(&s.dir), true, &s.connections)
                .unwrap();
            assert!(!s.agents.config.hosts.contains_key("ghost"));
        });
    });

    // 6. Unreadable hosts.json (hosts_store_error = true) must never wipe agents.json.
    app.update(|cx| {
        shell.update(cx, |s, _| {
            s.agents
                .set_allowed_services(
                    Some(&s.dir),
                    "staging",
                    vec!["api.service".into()],
                    &s.connections,
                )
                .unwrap();
            s.connections.clear();
            let removed = s
                .agents
                .prune_orphaned_hosts(Some(&s.dir), &s.connections, true);
            assert_eq!(removed, 0);
            assert!(s.agents.config.hosts.contains_key("staging"));
            assert!(store::load(&s.dir).unwrap().hosts.contains_key("staging"));
        });
    });
}

#[cfg(unix)]
#[test]
fn broker_start_failure_returns_error_rolls_back_enabled_and_surfaces_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());

    // Place a directory at agent.sock so binding the Unix domain socket fails cleanly.
    let sock = socket_path(dir.path());
    std::fs::create_dir(&sock).unwrap();

    let shell = app
        .update(|cx| cx.new(|_| TestShell::new(dir.path().to_path_buf(), vec![conn("staging")])));

    // 1. Toggling Enable MCP on must return Err, leave enabled = false, and not persist enabled = true.
    let err = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents
                .set_enabled(Some(&s.dir), true, &s.connections, weak, cx)
        })
    });
    let msg = err.expect_err("binding over a directory must fail, never silently succeed");
    assert!(
        msg.contains("Could not start local Agent broker"),
        "unexpected error message: {msg}"
    );
    app.update(|cx| {
        let s = shell.read(cx);
        assert!(
            !s.agents.config.enabled,
            "enabled toggle must not stay on after bind failure"
        );
        assert!(s.agents.broker.is_none());
    });
    assert!(
        !store::load(dir.path()).unwrap().enabled,
        "agents.json must not persist enabled = true when bind failed"
    );

    // 2. Startup with enabled = true in agents.json must surface an error rather than swallowing it,
    //    and keep enabled = false in memory without clobbering agents.json on disk.
    let enabled_cfg = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    store::save(dir.path(), &enabled_cfg, &[conn("staging")]).unwrap();

    let startup_err = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents
                .load_and_start(&s.dir, &s.connections, false, weak, cx)
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
    assert!(
        store::load(dir.path()).unwrap().enabled,
        "startup bind failure must not clobber agents.json on disk"
    );

    // 3. Once the obstruction is cleared, enabling succeeds and binds agent.sock.
    std::fs::remove_dir(&sock).unwrap();
    let ok = app.update(|cx| {
        shell.update(cx, |s, cx| {
            let weak = cx.entity().downgrade();
            s.agents
                .set_enabled(Some(&s.dir), true, &s.connections, weak, cx)
        })
    });
    assert_eq!(ok, Ok(false));
    app.update(|cx| {
        let s = shell.read(cx);
        assert!(s.agents.config.enabled);
        assert!(s.agents.broker.is_some());
    });
}
