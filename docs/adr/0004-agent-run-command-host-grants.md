# ADR 0004: Replace Agent service allowlists with `run_command` and per-Host Grants

Status: accepted (2026-11-24) — partially supersedes [ADR 0003](./0003-agent-exec-isolated-local-broker.md)

## Context

In daily use, maintaining per-Host systemd `Allowed services` lists added setup friction when the
person is already instructing a local **Agent** directly and needs it to run shell commands and
scripts (`.sh`, `.js`, `.ts`, `.py`, etc.) across Linux, macOS, Alpine, and Docker Hosts.

## Decision

- **Two minimal MCP tools (`list_hosts` and `run_command`).** `service_status` and per-Host
  systemd allowlists are removed. `run_command(host, command, timeout_secs?)` executes `command`
  (capped at `64 KiB`, staying safely below Linux `MAX_ARG_STRLEN` of `128 KiB`) in the remote
  user's default login shell over the existing isolated, short-lived, non-PTY SSH `exec` channel
  (immediate stdin EOF, `forward_agent = false`, no port forwards). Timeout defaults to `30 s`,
  clamped to `1..=300 s`; `stdout` and `stderr` remain capped at `64 KiB` each with truncation
  flags, returned with `exit_code`, `signal`, and `duration_ms`.
- **All Saved connections available when MCP is enabled.** Neither `Allowed services` nor a
  per-Host `Allow Agents` allowlist is kept. When **Enable MCP** is on and not paused, all
  **Saved connections** (`hosts.json`) are visible to Agents; `~/.ssh/config` entries still
  require importing as Saved connections first. `agents.json` (`0o600`, off `tern-sync`) stores
  only `enabled` and `paused`, with no v1 migration layer.
- **Compact Decisions and per-Host in-memory Grants.** A `run_command` call without an active
  **Grant** raises a click-only **Decision** (60 s countdown, compact first-line + `(+N lines)`
  preview with control characters stripped, macOS notification + Dock bounce without focus steal)
  offering `Allow once`, `Allow host until lock`, and `Deny`. `Allow host until lock` creates an
  in-memory **Grant** fingerprinted over the resolved Host settings (`host`, `port`, `user`,
  `proxy_jump`, `proxy_command`) that lets Agents run commands on that Host until sleep, screen
  lock, session switch, `Pause agents`, quit, manual revoke, or Host edit/delete.
- **Activity only; drop `agent-audit.jsonl`.** Execution outcomes appear in tern's quiet
  **Activity** list. The rotating `agent-audit.jsonl` file is removed to avoid building and
  maintaining secret-redaction logic before real usage warrants it.

## Considered options

- **Per-Host `Allow Agents` toggle or per-service allowlists.** Rejected. When the human is
  actively directing the Agent and already gates first contact via **Decision** and `Pause agents`,
  extra allowlist toggles add friction without changing the practical workflow.
- **Adding a `stdin_path` parameter on `run_command` to stream local Mac files.** Rejected. Mixing
  local filesystem reads into remote command execution blurs boundaries, creates a TOCTOU gap
  between what the **Decision** displays and what executes, and still fails for `.ts` or multi-file
  scripts that need on-disk paths. Remote files already run via `run_command` (`node ./script.js`),
  and local file transfer belongs in dedicated SFTP tools later.
- **Forcing `bash -s` over stdin.** Rejected. Breaks on Alpine, BusyBox, and minimal Docker Hosts
  that only ship `/bin/sh`; the remote login shell works everywhere like `ssh host <command>`.
- **Retaining `agent-audit.jsonl` with secret redaction.** Deferred/removed for now to keep the
  iteration small and fast to validate.

## Consequences

- The rest of [ADR 0003](./0003-agent-exec-isolated-local-broker.md) remains in force: isolated
  non-PTY `exec` (never sharing an open terminal **Session**), strict headless **Login ladder**
  (never touching Keychain), Saved connections only, tokenless `agent.sock` (`0o600`), and manual
  `Fill`.
- `GLOSSARY.md`'s **Grant** entry now reflects per-Host command approval rather than per-operation
  repetition.
- SFTP file transfer (`download_file` / `upload_file`) is deferred until `run_command` is validated
  in daily use.
