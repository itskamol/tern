# Contributing to tern

Thanks for helping. This file is short on purpose; the rules that matter are enforced by CI.

## Your first contribution

- Pick an [open issue](https://github.com/itskamol/tern/issues) labelled `ready-for-agent` or `ready-for-human` (the labels are described in [docs/agents/triage-labels.md](docs/agents/triage-labels.md)).
- Comment that you are taking it, so two people don't do the same work.
- Your first pull request waits for a maintainer to approve its CI run. That is a GitHub rule for new contributors, not a judgement on the change.

## Before you start

- For anything not on the plan, open an issue first. Bugs need a runnable reproduction; features use the two-question template.
- A change that copies code from another project must keep a `// Adapted from <repo> <path> (<license>).` header and add the source to [NOTICE.md](NOTICE.md). The license must be compatible with GPL-3.0.

## Setting up

Install Rust with [rustup](https://rustup.rs). tern needs Rust 1.99 or newer, which distribution packages usually don't ship. [docs/building.md](docs/building.md) has the build steps for macOS, Linux (with the system packages to install first) and Windows.

| Crate | What it does |
| --- | --- |
| `crates/tern` | The app: window, sidebar, tabs, settings, SFTP panel, reconnect |
| `crates/tern-term` | The terminal view (alacritty_terminal under GPUI) |
| `crates/tern-ssh` | SSH sessions, forwards and SFTP on russh |
| `crates/tern-vault` | The age-encrypted store for passwords and keys |
| `crates/tern-sync` | Encrypted sync to a Git repository or gist |

How they fit together is drawn in [docs/architecture.svg](docs/architecture.svg) (editable source: [docs/architecture.excalidraw](docs/architecture.excalidraw)) and explained in [docs/architecture.html](docs/architecture.html). The [UI spec](docs/ui.md) and [performance notes](docs/perf.md) cover the rest.

## Making a change

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-craft.sh
```

Enable the pre-push hook once per clone so a push that would fail CI is refused locally:

```sh
git config core.hooksPath scripts/hooks
```

All four must pass; CI runs the same commands plus `cargo deny`. [CRAFT.md](CRAFT.md) explains the rules behind them: no panics or `unsafe` in shipped code, no file over 800 lines, bounded memory, comments only where they say why.

- **Tests:** put unit tests beside the code. For risky logic (key encoding, host-key checks, parsing, buffering), break the code on purpose and confirm the test fails before you submit.
- **UI changes:** run the app, take a screenshot, and attach it to the pull request with what you checked.
- **Memory:** if a change could affect memory, measure before and after with `scripts/measure-rss.sh` and record the result in [docs/perf.md](docs/perf.md).
- **Security-sensitive code** (`tern-vault`, `tern-sync`, host-key checks): open an issue before changing it, and expect a slower, closer review.

## Pull requests

- One logical change per pull request, linked to its issue with `Closes #N`.
- Fill in the template: what changed, why, how you verified it (the command and its result), what to review first. If you couldn't run something, say so; that helps more than a vague "tested".
- Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.
- Commit messages: a short summary line, a blank line, then why the change was needed.

## Using AI tools

AI tools are welcome, and using one is nothing to hide. If a tool helped with a change, say so in the pull request description, for example "Drafted with an AI assistant; I reviewed and tested every line", and mention anything you didn't check yourself. It tells the reviewer where to look harder. It never counts against the change.

You are still the author: read every line you submit, run the checks above, and be ready to explain the change in review.

## Review

A maintainer reviews every pull request. If nobody has replied after a week, comment "ping". Review comments are about the change, never about you; reply to each one or push a fix, then ask for another look.

Report security problems privately, as described in [SECURITY.md](SECURITY.md), never in a public issue or pull request.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
