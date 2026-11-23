# ADR 0003: Agent operations run over isolated headless SSH connections through a local broker

Status: accepted (2026-11-23)

## Context

We want local AI coding tools (**Agents**) to inspect hosts managed by tern without receiving SSH
keys, passwords, or direct shell access. Credentials, host-key verification, and connection setup
stay inside tern, and the person approves remote work inside tern's own UI rather than in chat.

The obvious shortcuts all carry hidden traps: `tern-ssh` only opens PTY shells or SFTP today;
`Files` reuses an open terminal `Session`'s transport; `Snippet`s are free-text terminal strings
that collide with shell injection; and `hosts.json` syncs across Macs via `tern-sync`.

## Decision

- **Isolated `exec` connections.** Every Agent command dials its own short-lived, non-PTY SSH
  connection (`forward_agent = false`, no port forwards, stdin closed immediately with EOF) on top
  of an internal authenticated-connection seam extracted inside `tern-ssh`. It never rides on an
  open terminal `Session`. Each run enforces bounded output (64 KiB stdout, 64 KiB stderr) and a
  timeout (default 15 s, caller-configurable and clamped to `1..=60` s).
- **Saved connections only, strictly headless login.** Only **Saved connections** (`hosts.json`)
  can be opened to Agents, never `~/.ssh/config` entries directly. Login reuses the **Login
  ladder** in strict no-prompt mode: already-open vault secrets, already-approved password or
  proxy commands, `ssh-agent`, and unencrypted keys work; anything requiring human interaction —
  a locked vault (even with Keychain opt-in enabled), an unapproved command, an unknown host key,
  a password prompt, or 2FA (`Challenge`) — fails closed with `user_action_required`. Hosts that
  do not use the vault are unaffected when the vault is locked.
- **Concrete typed MCP tools, not a generic action catalog or snippets.** v1 ships MCP over stdio
  (`tern mcp`, staying alive even when tern is closed and connecting per request to a dedicated
  `agent.sock` with mode `0o600`, leaving `tern.sock` untouched). It exposes two static tools:
  `list_hosts()` and `service_status(host, service, timeout_secs?)`. The person allowlists systemd
  unit names per Saved connection (validated as `[A-Za-z0-9:_@.-]`, never starting with `-`); tern
  runs `systemctl show -p LoadState,ActiveState,SubState --value -- <unit>` and returns structured
  state, separating "service is down" from transport or authorization errors (`tern_not_running`,
  `agents_paused`, `denied`, `timed_out`, `user_action_required`, `ssh_error`,
  `unsupported_host`).
- **Tokenless v1 under a single-user threat model.** On a single macOS user account, any local
  process with filesystem access can read MCP configs and tern's settings directory, so per-Agent
  bearer tokens do not add real cryptographic isolation. v1 uses a single local Enable / Pause
  switch plus `0o600` socket permissions, showing MCP's self-reported `clientInfo.name` as an
  unverified label in Decisions and audit records. Honest security claim: the Agent never receives
  SSH credentials; preventing a local Agent from bypassing tern directly requires isolating the
  Agent's own environment.
- **In-memory Grants and non-focus-stealing Decisions.** Agent requests enter tern's **Decision**
  queue (FIFO, 60 s countdown, click-only `Allow once` / `Allow until lock` / `Deny`, displaying
  the exact command with control characters stripped) and alert via macOS notification + Dock
  informational bounce without stealing window focus. Caller disconnect or cancellation dismisses
  a pending Decision before `exec` starts; after `exec` has been sent to the host, disconnect
  records `unknown_outcome`. **Grants** live in memory only, keyed by a fingerprint of the
  resolved host settings (`host`, `port`, `user`, `proxy_jump`, `proxy_command`) and service name;
  they clear on `Sleep`, `ScreenLocked`, `SessionSwitched`, app quit, `Pause agents`, or any
  host/service edit.
- **Local-only config and audit.** Allowed services (`agents.json`, `0o600`) and audit entries
  (`agent-audit.jsonl`, `0o600`, bounded retention) stay strictly off `tern-sync`, like
  `approved-commands.json`. Outcomes also appear in the **Activity** list.

## Considered options

- **Reusing an open `Session`'s transport (as `Files` does).** Rejected. It would let Agents piggyback
  on human-authenticated 2FA sessions, coupling Agent execution to human tab lifecycle and
  blurring user vs. Agent boundaries.
- **Parameterised `Snippet`s or a generic `Action` catalog (`list_actions`/`run_action`).**
  Rejected. `Snippet`s are raw PTY text prone to shell/flag injection, `Action` collides with
  GPUI's own `Action` concept, and a generic catalog forces upfront setup before a person can run
  a single check.
- **Per-Agent pairing tokens (`TERN_AGENT_TOKEN`) in v1.** Rejected for v1. Same-UID processes can
  read each other's config files anyway; tokens add setup and revocation UI before multi-client or
  sandboxed-container Agents need them.
- **15-minute wall-clock Grants or persisted `Always` Grants.** Rejected. Fixed timers interrupt
  longer autonomous sessions while the person is at the Mac; persisted Grants linger across sleep
  and restarts. `Allow until lock` follows physical presence via `lock_on_sleep`.
- **Running a background menu-bar daemon when tern is closed.** Deferred. In v1 `tern mcp` returns
  `tern_not_running` immediately when the app is closed; real usage will show if a menu-bar mode
  is needed.

## Consequences

- `tern-ssh` first gets a standalone refactor extracting authenticated connection setup from
  `session::run_inner` without changing Terminal or `Files` behaviour; `exec` is added on that
  seam in the follow-up PR.
- `tern` does not ship a CLI wrapper (`tern run`) or host-to-host `scp` in v1; both will plug into
  the same local broker and `Decision` queue once the MCP flow is proven (`scp` will enforce
  no-overwrite via `O_CREAT | O_EXCL` rather than relying on pre-flight `stat`).
- `Fill` (typing passwords into a terminal) remains strictly manual and is never exposed to Agents.
