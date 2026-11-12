//! The gist side: find or create the private `tern-sync` gist, read and write its files.
//! Blocking HTTP (ureq with the system TLS); callers run it off the UI thread.
// Test builds swap in fakes at `current_*`, so the real adapters are never built there.
#![cfg_attr(test, allow(dead_code))]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const API: &str = "https://api.github.com";
const DESCRIPTION: &str = "tern-sync";
const META: &str = "tern-meta.json";
const VAULT: &str = "vault.age.b64";
const DATA: &str = "data.age.b64";
/// The most of a gist response that is read; two base64 files of a few KiB come to far less.
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;
const KEYCHAIN_SERVICE: &str = "tern";
const KEYCHAIN_ACCOUNT: &str = "github-token";

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("no GitHub token: paste one in Settings → Sync, or sign in with `gh auth login`")]
    NoToken,
    #[error("GitHub refused the token (it needs the gist scope)")]
    Unauthorized,
    #[error("GitHub: {0}")]
    Http(String),
    #[error("the tern-sync gist is not in the expected format: {0}")]
    Format(String),
    #[error("Keychain: {0}")]
    Keychain(String),
}

impl From<ureq::Error> for GithubError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::StatusCode(401 | 403) => GithubError::Unauthorized,
            other => GithubError::Http(other.to_string()),
        }
    }
}

/// Where the token came from, for the Settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Keychain,
    GhCli,
}

pub(crate) trait Keychain: Send + Sync {
    fn get(&self) -> Result<String, keyring::Error>;
    fn set(&self, token: &str) -> Result<(), keyring::Error>;
    fn delete(&self) -> Result<(), keyring::Error>;
    fn clone_box(&self) -> Box<dyn Keychain>;
}

impl Clone for Box<dyn Keychain> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct OsKeychain;

