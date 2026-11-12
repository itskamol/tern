//! Writes an empty vault sealed with a passphrase, for `scripts/measure-memory.sh`: a copied tern
//! opens it over its own `TERN_CONFIG_DIR`, so the person's vault and Keychain stay out of it.
//!
//! `cargo run -p tern --example throwaway_vault -- CONFIG_DIR/vault.age PASSPHRASE`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::Path;

use tern_ssh::SecretString;
use tern_vault::Vault;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(passphrase)) = (args.next(), args.next()) else {
        eprintln!("usage: throwaway_vault PATH PASSPHRASE");
        std::process::exit(2);
    };
    Vault::new(SecretString::from(passphrase))
        .save(Path::new(&path))
        .expect("the vault is written");
}
