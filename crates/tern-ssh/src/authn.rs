// Adapted from russh russh/examples/client_exec_interactive.rs and russh/src/client/mod.rs
// (Apache-2.0) for the publickey / keyboard-interactive / password calls, and from
// CrabPort crabport-ssh/src/backend.rs (Apache-2.0) for the overall auth ordering.
// Adapted from russh russh/examples/client_exec_interactive.rs (Apache-2.0) for the
// `agent_forward` request; the channel proxy follows `ssh(1)` agent forwarding (PROTOCOL.agent).
// Adapted from russh russh/examples/client_open_direct_tcpip.rs (Apache-2.0): direct-tcpip
// channel, then a handshake over its stream, repeated once per hop.
//! Agent forwarding: the server opens `auth-agent@openssh.com` channels, and each one is wired to
//! the local agent socket, so the remote side can sign with keys that never leave this machine.
//!
//! ProxyJump: each hop is a full SSH login, and the next connection runs inside a direct-tcpip
//! channel of the one before, as `ssh -J` does (russh `channel_open_direct_tcpip`, then a new
//! handshake over `Channel::into_stream`, after russh `examples/client_open_direct_tcpip.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::channel::oneshot;
use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use russh::{MethodKind, MethodSet};
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Cause, Error, Failure};
use crate::hostkey::{Asking, Handler, within};
use crate::{ChallengePrompt, ConnectSpec, MemoryKey, Prompt, SessionEvent};
use std::ffi::OsStr;

use russh::Channel;
use russh::client::Handle;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config;
use crate::forward::RemoteTargets;
use crate::session::{CONNECT_TIMEOUT, client_config, connect_failure};

const MAX_TRIES: usize = 3;

pub(crate) struct Authenticator<'a> {
    pub session: &'a mut client::Handle<Handler>,
    pub user: &'a str,
    pub host: &'a str,
    pub identity_files: Vec<PathBuf>,
    pub events: &'a async_channel::Sender<SessionEvent>,
    memory_keys: Vec<MemoryKey>,
    remaining: MethodSet,
    /// The last failure said one step of a multi-step login passed (`partial_success`).
    progressed: bool,
}

impl<'a> Authenticator<'a> {
    pub fn new(
        session: &'a mut client::Handle<Handler>,
        user: &'a str,
        host: &'a str,
        identity_files: Vec<PathBuf>,
        events: &'a async_channel::Sender<SessionEvent>,
    ) -> Self {
        Self {
            session,
            user,
            host,
            identity_files,
            events,
            memory_keys: Vec::new(),
            remaining: MethodSet::client_supported(),
            progressed: false,
        }
    }

    /// Keys held in memory (the vault's), tried after the agent and before key files.
    pub fn with_memory_keys(mut self, keys: Vec<MemoryKey>) -> Self {
        self.memory_keys = keys;
        self
    }

    fn allows(&self, m: MethodKind) -> bool {
        self.remaining.contains(&m)
    }

    /// Folds an auth result into server-advertised state. Returns true on success.
    fn absorb(&mut self, r: AuthResult) -> bool {
        match r {
            AuthResult::Success => true,
            AuthResult::Failure {
                remaining_methods,
                partial_success,
            } => {
                self.remaining = remaining_methods;
                self.progressed = partial_success;
                false
            }
        }
    }

    pub async fn run(&mut self) -> Result<(), Failure> {
        let none = self.session.authenticate_none(self.user).await?;
        if self.absorb(none) {
            tracing::info!(host = %self.host, auth_method = "none", "ssh_authenticated");
            return Ok(());
        }
        if self.allows(MethodKind::PublicKey) {
            if self.try_agent().await? {
                tracing::info!(host = %self.host, auth_method = "agent", "ssh_authenticated");
                return Ok(());
            }
            if self.try_memory_keys().await? {
                tracing::info!(host = %self.host, auth_method = "vault_key", "ssh_authenticated");
                return Ok(());
            }
            if self.try_identity_files().await? {
                tracing::info!(host = %self.host, auth_method = "publickey", "ssh_authenticated");
                return Ok(());
            }
        }
        if self.try_interactive_or_password().await? {
            return Ok(());
        }
        Err(Failure::AuthFailed)
    }

    /// The local agent: `$SSH_AUTH_SOCK` on Unix, the OpenSSH agent's named pipe on Windows.
    async fn try_agent(&mut self) -> Result<bool, Failure> {
        #[cfg(unix)]
        let connected = AgentClient::connect_env().await;
        #[cfg(windows)]
        let connected = AgentClient::connect_named_pipe(WINDOWS_AGENT_PIPE).await;
        match connected {
            Ok(agent) => self.sign_with_agent(agent).await,
            Err(e) => {
                tracing::debug!(error = %e, "ssh_agent_unavailable");
                Ok(false)
            }
        }
    }

