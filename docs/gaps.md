# Gaps

What tern lacks against zeron (look and feel), Termius and Tabby (features), and a shipping
macOS app. Found 2026-10-06 by using the app like a customer, comparing screenshots with
zeron, and checking the code. ✓ marks what is done; each open line names how it shows up.

## 1. Look and feel against zeron

| # | Gap | Today in tern | zeron |
| --- | --- | --- | --- |
| ✓ L1 | Icons | Unicode glyphs (◐ ✎ × ⧉ +) | Solar SVG icons everywhere (assets now bundled) |
| ✓ L2 | Sidebar rows | Two lines, ~56 px, address under the name | One line ~32 px: dot, icon, name, faint meta on the right |
| ✓ L3 | Hover actions | Three invisible buttons reserve width, so addresses truncate early | A single "…" on hover; actions in the right-click menu |
| ✓ L4 | Section headers | Plain "Hosts" and "~/.ssh/config" labels | "Sessions" with a collapse chevron |
| ✓ L5 | Sidebar footer | None | Account avatar + settings gear |
| ✓ L6 | Titlebar controls | None next to the traffic lights | Sidebar toggle, back/forward, + |
| ✓ L7 | Menus | Text only | 16 px icon per item, submenu chevrons |
| ✓ L8 | Settings nav | Text only | Icon per section |
| ✓ L9 | Light appearance | Dark only | Light, dark, follow system |
| ✓ L10 | Empty states | Text and shortcuts | Illustration-free but iconised, with primary action buttons — icon, one line and a button on: sidebar and ⌘K picker (no hosts, New connection), snippets page (New snippet) and snippet picker (Add in Settings), file browser empty folder (Upload files). Not connected stays a toast; no-match lines stay plain text |
| ✓ W1 | Wallpaper (asked 2026-10-06) | — | zeron's wallpaper: image as the empty view's hero, effects, visibility, colours from the image (surfaces tinted, text hardened to 4.5:1) |
| ✓ L11 | Status | Dot only | Status line under the panel (zeron: checkout, branch, usage) — tern: host, latency, session time |

## 2. Terminal

| # | Gap | Impact |
| --- | --- | --- |
| ✓ T1 | Find in scrollback (⌘F) | Cannot search output |
| ✓ T2 | Clickable URLs and OSC 8 links | Links must be copied by hand |
| ✓ T3 | Split panes (⌘D, ⌘⇧D) | One session per view |
| ✓ T4 | Font family choice | Geist Mono only |
| ✓ T5 | Cursor style and blink | Fixed block |
| ✓ T6 | Scrollback length setting | Fixed in code |
| ✓ T7 | Copy on select, paste on middle-click (option) | Common Linux habit missing |
| ✓ T8 | Bell: visual flash or Dock bounce | Bell is ignored |
| ✓ T9 | Tab rename and drag to reorder | Tabs fixed by open order |
| ✓ T10 | Broadcast input to several tabs | Cannot run one command on many hosts |
| ✓ T11 | Restore tabs on relaunch | Sessions lost on quit |
| ✓ T12 | Session logging to file | No transcript |
| ~~T13~~ | Local shell tab | Dropped 2026-10-06: tern is for SSH into servers only (owner) |

## 3. SSH

| # | Gap | Impact |
| --- | --- | --- |
| ✓ S1 | ProxyJump / jump host | Only ProxyCommand; bastion setups need ProxyCommand by hand |
| ✓ S2 | Port forwarding (local, remote, dynamic SOCKS) | No tunnels |
| ✓ S3 | Agent forwarding | git on the remote cannot use local keys |
| ✓ S4 | SFTP file browser, drag and drop upload | No file transfer |
| ✓ S5 | Auto-reconnect after network drop or sleep | Manual Enter only |
| ✓ S6 | Keep-alive interval per host | Fixed 30 s |
| ✓ S7 | Host groups / folders and tags | Flat list; poor with many hosts |
| ✓ S8 | Search in the sidebar | Only the ⌘K picker filters |
| ✓ S9 | Recent hosts | No history |
| ✓ S10 | Snippets (saved commands) | Termius core feature |
| S11 | Mosh / serial / telnet | SSH only (out of scope unless asked) |

## 4. Vault and keys

| # | Gap | Impact |
| --- | --- | --- |
| ✓ V1 | Keychain unlock (opt-in) | Passphrase every launch (#15) |
| ✓ V2 | Keys stored in the vault, key generation | Keys must be files on disk (#15) |
| ✓ V3 | List and delete saved secrets | Vault page shows a count only |
| ✓ V4 | Change vault passphrase | Not possible |
| ✓ V5 | Auto-lock after idle | Stays unlocked until quit |
| ✓ V6 | Lock when the Mac sleeps or the screen locks | An open vault stayed open behind a locked screen |

## 5. Sync

| # | Gap | Impact |
| --- | --- | --- |
| ✓ Y1 | Private git repo backend using the user's git (requested) | Gist only |
| ✓ Y2 | Automatic sync (on change, on launch) | Manual Sync now |
| ✓ Y3 | Per-file merge on conflict | Whole-bundle keep-this / use-GitHub |

## 6. Lifecycle states not yet walked

| # | State | Note |
| --- | --- | --- |
| ✓ F1 | Host key changed | Wording tested in tern-ssh; not seen in the app |
| ✓ F2 | Network drop mid-session, Mac sleep and wake | Keep-alive exists; UI not checked |
| ✓ F3 | Keyboard-interactive (2FA) in the app | Tested in tern-ssh only |
| ✓ F4 | Window at its 900×600 minimum, fullscreen | Not checked since tabs and settings |
| ✓ F5 | Many hosts (200+) in sidebar and picker | Scroll and speed not measured |
| ✓ F6 | Crash reported this morning | It was the forced-panic test (same binary UUID d0ce8089); no panic since |
| F7 | 153 MB start-up memory peak | Unexplained (docs/perf.md) |
| ✓ F8 | macOS Reduce motion | Follows the system setting via objc2-app-kit (safe), re-read on activation |

## 6b. Standard (1×) displays

| # | Gap | Impact |
| --- | --- | --- |
| ✓ D1 | Soft text and icons on 1080p monitors | Whole-pixel sizes, 1 px icon strokes, opaque fill and stronger muted text at 1× (Settings → Appearance → Sharper text) |
| ✓ D2 | Waiting-for-input state | A session at a password prompt shows the connecting dot |

## 7. Shipping

| # | Gap | Impact |
| --- | --- | --- |
| ✓ P1 | Signed, notarised .app with an icon (ad-hoc signed; notarising needs TERN_SIGN_ID + TERN_NOTARY_PROFILE) | Runs from Terminal only; not in Dock or Spotlight |
| ✓ P2 | Auto-update | Manual cargo install |
| ✓ P3 | Homebrew cask | No one-line install |
| ✓ P4 | Accessibility (VoiceOver labels) | Unlabelled controls |
