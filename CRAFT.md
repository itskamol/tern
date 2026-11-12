# Craft rules

tern is held to a higher bar than the projects it copies from. These rules are enforced by
`cargo clippy -D warnings`, `scripts/check-craft.sh` and CI, not by good intentions.

- **No panics in shipped code.** `unwrap`, `expect`, `panic!`, `todo!` are denied outside tests.
  Errors are typed (`thiserror`) in libraries and reported to the user, never swallowed.
- **`unsafe` is denied workspace-wide.** The exceptions are modules calling a platform or C API
  with no safe binding (`alloc.rs`, `drawables.rs`, `lock_on_sleep.rs`): each allows it in the
  smallest scope and says why every `unsafe` holds.
- **No god-files.** Every source file stays under 800 lines. Split by responsibility, not by size.
- **Files: keep related code together.** Add to the closest related file while it stays under 800 lines;
  split only when a file would cross the limit, along a real seam. Tests live in the module's own
  `mod tests` unless that crosses the limit.
- **Copy, don't invent — and say so.** Copied code keeps a `// Adapted from <repo> <path> (<license>).` header and is listed in `NOTICE.md`.
- **Small seams.** `tern-ssh` knows nothing about GPUI; `tern-term` knows nothing about SSH. The app wires them.
- **Idle means idle.** No per-frame loops; repaint only on new bytes, input or resize. Memory is a feature:
  idle RSS with one tab is measured on every release and must not regress.
- **Memory is designed, not hoped for.** See `docs/perf.md`.
  - Scrollback is bounded (default 5,000 lines) because it dominates: 24 B per cell, and a wrapped
    line's rows are full width. Costs: `docs/perf.md`.
  - Every channel between SSH and UI is **bounded**. A fast `cat` must apply backpressure to the
    socket, never pile up in an unbounded queue.
  - One tokio runtime, on one dedicated thread, for all SSH I/O. No thread per connection.
  - Per-frame work is bounded by the viewport, never by scrollback or history. GPUI's prepaint
    allocates per frame by design (quads, shaped text), so the rule is about scale, not zero.
  - No new dependency without `default-features = false` and only the features used.
  - Optimise only with a measurement before and after, recorded in `docs/perf.md`.
- **Security is not optional.** Host-key changes are a hard stop. Secrets never reach logs.
- **Tests that can fail.** Risky logic (key encoding, host-key checks, parsing) gets tests that were
  mutation-checked: break the code, watch the test go red.
- **Rust practice.** Typed errors with `thiserror` in libraries, `anyhow` only in the binary.
  Borrow instead of clone, `&str`/`&[u8]` in APIs, `#[must_use]` on handles and results that matter.
  GPUI entities for UI state, never `Arc<Mutex<_>>`. `cargo deny` gates licenses (GPL-compatible),
  advisories and sources.
- **Verified by eye.** UI changes are checked by running the app and looking at a screenshot.

## Style

We follow written conventions of well-maintained projects, not taste. Each rule names its source.

- **Formatting:** `rustfmt` with `style_edition = "2024"`, as in Zed and gpui-component, so copied code needs no reformatting.
- **API shape:** the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/): `C-FAILURE` (every public fallible fn
  has an `# Errors` doc section, enforced by `clippy::missing_errors_doc`), `C-DEBUG` (all public types implement `Debug`),
  `C-GOOD-ERR` (error types are `std::error::Error`, messages lowercase without trailing punctuation).
- **No blocking in async or UI code:** Zed's `clippy.toml` pattern, a `disallowed-methods` list that names the replacement.
- **Comments are rare and say why.** No comment restates the code, narrates a step, or names
  the author's intent the code already shows. Public items get one-line docs plus `# Errors` where
  required; a module gets at most a few `//!` lines naming the decision it hides.
- **Module docs say why:** each module opens with `//!` stating the decision it hides and its limits (tokio and ripgrep style),
  not a restatement of the code.
- **Logging:** `tracing` with constant `snake_case` messages and structured fields, never interpolated strings,
  never secrets or session bytes (org logging standard).
- **Tests:** unit tests beside the code in `mod tests`; names state the behaviour (`slow_consumer_keeps_buffer_bounded…`).