    async fn sign_with_agent<S>(&mut self, mut agent: AgentClient<S>) -> Result<bool, Failure>
    where
        S: russh::keys::agent::client::AgentStream + Unpin + Send + 'static,
    {
        let identities = match agent.request_identities().await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "ssh_agent_list_failed");
                return Ok(false);
            }
        };
        tracing::debug!(identity_count = identities.len(), "ssh_agent_identities");
        let rsa_hash = self.session.best_supported_rsa_hash().await?.flatten();
        for id in identities {
            // Certificates in the agent are not supported yet; plain keys only.
            let AgentIdentity::PublicKey { key, .. } = id else {
                continue;
            };
            if !self.allows(MethodKind::PublicKey) {
                break;
            }
            let hash = if key.algorithm().is_rsa() {
                rsa_hash
            } else {
                None
            };
            match self
                .session
                .authenticate_publickey_with(self.user, key, hash, &mut agent)
                .await
            {
                Ok(r) => {
                    if self.absorb(r) {
                        return Ok(true);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "ssh_agent_sign_failed"),
            }
        }
        Ok(false)
    }

    /// Offers each in-memory key; an unreadable one is skipped, never printed.
    async fn try_memory_keys(&mut self) -> Result<bool, Failure> {
        if self.memory_keys.is_empty() {
            return Ok(false);
        }
        let rsa_hash = self.session.best_supported_rsa_hash().await?.flatten();
        for mk in self.memory_keys.clone() {
            if !self.allows(MethodKind::PublicKey) {
                break;
            }
            let key = match russh::keys::decode_secret_key(mk.openssh.expose_secret(), None) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(key = %mk.name, error = %e, "ssh_vault_key_unreadable");
                    continue;
                }
            };
            let hash = if key.algorithm().is_rsa() {
                rsa_hash
            } else {
                None
            };
            let r = self
                .session
                .authenticate_publickey(self.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?;
            if self.absorb(r) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn try_identity_files(&mut self) -> Result<bool, Failure> {
        let files = if self.identity_files.is_empty() {
            crate::config::default_identity_files()
        } else {
            self.identity_files.clone()
        };
        let rsa_hash = self.session.best_supported_rsa_hash().await?.flatten();
        for path in files {
            if !self.allows(MethodKind::PublicKey) {
                break;
            }
            let Some(key) = self.load_key(&path).await? else {
                continue;
            };
            let hash = if key.algorithm().is_rsa() {
                rsa_hash
            } else {
                None
            };
            let r = self
                .session
                .authenticate_publickey(self.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?;
            if self.absorb(r) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Loads a private key, asking for its passphrase (up to 3 times) if it is encrypted.
    /// `None` means "skip this key".
    async fn load_key(&self, path: &Path) -> Result<Option<russh::keys::PrivateKey>, Failure> {
        match load_secret_key(path, None) {
            Ok(k) => return Ok(Some(k)),
            Err(russh::keys::Error::KeyIsEncrypted) => {}
            Err(russh::keys::Error::IO(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "ssh_identity_missing");
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ssh_identity_unreadable");
                return Ok(None);
            }
        }
        for _ in 0..MAX_TRIES {
            let reply = ask(self.events, |reply| Prompt::KeyPassphrase {
                path: path.to_path_buf(),
                reply,
            })
            .await?;
            let Some(pass) = reply else {
                return Ok(None);
            };
            match load_secret_key(path, Some(pass.expose_secret())) {
                Ok(k) => return Ok(Some(k)),
                Err(_) => tracing::debug!(path = %path.display(), "ssh_passphrase_rejected"),
            }
        }
        Ok(None)
    }

    async fn try_interactive_or_password(&mut self) -> Result<bool, Failure> {
        let mut kbd = self.allows(MethodKind::KeyboardInteractive);
        let mut tries = 0;
        while tries < MAX_TRIES {
            if kbd {
                match self.keyboard_interactive().await? {
                    Kbd::Success => {
                        tracing::info!(host = %self.host, auth_method = "keyboard_interactive", "ssh_authenticated");
                        return Ok(true);
                    }
                    Kbd::Rejected { prompted } => {
                        if !prompted || !self.allows(MethodKind::KeyboardInteractive) {
                            kbd = false;
                        } else {
                            tries += 1;
                        }
                    }
                }
            } else if self.allows(MethodKind::Password) {
                let Some(pass) = self.ask_password().await? else {
                    return Err(Failure::AuthCancelled);
                };
                let r = self
                    .session
                    .authenticate_password(self.user, pass.expose_secret())
                    .await?;
                if self.absorb(r) {
                    tracing::info!(host = %self.host, auth_method = "password", "ssh_authenticated");
                    return Ok(true);
                }
                if self.progressed {
                    // The password was right and the server wants another step (a code, as
                    // `AuthenticationMethods password,keyboard-interactive` does): that is not a
                    // wrong try, and the next step is the one it now offers.
                    kbd = self.allows(MethodKind::KeyboardInteractive);
                } else {
                    tries += 1;
                }
            } else {
                break;
            }
        }
        Ok(false)
    }

    async fn keyboard_interactive(&mut self) -> Result<Kbd, Failure> {
        let mut prompted = false;
        let mut resp = self
            .session
            .authenticate_keyboard_interactive_start(self.user, None)
            .await?;
        loop {
            match resp {
                KeyboardInteractiveAuthResponse::Success => return Ok(Kbd::Success),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods,
                    partial_success,
                } => {
                    self.remaining = remaining_methods;
                    self.progressed = partial_success;
                    return Ok(Kbd::Rejected { prompted });
                }
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    // A round with no prompts still needs an (empty) response.
                    let answers = if prompts.is_empty() {
                        Vec::new()
                    } else {
                        prompted = true;
                        let Some(answers) = self.ask_challenge(name, instructions, prompts).await?
                        else {
                            return Err(Failure::AuthCancelled);
                        };
                        answers
                    };
                    resp = self
                        .session
                        .authenticate_keyboard_interactive_respond(answers)
                        .await?;
                }
            }
        }
    }

    async fn ask_challenge(
        &self,
        name: String,
        instructions: String,
        prompts: Vec<russh::client::Prompt>,
    ) -> Result<Option<Vec<String>>, Failure> {
        let expected = prompts.len();
        let prompts = prompts
            .into_iter()
            .map(|p| ChallengePrompt {
                text: p.prompt,
                echo: p.echo,
            })
            .collect();
        let (tx, rx) = oneshot::channel();
        self.events
            .send(SessionEvent::Prompt(Prompt::Challenge {
                name,
                instructions,
                prompts,
                reply: tx,
            }))
            .await
            .map_err(|_| Failure::UiGone)?;
        Ok(rx
            .await
            .ok()
            .flatten()
            .filter(|a| a.len() == expected)
            .map(|a| a.iter().map(|s| s.expose_secret().to_string()).collect()))
    }

    async fn ask_password(&self) -> Result<Option<SecretString>, Failure> {
        ask(self.events, |reply| Prompt::Password {
            user: self.user.to_string(),
            host: self.host.to_string(),
            reply,
        })
        .await
    }
}

enum Kbd {
    Success,
    Rejected { prompted: bool },
}

/// Sends a prompt to the UI and awaits its reply. A dropped reply sender counts as "cancelled".
async fn ask(
    events: &async_channel::Sender<SessionEvent>,
    make: impl FnOnce(oneshot::Sender<Option<SecretString>>) -> Prompt,
) -> Result<Option<SecretString>, Failure> {
    let (tx, rx) = oneshot::channel();
    events
        .send(SessionEvent::Prompt(make(tx)))
        .await
        .map_err(|_| Failure::UiGone)?;
    Ok(rx.await.unwrap_or(None))
}

/// Where the Windows OpenSSH agent service listens.
#[cfg(windows)]
pub(crate) const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// The agent socket to forward for a spec that asks for forwarding: its own path, else
/// `$SSH_AUTH_SOCK`.
pub(crate) fn socket_for(wanted: bool, explicit: Option<&Path>) -> Option<PathBuf> {
    if !wanted {
        return None;
    }
    let found = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty());
    // Windows OpenSSH's agent has no environment variable: it is always on this pipe.
    #[cfg(windows)]
    let found = found.or_else(|| Some(PathBuf::from(WINDOWS_AGENT_PIPE)));
    if found.is_none() {
        tracing::warn!("ssh_agent_forward_skipped: no SSH_AUTH_SOCK");
    }
    found
}

