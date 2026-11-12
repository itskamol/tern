//! Tells the app when the Mac sleeps, the screen locks or the session is handed to another
//! user, so the open vault can lock (Settings -> Vault). macOS only; elsewhere nothing starts.
//!
//! The system posts these on the main thread, outside GPUI, so each observer only sends a
//! [`Cause`] down a channel and the app reads it on the UI thread (the pattern of
//! `single_instance::serve`). The handler decides whether to lock and calls `Keeper::lock`.

use gpui::App;

use crate::keeper::Keeper;

/// Why the vault is asked to lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Sleep,
    ScreenLocked,
    SessionSwitched,
}

impl Cause {
    /// The toast text: what happened to the Mac.
    pub fn message(self) -> &'static str {
        match self {
            Self::Sleep => "Vault locked: the Mac went to sleep",
            Self::ScreenLocked => "Vault locked: the screen was locked",
            Self::SessionSwitched => "Vault locked: another user took over the session",
        }
    }
}

/// Starts watching and runs `on_signal` on the UI thread for each signal, for the rest of the
/// run. A signal that arrives while the UI thread is busy (or the process is suspended by the
/// sleep) is delivered at the first turn after, before any input is.
pub fn start(cx: &mut App, on_signal: impl Fn(Cause, &mut App) + 'static) {
    let (tx, rx) = async_channel::unbounded();
    watch::observe(&tx, cx);
    listen(rx, cx, on_signal);
}

/// Hands each cause that arrives on `rx` to `on_signal` on the UI thread.
pub(crate) fn listen(
    rx: async_channel::Receiver<Cause>,
    cx: &mut App,
    on_signal: impl Fn(Cause, &mut App) + 'static,
) {
    cx.spawn(async move |cx| {
        while let Ok(cause) = rx.recv().await {
            cx.update(|cx| on_signal(cause, cx));
        }
    })
    .detach();
}

/// Locks the open vault as the person's own Lock now does: the keeper forgets it and the
/// next need asks for the passphrase again, while open sessions keep running. A write already
/// handed to the disk (`Keeper::persist` took its copy before returning) is not lost. Returns
/// `true` when a vault was open and is now locked; `enabled` is the setting.
pub fn lock(enabled: bool, cx: &mut App) -> bool {
    enabled && Keeper::lock(cx)
}

#[cfg(target_os = "macos")]
mod watch {
    #![allow(unsafe_code)]

    use std::ptr::NonNull;

    use async_channel::Sender;
    use block2::RcBlock;
    use gpui::{App, Global};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceSessionDidResignActiveNotification,
        NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::{
        NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSNotificationName,
        NSObjectProtocol, NSString,
    };

    use super::Cause;

    /// Posted by the login window when the screen locks (a lock screen, a screensaver with a
    /// password, Control-Command-Q). It is not a public constant, but the name is stable and
    /// widely relied on.
    const SCREEN_LOCKED: &str = "com.apple.screenIsLocked";

    /// The registrations, kept so they stay alive for the run.
    struct Observers(#[allow(dead_code)] Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>);

    impl Global for Observers {}

    /// Registers for the three notifications. Must run on the main thread, as GPUI's startup
    /// does.
    pub(super) fn observe(tx: &Sender<Cause>, cx: &mut App) {
        let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
        let distributed = NSDistributedNotificationCenter::defaultCenter();
        let screen_locked = NSString::from_str(SCREEN_LOCKED);
        // SAFETY: the statics are Foundation constants that live for the process.
        let (sleep, resigned) = unsafe {
            (
                NSWorkspaceWillSleepNotification,
                NSWorkspaceSessionDidResignActiveNotification,
            )
        };
        let observers = vec![
            add(&workspace, sleep, tx, Cause::Sleep),
            add(&workspace, resigned, tx, Cause::SessionSwitched),
            add(&distributed, &screen_locked, tx, Cause::ScreenLocked),
        ];
        cx.set_global(Observers(observers));
    }

    fn add(
        center: &NSNotificationCenter,
        name: &NSNotificationName,
        tx: &Sender<Cause>,
        cause: Cause,
    ) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
        let tx = tx.clone();
        // The channel is unbounded, so this never blocks the main thread; once the receiver is
        // gone (the app is quitting) the signal has nobody to reach.
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            let _ = tx.try_send(cause);
        });
        // SAFETY: no object is passed, no queue is given (the block runs on the posting thread,
        // the main thread for these centers) and the block only touches a `Sender`, which is
        // `Send + Sync`, as the method's "sendable block" requirement asks. The returned
        // token is kept in `Observers`, so the registration lives as long as the process.
        unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) }
    }
}

#[cfg(not(target_os = "macos"))]
mod watch {
    use async_channel::Sender;
    use gpui::App;

    use super::Cause;

    /// Nothing posts these elsewhere: the channel stays open and quiet.
    pub(super) fn observe(tx: &Sender<Cause>, _cx: &mut App) {
        // A sender kept for the run: with none left the listener would end at once.
        std::mem::forget(tx.clone());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn each_cause_reaches_the_ui_thread_in_order() {
        let mut app = gpui::TestApp::new();
        let (tx, rx) = async_channel::unbounded();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        app.update(|cx| listen(rx, cx, move |cause, _| log.borrow_mut().push(cause)));
        tx.send_blocking(Cause::ScreenLocked).unwrap();
        tx.send_blocking(Cause::Sleep).unwrap();
        app.run_until_parked();
        assert_eq!(*seen.borrow(), [Cause::ScreenLocked, Cause::Sleep]);
    }

    /// The workspace center is per process, so posting to it here reaches only this test. The
    /// distributed `com.apple.screenIsLocked` is system-wide and is not posted from a test.
    #[cfg(target_os = "macos")]
    #[test]
    #[allow(unsafe_code)]
    fn the_workspace_notifications_reach_the_channel() {
        use objc2_app_kit::{
            NSWorkspace, NSWorkspaceSessionDidResignActiveNotification,
            NSWorkspaceWillSleepNotification,
        };

        let mut app = gpui::TestApp::new();
        let (tx, rx) = async_channel::unbounded();
        app.update(|cx| watch::observe(&tx, cx));
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: the constants are Foundation statics; no object is attached to the posts.
        unsafe {
            center.postNotificationName_object(NSWorkspaceWillSleepNotification, None);
            center.postNotificationName_object(NSWorkspaceSessionDidResignActiveNotification, None);
        }
        assert_eq!(rx.try_recv(), Ok(Cause::Sleep));
        assert_eq!(rx.try_recv(), Ok(Cause::SessionSwitched));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn every_cause_has_its_own_message() {
        let all = [Cause::Sleep, Cause::ScreenLocked, Cause::SessionSwitched];
        for (i, a) in all.iter().enumerate() {
            assert!(a.message().starts_with("Vault locked"));
            for b in &all[i + 1..] {
                assert_ne!(a.message(), b.message());
            }
        }
    }
}
