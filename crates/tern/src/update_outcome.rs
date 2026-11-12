//! What a person sees for an update check: one place maps the check's result to the toast and the
//! Version row in Settings → About, so the two cannot drift apart. Also the rule that only one
//! check runs at a time.

/// A newer release: its version text and the page to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub version: String,
    pub url: String,
}

/// How a check ended, or that it is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Checking,
    UpToDate,
    /// A newer version, being downloaded and checked.
    Downloading(String),
    /// A newer version, downloaded and checked: installs on quit or "Restart now".
    Ready(String),
    /// A newer version this build cannot install by itself: offered as a download.
    NotInstallable(Found),
    Failed(String),
}

impl Outcome {
    /// The snackbar's message; `current` is the running version.
    pub fn toast_text(&self, current: &str) -> String {
        match self {
            Self::Checking => "Checking for updates…".into(),
            Self::UpToDate => format!("tern is up to date ({current})"),
            Self::Downloading(v) => format!("tern {v} is available; downloading it…"),
            Self::Ready(v) => format!("tern {v} is ready; it installs when tern quits"),
            Self::NotInstallable(f) => format!("tern {} is available", f.version),
            Self::Failed(why) => format!("Could not check for updates: {why}"),
        }
    }

    /// Whether "Restart now" goes with it.
    pub fn restarts(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The page "Download" opens.
    pub fn download_url(&self) -> Option<&str> {
        match self {
            Self::NotInstallable(f) => Some(&f.url),
            _ => None,
        }
    }
}

/// The Version row's text in Settings → About. A downloaded update waiting for install shows
/// whatever the last check said, unless a check is running now.
pub fn version_row(current: &str, shown: Option<&Outcome>, waiting: Option<&str>) -> String {
    let state = match (shown, waiting) {
        (Some(Outcome::Checking), _) => "checking for updates…".to_string(),
        (_, Some(next)) => format!("{next} is downloaded and installs when tern quits"),
        (None, None) => return current.to_string(),
        (Some(Outcome::UpToDate), None) => "up to date".into(),
        (Some(Outcome::Downloading(v)), None) => format!("{v} is downloading"),
        (Some(Outcome::Ready(v)), None) => {
            format!("{v} is downloaded and installs when tern quits")
        }
        (Some(Outcome::NotInstallable(f)), None) => {
            format!("{} is available to download", f.version)
        }
        (Some(Outcome::Failed(why)), None) => format!("the last check failed: {why}"),
    };
    format!("{current}; {state}")
}

/// Whether a check runs, and how the last one ended.
#[derive(Debug, Default)]
pub struct CheckState {
    running: bool,
    last: Option<Outcome>,
}

impl CheckState {
    pub fn running(&self) -> bool {
        self.running
    }

    /// Starts a check; `false` when one already runs, and the request is dropped.
    pub fn begin(&mut self) -> bool {
        let started = !std::mem::replace(&mut self.running, true);
        if started {
            self.last = None;
        }
        started
    }

    pub fn finish(&mut self, outcome: Outcome) {
        self.running = false;
        self.last = Some(outcome);
    }

    /// Records a result without ending the check, e.g. "downloading".
    pub fn progress(&mut self, outcome: Outcome) {
        self.last = Some(outcome);
    }

    /// What the Version row reports: how the running check has got so far ("checking" until it
    /// says more), else the last result.
    pub fn shown(&self) -> Option<&Outcome> {
        match (&self.last, self.running) {
            (None, true) => Some(&Outcome::Checking),
            (last, _) => last.as_ref(),
        }
    }
}

/// Why a check could not get a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    Status(u16),
    Unreachable,
    Malformed,
}

pub fn reason(trouble: &Trouble) -> String {
    match trouble {
        Trouble::Status(code) => format!("GitHub answered {code}; try again later"),
        Trouble::Unreachable => "could not reach the update feed; are you offline?".into(),
        Trouble::Malformed => "the update feed's reply was not understood".into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "update_outcome_tests.rs"]
mod tests;
