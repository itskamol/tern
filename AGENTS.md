# tern

- Rules every change is held to, and the scripts that enforce them: `CRAFT.md`.
- Gates: `scripts/hooks/pre-commit` checks formatting and the craft rules in seconds;
  `scripts/hooks/pre-push` runs what CI runs. Wire both once per clone with
  `git config core.hooksPath scripts/hooks`.
- Testing: how tests are judged, scoped, and mutated: `docs/testing.md`.
- UI changes are checked by eye (`docs/testing.md` § 5): `scripts/dev-check.sh` runs a debug
  build as its own app (bundle id `dev.tern.check`) against a seeded config and a local demo SSH
  server (`agent-shot <out.png>` captures an Agent Decision card in one step); its header lists
  what it sets up.
- Local install & MCP smoke: `scripts/install-local.sh` builds, preserves `"tern local"` signing,
  replaces `/Applications/tern.app`, verifies `tools/list`, and lists stale `tern mcp` PIDs;
  `scripts/mcp-smoke.py` probes `tern mcp` over stdio with `TERN_LOG=info` stderr logs.
- Memory and frame budgets, and how to measure them: `docs/perf.md`.
- Releases: `RELEASING.md`.
- Remotes: `fork` (itskamol/tern) is where `main` is pushed; `origin` (your-moon/tern) is upstream.
- CI does not run on the fork, so a red check there says nothing: before merging a pull request,
  `scripts/check-merge.sh <number>` runs the push gates on the merged tree.

## Agent skills

### Issue tracker

GitHub Issues on itskamol/tern (not upstream your-moon/tern). See `docs/agents/issue-tracker.md`.

### Triage labels

The five default roles, each label named as its role. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the root. See `docs/agents/domain.md`.
