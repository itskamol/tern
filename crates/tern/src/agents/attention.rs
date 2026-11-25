//! Non-focus-stealing background attention cues for incoming Agent Decisions (ADR 0003, ADR 0004).
//!
//! Defines the thin [`AgentAttention`] platform boundary so `gpui::TestApp` tests inject a spy
//! without touching macOS AppKit or Notification Center. The macOS implementation checks
//! `NSApplication::isActive` on the main thread, requests a Dock `InformationalRequest` bounce,
//! and posts a `UNUserNotificationCenter` notification only when running inside a real `.app`
//! bundle with a `CFBundleIdentifier` — never calling `cx.activate(true)` or stealing focus.

use std::fmt;
use std::sync::Arc;

use crate::decisions::sanitize_display_text;

/// The card to bring to the top when the person opens a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    Approval,
    Vault,
}

#[cfg(target_os = "macos")]
impl AttentionKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Approval => "tern-agent-decision-",
            Self::Vault => "tern-agent-vault-",
        }
    }

    pub fn from_notification(id: &str) -> Option<Self> {
        [Self::Approval, Self::Vault]
            .into_iter()
            .find(|kind| id.starts_with(kind.prefix()))
    }
}

/// Sanitized title and body delivered when an Agent Decision arrives while tern is inactive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionCue {
    pub kind: AttentionKind,
    pub title: String,
    pub body: String,
}

impl AttentionCue {
    /// Builds a sanitized attention cue for a pending Agent `run_command` Decision.
    #[must_use]
    pub fn for_run_command(caller: &str, host: &str, command_preview: &str) -> Self {
        let caller = sanitize_display_text(caller);
        let host = sanitize_display_text(host);
        let preview = sanitize_display_text(command_preview);
        let body = if preview.is_empty() {
            format!("{caller} wants to run a command on {host}")
        } else {
            format!("{caller} wants to run on {host}: {preview}")
        };
        Self {
            kind: AttentionKind::Approval,
            title: format!("Approval needed for {host}"),
            body,
        }
    }

    /// Builds a sanitized attention cue when an approved Agent request is waiting for the Vault
    /// to be unlocked in tern.
    #[must_use]
    pub fn for_vault_unlock(caller: &str, host: &str) -> Self {
        let caller = sanitize_display_text(caller);
        let host = sanitize_display_text(host);
        Self {
            kind: AttentionKind::Vault,
            title: format!("Unlock vault for {host}"),
            body: format!("{caller} is waiting for the vault to unlock to connect to {host}"),
        }
    }
}

/// Platform attention seam invoked when a new Agent Decision enters the queue.
///
/// Implementations must never steal keyboard focus or activate tern's window.
pub trait AgentAttention: Send + Sync + fmt::Debug {
    /// Returns `true` when tern is currently the active frontmost application.
    fn is_app_active(&self) -> bool;

    /// Delivers a non-focus-stealing background attention cue (Dock bounce + OS notification).
    fn alert_background(&self, cue: &AttentionCue);
}

/// Alerts the person if and only if tern is not currently active. Returns `true` when alerted.
pub fn notify_if_inactive(attention: &dyn AgentAttention, cue: &AttentionCue) -> bool {
    if attention.is_app_active() {
        return false;
    }
    attention.alert_background(cue);
    true
}

/// Returns the default platform [`AgentAttention`] implementation for this OS.
#[must_use]
pub fn default_attention() -> Arc<dyn AgentAttention> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacOsAttention)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(NoopAttention)
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
struct NoopAttention;

#[cfg(not(target_os = "macos"))]
impl AgentAttention for NoopAttention {
    fn is_app_active(&self) -> bool {
        true
    }

    fn alert_background(&self, _cue: &AttentionCue) {}
}

