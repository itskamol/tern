#![allow(clippy::unwrap_used)]
//! The lock the OS signals ask for (`lock_on_sleep`), against a keeper over a temp vault file.
use tern_ssh::SecretString;
use tern_vault::Vault;

use super::tests::{has, keeper, login, vault_with};
use super::*;

/// What the OS notification does, minus the OS: a cause on the channel reaches the UI
/// thread and the keeper ends up locked. Real sleep and screen lock cannot be driven in CI.
#[test]
fn a_sleep_signal_locks_the_open_vault() {
    use crate::lock_on_sleep::{self, Cause};
    let (mut app, _dir) = keeper(Some(vault_with("a")));
    let (tx, rx) = async_channel::unbounded();
    app.update(|cx| {
        lock_on_sleep::listen(rx, cx, |_, cx| {
            lock_on_sleep::lock(true, cx);
        })
    });
    assert!(has(&mut app, "a"));
    tx.send_blocking(Cause::Sleep).unwrap();
    app.run_until_parked();
    assert!(!has(&mut app, "a"));
    assert_eq!(app.update(|cx| Keeper::len(cx)), None);
    // A second signal (sleep and the screen lock arrive together) finds nothing to lock.
    app.update(|cx| {
        assert!(!lock_on_sleep::lock(true, cx));
    });
}

#[test]
fn with_the_setting_off_a_signal_leaves_the_vault_open() {
    use crate::lock_on_sleep;
    let (mut app, _dir) = keeper(Some(vault_with("a")));
    app.update(|cx| assert!(!lock_on_sleep::lock(false, cx)));
    assert!(has(&mut app, "a"));
}

/// The lock lands right after a store was asked for, before the background write ran:
/// `persist` took its own copy, so the secret still reaches the disk.
#[test]
fn locking_during_a_store_does_not_lose_the_write() {
    use crate::lock_on_sleep;
    let (mut app, dir) = keeper(Some(vault_with("a")));
    // Above any ticket the other tests use, so the shared write counter lets this one through.
    app.update(|cx| cx.global_mut::<Keeper>().ticket = 500_000);
    let task = app
        .update(|cx| {
            let task = Keeper::store(login("b"), SecretString::from("pw".to_owned()), cx);
            assert!(lock_on_sleep::lock(true, cx));
            task
        })
        .unwrap();
    assert!(!has(&mut app, "b"));
    let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = saved.clone();
    app.update(|cx| {
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
            .detach();
    });
    app.run_until_parked();
    assert_eq!(*saved.borrow(), Some(Ok(())));
    let reopened = Vault::unlock(
        &dir.path().join("vault.age"),
        SecretString::from("p".to_owned()),
    );
    let reopened = reopened.unwrap();
    assert!(reopened.get(&login("a")).is_some() && reopened.get(&login("b")).is_some());
}
