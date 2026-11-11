//! SSH client sessions for tern.
//!
//! Events are delivered over an `async-channel` and prompts are answered through
//! `futures::channel::oneshot` senders, so the consumer (GPUI) does not need a tokio
//! context; the session itself runs on the tokio runtime handle passed to [`connect`].
//!
//! A host with a `ProxyCommand` in `~/.ssh/config` is reached through that command, as
//! OpenSSH does; it is resolved once, with the rest of the alias, into [`ConnectSpec`].

mod authn;
mod config;
mod error;
mod files;
mod files_local;
mod files_sftp;
mod forward;
mod hostkey;
mod session;
mod sftp;
mod socks;
mod sshconf;
mod transfer;
mod transfer_plan;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

pub use config::load_ssh_config_hosts;
pub use error::{Disconnect, Error, ForwardError, InputError, Result};
pub use files::{
    Call, FileEntry, FileError, FileSource, LocalFs, PART_SUFFIX, Reader, SftpFiles, Writer,
    file_name, join, keep_both, parent,
};
pub use forward::{Forward, ForwardHandle, ForwardInfo};
pub use futures::channel::oneshot;
pub use secrecy::{self, ExposeSecret, SecretString};
pub use sftp::{Sftp, SftpError};
pub use transfer::{
    Conflict, CopyJob, Event, MAX_RUNNING, Progress as TransferProgress, Question, Summary,
    TransferQueue,
};

/// Keep-alive probe interval when the host sets no `ServerAliveInterval`.
pub const DEFAULT_SERVER_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Unanswered keep-alives before the connection is dropped, when the host sets no
/// `ServerAliveCountMax` (OpenSSH's default too).
pub const DEFAULT_SERVER_ALIVE_COUNT_MAX: u32 = 3;

/// One hop of a `ProxyJump` chain, already resolved through `~/.ssh/config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JumpHop {
    pub host: String,
    /// `0` marks an entry that could not be read; connecting through it fails by name.
    pub port: u16,
    /// `None` means the local user name, as in OpenSSH.
    pub user: Option<String>,
    pub identity_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEntry {
    pub alias: String,
    pub host_name: String,
    pub port: u16,
    pub user: Option<String>,
    pub identity_files: Vec<PathBuf>,
    pub proxy_command: Option<String>,
    /// `ProxyJump` hops in connection order; empty when none, or when `ProxyCommand` came first.
    pub proxy_jump: Vec<JumpHop>,
    /// `LocalForward`, `RemoteForward` and `DynamicForward` lines, in file order. Start each
    /// with [`SessionHandle::start_forward`] once the session is connected.
    pub forwards: Vec<Forward>,
    /// `ForwardAgent` is on.
    pub forward_agent: bool,
    /// The agent socket `ForwardAgent` named; `None` means `$SSH_AUTH_SOCK`.
    pub agent_socket: Option<PathBuf>,
    /// `ServerAliveInterval`; `None` when the host does not set it, `Some(ZERO)` when it
    /// turns keep-alives off.
    pub server_alive_interval: Option<Duration>,
    /// `ServerAliveCountMax`; `None` when the host does not set it.
    pub server_alive_count_max: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<PathBuf>,
    /// Run as the transport instead of dialling `host:port`; `%h %p %r` are expanded.
    pub proxy_command: Option<String>,
    /// Jump hosts to pass through, in order, each checked and logged in to like the target.
    /// Used instead of `proxy_command` when both are set. A hop's own `ProxyJump` is not
    /// followed; list every hop here.
    pub proxy_jump: Vec<JumpHop>,
    /// Forwards the host's config asks for; the session does not start them by itself.
    pub forwards: Vec<Forward>,
    /// Let the server use the local ssh-agent for the session (`ssh -A`). Off by default: a
    /// server you forward to can ask your agent to sign while you are connected.
    pub forward_agent: bool,
    /// The agent socket to forward; `None` means `$SSH_AUTH_SOCK`.
    pub agent_socket: Option<PathBuf>,
    /// known_hosts file to check and learn host keys in; `None` means `~/.ssh/known_hosts`.
    pub known_hosts: Option<PathBuf>,
    /// Private keys held in memory (tern's vault), offered after the agent and before the key
    /// files. Never written to disk or logged.
    pub memory_keys: Vec<MemoryKey>,
    /// Silence before a keep-alive is sent; `Duration::ZERO` turns keep-alives off, as
    /// `ServerAliveInterval 0` does in OpenSSH.
    pub server_alive_interval: Duration,
    /// Keep-alives that may go unanswered before the session ends as [`Disconnect::Timeout`];
    /// `0` never gives up.
    pub server_alive_count_max: u32,
}

