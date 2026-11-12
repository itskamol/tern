# ADR 0002: Messages are Activity, Alert or Decision, not one snackbar

Status: accepted (2026-11-12)

Every message tern shows goes through one snackbar today (`shell_toast.rs`, from seed-design):
70 call sites, of which 31 are failures and 6 carry an action (Undo, Show, Reveal, Restart now,
Download). Each covers the terminal's top lines for 4 s, so "Copied 3 files" gets the same weight as
"Sync failed". We split messages by what they ask of the person:

- **Activity** needs nothing. Shown in the status line, or next to the Activity button in the
  titlebar when there is no status line (Settings open, Files tab in front, status line off, no tab).
  Kept in the Activity list, which the titlebar button opens from anywhere.
- **Alert** is a failure, or a message with one action. It stays a snackbar, moved from the top
  centre to the top right under the titlebar. It stays at the top so the terminal's live last lines
  stay clear, and at the edge so it covers less.
- **Decision** is a question tern cannot go on without. A card that stays until answered.

## Considered options

- **Two kinds (quiet and decision).** Rejected. Failures would pass silently, and Undo needs
  something to click.
- **Move only the 40 Default and Positive messages.** Rejected. It leaves the snackbar doing two
  jobs, and "Saved" vs "Sync failed" would still not be told apart.
- **Alerts at the bottom right.** Rejected. The bottom of the terminal is where the prompt and
  `top` live.

## Consequences

- Each of the 70 call sites names its kind. A new message has to choose one.
- Disconnect is not a Decision. It stays in the terminal, with the reconnect countdown and
  Enter / Esc (`lifecycle.rs`), plus a red dot for that pane on its tab.
- The first Decision is OSC 52. The clipboard setting becomes Allow / Ask / Deny, with Allow
  still the default. Ask shows "Allow once / Always for this host / Deny". Always is stored on
  the saved connection; for a `~/.ssh/config` host it lasts for that session only.
- The updater's progress snackbar becomes an Activity with progress, as transfers are.
