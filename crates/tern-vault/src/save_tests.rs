#![allow(clippy::unwrap_used)]

//! Saves that overlap: two tern processes, or a sync beside an edit.

use super::*;

fn pass(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

#[test]
fn saves_at_once_each_publish_a_whole_vault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.age");
    std::thread::scope(|s| {
        for i in 0..8 {
            let path = &path;
            s.spawn(move || {
                let mut v = Vault::new(pass("correct horse")).with_work_factor(4);
                let key = Key::SshKey {
                    name: format!("key-{i}"),
                };
                v.set(key, pass(&"x".repeat(4096 * (i + 1))));
                for _ in 0..5 {
                    v.save(path).unwrap();
                }
            });
        }
    });
    let v = Vault::unlock(&path, pass("correct horse")).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "a temp file was left"
    );
}
