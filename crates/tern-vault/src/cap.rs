//! The most scrypt work tern accepts on read: [`crate::WORK_FACTOR`], the one it writes.

use age::secrecy::SecretString;

use crate::{VaultError, WORK_FACTOR};

/// The age identity every scrypt read goes through: it refuses, before scrypt runs, a file whose
/// header asks for more than [`WORK_FACTOR`].
pub(crate) fn identity(passphrase: SecretString) -> age::scrypt::Identity {
    let mut identity = age::scrypt::Identity::new(passphrase);
    identity.set_max_work_factor(WORK_FACTOR);
    identity
}

/// `error` as the person's error when it is the cap refusing the file, else what `otherwise`
/// makes of it.
pub(crate) fn read_error(
    error: age::DecryptError,
    otherwise: impl FnOnce(age::DecryptError) -> VaultError,
) -> VaultError {
    match error {
        age::DecryptError::ExcessiveWork { required, .. } => VaultError::TooMuchWork { required },
        other => otherwise(other),
    }
}
