//! Settings → Sync: keep tern's connections, settings, shortcuts and vault the same on every
//! Mac through a private GitHub gist (see `tern-sync`). The vault file travels as it is; the
//! rest is sealed with the vault passphrase first, so GitHub only ever holds ciphertext.

use crate::hover::Accessible as _;
use std::time::{Duration, Instant};

use gpui::InteractiveElement as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Subscription, div, px,
};
use tern_sync::{Plan, TokenSource};

use super::Shell;
use crate::keeper::Keeper;
use crate::settings_widgets as w;
use crate::text_input::TextInput;

#[path = "shell_sync_engine.rs"]
mod engine;
use engine::{Outcome, Resolutions, Side, Snapshot, Throttle, run, snapshot};

/// Edits are batched this long, so typing a host name is one sync, not twenty.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// How often the synced files are looked at for changes.
const POLL: Duration = Duration::from_secs(2);
/// After a failed automatic sync, the same data is not retried for this long (offline).
const BACKOFF: Duration = Duration::from_secs(300);
/// Bringing the window to the front looks at the remote, but not more than once a minute.
const ACTIVATE_EVERY: Duration = Duration::from_secs(60);
/// While tern stays open the remote is looked at this often, for changes from other Macs.
const PERIODIC_EVERY: Duration = Duration::from_secs(300);

/// What the user still has to decide after a conflict.
struct Ask {
    /// Nothing is known about the last sync: one choice for the whole bundle.
    whole: bool,
    items: Vec<String>,
    choices: Resolutions,
}

pub(super) struct SyncUi {
    repo_input: Entity<TextInput>,
    token_input: Entity<TextInput>,
    pass_input: Entity<TextInput>,
    token_source: Option<TokenSource>,
    busy: bool,
    /// The last outcome: ok?, text.
    message: Option<(bool, String)>,
    ask: Option<Ask>,
    _repaint: Vec<Subscription>,
}

/// Automatic sync's bookkeeping; lives on the shell so it runs before Settings is opened.
pub(super) struct AutoSync {
    started: bool,
    /// Looking at the remote when the window comes to the front: at most once a minute.
    on_activate: Throttle,
    /// And while the app stays open: every 5 minutes.
    periodic: Throttle,
    /// Sync once when the app starts (waits for the vault to be unlocked).
    launch_pending: bool,
    /// The local hash the debounce is waiting on, and when it may run.
    seen: Option<String>,
    deadline: Option<Instant>,
    /// The last automatic failure: the data it failed on, when, and the text already shown.
    failed: Option<(String, Instant)>,
    alerted: Option<String>,
    /// The line shown in Settings → Sync.
    status: Option<String>,
}

impl Default for AutoSync {
    fn default() -> Self {
        Self {
            started: false,
            on_activate: Throttle::new(ACTIVATE_EVERY),
            periodic: Throttle::new(PERIODIC_EVERY),
            launch_pending: false,
            seen: None,
            deadline: None,
            failed: None,
            alerted: None,
            status: None,
        }
    }
}

fn item_label(name: &str) -> &'static str {
    match name {
        "hosts.json" => "Connections",
        "settings.json" => "Settings",
        "keymap.json" => "Shortcuts",
        "snippets.json" => "Snippets",
        _ => "Vault",
    }
}

impl Shell {
    /// Builds the page state the first time Sync is opened and checks for a token off the UI
    /// thread (`gh auth token` is a process).
    pub(super) fn ensure_sync_ui(&mut self, cx: &mut Context<Self>) {
        if self.sync_ui.is_some() {
            return;
        }
        let t = self.theme;
        let colors = t.input_colors();
        let token_input =
            cx.new(|cx| TextInput::new("ghp_… with the gist scope", true, colors, cx));
        let pass_input = cx.new(|cx| TextInput::new("Vault passphrase", true, colors, cx));
        let current = self.settings.sync_remote.clone().unwrap_or_default();
        let repo_input = cx.new(|cx| {
            let mut i = TextInput::new("git@github.com:you/tern-sync.git", false, colors, cx);
            i.set_text(current, cx);
            i
        });
        let repaint = vec![
            cx.observe(&repo_input, |_, _, cx| cx.notify()),
            cx.observe(&token_input, |_, _, cx| cx.notify()),
            cx.observe(&pass_input, |_, _, cx| cx.notify()),
        ];
        self.sync_ui = Some(SyncUi {
            repo_input,
            token_input,
            pass_input,
            token_source: None,
            busy: false,
            message: None,
            ask: None,
            _repaint: repaint,
        });
        self.refresh_token_source(cx);
    }

