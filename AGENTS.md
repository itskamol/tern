# tern

- Rules every change is held to, and the scripts that enforce them: `CRAFT.md`.
- Gates: `scripts/hooks/pre-push` runs what CI runs; wire it once per clone with
  `git config core.hooksPath scripts/hooks`.
- UI changes are checked by eye (`docs/testing.md` § 5): `scripts/dev-check.sh` runs a debug
  build as its own app (bundle id `dev.tern.check`) against a seeded config and a local demo SSH
  server; its header lists what it sets up. Frame cost limits: `docs/perf.md`.
- Releases: `RELEASING.md`. Decisions not to re-open: `docs/adr/`.
- Remotes: `fork` (itskamol/tern) is where `main` is pushed; `origin` (your-moon/tern) is upstream.

## Agent skills

### Issue tracker

GitHub Issues on itskamol/tern (not upstream your-moon/tern). See `docs/agents/issue-tracker.md`.

### Triage labels

The five default roles, each label named as its role. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the root. See `docs/agents/domain.md`.
