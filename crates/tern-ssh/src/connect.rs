// Adapted from russh russh/examples/client_open_direct_tcpip.rs (Apache-2.0) for ProxyJump
// direct-tcpip chaining, and from CrabPort crabport-ssh/src/backend.rs (Apache-2.0) for
// protocol-level keepalive settings.
//! Authenticated SSH connection setup shared by terminal sessions, files connections, and
//! non-PTY exec calls.
//!
//! Hides transport selection (`ProxyJump`, `ProxyCommand`, direct TCP), host-key verification,
//! authentication, and RAII ownership of intermediate jump hops and proxy child processes so
//! they stay alive for the full connection lifetime and tear down cleanly when it ends.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, Msg};
use russh::{Channel, Disconnect as SshDisconnect, Preferred};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::authn::{self, Authenticator};
use crate::config;
use crate::error::{Cause, Error, Failure};
use crate::forward::RemoteTargets;
use crate::hostkey::{Asking, Handler, known_algorithms, within};
use crate::{ConnectSpec, SessionEvent};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Grace window for a proxy child to settle its exit status/stderr on handshake failure, or
/// to exit on stdin EOF after SSH disconnect before being killed.
const PROXY_EXIT_GRACE: Duration = Duration::from_millis(200);
/// Unread SSH messages russh buffers per channel before it stops reading the socket.
/// russh's default is 100; at up to 32 KiB a packet that is 3 MiB per session.
const CHANNEL_BUFFER: usize = 16;

/// Capabilities enabled on the authenticated target connection.
///
/// The default posture is the most restrictive (no agent forwarding, no remote port-forward
/// targets, fresh disconnect cause), matching isolated non-interactive callers.
#[derive(Clone, Default)]
pub(crate) struct ConnectOptions {
    /// Whether server-opened `auth-agent@openssh.com` channels may be bridged to a local agent.
    /// Only terminal sessions enable this; files and exec connections keep it `false`.
    pub allow_agent_forwarding: bool,
    /// Allowed targets for server-opened `forwarded-tcpip` channels (`-R`). Empty refuses all.
    pub remote_targets: RemoteTargets,
    /// Records why russh reports the target connection dead after handshake.
    pub cause: Cause,
}

/// An authenticated SSH handle together with the jump hops and proxy child keeping its
/// transport alive.
///
/// Fields drop top-to-bottom so the target session closes before the jump hops or proxy child
/// carrying its bytes. Call [`Self::disconnect`] for a clean SSH-level disconnect before
/// tearing down the transport.
#[must_use = "dropping an AuthenticatedConnection closes the SSH connection and its transport"]
pub(crate) struct AuthenticatedConnection {
    session: Arc<Handle<Handler>>,
    agent_forwarding: bool,
    hops: Vec<Handle<Handler>>,
    proxy: Option<ProxyProcess>,
}

