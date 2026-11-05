# ADR 0001: Unlock the vault with Touch ID or the login password, replacing the PIN

Status: proposed, blocked: the gate needs a Developer ID build, which this fork cannot make
(see "Results")

## Context

The PIN seals the vault passphrase into a plain Keychain item (`vault_pin.rs`, `keeper.rs`
`keychain` backend, `set_generic_password`, no access control). 4–8 digits under scrypt 2^16
fall offline in minutes to anyone who can read that item, and the 5-try counter is a deletable
file (`vault-pin.json`). It is a speed bump, not a lock.

## Threat model

Protects against:
- a copied disk, Time Machine backup, or Keychain export: the item is `ThisDeviceOnly`, so it
  never leaves this Mac, and it cannot be read without the person present;
- someone at an unattended, unlocked Mac after the vault auto-locked.

Does not protect against:
- malware running as the user while the vault is open (it reads tern's memory);
- malware that triggers the system sheet and the person approves it;
- anyone who knows the login password. That is the same bar as the Mac itself.

The passphrase stays the root of trust. This replaces typing it, not knowing it.

## Decision

- Store the passphrase in the data-protection Keychain (`kSecUseDataProtectionKeychain`) with
  `SecAccessControl(.userPresence)` and `kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly`.
- Use `.userPresence`, not `.biometryCurrentSet`. It works with Touch ID or the login password,
  so Mac mini, Mac Studio and a closed-lid MacBook work too. Adding a fingerprint does not delete
  the item. The cost: the login password also opens the vault, which is the bar stated above.
- Fallback chain: system sheet (Touch ID, then login password) → tern's own passphrase prompt.
  The passphrase prompt is always reachable: on cancel, on item missing, on `-34018`
  (missing entitlement), and when no passcode is set.
- Threading: no `LAContext` callback. `SecItemCopyMatching` on a protected item blocks its
  thread while the system sheet is up. It runs in `cx.background_spawn`, never on the UI thread,
  and the result reaches GPUI state through the `this.update(cx, …)` that awaits it. This is the
  pattern `Keeper::keychain_then` already uses.
- The PIN is removed. An existing PIN user is offered the switch after their next PIN unlock.
  The sealed item and `vault-pin.json` are deleted when they accept or turn the PIN off.

## Gate (before any product code)

A 50-line prototype, run on three builds:
1. Developer ID + hardened runtime;
2. the "tern local" identity;
3. ad-hoc.

It must show which of these can write and read a `.userPresence` item, and which entitlement or
profile each needs (`keychain-access-groups` may need a Developer ID provisioning profile).
Builds that cannot get `-34018` and hide the option. The current PIN stays until the prototype
passes on the release signing.

### Results (2026-11-05, macOS 27, Apple silicon, security-framework 3.7.0)

The probe writes a generic password with `SecAccessControl(.userPresence)` and
`WhenPasscodeSetThisDeviceOnly`. Only ad-hoc signatures could be tried: this Mac has no
Developer ID and no "tern local" identity.

| Build | Data-protection Keychain | Legacy Keychain + same ACL |
| --- | --- | --- |
| Bare binary, ad-hoc (linker-signed) | `-34018` missing entitlement | `-34018` |
| `.app`, ad-hoc, no entitlements | `-34018` | not run |
| `.app`, ad-hoc + `keychain-access-groups` | killed at launch (exit 137) | not run |

What this means:
- An access-controlled item needs `keychain-access-groups`, even in the legacy Keychain.
- That entitlement is restricted. A signature without an Apple-issued certificate and an
  embedded provisioning profile is not allowed to launch with it. The exit code points to
  AMFI, but no log line was captured.
- So ad-hoc and "tern local" builds can never offer this; only a Developer ID build can, and
  only with a Developer ID provisioning profile from the maintainer's Apple Developer account.

Still open:
- Run the probe signed with the maintainer's Developer ID plus that profile.
- Decide what builds without the entitlement keep. They cannot have the new option, and the
  PIN is the weakness this ADR is about. Passphrase only is the safe answer; keeping the PIN
  there is the convenient one.

## Tests

- The store sits behind a trait, like `keeper_gate::Passphrases`. A fake returns Found,
  Cancelled, NotFound, MissingEntitlement or NoPasscode.
- Unit tests cover every fallback edge, and check that no path leaves the vault open without
  one of the two secrets.
- Manual matrix:
  - a Touch ID MacBook;
  - the same MacBook in clamshell mode;
  - a Mac with no Touch ID (or a VM);
  - after adding a fingerprint;
  - after the login password changes;
  - an ad-hoc build.