/// Reads `ForwardAgent`: `yes` uses `$SSH_AUTH_SOCK`, `no` is off, an absolute path or `~/` path
/// names the socket, and `$NAME` / `${NAME}` takes it from that variable.
pub(crate) fn parse_forward_agent(value: &str) -> (bool, Option<PathBuf>) {
    let v = value.trim();
    match v.to_ascii_lowercase().as_str() {
        "yes" | "true" => return (true, None),
        "no" | "false" | "" => return (false, None),
        _ => {}
    }
    if let Some(name) = v.strip_prefix('$') {
        let name = name.trim_start_matches('{').trim_end_matches('}');
        return match std::env::var_os(OsStr::new(name)) {
            Some(p) if !p.is_empty() => (true, Some(PathBuf::from(p))),
            _ => (false, None),
        };
    }
    if let Some(rest) = v.strip_prefix("~/") {
        return match crate::config::home_dir() {
            Some(h) => (true, Some(h.join(rest))),
            None => (false, None),
        };
    }
    if v.starts_with('/') {
        return (true, Some(PathBuf::from(v)));
    }
    (false, None)
}

/// Copies between a server-opened agent channel and the local agent until either ends.
#[cfg(unix)]
pub(crate) async fn bridge(channel: Channel<Msg>, socket: PathBuf) {
    match tokio::net::UnixStream::connect(&socket).await {
        Ok(mut agent) => {
            let mut ch = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut agent, &mut ch).await;
        }
        Err(e) => {
            tracing::debug!(socket = %socket.display(), error = %e, "ssh_agent_connect_failed");
            // Tell the server now, rather than leaving it waiting on a channel nobody serves.
            let _ = channel.close().await;
        }
    }
}