    fn refresh_token_source(&mut self, cx: &mut Context<Self>) {
        let work = cx.background_spawn(async { tern_sync::token().ok().map(|(_, s)| s) });
        cx.spawn(async move |this, cx| {
            let source = work.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.sync_ui.as_mut() {
                    ui.token_source = source;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_token(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = self.sync_ui.as_mut() else {
            return;
        };
        let token = ui.token_input.read(cx).text().to_owned();
        let saved = tern_sync::save_token(&token);
        ui.token_input.update(cx, |i, cx| i.set_text("", cx));
        match saved {
            Ok(()) => self.activity("GitHub token saved", cx),
            Err(e) => self.alert_failed(e.to_string(), cx),
        }
        self.refresh_token_source(cx);
    }

    fn save_repo(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &self.sync_ui else {
            return;
        };
        let url = ui.repo_input.read(cx).text().trim().to_owned();
        let value = (!url.is_empty()).then_some(url);
        self.update_settings(|s| s.sync_remote = value.clone(), cx);
        self.activity(
            match value {
                Some(_) => "Syncing through your git repository",
                None => "Syncing through a GitHub gist",
            },
            cx,
        );
    }

    /// `gh repo create tern-sync --private`, then uses its SSH URL.
    fn create_repo(&mut self, cx: &mut Context<Self>) {
        let work = cx.background_spawn(async { tern_sync::create_github_repo() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| match result {
                Ok(url) => {
                    if let Some(ui) = &s.sync_ui {
                        ui.repo_input
                            .update(cx, |i, cx| i.set_text(url.clone(), cx));
                    }
                    s.update_settings(|st| st.sync_remote = Some(url.clone()), cx);
                    s.activity(format!("Private repo ready: {url}"), cx);
                }
                Err(e) => s.alert_failed(e.to_string(), cx),
            });
        })
        .detach();
    }

    fn forget_token(&mut self, cx: &mut Context<Self>) {
        match tern_sync::forget_token() {
            Ok(()) => self.activity("GitHub token removed", cx),
            Err(e) => self.alert_failed(e.to_string(), cx),
        }
        self.refresh_token_source(cx);
    }

    /// Syncs. `force` settles a whole-bundle conflict (Push keeps this Mac, Pull takes the
    /// remote's copy); `resolve` answers a per-item conflict. `auto` is the background run:
    /// it never asks for a passphrase and keeps its errors to the status line and one Alert.
    fn sync_now(
        &mut self,
        force: Option<Plan>,
        resolve: Resolutions,
        auto: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = crate::settings::dir() else {
            return;
        };
        if auto {
            self.ensure_sync_ui(cx);
        }
        let Some(ui) = self.sync_ui.as_mut() else {
            return;
        };
        if ui.busy {
            return;
        }
        let passphrase = if auto {
            None
        } else {
            Some(ui.pass_input.read(cx).text().to_owned()).filter(|p| !p.is_empty())
        };
        let remote_url = self.settings.sync_remote.clone();
        // A copy: the open vault stays in the keeper, so tabs keep reading it while the network
        // is slow, and writes made meanwhile are not lost (see `Keeper::sync_done`).
        let open = Keeper::snapshot(cx);
        let (opening, since) = (open.is_none(), Keeper::writes(cx));
        if open.is_none() && passphrase.is_none() {
            if !auto {
                self.alert_failed(
                    "Enter the vault passphrase first: it encrypts what goes to GitHub",
                    cx,
                );
            }
            return;
        }
        ui.busy = true;
        ui.message = None;
        ui.ask = None;
        let started_on = self.auto_sync.seen.clone();
        cx.notify();
        let work = cx.background_spawn(async move {
            run(
                &dir,
                remote_url.as_deref(),
                open,
                passphrase.as_deref(),
                force,
                &resolve,
            )
        });
        cx.spawn(async move |this, cx| {
            let (vault, result) = work.await;
            let _ = this.update_in(cx, |s, window, cx| {
                if let Some(v) = vault {
                    let downloaded = matches!(result, Ok(Outcome::Pulled | Outcome::Merged));
                    Keeper::sync_done(v, opening, downloaded, since, cx);
                }
                // The passphrase field may now be hidden; a hidden field must not keep focus,
                // or Escape and every shortcut go nowhere. A background run leaves focus be.
                if !auto {
                    window.focus(&s.focus, cx);
                }
                let reload = matches!(result, Ok(Outcome::Pulled | Outcome::Merged));
                if let Some(ui) = s.sync_ui.as_mut() {
                    ui.busy = false;
                    // A conflict needs a choice, so it stays on the page; the rest is feedback.
                    ui.ask = None;
                    ui.message = None;
                    if let Ok(Outcome::Conflict { whole, items }) = &result {
                        ui.message = Some((
                            false,
                            if *whole {
                                "This Mac and the remote both changed since the last sync. \
                                 Choose which one to keep."
                                    .into()
                            } else {
                                "Both sides changed these items. Choose which copy to keep \
                                 for each."
                                    .into()
                            },
                        ));
                        ui.ask = Some(Ask {
                            whole: *whole,
                            items: items.clone(),
                            choices: Resolutions::new(),
                        });
                    }
                    if result.is_ok() && !auto {
                        ui.pass_input.update(cx, |i, cx| i.set_text("", cx));
                    }
                }
                s.auto_sync.seen = None;
                s.auto_sync.status = Some(match &result {
                    Ok(Outcome::UpToDate) => "Up to date".into(),
                    Ok(Outcome::Pushed) => "Uploaded this Mac's changes".into(),
                    Ok(Outcome::Pulled) => "Downloaded the remote's changes".into(),
                    Ok(Outcome::Merged) => "Merged this Mac with the remote".into(),
                    Ok(Outcome::Conflict { .. }) => "Waiting for your choice".into(),
                    Err(e) => format!("Last sync failed: {e}"),
                });
                match &result {
                    Ok(Outcome::Conflict { .. }) => {
                        if auto && s.auto_sync.alerted.is_none() {
                            s.auto_sync.alerted = Some("conflict".into());
                            s.alert_failed("Sync needs your decision: open Settings, Sync", cx);
                        }
                    }
                    Ok(outcome) => {
                        s.auto_sync.alerted = None;
                        s.auto_sync.failed = None;
                        // A quiet background run only speaks when something changed here.
                        let text = match outcome {
                            Outcome::UpToDate => "Already up to date",
                            Outcome::Pushed => "Uploaded to the remote",
                            Outcome::Pulled => "Downloaded from the remote",
                            _ => "Merged with the remote",
                        };
                        if !auto || reload {
                            s.activity(text, cx);
                        }
                    }
                    Err(e) if auto => {
                        if let Some(h) = started_on {
                            s.auto_sync.failed = Some((h, Instant::now()));
                        }
                        // Offline retries must not repeat the same Alert.
                        if s.auto_sync.alerted.as_deref() != Some(e.as_str()) {
                            s.auto_sync.alerted = Some(e.clone());
                            s.alert_failed(e.clone(), cx);
                        }
                    }
                    Err(e) => s.alert_failed(e.clone(), cx),
                }
                if reload {
                    s.reload_from_disk(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Starts watching the synced files; called once, when the window opens.
    pub(super) fn start_auto_sync(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.auto_sync.started {
            return;
        }
        self.auto_sync.started = true;
        // Coming back to tern after working on another Mac: look at the remote.
        cx.observe_window_activation(window, |s, window, cx| {
            if window.is_window_active() && s.auto_sync.on_activate.allow(Instant::now()) {
                s.auto_sync.launch_pending = true;
                cx.notify();
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Some(dir) = crate::settings::dir() else {
                    break;
                };
                let snap = cx
                    .background_executor()
                    .spawn(async move { snapshot(&dir) })
                    .await;
                if let Some(snap) = snap
                    && this.update(cx, |s, cx| s.auto_tick(snap, cx)).is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// One look at the files: arms the debounce on a change, and runs the sync once it is due.
    fn auto_tick(&mut self, snap: Snapshot, cx: &mut Context<Self>) {
        let enabled =
            self.settings.sync_auto && (self.settings.sync_remote.is_some() || snap.last.is_some());
        if !enabled {
            return;
        }
        // The first look is the launch sync; after that every few minutes. `launch_pending`
        // is "look at the remote", and stays set until the vault is open and a run starts.
        if self.auto_sync.periodic.allow(Instant::now()) {
            self.auto_sync.launch_pending = true;
        }
        // One sync at a time; a change meanwhile shows up as a difference afterwards.
        if self
            .sync_ui
            .as_ref()
            .is_some_and(|u| u.busy || u.ask.is_some())
        {
            return;
        }
        let dirty = snap.last.as_deref() != Some(snap.local.as_str());
        let now = Instant::now();
        let a = &mut self.auto_sync;
        if !dirty && !a.launch_pending {
            a.seen = None;
            a.deadline = None;
            return;
        }
        if dirty && a.seen.as_deref() != Some(snap.local.as_str()) {
            a.seen = Some(snap.local.clone());
            a.deadline = Some(now + DEBOUNCE);
        }
        if !(a.launch_pending || a.deadline.is_some_and(|d| now >= d)) {
            return;
        }
        if let Some((hash, at)) = &a.failed
            && *hash == snap.local
            && now.duration_since(*at) < BACKOFF
        {
            return;
        }
        if Keeper::len(cx).is_none() {
            // Never prompt for a passphrase from the background.
            let text = "Automatic sync is waiting for the vault to be unlocked";
            if a.status.as_deref() != Some(text) {
                a.status = Some(text.into());
                cx.notify();
            }
            return;
        }
        a.launch_pending = false;
        a.deadline = None;
        a.seen = Some(snap.local);
        self.sync_now(None, Resolutions::new(), true, cx);
    }

    /// Records the answer for one conflicted item; the sync runs once every item has one.
    fn choose(&mut self, item: &str, side: Side, cx: &mut Context<Self>) {
        let Some(ask) = self.sync_ui.as_mut().and_then(|u| u.ask.as_mut()) else {
            return;
        };
        ask.choices.insert(item.to_owned(), side);
        cx.notify();
        if ask.choices.len() == ask.items.len() {
            let choices = ask.choices.clone();
            self.sync_now(None, choices, false, cx);
        }
    }

    /// After a pull: settings, shortcuts and connections are re-read and applied.
    fn reload_from_disk(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = crate::settings::dir() else {
            return;
        };
        let old = std::mem::replace(&mut self.settings, crate::settings::Settings::load(&dir));
        self.reapply(old.reapply(&self.settings), cx);
        cx.set_global(crate::keymap::Keymap::load(&dir));
        crate::keymap::apply(cx);
        match crate::connections::load(&dir) {
            Ok(list) => {
                self.connections = list;
                self.store_error = None;
            }
            Err(e) => self.store_error = Some(e.to_string()),
        }
        self.refresh_hosts();
    }

    pub(super) fn sync_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let Some(ui) = &self.sync_ui else {
            return w::page_column().child(w::page_header(&t, "Sync"));
        };
        let account = match ui.token_source {
            Some(TokenSource::GhCli) => "Using the GitHub CLI's sign-in (gh)",
            Some(TokenSource::Keychain) => "Token saved in the macOS Keychain",
            None => "Not connected",
        };
        let mut github = w::card(&t).child(w::row(
            &t,
            true,
            "GitHub",
            Some(account.into()),
            match ui.token_source {
                Some(TokenSource::Keychain) => w::button(&t, "sync-forget", "Forget token")
                    .on_click(cx.listener(|s, _, _, cx| s.forget_token(cx)))
                    .into_any_element(),
                _ => div().into_any_element(),
            },
        ));
        if ui.token_source != Some(TokenSource::Keychain) {
            github = github.child(w::row(
                &t,
                false,
                "Personal access token",
                Some("Optional when gh is signed in. Needs only the gist scope.".into()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(field(&ui.token_input, &t))
                    .child(
                        w::button(&t, "sync-save-token", "Save")
                            .on_click(cx.listener(|s, _, _, cx| s.save_token(cx))),
                    ),
            ));
        }
        let needs_pass = Keeper::len(cx).is_none();
        let mut sync = w::card(&t);
        if needs_pass {
            sync = sync.child(w::row(
                &t,
                true,
                "Vault passphrase",
                Some(if Keeper::exists(cx) {
                    "Unlocks this Mac's vault for the sync".into()
                } else {
                    "No vault here yet: GitHub's passphrase, or a new one to start".into()
                }),
                field(&ui.pass_input, &t),
            ));
        }
        let label = if ui.busy { "Syncing…" } else { "Sync now" };
        sync = sync.child(w::row(
            &t,
            !needs_pass,
            "Sync now",
            Some(
                "Connections, settings, shortcuts and the vault, encrypted before they leave"
                    .into(),
            ),
            w::button(&t, "sync-now", label).on_click(
                cx.listener(|s, _, _, cx| s.sync_now(None, Resolutions::new(), false, cx)),
            ),
        ));
        sync = sync
            .child(w::row(
                &t,
                false,
                "Sync automatically",
                Some(
                    "At launch and 5 seconds after a change to connections, settings, shortcuts \
                     or the vault"
                        .into(),
                ),
                div()
                    .id("toggle-sync-auto")
                    .switch("Sync automatically", self.settings.sync_auto)
                    .cursor_pointer()
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.update_settings(|st| st.sync_auto = !st.sync_auto, cx);
                    }))
                    .child(w::toggle(&t, self.settings.sync_auto, "sync-auto")),
            ))
            .when_some(self.auto_sync.status.clone(), |c, status| {
                c.child(w::row(&t, false, "Status", Some(status.into()), div()))
            });
        if let Some(ask) = &ui.ask {
            if ask.whole {
                sync = sync.child(w::row(
                    &t,
                    false,
                    "Resolve",
                    Some(
                        "Keep this Mac uploads it over the remote's; Use remote replaces this Mac's"
                            .into(),
                    ),
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(
                            w::button(&t, "sync-keep-mac", "Keep this Mac").on_click(
                                cx.listener(|s, _, _, cx| {
                                    s.sync_now(Some(Plan::Push), Resolutions::new(), false, cx)
                                }),
                            ),
                        )
                        .child(
                            w::button(&t, "sync-use-github", "Use remote").on_click(
                                cx.listener(|s, _, _, cx| {
                                    s.sync_now(Some(Plan::Pull), Resolutions::new(), false, cx)
                                }),
                            ),
                        ),
                ));
            } else {
                for name in &ask.items {
                    let picked = ask.choices.get(name).copied();
                    let (keep, take) = (name.clone(), name.clone());
                    let mark = |side| if picked == Some(side) { " ✓" } else { "" };
                    sync =
                        sync.child(w::row(
                            &t,
                            false,
                            item_label(name),
                            Some("Changed on this Mac and on the remote".into()),
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(
                                    w::button(
                                        &t,
                                        SharedString::from(format!("sync-keep-{name}")),
                                        format!("Keep this Mac{}", mark(Side::Mac)),
                                    )
                                    .on_click(cx.listener(
                                        move |s, _, _, cx| s.choose(&keep, Side::Mac, cx),
                                    )),
                                )
                                .child(
                                    w::button(
                                        &t,
                                        SharedString::from(format!("sync-use-{name}")),
                                        format!("Use remote{}", mark(Side::Remote)),
                                    )
                                    .on_click(cx.listener(
                                        move |s, _, _, cx| s.choose(&take, Side::Remote, cx),
                                    )),
                                ),
                        ));
                }
            }
        }
        w::page_column()
            .child(w::page_header(&t, "Sync"))
            .child(w::page_subtitle(
                &t,
                if self.settings.sync_remote.is_some() {
                    "Your own private git repository. It stores only encrypted data."
                } else {
                    "One private gist on your GitHub account. GitHub stores only encrypted data."
                },
            ))
            .when_some(ui.message.clone(), |el, (ok, text)| {
                el.child(
                    w::page_subtitle(&t, SharedString::from(text)).text_color(if ok {
                        t.muted
                    } else {
                        t.danger
                    }),
                )
            })
            .child(w::section(
                &t,
                "Repository",
                w::card(&t)
                    .child(w::row(
                        &t,
                        true,
                        "Git remote",
                        Some(match &self.settings.sync_remote {
                            Some(_) => "Synced with your own git and its SSH keys".into(),
                            None => "Empty: a private GitHub gist is used instead".into(),
                        }),
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(field(&ui.repo_input, &t))
                            .child(
                                w::button(&t, "sync-save-repo", "Save")
                                    .on_click(cx.listener(|s, _, _, cx| s.save_repo(cx))),
                            ),
                    ))
                    .child(w::row(
                        &t,
                        false,
                        "Create a private repo on GitHub",
                        Some("Runs gh repo create tern-sync --private".into()),
                        w::button(&t, "sync-create-repo", "Create")
                            .on_click(cx.listener(|s, _, _, cx| s.create_repo(cx))),
                    )),
            ))
            .when(self.settings.sync_remote.is_none(), |el| {
                el.child(w::section(&t, "Account", github))
            })
            .child(w::section(&t, "Sync", sync))
    }
}

fn field(input: &Entity<TextInput>, t: &crate::theme::Theme) -> impl IntoElement + use<> {
    div()
        .w(px(240.))
        .h(px(30.))
        .px(px(10.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(t.border)
        .bg(t.row_hover)
        .overflow_hidden()
        .text_sm()
        .child(input.clone())
}