impl AuthenticatedConnection {
    /// Dials `spec` (via `ProxyJump`, `ProxyCommand`, or direct TCP), verifies host keys, and
    /// authenticates the target user.
    pub(crate) async fn open(
        spec: &ConnectSpec,
        options: ConnectOptions,
        events: &async_channel::Sender<SessionEvent>,
    ) -> Result<Self, Failure> {
        let agent_socket = authn::socket_for(
            options.allow_agent_forwarding && spec.forward_agent,
            spec.agent_socket.as_deref(),
        );
        let agent_forwarding = agent_socket.is_some();
        let asking = Asking::default();
        let handler = make_handler(
            &spec.host,
            spec.port,
            spec.known_hosts.as_deref(),
            events,
            options,
            agent_socket,
            &asking,
        );
        let cfg = client_config(spec, &spec.host, spec.port);

        tracing::info!(host = %spec.host, port = spec.port, user = %spec.user, "ssh_connecting");

        // Declared before `session` so if authentication fails, `session` drops before the
        // proxy child or jump hops carrying its stream.
        let (hops, proxy, mut session) = if !spec.proxy_jump.is_empty() {
            let chain = open_jump_chain(spec, events).await?;
            let connecting = client::connect_stream(cfg, chain.stream, handler);
            let session = finish_handshake(
                within(CONNECT_TIMEOUT, &asking, connecting).await,
                &spec.host,
                spec.port,
            )?;
            (chain.hops, None, session)
        } else if let Some(cmd) = spec.proxy_command.as_deref() {
            let cmd = config::expand_proxy_command(cmd, &spec.host, spec.port, &spec.user);
            tracing::debug!("ssh_proxy_command_start");
            let (mut proxy, stream) = ProxyProcess::spawn(&cmd)?;
            let connecting = client::connect_stream(cfg, stream, handler);
            let connected = within(CONNECT_TIMEOUT, &asking, connecting)
                .await
                .ok_or(Failure::ConnectTimeout)?;
            let session = match connected {
                Ok(s) => s,
                Err(e) => {
                    if is_transport_failure(&e)
                        && let Some(proxy_err) = proxy.exit_failure().await
                    {
                        return Err(proxy_err);
                    }
                    return Err(connect_failure(e, &spec.host, spec.port));
                }
            };
            (Vec::new(), Some(proxy), session)
        } else {
            let connecting = client::connect(cfg, (spec.host.as_str(), spec.port), handler);
            let session = finish_handshake(
                within(CONNECT_TIMEOUT, &asking, connecting).await,
                &spec.host,
                spec.port,
            )?;
            (Vec::new(), None, session)
        };

        Authenticator::new(
            &mut session,
            &spec.user,
            &spec.host,
            spec.identity_files.clone(),
            events,
        )
        .with_memory_keys(spec.memory_keys.clone())
        .run()
        .await?;

        Ok(Self {
            session: Arc::new(session),
            agent_forwarding,
            hops,
            proxy,
        })
    }

    pub(crate) fn session(&self) -> &Arc<Handle<Handler>> {
        &self.session
    }

    pub(crate) fn agent_forwarding(&self) -> bool {
        self.agent_forwarding
    }

    /// Sends an SSH disconnect to the target and any jump hops in reverse order, then shuts
    /// down any `ProxyCommand` child process.
    pub(crate) async fn disconnect(self) {
        let Self {
            session,
            mut hops,
            proxy,
            ..
        } = self;
        let _ = session
            .disconnect(SshDisconnect::ByApplication, "", "en")
            .await;
        drop(session);
        while let Some(hop) = hops.pop() {
            let _ = hop.disconnect(SshDisconnect::ByApplication, "", "en").await;
        }
        if let Some(proxy) = proxy {
            proxy.shutdown().await;
        }
    }
}

fn make_handler(
    host: &str,
    port: u16,
    known_hosts: Option<&Path>,
    events: &async_channel::Sender<SessionEvent>,
    options: ConnectOptions,
    agent_socket: Option<PathBuf>,
    asking: &Asking,
) -> Handler {
    Handler {
        host: host.to_owned(),
        port,
        known_hosts: known_hosts.map(Path::to_path_buf),
        events: events.clone(),
        cause: options.cause,
        remote: options.remote_targets,
        agent_socket,
        asking: asking.clone(),
    }
}

fn finish_handshake(
    result: Option<Result<Handle<Handler>, Failure>>,
    host: &str,
    port: u16,
) -> Result<Handle<Handler>, Failure> {
    result
        .ok_or(Failure::ConnectTimeout)?
        .map_err(|e| connect_failure(e, host, port))
}

/// Only transport/IO failures during a proxy handshake may be explained by the proxy exiting;
/// host-key refusals and UI cancellations must never be masked by a proxy exiting on EOF.
fn is_transport_failure(e: &Failure) -> bool {
    matches!(e, Failure::Io(_) | Failure::Transport(_))
}

/// A byte stream both ways; russh's channel stream type is not public.
trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// The logged-in jump hosts (kept alive by being held) and a stream to the final target.
struct Chain {
    hops: Vec<Handle<Handler>>,
    stream: Box<dyn Duplex>,
}

