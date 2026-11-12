//! Update check: once a day at launch, ask GitHub Releases for the latest tern. When it is newer
//! than this build, a Mac build signed with a certificate downloads and checks it (`updater`) and
//! offers "Restart to update"; nothing is installed until the person chooses that. Any other
//! build offers the release page.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{AppContext as _, Context};
use serde::Deserialize;

use super::{Alert, Shell};
use crate::messages::{Kind, Work};
use crate::settings;
use crate::update_outcome::{Found, Outcome, Trouble, reason, version_row};

/// Only a page on GitHub is ever opened, whatever the response says.
const PAGE_PREFIX: &str = "https://github.com/";
const STAMP_FILE: &str = "update-check";
const INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Where releases come from: this build's repository on GitHub, or `TERN_UPDATE_FEED`, a URL
/// serving `latest` (GitHub's release JSON) and the zips side by side, for trying an update
/// end to end. A download is checked the same way whichever it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    latest: String,
    download: String,
    custom: bool,
}

impl Feed {
    pub fn get() -> Option<Self> {
        match std::env::var("TERN_UPDATE_FEED") {
            Ok(feed) if !feed.is_empty() => Some(Self::custom(&feed)),
            _ => Self::github(env!("CARGO_PKG_REPOSITORY")),
        }
    }

    fn github(repository: &str) -> Option<Self> {
        let repo = repository.strip_prefix("https://github.com/")?;
        Some(Self {
            latest: format!("https://api.github.com/repos/{repo}/releases/latest"),
            download: format!("https://github.com/{repo}/releases/download/"),
            custom: false,
        })
    }

    fn custom(feed: &str) -> Self {
        let feed = feed.trim_end_matches('/');
        Self {
            latest: format!("{feed}/latest"),
            download: format!("{feed}/"),
            custom: true,
        }
    }

    /// The latest-release JSON.
    pub fn latest(&self) -> &str {
        &self.latest
    }

    /// A test feed: checked on every launch, also from a debug build.
    pub fn is_custom(&self) -> bool {
        self.custom
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn zip_url(&self, version: &str) -> String {
        if self.custom {
            format!("{}{}", self.download, zip_name(version))
        } else {
            format!("{}v{version}/{}", self.download, zip_name(version))
        }
    }
}

/// The release asset `scripts/release.sh` builds and the Homebrew cask installs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn zip_name(version: &str) -> String {
    format!("tern-{version}-macos-arm64.zip")
}

/// `major.minor.patch`; compared field by field, so 0.10.0 is newer than 0.9.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl Version {
    /// `v0.2.0` or `0.2.0`. A pre-release (`0.2.0-rc.1`) is `None`: tern only announces
    /// finished releases: `-rc.1` makes the patch field or the field count wrong, so it
    /// fails to parse. Build metadata (`+abc`) is ignored.
    fn parse(tag: &str) -> Option<Self> {
        let tag = tag.trim().trim_start_matches('v');
        let tag = tag.split('+').next()?;
        let mut parts = tag.split('.');
        let v = Version(
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(v)
    }
}

/// The version to announce: the release's, when it parses, is not a pre-release and is newer.
fn newer(current: &str, latest_tag: &str) -> Option<String> {
    let current = Version::parse(current)?;
    let latest = Version::parse(latest_tag)?;
    (latest > current).then(|| latest_tag.trim().trim_start_matches('v').to_string())
}