impl Keychain for OsKeychain {
    fn get(&self) -> Result<String, keyring::Error> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?.get_password()
    }

    fn set(&self, token: &str) -> Result<(), keyring::Error> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?.set_password(token)
    }

    fn delete(&self) -> Result<(), keyring::Error> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?.delete_credential()
    }

    fn clone_box(&self) -> Box<dyn Keychain> {
        Box::new(self.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GhOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) trait GhCli: Send + Sync {
    fn run(&self, args: &[&str]) -> Result<GhOutput, std::io::Error>;
    fn clone_box(&self) -> Box<dyn GhCli>;
}

impl Clone for Box<dyn GhCli> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SystemGh;

// Runs on a background thread, never on an executor, so the blocking call is fine here.
#[allow(clippy::disallowed_methods)]
impl GhCli for SystemGh {
    fn run(&self, args: &[&str]) -> Result<GhOutput, std::io::Error> {
        let out = std::process::Command::new("gh").args(args).output()?;
        Ok(GhOutput {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    fn clone_box(&self) -> Box<dyn GhCli> {
        Box::new(self.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Post,
    Patch,
}

/// The request `send` makes: the verb, the GitHub headers and the JSON body. Built apart from
/// the agent so a test can read it. `send_json` set the Content-Type itself; this must too.
fn write_request(
    token: &str,
    method: Method,
    url: &str,
    body: &Value,
) -> Result<ureq::http::Request<ureq::SendBody<'static>>, GithubError> {
    ureq::http::Request::builder()
        .method(verb(method))
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "tern")
        .header("Content-Type", "application/json; charset=utf-8")
        .body(ureq::SendBody::from_json(body)?)
        .map_err(|e| GithubError::Http(e.to_string()))
}

/// The HTTP verb for a [`Method`]; ureq 3 has no string-verb request, so `send` feeds this to `http::Request`.
fn verb(method: Method) -> &'static str {
    match method {
        Method::Post => "POST",
        Method::Patch => "PATCH",
    }
}

pub(crate) trait GithubHttp: std::fmt::Debug + Send + Sync {
    fn get(&self, token: &str, path: &str) -> Result<Value, GithubError>;
    fn send(&self, token: &str, method: Method, path: &str, body: Value)
    -> Result<(), GithubError>;
    fn clone_box(&self) -> Box<dyn GithubHttp>;
}

impl Clone for Box<dyn GithubHttp> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub(crate) fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    // The macOS trust store, not a bundled root list.
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .http_status_as_error(true)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

/// Parses the body as JSON, failing once it reaches `cap` bytes instead of buffering all of it.
fn read_json_capped(body: &mut ureq::Body, cap: u64) -> Result<Value, GithubError> {
    body.with_config()
        .limit(cap)
        .read_json::<Value>()
        .map_err(|e| GithubError::Http(e.to_string()))
}

impl GithubHttp for UreqHttp {
    fn get(&self, token: &str, path: &str) -> Result<Value, GithubError> {
        let mut res = self
            .agent
            .get(format!("{API}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tern")
            .call()?;
        read_json_capped(res.body_mut(), MAX_RESPONSE)
    }

    fn send(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<(), GithubError> {
        let request = write_request(token, method, &format!("{API}{path}"), &body)?;
        self.agent.run(request)?;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn GithubHttp> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
thread_local! {
    static TEST_KEYCHAIN: std::cell::RefCell<Option<Box<dyn Keychain>>> = const { std::cell::RefCell::new(None) };
    static TEST_GH: std::cell::RefCell<Option<Box<dyn GhCli>>> = const { std::cell::RefCell::new(None) };
    static TEST_HTTP: std::cell::RefCell<Option<Box<dyn GithubHttp>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn current_keychain() -> Box<dyn Keychain> {
    TEST_KEYCHAIN
        .with(|slot| slot.borrow().as_ref().map(|b| b.clone_box()))
        .unwrap_or_else(|| Box::new(UnsetKeychain))
}

#[cfg(not(test))]
pub(crate) fn current_keychain() -> Box<dyn Keychain> {
    Box::new(OsKeychain)
}

#[cfg(test)]
pub(crate) fn current_gh() -> Box<dyn GhCli> {
    TEST_GH
        .with(|slot| slot.borrow().as_ref().map(|b| b.clone_box()))
        .unwrap_or_else(|| Box::new(UnsetGh))
}

#[cfg(not(test))]
pub(crate) fn current_gh() -> Box<dyn GhCli> {
    Box::new(SystemGh)
}

#[cfg(test)]
pub(crate) fn current_http() -> Box<dyn GithubHttp> {
    TEST_HTTP
        .with(|slot| slot.borrow().as_ref().map(|b| b.clone_box()))
        .unwrap_or_else(|| Box::new(UnsetHttp))
}

#[cfg(not(test))]
pub(crate) fn current_http() -> Box<dyn GithubHttp> {
    Box::new(UreqHttp::new())
}

#[cfg(test)]
pub(crate) struct KeychainGuard;
#[cfg(test)]
impl Drop for KeychainGuard {
    fn drop(&mut self) {
        TEST_KEYCHAIN.with(|k| *k.borrow_mut() = None);
    }
}
#[cfg(test)]
pub(crate) fn set_test_keychain(kc: Box<dyn Keychain>) -> KeychainGuard {
    TEST_KEYCHAIN.with(|k| *k.borrow_mut() = Some(kc));
    KeychainGuard
}

#[cfg(test)]
pub(crate) struct GhGuard;
#[cfg(test)]
impl Drop for GhGuard {
    fn drop(&mut self) {
        TEST_GH.with(|g| *g.borrow_mut() = None);
    }
}
#[cfg(test)]
pub(crate) fn set_test_gh(gh: Box<dyn GhCli>) -> GhGuard {
    TEST_GH.with(|g| *g.borrow_mut() = Some(gh));
    GhGuard
}

#[cfg(test)]
pub(crate) struct HttpGuard;
#[cfg(test)]
impl Drop for HttpGuard {
    fn drop(&mut self) {
        TEST_HTTP.with(|h| *h.borrow_mut() = None);
    }
}
#[cfg(test)]
pub(crate) fn set_test_http(http: Box<dyn GithubHttp>) -> HttpGuard {
    TEST_HTTP.with(|h| *h.borrow_mut() = Some(http));
    HttpGuard
}

/// The token from the Keychain, else from `gh auth token`.
///
/// # Errors
/// [`GithubError::NoToken`] when neither has one.
pub fn token() -> Result<(String, TokenSource), GithubError> {
    token_with(&*current_keychain(), &*current_gh())
}

pub(crate) fn token_with(
    keychain: &dyn Keychain,
    gh: &dyn GhCli,
) -> Result<(String, TokenSource), GithubError> {
    if let Ok(t) = keychain.get()
        && !t.trim().is_empty()
    {
        return Ok((t, TokenSource::Keychain));
    }
    gh_token_with(gh)
        .map(|t| (t, TokenSource::GhCli))
        .ok_or(GithubError::NoToken)
}

pub(crate) fn gh_token_with(gh: &dyn GhCli) -> Option<String> {
    let out = gh.run(&["auth", "token"]).ok()?;
    let t = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.success && !t.is_empty()).then_some(t)
}

/// Stores a pasted token in the macOS Keychain.
///
/// # Errors
/// [`GithubError::Keychain`] when the Keychain refuses.
pub fn save_token(token: &str) -> Result<(), GithubError> {
    save_token_with(&*current_keychain(), token)
}

pub(crate) fn save_token_with(keychain: &dyn Keychain, token: &str) -> Result<(), GithubError> {
    keychain
        .set(token.trim())
        .map_err(|e| GithubError::Keychain(e.to_string()))
}

/// Removes tern's token from the Keychain; `gh` is left alone.
///
/// # Errors
/// [`GithubError::Keychain`] when the Keychain refuses.
pub fn forget_token() -> Result<(), GithubError> {
    forget_token_with(&*current_keychain())
}

pub(crate) fn forget_token_with(keychain: &dyn Keychain) -> Result<(), GithubError> {
    match keychain.delete() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(GithubError::Keychain(e.to_string())),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    version: u32,
    hash: String,
    /// Seconds since the epoch, for display.
    updated_at: u64,
    device: String,
    /// The pusher's bundle number, a hint for the next one; 0 from a tern before numbering.
    /// The checked number travels sealed (the app's `tern-bundle` manifest).
    #[serde(default)]
    seq: u64,
}

/// What the gist holds, decoded.
#[derive(Debug, Clone)]
pub struct RemoteBundle {
    pub hash: String,
    pub updated_at: u64,
    pub device: String,
    /// Unchecked: what `tern-meta.json` claims, for numbering the next push only.
    pub seq: u64,
    pub vault: Vec<u8>,
    pub sealed: Vec<u8>,
}

pub struct Gist {
    http: Box<dyn GithubHttp>,
    token: String,
}

impl std::fmt::Debug for Gist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gist").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct GistSummary {
    id: String,
    description: Option<String>,
}

impl Gist {
    pub fn new(token: String) -> Self {
        Self::with_http(current_http(), token)
    }

    pub(crate) fn with_http(http: Box<dyn GithubHttp>, token: String) -> Self {
        Self { http, token }
    }

    fn get(&self, path: &str) -> Result<Value, GithubError> {
        self.http.get(&self.token, path)
    }

    fn find(&self) -> Result<Option<String>, GithubError> {
        for page in 1..=10 {
            let list: Vec<GistSummary> =
                serde_json::from_value(self.get(&format!("/gists?per_page=100&page={page}"))?)
                    .map_err(|e| GithubError::Format(e.to_string()))?;
            if let Some(g) = list
                .iter()
                .find(|g| g.description.as_deref() == Some(DESCRIPTION))
            {
                return Ok(Some(g.id.clone()));
            }
            if list.len() < 100 {
                break;
            }
        }
        Ok(None)
    }

    /// The gist's bundle, or `None` when there is no `tern-sync` gist yet.
    ///
    /// # Errors
    /// Network, auth or format errors.
    pub fn fetch(&self) -> Result<Option<RemoteBundle>, GithubError> {
        let Some(id) = self.find()? else {
            return Ok(None);
        };
        let gist = self.get(&format!("/gists/{id}"))?;
        let file = |name: &str| -> Result<String, GithubError> {
            gist["files"][name]["content"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| GithubError::Format(format!("{name} missing")))
        };
        let meta: Meta =
            serde_json::from_str(&file(META)?).map_err(|e| GithubError::Format(e.to_string()))?;
        let decode = |name: &str| -> Result<Vec<u8>, GithubError> {
            B64.decode(file(name)?.trim())
                .map_err(|e| GithubError::Format(format!("{name}: {e}")))
        };
        Ok(Some(RemoteBundle {
            hash: meta.hash,
            updated_at: meta.updated_at,
            device: meta.device,
            seq: meta.seq,
            vault: decode(VAULT)?,
            sealed: decode(DATA)?,
        }))
    }

    /// Writes the bundle, creating the private gist on first use.
    ///
    /// # Errors
    /// Network or auth errors.
    pub fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), GithubError> {
        let meta = Meta {
            version: 1,
            hash: hash.to_owned(),
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            device: device.to_owned(),
            seq,
        };
        let files = json!({
            META: { "content": serde_json::to_string_pretty(&meta).map_err(|e| GithubError::Format(e.to_string()))? },
            VAULT: { "content": B64.encode(vault) },
            DATA: { "content": B64.encode(sealed) },
        });
        match self.find()? {
            Some(id) => self.http.send(
                &self.token,
                Method::Patch,
                &format!("/gists/{id}"),
                json!({ "files": files }),
            ),
            None => self.http.send(
                &self.token,
                Method::Post,
                "/gists",
                json!({ "description": DESCRIPTION, "public": false, "files": files }),
            ),
        }
    }
}

#[cfg(test)]
#[path = "github_test_support.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::FakeGh;
#[cfg(test)]
use test_support::{UnsetGh, UnsetHttp, UnsetKeychain};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
#[path = "github_tests.rs"]
mod tests;