/// Logs in to every hop in order, then opens a channel from the last one to `spec.host:port`.
/// Host keys and credentials are checked per hop; prompts name the hop they are about.
async fn open_jump_chain(
    spec: &ConnectSpec,
    events: &async_channel::Sender<SessionEvent>,
) -> Result<Chain, Failure> {
    let mut hops: Vec<Handle<Handler>> = Vec::new();
    let mut last_label = String::new();
    for hop in &spec.proxy_jump {
        if hop.port == 0 {
            return Err(Failure::BadJump(hop.host.clone()));
        }
        let label = format!("{}:{}", hop.host, hop.port);
        let wrap = |f: Failure| Failure::Jump(label.clone(), Box::new(f));
        let asking = Asking::default();
        let handler = make_handler(
            &hop.host,
            hop.port,
            spec.known_hosts.as_deref(),
            events,
            ConnectOptions::default(),
            None,
            &asking,
        );
        let cfg = client_config(spec, &hop.host, hop.port);
        let connecting = async {
            match hops.last() {
                None => client::connect(cfg, (hop.host.as_str(), hop.port), handler).await,
                Some(prev) => {
                    let ch = open_channel(prev, &hop.host, hop.port).await?;
                    client::connect_stream(cfg, ch.into_stream(), handler).await
                }
            }
        };
        let mut session = finish_handshake(
            within(CONNECT_TIMEOUT, &asking, connecting).await,
            &hop.host,
            hop.port,
        )
        .map_err(wrap)?;
        let user = hop
            .user
            .clone()
            .or_else(config::local_user)
            .ok_or_else(|| wrap(Failure::Transport(Error::NoLocalUser.to_string())))?;
        Authenticator::new(
            &mut session,
            &user,
            &hop.host,
            hop.identity_files.clone(),
            events,
        )
        .run()
        .await
        .map_err(wrap)?;
        tracing::info!(host = %hop.host, port = hop.port, "ssh_jump_connected");
        hops.push(session);
        last_label = label;
    }
    let last = hops.last().ok_or(Failure::UiGone)?;
    let ch = open_channel(last, &spec.host, spec.port)
        .await
        .map_err(|f| Failure::Jump(last_label, Box::new(f)))?;
    Ok(Chain {
        hops,
        stream: Box::new(ch.into_stream()),
    })
}

async fn open_channel(
    from: &Handle<Handler>,
    host: &str,
    port: u16,
) -> Result<Channel<Msg>, Failure> {
    from.channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
        .await
        .map_err(|e| match e {
            russh::Error::ChannelOpenFailure(_) => {
                Failure::Transport(format!("it could not open a connection to {host}:{port}"))
            }
            other => other.into(),
        })
}

/// The proxy's first non-empty stderr line, which says why it failed when it does.
#[derive(Clone, Default)]
struct ProxyReason(Arc<std::sync::Mutex<Option<String>>>);

impl ProxyReason {
    fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }
}