#[cfg(target_os = "macos")]
pub(crate) use macos::{has_app_bundle, request_alert_permission};

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::atomic::{AtomicU64, Ordering};

    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2::runtime::Bool;
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    use super::{AgentAttention, AttentionCue};

    static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);

    /// macOS attention cue: Dock `InformationalRequest` bounce + `UNUserNotificationCenter` alert
    /// strictly guarded behind `.app` bundle detection. Never calls `activate` or steals focus.
    #[derive(Debug)]
    pub(super) struct MacOsAttention;

    /// Returns `true` only when running inside a real macOS `.app` bundle with a bundle ID.
    ///
    /// `UNUserNotificationCenter::currentNotificationCenter()` throws an ObjC exception if called
    /// from an unbundled executable (`cargo run` or `cargo test`), so this guard is mandatory.
    pub(crate) fn has_app_bundle() -> bool {
        crate::updater::running_bundle().is_some()
            && NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    fn post_user_notification(cue: &AttentionCue) {
        let id = format!(
            "{}{}",
            cue.kind.prefix(),
            NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed)
        );
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&cue.title));
        content.setBody(&NSString::from_str(&cue.body));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&id),
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    }

    /// Only the button in Agents settings asks macOS for permission to post alerts.
    pub(crate) async fn request_alert_permission() -> Result<(), &'static str> {
        if !has_app_bundle() {
            return Err("Open the Tern app bundle to enable notifications.");
        }
        let (tx, rx) = async_channel::bounded(1);
        let reply = RcBlock::new(move |granted: Bool, err: *mut NSError| {
            let result = if !err.is_null() {
                Err("macOS could not enable Tern notifications.")
            } else if granted.as_bool() {
                Ok(())
            } else {
                Err("Allow notifications for Tern in System Settings → Notifications.")
            };
            let _ = tx.try_send(result);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert,
                &reply,
            );
        drop(reply);
        rx.recv()
            .await
            .unwrap_or(Err("Notification settings are unavailable."))
    }

    impl AgentAttention for MacOsAttention {
        fn is_app_active(&self) -> bool {
            MainThreadMarker::new()
                .is_some_and(|mtm| NSApplication::sharedApplication(mtm).isActive())
        }

        fn alert_background(&self, cue: &AttentionCue) {
            if let Some(mtm) = MainThreadMarker::new() {
                let app = NSApplication::sharedApplication(mtm);
                if !app.isActive() {
                    let _ =
                        app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
                }
            }
            if has_app_bundle() {
                post_user_notification(cue);
            } else {
                tracing::debug!("agent_notification_skipped_unbundled");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Debug)]
    struct FakeAttention {
        active: AtomicBool,
        cues: Mutex<Vec<AttentionCue>>,
    }

    impl FakeAttention {
        fn new(active: bool) -> Self {
            Self {
                active: AtomicBool::new(active),
                cues: Mutex::new(Vec::new()),
            }
        }
    }

    impl AgentAttention for FakeAttention {
        fn is_app_active(&self) -> bool {
            self.active.load(Ordering::SeqCst)
        }

        fn alert_background(&self, cue: &AttentionCue) {
            self.cues.lock().unwrap().push(cue.clone());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn notification_ids_route_to_the_card_that_requested_attention() {
        for kind in [AttentionKind::Approval, AttentionKind::Vault] {
            let id = format!("{}42", kind.prefix());
            assert_eq!(AttentionKind::from_notification(&id), Some(kind));
        }
        assert_eq!(AttentionKind::from_notification("unrelated"), None);
    }

    #[test]
    fn cue_sanitizes_ansi_control_and_bidi_overrides() {
        let cue = AttentionCue::for_run_command(
            "Claude\x1b[31m\u{202e}\nCode\x1b[0m",
            "prod\x07-host",
            "node\r ./check.js",
        );
        assert_eq!(cue.title, "Approval needed for prod-host");
        assert_eq!(
            cue.body,
            "ClaudeCode wants to run on prod-host: node ./check.js"
        );
    }

    #[test]
    fn notify_if_inactive_gates_on_active_state() {
        let cue = AttentionCue::for_run_command("Claude Code", "staging", "df -h");
        let fake = FakeAttention::new(true);
        assert!(!notify_if_inactive(&fake, &cue));
        assert!(fake.cues.lock().unwrap().is_empty());

        fake.active.store(false, Ordering::SeqCst);
        assert!(notify_if_inactive(&fake, &cue));
        assert_eq!(*fake.cues.lock().unwrap(), vec![cue]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unbundled_test_process_skips_user_notifications_safely() {
        assert!(!macos::has_app_bundle());
        let cue = AttentionCue::for_run_command("Claude Code", "staging", "df -h");
        // Must never throw an Objective-C exception when invoked from an unbundled cargo test.
        macos::MacOsAttention.alert_background(&cue);
    }
}