/// The same, over a Windows named pipe (the OpenSSH agent service).
#[cfg(windows)]
pub(crate) async fn bridge(channel: Channel<Msg>, socket: PathBuf) {
    use tokio::net::windows::named_pipe::ClientOptions;
    match ClientOptions::new().open(&socket) {
        Ok(mut agent) => {
            let mut ch = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut agent, &mut ch).await;
        }
        Err(e) => {
            tracing::debug!(pipe = %socket.display(), error = %e, "ssh_agent_connect_failed");
            let _ = channel.close().await;
        }
    }
}

/// A byte stream both ways; russh's channel stream type is not public.
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// The logged-in jump hosts (kept alive by being held) and a stream to the final target.
pub(crate) struct Chain {
    pub hops: Vec<Handle<Handler>>,
    pub stream: Box<dyn Duplex>,
}

/// Logs in to every hop in order, then opens a channel from the last one to `spec.host:port`.
/// Host keys and credentials are checked per hop; prompts name the hop they are about.
pub(crate) async fn open_jump_chain(
    spec: &ConnectSpec,
    events: &async_channel::Sender<SessionEvent>,
) -> Result<Chain, Failure> {
    let mut hops: Vec<Handle<Handler>> = Vec::new();
    for hop in &spec.proxy_jump {
        if hop.port == 0 {
            return Err(Failure::BadJump(hop.host.clone()));
        }
        let label = format!("{}:{}", hop.host, hop.port);
        let wrap = |f: Failure| Failure::Jump(label.clone(), Box::new(f));
        let asking = Asking::default();
        let handler = Handler {
            host: hop.host.clone(),
            port: hop.port,
            known_hosts: spec.known_hosts.clone(),
            events: events.clone(),
            cause: Cause::default(),
            remote: RemoteTargets::default(),
            agent_socket: None,
            asking: asking.clone(),
        };
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
        let mut session = within(CONNECT_TIMEOUT, &asking, connecting)
            .await
            .ok_or_else(|| wrap(Failure::ConnectTimeout))?
            .map_err(|e| wrap(connect_failure(e, &hop.host, hop.port)))?;
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
    }
    let last = hops.last().ok_or(Failure::UiGone)?;
    let label = format!("{}:{}", spec.proxy_jump.last().map_or("", |h| &h.host), {
        spec.proxy_jump.last().map_or(0, |h| h.port)
    });
    let ch = open_channel(last, &spec.host, spec.port)
        .await
        .map_err(|f| Failure::Jump(label, Box::new(f)))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_agent_values() {
        assert_eq!(parse_forward_agent("yes"), (true, None));
        assert_eq!(parse_forward_agent("YES"), (true, None));
        assert_eq!(parse_forward_agent("no"), (false, None));
        assert_eq!(
            parse_forward_agent("/run/agent.sock"),
            (true, Some(PathBuf::from("/run/agent.sock")))
        );
        assert_eq!(parse_forward_agent("$TERN_SURELY_UNSET_VAR"), (false, None));
        assert_eq!(parse_forward_agent("maybe"), (false, None));
    }

    #[test]
    fn no_forwarding_means_no_socket() {
        assert_eq!(socket_for(false, Some(Path::new("/x"))), None);
        assert_eq!(
            socket_for(true, Some(Path::new("/x"))),
            Some(PathBuf::from("/x"))
        );
    }
}
