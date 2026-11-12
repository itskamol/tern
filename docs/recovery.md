# Recovery

How to back up a tern setup and get it back on a reset or replaced Mac. Nothing secret belongs
in tern's config folder except `vault.age`, which is encrypted.

## The chain

```
something you keep offline (e.g. a GPG key, or a password manager's master password)
  → your password manager
    → the vault passphrase
      → tern sync backup  (your private Git repo or gist, age-encrypted)
        → hosts, snippets, settings, keymap, vault
```

Keep the vault passphrase in a password manager you can reach from a new machine. Without it,
the sync backup cannot be opened.

## What lives where

| What | Where | Synced |
| --- | --- | --- |
| Connections, snippets, settings, keymap | `~/Library/Application Support/tern/*.json` | yes, encrypted |
| Saved secrets (vault) | `~/Library/Application Support/tern/vault.age` | yes, encrypted |
| PIN unlock, Keychain unlock | login Keychain items `tern vault pin`, `tern vault` | no, per Mac |
| Approved password commands | `approved-commands.json` | no, per Mac by design |
| Signing identity for local builds | login Keychain identity `tern local` | no; rerun `scripts/make-local-identity.sh` |

## Restore on a fresh Mac

1. Restore access to your password manager.
2. Install tern (download the `.dmg` from Releases, or build: see the README).
3. Open tern → Settings → Sync → your repository → Sync now. When it asks for the vault
   passphrase, copy it from your password manager and paste it.
4. Settings → Vault: turn on Keychain unlock and set the PIN again.
5. The first time each password command runs, approve it ("Run it? yes").