/// Running `ProxyCommand` child and its stderr drain task, killed on drop or shutdown.
struct ProxyProcess {
    child: Child,
    reason: ProxyReason,
    stderr_task: JoinHandle<()>,
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

impl ProxyProcess {
    /// Spawns `cmd` through the platform shell; its stdin/stdout carry the SSH stream.
    fn spawn(
        cmd: &str,
    ) -> Result<(Self, impl AsyncRead + AsyncWrite + Unpin + Send + 'static), Failure> {
        let mut shell = proxy_shell(cmd);
        let mut child = shell
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Failure::Proxy(e.to_string()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Failure::Proxy("stdio unavailable".into()));
        };
        let reason = ProxyReason::default();
        let first = reason.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(proxy_stderr = %line, "ssh_proxy_stderr");
                // The first line is usually the error; what follows tends to be usage text.
                if let Ok(mut slot) = first.0.lock()
                    && slot.is_none()
                    && !line.trim().is_empty()
                {
                    *slot = Some(line.trim().to_owned());
                }
            }
        });
        Ok((
            Self {
                child,
                reason,
                stderr_task,
            },
            tokio::io::join(stdout, stdin),
        ))
    }

    /// A proxy that died during the handshake explains the failure better than russh's
    /// "Disconnected". Waits briefly for a just-exiting child and its stderr reader to settle.
    async fn exit_failure(&mut self) -> Option<Failure> {
        let status = match self.child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => tokio::time::timeout(PROXY_EXIT_GRACE, self.child.wait())
                .await
                .ok()?
                .ok()?,
            Err(_) => return None,
        };
        let _ = tokio::time::timeout(PROXY_EXIT_GRACE, &mut self.stderr_task).await;
        Some(Failure::Proxy(
            self.reason.get().unwrap_or_else(|| status.to_string()),
        ))
    }

    /// Gives the proxy a brief chance to exit cleanly once its SSH stream closes, then kills
    /// and reaps it if it lingers.
    async fn shutdown(mut self) {
        if tokio::time::timeout(PROXY_EXIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// The ProxyCommand wrapped in the platform shell: `sh -c` on Unix, `cmd /C` on Windows.
fn proxy_shell(cmd: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c").arg(cmd);
        shell
    }
    #[cfg(windows)]
    {
        /// `CREATE_NO_WINDOW`: no console window flashes up for a GUI app.
        const NO_WINDOW: u32 = 0x0800_0000;
        let mut shell = tokio::process::Command::new("cmd");
        // `raw_arg`: cmd parses its own command line, so Rust's quoting must not touch it.
        shell.arg("/C").raw_arg(cmd).creation_flags(NO_WINDOW);
        shell
    }
}

/// The russh settings for one connection: keep-alive from the spec, and the host key types
/// already in known_hosts for `host:port` asked for first.
fn client_config(spec: &ConnectSpec, host: &str, port: u16) -> Arc<client::Config> {
    let mut preferred = Preferred::default();
    let known = known_algorithms(host, port, spec.known_hosts.as_deref());
    if !known.is_empty() {
        let mut keys = known;
        let rest: Vec<_> = preferred
            .key
            .iter()
            .filter(|a| !keys.contains(a))
            .cloned()
            .collect();
        keys.extend(rest);
        preferred.key = Cow::Owned(keys);
    }
    Arc::new(client::Config {
        // OpenSSH: an interval of 0 turns keep-alives off; a count of 0 never gives up (russh
        // reads 0 the same way).
        keepalive_interval: Some(spec.server_alive_interval).filter(|d| !d.is_zero()),
        keepalive_max: spec.server_alive_count_max as usize,
        nodelay: true,
        channel_buffer_size: CHANNEL_BUFFER,
        preferred,
        ..Default::default()
    })
}

/// A connect error in words a person can act on, naming where tern tried to go.
fn connect_failure(e: Failure, host: &str, port: u16) -> Failure {
    match e {
        Failure::Io(io) => Failure::Unreachable(describe_io(&io, host, port)),
        other => other,
    }
}

fn describe_io(e: &std::io::Error, host: &str, port: u16) -> String {
    use std::io::ErrorKind as K;
    let text = e.to_string();
    match e.kind() {
        K::ConnectionRefused => {
            format!("{host}:{port} refused the connection (is an SSH server running there?)")
        }
        K::TimedOut => format!("{host}:{port} did not answer"),
        K::HostUnreachable | K::NetworkUnreachable => format!("no route to {host}"),
        K::ConnectionReset => format!("{host}:{port} closed the connection"),
        _ if text.contains("lookup address") || text.contains("nodename nor servname") => {
            format!("could not find host {host}")
        }
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{Error, ErrorKind};
    use std::time::Duration;

    use super::{
        CHANNEL_BUFFER, client_config, connect_failure, describe_io, is_transport_failure,
    };
    use crate::ConnectSpec;
    use crate::error::Failure;

    const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBIQTDbXKX4X4fQAXgrXaqurGfi9GnEVfOaM4DyG4bO5oB/P9sCFUTj2hMxJ0DHWaXF+gkcqcceqOio5b205RMEs=";

    #[test]
    fn common_failures_read_as_sentences_naming_the_target() {
        let refused = Error::from(ErrorKind::ConnectionRefused);
        assert_eq!(
            describe_io(&refused, "10.0.0.5", 2222),
            "10.0.0.5:2222 refused the connection (is an SSH server running there?)"
        );
        let timed_out = Error::from(ErrorKind::TimedOut);
        assert_eq!(
            describe_io(&timed_out, "10.0.0.5", 2222),
            "10.0.0.5:2222 did not answer"
        );
        let host_unreach = Error::from(ErrorKind::HostUnreachable);
        assert_eq!(
            describe_io(&host_unreach, "10.0.0.5", 2222),
            "no route to 10.0.0.5"
        );
        let net_unreach = Error::from(ErrorKind::NetworkUnreachable);
        assert_eq!(
            describe_io(&net_unreach, "10.0.0.5", 2222),
            "no route to 10.0.0.5"
        );
        let reset = Error::from(ErrorKind::ConnectionReset);
        assert_eq!(
            describe_io(&reset, "10.0.0.5", 2222),
            "10.0.0.5:2222 closed the connection"
        );
        let dns_nodename =
            Error::other("failed to lookup address information: nodename nor servname provided");
        assert_eq!(
            describe_io(&dns_nodename, "nope.example", 22),
            "could not find host nope.example"
        );
        let dns_lookup = Error::other("failed to lookup address information");
        assert_eq!(
            describe_io(&dns_lookup, "missing.example", 22),
            "could not find host missing.example"
        );
        let dns_servname = Error::other("nodename nor servname provided");
        assert_eq!(
            describe_io(&dns_servname, "servname.example", 22),
            "could not find host servname.example"
        );
        let odd = Error::other("something else");
        assert_eq!(describe_io(&odd, "h", 22), "something else");
    }

    #[test]
    fn connect_failure_turns_io_errors_into_unreachable_and_preserves_other_failures() {
        let io = connect_failure(Failure::Io(Error::from(ErrorKind::TimedOut)), "db", 2200);
        assert!(matches!(io, Failure::Unreachable(ref msg) if msg == "db:2200 did not answer"));

        let changed = connect_failure(Failure::HostKeyChanged { line: 4 }, "db", 2200);
        assert!(matches!(changed, Failure::HostKeyChanged { line: 4 }));
    }

    #[test]
    fn host_key_and_ui_failures_are_never_treated_as_transport_failures() {
        assert!(is_transport_failure(&Failure::Io(Error::from(
            ErrorKind::UnexpectedEof
        ))));
        assert!(is_transport_failure(&Failure::Transport("eof".into())));
        assert!(!is_transport_failure(&Failure::HostKeyChanged { line: 1 }));
        assert!(!is_transport_failure(&Failure::HostKeyRejected));
        assert!(!is_transport_failure(&Failure::HostKeyUnsupported));
        assert!(!is_transport_failure(&Failure::HostKeyCheck("err".into())));
        assert!(!is_transport_failure(&Failure::UiGone));
        assert!(!is_transport_failure(&Failure::AuthFailed));
    }

    #[test]
    fn client_config_prefers_known_host_algorithms_without_duplicates_and_applies_keepalive() {
        let dir = tempfile::tempdir().unwrap();
        let known_hosts = dir.path().join("known_hosts");
        std::fs::write(&known_hosts, format!("[box]:2222 {ECDSA}\n")).unwrap();

        let spec = ConnectSpec {
            host: "box".into(),
            port: 2222,
            known_hosts: Some(known_hosts),
            server_alive_interval: Duration::from_secs(12),
            server_alive_count_max: 5,
            ..Default::default()
        };
        let cfg = client_config(&spec, "box", 2222);
        assert_eq!(cfg.preferred.key[0].to_string(), "ecdsa-sha2-nistp256");
        let ecdsa_count = cfg
            .preferred
            .key
            .iter()
            .filter(|a| a.to_string() == "ecdsa-sha2-nistp256")
            .count();
        assert_eq!(ecdsa_count, 1);
        assert!(cfg.preferred.key.len() > 1);
        assert_eq!(cfg.keepalive_interval, Some(Duration::from_secs(12)));
        assert_eq!(cfg.keepalive_max, 5);
        assert!(cfg.nodelay);
        assert_eq!(cfg.channel_buffer_size, CHANNEL_BUFFER);

        // Zero interval disables keep-alives; unlisted host keeps default algorithm preference.
        let mut quiet = spec;
        quiet.server_alive_interval = Duration::ZERO;
        let quiet_cfg = client_config(&quiet, "unknown-host", 22);
        assert_eq!(quiet_cfg.keepalive_interval, None);
        assert_eq!(
            quiet_cfg.preferred.key.as_ref(),
            russh::Preferred::default().key.as_ref()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_proxy_process_aborts_stderr_reader_even_if_stderr_stays_open() {
        let (proxy, _stream) = super::ProxyProcess::spawn("(sleep 2 &); exec sleep 2").unwrap();
        let abort_handle = proxy.stderr_task.abort_handle();
        // Let the shell spawn the background holder of stderr.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!abort_handle.is_finished());

        drop(proxy);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            abort_handle.is_finished(),
            "stderr reader task should be aborted immediately on drop"
        );
    }
}