impl Default for ConnectSpec {
    /// An empty target with today's keep-alive settings; fill in the host, port and user.
    fn default() -> Self {
        ConnectSpec {
            host: String::new(),
            port: 22,
            user: String::new(),
            identity_files: Vec::new(),
            proxy_command: None,
            proxy_jump: Vec::new(),
            forwards: Vec::new(),
            forward_agent: false,
            agent_socket: None,
            known_hosts: None,
            memory_keys: Vec::new(),
            server_alive_interval: DEFAULT_SERVER_ALIVE_INTERVAL,
            server_alive_count_max: DEFAULT_SERVER_ALIVE_COUNT_MAX,
        }
    }
}

/// An unencrypted OpenSSH private key held in memory, with a label for logs.
#[derive(Debug, Clone)]
pub struct MemoryKey {
    pub name: String,
    pub openssh: SecretString,
}

impl PartialEq for MemoryKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.openssh.expose_secret() == other.openssh.expose_secret()
    }
}

impl Eq for MemoryKey {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// A question for the user. Dropping the reply sender counts as cancelling.
/// Secrets are [`SecretString`] (zeroized on drop, redacted in `Debug`).
/// One question in a [`Prompt::Challenge`]; `echo` is false for secrets such as codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengePrompt {
    pub text: String,
    pub echo: bool,
}

#[derive(Debug)]
pub enum Prompt {
    Password {
        user: String,
        host: String,
        reply: oneshot::Sender<Option<SecretString>>,
    },
    KeyPassphrase {
        path: PathBuf,
        reply: oneshot::Sender<Option<SecretString>>,
    },
    /// A keyboard-interactive round (RFC 4256), shown as the server wrote it. Reply with one
    /// answer per prompt, in order.
    Challenge {
        name: String,
        instructions: String,
        prompts: Vec<ChallengePrompt>,
        reply: oneshot::Sender<Option<Vec<SecretString>>>,
    },
    UnknownHostKey {
        host: String,
        port: u16,
        algorithm: String,
        /// e.g. `SHA256:abc...`
        fingerprint_sha256: String,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Debug)]
pub enum SessionEvent {
    Connected,
    Data(Vec<u8>),
    /// Smoothed round-trip time to the server, about every 5 s once logged in.
    Latency(std::time::Duration),
    Prompt(Prompt),
    Closed {
        exit_status: Option<u32>,
        error: Option<String>,
        /// Why it ended; reconnect only when [`Disconnect::is_network`] is true.
        reason: Disconnect,
    },
}

/// Commands queued from the UI to one session. Keystrokes arrive at human speed and a paste
/// is one command, so a short queue only fills when the remote has stopped reading.
const COMMAND_QUEUE: usize = 64;

/// Sends input to one session. Cheap to clone; every clone drives the same session.
#[derive(Debug, Clone)]
#[must_use = "dropping every SessionHandle closes the session"]
pub struct SessionHandle {
    tx: mpsc::Sender<session::Command>,
    forwards: Arc<forward::Registry>,
}

