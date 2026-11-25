//! The Mac menu bar keeps a hidden broker window reachable. Native callbacks only send UI intents.
#![allow(unsafe_code)]

use async_channel::Sender;
use gpui::{App, Context};
use objc2::rc::Retained;
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNNotificationDefaultActionIdentifier, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::agents::attention::AttentionKind;
use crate::shell::{Shell, main_window, show_main_window};

#[derive(Debug, Clone, Copy)]
enum Action {
    Open,
    Notification(AttentionKind),
    Pause,
    Quit,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. The only ivar is a thread-safe sender.
    #[unsafe(super = NSObject)]
    #[name = "TernAgentMenuTarget"]
    #[ivars = Sender<Action>]
    #[derive(Debug)]
    struct Target;

    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for Target {}
    impl Target {
        #[unsafe(method(openTern:))]
        fn open(&self, _: &NSMenuItem) {
            let _ = self.ivars().try_send(Action::Open);
        }
        #[unsafe(method(toggleAgents:))]
        fn pause(&self, _: &NSMenuItem) {
            let _ = self.ivars().try_send(Action::Pause);
        }
        #[unsafe(method(quitTern:))]
        fn quit(&self, _: &NSMenuItem) {
            let _ = self.ivars().try_send(Action::Quit);
        }
    }
    // SAFETY: Notification Center can call from a background thread. Only the sender crosses it.
    unsafe impl UNUserNotificationCenterDelegate for Target {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn notification_clicked(
            &self,
            _: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &block2::DynBlock<dyn Fn()>,
        ) {
            // SAFETY: this immutable framework constant lives for the process.
            if *response.actionIdentifier() == *unsafe { UNNotificationDefaultActionIdentifier }
                && let Some(kind) = AttentionKind::from_notification(&response.notification().request().identifier().to_string()) {
                let _ = self.ivars().try_send(Action::Notification(kind));
            }
            done.call(());
        }
    }
);

#[derive(Debug)]
pub(crate) struct AgentMenu {
    item: Retained<NSStatusItem>,
    pause: Retained<NSMenuItem>,
    // AppKit menu items and Notification Center hold their target weakly.
    _target: Retained<Target>,
    _listener: gpui::Task<()>,
}

pub(crate) fn reopen(cx: &mut App) {
    if let Some(handle) = main_window(cx) {
        let _ = handle.update(cx, |s, window, cx| {
            if s.agent_menu().is_some() {
                show_main_window(window, cx);
            }
        });
    }
}

impl AgentMenu {
    pub(crate) fn new(cx: &mut Context<Shell>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let (tx, rx) = async_channel::bounded(16);
        let target = Target::alloc().set_ivars(tx);
        // SAFETY: NSObject's designated initializer initializes the allocated subclass.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        item.button(mtm)?.setTitle(&NSString::from_str("Tern"));
        let menu = NSMenu::new(mtm);
        let add = |title: &str, selector: Sel| {
            // SAFETY: each selector is implemented on Target and accepts the sending NSMenuItem.
            let entry = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(selector),
                    &NSString::new(),
                )
            };
            // SAFETY: AgentMenu retains Target while this menu is installed.
            unsafe { entry.setTarget(Some(&target)) };
            menu.addItem(&entry);
            entry
        };
        add("Open Tern", sel!(openTern:));
        let pause = add("Pause Agents", sel!(toggleAgents:));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("Quit Tern", sel!(quitTern:));
        item.setMenu(Some(&menu));
        if crate::agents::attention::has_app_bundle() {
            UNUserNotificationCenter::currentNotificationCenter()
                .setDelegate(Some(ProtocolObject::from_ref(&*target)));
        }
        let listener = cx.spawn(async move |_, cx| {
            while let Ok(action) = rx.recv().await {
                cx.update(|cx| {
                    let Some(handle) = main_window(cx) else {
                        return;
                    };
                    let _ = handle.update(cx, |shell, window, cx| match action {
                        Action::Open => show_main_window(window, cx),
                        Action::Notification(kind) => {
                            shell.show_agent_card(kind);
                            show_main_window(window, cx);
                        }
                        Action::Pause => {
                            if let Err(e) = shell.set_agents_paused(!shell.agents_paused(), cx) {
                                show_main_window(window, cx);
                                shell.alert_failed(e.to_string(), cx);
                            }
                        }
                        Action::Quit => shell.request_quit(window, cx),
                    });
                });
            }
        });
        Some(Self {
            item,
            pause,
            _target: target,
            _listener: listener,
        })
    }

    /// Drives the native menu through AppKit in dev-check, including its target/action wiring.
    #[cfg(any(debug_assertions, feature = "dev-keys"))]
    pub(crate) fn replay(&self, title: &str) {
        if let Some(mtm) = MainThreadMarker::new()
            && let Some(menu) = self.item.menu(mtm)
        {
            let index = menu.indexOfItemWithTitle(&NSString::from_str(title));
            if index >= 0 {
                menu.performActionForItemAtIndex(index);
            }
        }
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.pause.setTitle(&NSString::from_str(if paused {
            "Resume Agents"
        } else {
            "Pause Agents"
        }));
    }
}

impl Drop for AgentMenu {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}
