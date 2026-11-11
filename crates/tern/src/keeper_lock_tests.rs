#![allow(clippy::unwrap_used)]
//! Opening and locking the keeper's vault (`need_vault`, `try_unlock`, and the lock the OS
//! signals ask for), against a keeper over a temp vault file.
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

/// The Mac sleeps while a Keychain read is still running (its system dialog may be on screen
/// for minutes): the read must not open the vault once it returns, as the lock asked for it to
/// stay shut. What waited for it still runs, and finds the vault locked.
#[test]
fn a_keychain_read_running_when_the_mac_sleeps_does_not_reopen_the_vault() {
    use crate::keeper_gate::{Need, Read};
    use crate::lock_on_sleep;
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    let ran = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = ran.clone();
    app.update(|cx| {
        let k = cx.global_mut::<Keeper>();
        let need = k.gate.need(
            true,
            true,
            Box::new(move |cx: &mut gpui::App| {
                seen.set(Keeper::locked(cx));
            }),
        );
        assert_eq!(need, Need::Start);
        let since = cx.global::<Keeper>().locks;
        lock_on_sleep::lock(true, cx);
        Keeper::keychain_done(
            Read {
                vault: Some(vault_with("a")),
                refused: false,
            },
            since,
            cx,
        );
    });
    assert!(!has(&mut app, "a"));
    assert!(ran.get(), "the waiter ran and saw the vault locked");
}

/// Runs `try_unlock` to the end and returns what it said.
fn try_unlock(app: &mut gpui::TestApp, typed: Typed) -> Tried {
    let said = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = said.clone();
    app.update(|cx| {
        let task = Keeper::try_unlock(typed, cx);
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
            .detach();
    });
    app.run_until_parked();
    said.borrow_mut().take().unwrap()
}

fn passphrase(p: &str) -> Typed {
    Typed::Passphrase(SecretString::from(p.to_owned()))
}

#[test]
fn a_need_asks_for_the_pin_while_one_is_set_else_the_passphrase() {
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    let need = |app: &mut gpui::TestApp| app.update(|cx| Keeper::need_vault(cx, 7, |_, _| {}));
    assert_eq!(
        need(&mut app),
        Pending::Now(Opening::Ask(Ask::Passphrase), 7)
    );
    app.update(|cx| cx.global_mut::<Keeper>().pin = true);
    assert_eq!(need(&mut app), Pending::Now(Opening::Ask(Ask::Pin), 7));
    // A wiped PIN is forgotten: the next need goes straight to the passphrase.
    app.update(|cx| Keeper::unlock_done(vault_pin::Outcome::Wiped, 0, cx));
    assert_eq!(
        need(&mut app),
        Pending::Now(Opening::Ask(Ask::Passphrase), 7)
    );
    app.update(|cx| Keeper::adopt(vault_with("a"), cx));
    assert_eq!(need(&mut app), Pending::Now(Opening::Open, 7));
}

#[test]
fn a_typed_passphrase_opens_the_vault_for_every_tab() {
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    assert_eq!(
        try_unlock(&mut app, passphrase("wrong")),
        Tried::Failed("Wrong vault passphrase.".into())
    );
    assert!(!has(&mut app, "a"));
    assert_eq!(try_unlock(&mut app, passphrase("p")), Tried::Opened);
    assert!(has(&mut app, "a"));
}

/// Another tab opened the vault and saved to it while this unlock ran: the open one stays.
#[test]
fn an_unlock_never_replaces_a_vault_opened_meanwhile() {
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    app.update(|cx| Keeper::adopt(vault_with("typed-elsewhere"), cx));
    assert_eq!(try_unlock(&mut app, passphrase("p")), Tried::Opened);
    assert!(has(&mut app, "typed-elsewhere") && !has(&mut app, "a"));
}

#[test]
fn an_unlock_that_outlives_a_lock_leaves_the_vault_shut() {
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    let said = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = said.clone();
    app.update(|cx| {
        let task = Keeper::try_unlock(passphrase("p"), cx);
        Keeper::lock(cx);
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
            .detach();
    });
    app.run_until_parked();
    assert!(matches!(said.borrow().as_ref(), Some(Tried::Failed(_))));
    assert!(!has(&mut app, "a"));
}

/// A need that arrives while the Keychain read runs must keep its work (a server's password
/// prompt, whose reply closes the login if dropped) and get it back when the read answers,
/// also when the read did not open the vault and the person is asked next.
#[test]
fn work_waiting_on_a_keychain_read_is_handed_back_when_it_answers() {
    use crate::keeper_gate::{Need, Read};
    let (mut app, dir) = keeper(None);
    vault_with("a").save(&dir.path().join("vault.age")).unwrap();
    let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = got.clone();
    app.update(|cx| {
        // A read is already running, as the first need of the run started it.
        let k = cx.global_mut::<Keeper>();
        k.keychain = true;
        assert_eq!(
            k.gate.need(true, true, Box::new(|_: &mut gpui::App| {})),
            Need::Start
        );
        let need = Keeper::need_vault(cx, "prompt", move |work, cx| {
            sink.borrow_mut().push((work, Keeper::locked(cx)));
        });
        assert_eq!(need, Pending::Waiting);
        assert!(
            got.borrow().is_empty(),
            "held, not run, while the read runs"
        );
        let since = cx.global::<Keeper>().locks;
        Keeper::keychain_done(
            Read {
                vault: None,
                refused: true,
            },
            since,
            cx,
        );
    });
    assert_eq!(*got.borrow(), [("prompt", true)]);
}