impl SessionHandle {
    /// Queues bytes for the remote (keystrokes, paste, terminal replies).
    ///
    /// # Errors
    ///
    /// [`InputError::Busy`] when the remote has stopped reading and the queue is full;
    /// [`InputError::Closed`] once the session has ended.
    pub fn write(&self, bytes: Vec<u8>) -> std::result::Result<(), InputError> {
        self.send(session::Command::Write(bytes))
    }

    /// Tells the remote the terminal changed size (`window-change`).
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`].
    pub fn resize(&self, size: TermSize) -> std::result::Result<(), InputError> {
        self.send(session::Command::Resize(size))
    }

    /// Starts a port forward on this session. Local and dynamic forwards listen on loopback
    /// unless the [`Forward`] names another address.
    ///
    /// # Errors
    ///
    /// [`ForwardError::Listen`] when this machine cannot listen there, [`ForwardError::Refused`]
    /// when the server will not (remote forwards), [`ForwardError::Closed`] once the session
    /// has ended.
    pub async fn start_forward(
        &self,
        forward: Forward,
    ) -> std::result::Result<ForwardHandle, ForwardError> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(session::Command::Forward { forward, reply })
            .await
            .map_err(|_| ForwardError::Closed)?;
        let info = answer.await.map_err(|_| ForwardError::Closed)??;
        Ok(ForwardHandle {
            info,
            registry: self.forwards.clone(),
            cmds: self.tx.clone(),
        })
    }

    /// Opens SFTP on this session, on a channel of its own beside the shell.
    ///
    /// # Errors
    ///
    /// [`SftpError::NotOffered`] when the server has no SFTP service, [`SftpError::Closed`] once
    /// the session has ended.
    pub async fn open_sftp(&self) -> std::result::Result<Sftp, SftpError> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(session::Command::Sftp { reply })
            .await
            .map_err(|_| SftpError::Closed)?;
        answer.await.map_err(|_| SftpError::Closed)?
    }

    /// The forwards running on this session.
    #[must_use]
    pub fn forwards(&self) -> Vec<ForwardInfo> {
        self.forwards.list()
    }

    /// Asks the session to end; a [`SessionEvent::Closed`] follows.
    pub fn close(&self) {
        // A full or closed queue both mean the session is already on its way out.
        let _ = self.tx.try_send(session::Command::Close);
    }

    fn send(&self, cmd: session::Command) -> std::result::Result<(), InputError> {
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => InputError::Busy,
            mpsc::error::TrySendError::Closed(_) => InputError::Closed,
        })
    }
}

/// Starts a session on `rt`. The first event is a [`Prompt`] or [`SessionEvent::Connected`];
/// the last is always [`SessionEvent::Closed`].
///
/// Memory per session is bounded: see `session.rs` and `docs/perf.md`.
#[must_use = "the session runs only while its events are received"]
pub fn connect(
    spec: ConnectSpec,
    size: TermSize,
    rt: &tokio::runtime::Handle,
) -> (SessionHandle, async_channel::Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
    let forwards = Arc::new(forward::Registry::default());
    let (etx, erx) = async_channel::bounded(session::EVENT_QUEUE);
    rt.spawn(session::run(spec, Some(size), rx, etx, forwards.clone()));
    (SessionHandle { tx, forwards }, erx)
}

/// Starts a connection with no shell, no pty and no agent forwarding, for work that only needs
/// the connection ([`SessionHandle::open_sftp`]). Prompts, [`SessionEvent::Connected`] and
/// [`SessionEvent::Closed`] come as for [`connect`]; it never sends [`SessionEvent::Data`].
#[must_use = "the connection runs only while its events are received"]
pub fn connect_files(
    spec: ConnectSpec,
    rt: &tokio::runtime::Handle,
) -> (SessionHandle, async_channel::Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
    let forwards = Arc::new(forward::Registry::default());
    let (etx, erx) = async_channel::bounded(session::EVENT_QUEUE);
    rt.spawn(session::run(spec, None, rx, etx, forwards.clone()));
    (SessionHandle { tx, forwards }, erx)
}
