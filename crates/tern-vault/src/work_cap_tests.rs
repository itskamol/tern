#![allow(clippy::unwrap_used)]

//! A file asking for more scrypt work than tern writes is refused before scrypt runs.
//!
//! Sealing at 19 for real would cost half a GiB and seconds, so each test seals cheaply and
//! rewrites the work factor in the header. If scrypt ran, the doctored header (its MAC no
//! longer matches) would end as a wrong passphrase, not as the cap's refusal, and the 2^40
//! case would not return at all.

use super::*;

fn pass(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

/// `sealed` with the scrypt stanza's work factor rewritten to `log_n`.
fn asking_for(sealed: &[u8], log_n: u8) -> Vec<u8> {
    let tag = b"-> scrypt ";
    let at = sealed.windows(tag.len()).position(|w| w == tag).unwrap();
    let end = at + sealed[at..].iter().position(|&b| b == b'\n').unwrap();
    let space = at + sealed[at..end].iter().rposition(|&b| b == b' ').unwrap();
    [
        &sealed[..=space],
        log_n.to_string().as_bytes(),
        &sealed[end..],
    ]
    .concat()
}

fn refused(result: &Result<impl Sized, VaultError>, asked: u8) {
    assert!(
        matches!(result, Err(VaultError::TooMuchWork { required }) if *required == asked),
        "the cap should have refused a file asking for 2^{asked}"
    );
}

#[test]
fn a_vault_asking_for_more_work_is_refused() {
    let sealed = Vault::new(pass("pw"))
        .with_work_factor(4)
        .to_bytes()
        .unwrap();
    refused(
        &Vault::unlock_bytes(&asking_for(&sealed, WORK_FACTOR + 1), pass("pw")),
        19,
    );
    refused(
        &Vault::unlock_bytes(&asking_for(&sealed, 40), pass("pw")),
        40,
    );
}

#[test]
fn a_synced_bundle_asking_for_more_work_is_refused() {
    let vault = Vault::new(pass("pw")).with_work_factor(4);
    let sealed = vault.seal(b"bundle").unwrap();
    refused(&vault.open(&asking_for(&sealed, WORK_FACTOR + 1)), 19);
    let file = vault.to_bytes().unwrap();
    refused(&vault.reopen(&asking_for(&file, WORK_FACTOR + 1)), 19);
}

#[test]
fn a_pin_wrap_asking_for_more_work_is_refused() {
    let blob = pin::wrap(&pass("passphrase"), "4821").unwrap();
    refused(
        &pin::unwrap(&asking_for(&blob, WORK_FACTOR + 1), "4821"),
        19,
    );
}

#[test]
fn the_refusal_says_so_in_plain_words() {
    let message = VaultError::TooMuchWork { required: 19 }.to_string();
    assert!(
        message.contains("asks for more work than tern allows"),
        "{message}"
    );
    assert!(
        message.contains("2^19") && message.contains("2^18"),
        "{message}"
    );
}

#[test]
fn a_file_written_at_the_limit_still_opens() {
    let mut vault = Vault::new(pass("pw"));
    vault.set(Key::SshKey { name: "k".into() }, pass("secret"));
    let bytes = vault.to_bytes().unwrap();
    let back = Vault::unlock_bytes(&bytes, pass("pw")).unwrap();
    assert!(back.get(&Key::SshKey { name: "k".into() }).is_some());
    assert_eq!(&**vault.open(&vault.seal(b"x").unwrap()).unwrap(), b"x");
}