/// At most one check a day. A clock that went backwards counts as due, so a wrong date never
/// silences the check for years.
fn due(last: Option<u64>, now: u64) -> bool {
    match last {
        Some(last) if last <= now => now - last >= INTERVAL_SECS,
        _ => true,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn last_check(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join(STAMP_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

/// One check, the daily one and a manual one alike: stamps the attempt as the day's check (also
/// when it fails, so being offline does not retry on every launch), then asks the feed.
/// Blocking; runs on the background executor.
fn run_check(
    feed: &Feed,
    dir: Option<&Path>,
    now: u64,
    current: &str,
) -> Result<Option<Found>, Trouble> {
    if let Some(dir) = dir {
        let _ = std::fs::create_dir_all(dir);
        if let Err(e) = std::fs::write(dir.join(STAMP_FILE), now.to_string()) {
            tracing::warn!(error = %e, "update_stamp_failed");
        }
    }
    fetch(feed, current)
}

fn fetch(feed: &Feed, current: &str) -> Result<Option<Found>, Trouble> {
    let config = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(true)
        .build();
    let mut response = ureq::Agent::new_with_config(config)
        .get(feed.latest())
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "tern")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => Trouble::Status(code),
            e => {
                tracing::info!(error = %e, "update_check_unreachable");
                Trouble::Unreachable
            }
        })?;
    let release: Release = response.body_mut().read_json().map_err(|e| {
        tracing::info!(error = %e, "update_check_malformed");
        Trouble::Malformed
    })?;
    if release.prerelease
        || release.draft
        || !(feed.is_custom() || release.html_url.starts_with(PAGE_PREFIX))
    {
        return Ok(None);
    }
    Ok(newer(current, &release.tag_name).map(|version| Found {
        version,
        url: release.html_url,
    }))
}

#[cfg(target_os = "macos")]
pub(super) type Staged = crate::updater::Staged;
/// Only a Mac build updates itself.
#[cfg(not(target_os = "macos"))]
pub(super) type Staged = std::convert::Infallible;

/// Where a Mac build stages updates; `stage` empties it before each download.
fn update_cache() -> Option<std::path::PathBuf> {
    Some(std::env::home_dir()?.join("Library/Caches/tern"))
}

/// Downloads and checks `found` when this build can update itself; `None` otherwise or when a
/// step fails (logged), and the release page is offered instead.
fn stage(feed: &Feed, found: &Found) -> Option<Staged> {
    #[cfg(target_os = "macos")]
    {
        let (app, requirement) = crate::updater::self_updatable()?;
        let cache = update_cache()?;
        let url = feed.zip_url(&found.version);
        match crate::updater::stage(&url, &found.version, &app, &requirement, &cache) {
            Ok(staged) => Some(staged),
            Err(e) => {
                tracing::warn!(error = %e, version = %found.version, "update_stage_failed");
                None
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (feed, found);
        None
    }
}

/// The Alert for how a check ended; "Restart now" and "Download" go with the outcomes that have
/// something to do next.
fn outcome_alert(outcome: &Outcome) -> Alert {
    let text = outcome.message(env!("CARGO_PKG_VERSION"));
    let toast = match outcome {
        Outcome::Failed(_) => Alert::failure(text),
        _ => Alert::new(text),
    };
    if outcome.restarts() {
        return toast
            .staying(20)
            .action("Restart now", |s, _, cx| s.restart_to_update(cx));
    }
    match outcome.download_url() {
        Some(url) => {
            let url = url.to_string();
            toast.action("Download", move |_, _, cx| cx.open_url(&url))
        }
        None => toast,
    }
}

impl Shell {
    /// Runs the daily check when the setting allows it and it is due. A test feed
    /// (`TERN_UPDATE_FEED`) is checked on every launch, also from a debug build. Quiet unless it
    /// finds something.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = Feed::get() else { return };
        // Debug builds are test instances, started many times a day; they call GitHub never.
        if (cfg!(debug_assertions) && !feed.is_custom()) || !self.settings.check_for_updates {
            return;
        }
        let Some(dir) = settings::dir() else { return };
        if !feed.is_custom() && !due(last_check(&dir), now_secs()) {
            return;
        }
        self.start_check(feed, false, cx);
    }

    /// "Check for Updates…": checks now, whether or not the daily check is on or due, and says
    /// how it went. Does nothing new while a check runs.
    pub(crate) fn check_now(&mut self, cx: &mut Context<Self>) {
        match Feed::get() {
            Some(feed) => self.start_check(feed, true, cx),
            None => {
                let failed = Outcome::Failed("this build has no update feed".into());
                if self.update_check.begin() {
                    self.finish_check(&failed, true, cx);
                }
            }
        }
    }

    /// Whether a check is running; the menu item and the Settings button are off meanwhile.
    pub(crate) fn update_check_running(&self) -> bool {
        self.update_check.running()
    }

    /// What the Version row says about the last check.
    pub(super) fn update_row_text(&self) -> String {
        let waiting = self.update_waiting();
        version_row(
            env!("CARGO_PKG_VERSION"),
            self.update_check.shown(),
            waiting.as_deref(),
        )
    }

    /// `manual` checks show their progress and every result; the daily one only what is found.
    fn start_check(&mut self, feed: Feed, manual: bool, cx: &mut Context<Self>) {
        if !self.update_check.begin() {
            return;
        }
        if manual {
            let text = Outcome::Checking.message(env!("CARGO_PKG_VERSION"));
            self.progress_activity(Work::UpdateCheck, text, cx);
        }
        cx.notify();
        let dir = settings::dir();
        let now = now_secs();
        let asked = feed.clone();
        let work = cx.background_spawn(async move {
            let found = run_check(&asked, dir.as_deref(), now, env!("CARGO_PKG_VERSION"))?;
            if found.is_none()
                && let Some(cache) = update_cache()
            {
                // Up to date: what an installed update left behind (its zip) goes.
                let _ = std::fs::remove_dir_all(cache.join("update"));
            }
            Ok::<_, Trouble>(found)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| s.checked(feed, result, manual, cx));
        })
        .detach();
    }

    fn checked(
        &mut self,
        feed: Feed,
        result: Result<Option<Found>, Trouble>,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        let found = match result {
            Ok(Some(found)) => found,
            Ok(None) => return self.finish_check(&Outcome::UpToDate, manual, cx),
            Err(trouble) => {
                return self.finish_check(&Outcome::Failed(reason(&trouble)), manual, cx);
            }
        };
        // Already downloaded by an earlier check: nothing to fetch again.
        if self.update_waiting().as_deref() == Some(found.version.as_str()) {
            return self.finish_check(&Outcome::Ready(found.version), manual, cx);
        }
        let downloading = Outcome::Downloading(found.version.clone());
        if manual {
            self.progress_activity(
                Work::UpdateCheck,
                downloading.message(env!("CARGO_PKG_VERSION")),
                cx,
            );
        }
        self.update_check.progress(downloading);
        cx.notify();
        let work = cx.background_spawn(async move {
            let staged = stage(&feed, &found);
            (found, staged)
        });
        cx.spawn(async move |this, cx| {
            let (found, staged) = work.await;
            let _ = this.update(cx, |s, cx| {
                let outcome = s.keep_staged(found, staged, cx);
                s.finish_check(&outcome, manual, cx);
            });
        })
        .detach();
    }

    /// Ends the check and shows how it went. The daily check says nothing when tern is up to
    /// date or the check fails; it is only heard when there is a release to act on.
    fn finish_check(&mut self, outcome: &Outcome, manual: bool, cx: &mut Context<Self>) {
        let actionable = matches!(*outcome, Outcome::Ready(_) | Outcome::NotInstallable(_));
        match outcome {
            Outcome::Failed(why) => tracing::info!(error = %why, "update_check_failed"),
            Outcome::UpToDate => tracing::debug!("update_check_current"),
            _ => {}
        }
        self.update_check.finish(outcome.clone());
        self.end_progress_activity(Work::UpdateCheck, cx);
        if manual || actionable {
            match outcome.kind() {
                Kind::Alert => self.alert(outcome_alert(outcome), cx),
                Kind::Activity | Kind::Decision => {
                    self.activity(outcome.message(env!("CARGO_PKG_VERSION")), cx)
                }
            }
        }
        cx.notify();
    }

    /// A staged update waits for tern to quit, or for "Restart now" (restarting ends open
    /// sessions, so it is never forced); without one, the release page is offered.
    fn keep_staged(
        &mut self,
        found: Found,
        staged: Option<Staged>,
        cx: &mut Context<Self>,
    ) -> Outcome {
        #[cfg(target_os = "macos")]
        if let Some(staged) = staged {
            let first = self.update_ready.is_none();
            let version = staged.version.clone();
            self.update_ready = Some(std::rc::Rc::new(staged));
            if first {
                cx.on_app_quit(|s, _| {
                    // Failures are logged inside; tern is quitting either way.
                    let _ = s.install_update();
                    async {}
                })
                .detach();
            }
            return Outcome::Ready(version);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (staged, cx);
        Outcome::NotInstallable(found)
    }

    /// The version a downloaded update brings, while one waits to be installed.
    pub(super) fn update_waiting(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            self.update_ready.as_ref().map(|s| s.version.clone())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// Installs the waiting update and quits; tern opens again as the new version.
    pub(super) fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        match self.install_update() {
            Ok(true) => cx.quit(),
            Ok(false) => {}
            Err(e) => self.alert_failed(format!("Could not update: {e}"), cx),
        }
    }

    /// Hands the waiting update to the swap helper, once. `Ok(false)` when none waits.
    fn install_update(&mut self) -> Result<bool, String> {
        let Some(staged) = self.update_ready.take() else {
            return Ok(false);
        };
        #[cfg(target_os = "macos")]
        {
            crate::updater::install(&staged).map_err(|e| {
                tracing::warn!(error = %e, "update_install_failed");
                e
            })?;
            Ok(true)
        }
        #[cfg(not(target_os = "macos"))]
        match *staged {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numbers_not_text() {
        // (running, latest tag, announced)
        let cases = [
            ("0.9.0", "v0.10.0", Some("0.10.0")),
            ("0.10.0", "v0.9.0", None),
            ("0.1.0", "v0.1.1", Some("0.1.1")),
            ("0.1.0", "v0.1.0", None),
            ("0.2.9", "1.0.0", Some("1.0.0")),
            ("1.9.9", "v1.10.0", Some("1.10.0")),
            ("1.2.3", "v1.2.3+build5", None),
        ];
        for (running, tag, want) in cases {
            assert_eq!(newer(running, tag).as_deref(), want, "{running} vs {tag}");
        }
    }

    #[test]
    fn prereleases_and_junk_are_ignored() {
        for tag in [
            "v0.3.0-rc.1",
            "0.3.0-beta",
            "v0.3",
            "v0.3.0.1",
            "nightly",
            "",
        ] {
            assert_eq!(newer("0.1.0", tag), None, "{tag}");
        }
    }

    #[test]
    fn the_check_runs_at_most_daily() {
        let now = 10 * INTERVAL_SECS;
        assert!(due(None, now));
        assert!(!due(Some(now - 60), now));
        assert!(!due(Some(now - INTERVAL_SECS + 1), now));
        assert!(due(Some(now - INTERVAL_SECS), now));
        // The clock moved back past the stamp.
        assert!(due(Some(now + 5), now));
    }

    #[test]
    fn the_stamp_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_check(dir.path()), None);
        std::fs::write(dir.path().join(STAMP_FILE), "1760000000\n").ok();
        assert_eq!(last_check(dir.path()), Some(1_760_000_000));
    }

    #[test]
    fn this_build_knows_its_github_repository() {
        // Without `repository` in the app crate's manifest the feed is None and the daily
        // check silently never runs (0.2.0 and 0.3.0 shipped that way).
        let repository = env!("CARGO_PKG_REPOSITORY");
        assert!(
            Feed::github(repository).is_some(),
            "no GitHub feed from CARGO_PKG_REPOSITORY = {repository:?}"
        );
    }

    #[test]
    fn releases_come_from_this_build_s_repository_or_a_test_feed() {
        let github = Feed::github("https://github.com/someone/tern").unwrap();
        assert_eq!(
            github.latest(),
            "https://api.github.com/repos/someone/tern/releases/latest"
        );
        assert_eq!(
            github.zip_url("0.2.1"),
            "https://github.com/someone/tern/releases/download/v0.2.1/tern-0.2.1-macos-arm64.zip"
        );
        assert!(!github.is_custom());
        assert!(Feed::github("https://example.com/tern").is_none());
        let local = Feed::custom("http://127.0.0.1:8123/");
        assert_eq!(local.latest(), "http://127.0.0.1:8123/latest");
        assert_eq!(
            local.zip_url("0.2.1"),
            "http://127.0.0.1:8123/tern-0.2.1-macos-arm64.zip"
        );
        assert!(local.is_custom());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "shell_update_tests.rs"]
mod check_tests;
