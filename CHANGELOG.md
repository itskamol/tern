# Changelog

All notable changes to tern are recorded here. The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, a minor version may break things.

## [Unreleased]

### Added

- A Files panel beside the session (⇧⌘O): the focused pane's host, starting at the folder its shell reports (OSC 7) and following it as you `cd`. Opening a folder by hand switches to browsing, with a way back to the shell's folder. Each pane keeps its own folder, selection and browsing when focus moves. Without OSC 7 it opens at home, says so and offers a line to copy into the shell's rc file. A dropped pane shows Reconnect, one still logging in says it is waiting. Rows have name, size and modified columns, a clickable path, a Dotfiles toggle, double-click to open a folder and Download to ~/Downloads, over the pane's own connection. The terminal narrows to make room, and the width drags within the sidebar's range and is remembered. The Files tab moves to ⌥⌘O.
- Search Settings: one field at the top of the Settings sidebar searches every section (⌘F goes to it). Matching settings show in place with their own controls, grouped by section, with the matched words highlighted; "No setting matches" when nothing does. What you type is plain text.
- Hold ⌥⌘ in a split tab and each pane shows its number and host, in reading order; ⌥⌘1–9 moves to that pane. ⌥⌘ and the arrows still move between panes. The chords are fixed, listed under Settings → Shortcuts → Fixed and refused for other shortcuts.
- Zoom a pane: ⇧⌘↩ (Shell → Zoom Pane) makes the focused pane fill its tab, and again brings the split back as it was. The tab says "zoomed"; every session keeps running behind it. Splitting, closing or moving to another pane ends the zoom, and a relaunch opens the split.
- Each tab shows one status dot per pane, in reading order, instead of one for the tab. A pane whose session dropped is red, so a dead pane in a background tab is seen; VoiceOver says how many panes are disconnected.
- Ask before a program on the server sets the clipboard. Settings → Terminal → "Programs can copy to the clipboard" is now Allow (the default, as before), Ask or Deny. In Ask, the copy waits on a card at the top right until you choose Allow once, Always for this host or Deny. Always is kept with a saved connection; for a `~/.ssh/config` host it lasts while its tabs are open. A settings file from before keeps its choice.
- Settings → Appearance and Terminal show a sample terminal that follows font, size, theme, cursor and line height as you change them; it takes no input and opens no connection.

### Changed

- Messages start to come in two kinds. An Activity ("Copied 3 files", a host change, a program setting the clipboard) shows quietly in the status line for 4 s, or next to a new Activity button at the right of the titlebar when there is no status line (Settings open, Files tab in front, status line off). An Alert (a failure, or a message with Undo) is the snackbar, which now sits at the top right instead of the top centre. The Activity button opens the list of both, newest first; Escape or a click elsewhere closes it. Every message now names its kind. A running update download or copy shows as one Activity that updates in place, instead of a snackbar that stays for minutes.
- New terminals keep 5,000 lines of scrollback, down from 10,000. A tab full of long, wrapping log lines (pm2, docker logs) took about 25 MB instead of 80 MB in a measured run. A setting saved before keeps its value; Settings → Terminal changes it.

### Fixed

- Keep both keeps a compressed file's type: `dump.sql.gz` becomes `dump 2.sql.gz` and `release.tar.gz` becomes `release 2.tar.gz`, not `dump.sql 2.gz`. A rotated log keeps its number: `api.log.1.gz` becomes `api.log.1 2.gz`. The same applies to `.bz2`, `.xz`, `.zst` and `.lz4`.
- Activity Monitor no longer counts tern's own memory as GPU memory. The allocator labelled its memory "IOAccelerator"; it now shows as "App-Specific Tag 1".

## [0.3.1] - 2026-11-12

### Fixed

- Updates are found again. The app never knew its GitHub repository, so the daily check and Check for Updates… did nothing, without a word, in 0.2.0 and 0.3.0. From 0.3.1 tern finds, downloads and installs new releases by itself; a tern at 0.2.0 or 0.3.0 needs one `brew upgrade --cask tern` to get here.

## [0.3.0] - 2026-11-12

### Added

- Check for updates now: tern → Check for Updates… (after About) and a Check now button on Settings → About's Version row check at once, even with the daily check off. A toast and the Version row say what happened: checking, up to date, downloading, ready (Restart now), available (Download) or why the check failed.
- Splits can be resized: drag the divider, ⌃⌘ and an arrow move it in 5% steps, a double-click puts it back to half and half, and Shell → Equalize Panes evens a whole tab. A pane keeps at least 20 columns and 5 rows; a split that would go under says so instead. The remote hears the new size once the drag ends.
- Split with another host: ⌥⌘D and Shell → Split Right/Down with Host… open the host picker, and a host dragged from the sidebar onto one half of a pane splits that side. ⌘D and ⇧⌘D still split with the same host.
- With more than one pane, the focused pane has an accent border and the others are dimmed; the tab's title follows the focused pane's host with a "+N" for the others, unless you renamed the tab. Shell has Focus Pane ←/→/↑/↓, Close Pane and Equalize Panes.
- Fill in an unfocused pane: a pane waiting at a password prompt shows "Password asked · ⌘\\", the hint names its host, and ⌘\\ moves to that pane and fills it once. With two panes waiting, ⌘\\ asks you to click the one to fill. Fill is still one key press, never automatic.
- Command history: tern keeps the commands you run on each host, on this Mac only and never synced, up to 5000 per host. ⌘R opens it over the session with fuzzy search, newest first with repeats merged; ⌘A switches between this host and all hosts. Enter puts the line at the prompt, ⌘Enter also runs it, ⌫ deletes it, Esc closes. Lines with password, passwd, secret, token, api_key, apikey or private_key, `--password` or `-p<value>`, lines that start with a space, anything typed at a password prompt, in a full-screen program or while broadcasting are left out. Shells that mark their prompts (OSC 133) give the exact command; other shells are read from the screen. Settings → Command history clears one host's history or all of it.
- A Files tab (⇧⌘O) with two panels side by side, each the Mac's own files or one host's. A host panel uses the login of an open session, or logs in by itself and asks the host key, password and vault questions in the panel. Files and whole folders copy by dragging between panels or with Copy, also host to host; either panel can make a folder, rename and delete (the Mac's go to the Trash, a host's cannot be undone). Files dragged in from Finder upload to the panel they are dropped on.
- Copies are written as `name.part` and renamed when whole, removed on cancel or failure. A transfer list shows progress, speed and time left with Cancel and Cancel all; at most two copies run at once and one per destination host. A name that is already taken asks Replace, Keep both or Skip, with "apply to all". Copies keep going after the tab is closed, and quitting with copies running asks first.

### Changed

- The window keeps at most two Metal drawables instead of three, which saves one window-sized buffer (about 18.6 MB in a 1320×880 window, about 31 MB full screen) while output streams or the window resizes.
- Opening the vault, the sync bundle or the PIN-wrapped passphrase refuses a file that asks for more scrypt work than tern writes (2^18): "this file asks for more work than tern allows". A synced-in file can no longer make tern allocate gigabytes.
- `scripts/measure-memory.sh` replaces `scripts/measure-idle.sh`: it measures the no-tab, one-connected-tab and start-up peak scenes in a copied bundle and fails a release over its ceilings (RELEASING.md step 5).

### Fixed

- Sync no longer writes a merged `hosts.json` back as version 1: the merge keeps the highest version of its inputs, keeps keys it does not know, and refuses a version newer than this build can merge.

### Removed

- The SFTP side panel and its Browse files shortcut slot: ⇧⌘O now opens the Files tab, which does what the panel did and more.
- The wallpaper: the picture behind the window, Settings → Appearance → Wallpaper and its gallery, the effects (scanlines, ASCII, halftone, dither), theme colours taken from the picture, the seven built-in pictures (about 3.8 MB of the app) and the cache of rendered pictures. The window is always macOS's own blur, and opaque under Reduce transparency. The `wallpaper*` keys in `settings.json` are no longer read; a file that still has them loads, and the next save drops them. The `wallpapers/` folder (copies of your pictures and the cache) in tern's data folder is no longer used and can be deleted.

## [0.2.0] - 2026-11-11

### Added

- tern updates itself on macOS. The daily update check downloads a newer release, keeps it only if it is the same app at the announced version and signed with the same certificate as the installed tern, and installs it when tern quits or on "Restart now" (a toast, and Settings → About). A build signed ad hoc cannot be matched, so it keeps offering the release page. Settings → About → Check for updates turns both off.
- The app bundle carries the licences: `LICENSE`, `NOTICE.md` and `THIRD-PARTY.txt` in `tern.app/Contents/Resources`. The last is written at bundle time by `scripts/third-party.py`, offline, from `cargo metadata`: every crate linked into the binary with its licence and each distinct licence text once.
- `HashKnownHosts yes` in `~/.ssh/config` is honoured: a host key accepted in tern is written to known_hosts with the host name hashed (`|1|salt|hmac`), as `ssh` writes it, so the file does not list the hosts you connect to. A new known_hosts file is created 0600.
- On macOS the open vault locks when the Mac goes to sleep, the screen locks or another user takes over the session, like Lock now: open sessions keep running and a secret is asked for again. Settings → Vault → "Lock the vault when the Mac sleeps or the screen locks" turns it off.
- From the sidebar's search box (⇧⌘F), ↑ and ↓ move through the hosts shown, skipping folded groups, and Return connects to the selected one rather than always the top match.
- A full menu bar: tern (About, Services), File, Edit, View, Shell, Window and Help, so every shortcut can be found in a menu; ⌃⌘F enters full screen.
- ⌘Z and ⇧⌘Z undo and redo adding, editing, removing and moving hosts (Edit → Undo); the toast's Undo is the same step. An edit is not undone once sync or an import has changed the list since.
- Hosts in the sidebar can be dragged: onto another host to take its place (and its group), or onto a group's header to move to the end of that group. The order is saved in `hosts.json`.
- A session stopped at a password, passphrase or host-key question shows an amber ring instead of the pulsing connecting dot, and the status line says "Waiting for your answer" (#31).
- README keyboard table lists Windows and Linux shortcuts alongside macOS (#41).
- Once the tabs no longer fit, a button at the end of the strip lists every tab with the one in front checked; choosing one shows it. It appears only while the tabs overflow.
- `TERN_FRAME_STATS=1` logs once a second the frames drawn and the mean and worst time of the shell's render and of a terminal's prepaint (`frame_stats` in the log). Off, it costs one atomic read per frame.
- The terminal takes input from the system's input methods: Chinese, Japanese and Korean composition shows underlined at the cursor and is sent when committed, dead keys (Option+e, then e gives é) and the character picker work, and the candidate window sits at the cursor. Keys the terminal maps itself (arrows, ctrl combinations, Option as Meta) are unchanged; with Option as Meta off, Option composes characters. Holding a letter that has accents offers them, as in other Mac apps.
- With Option as Meta on, an Option combination that your layout makes a printable ASCII character is typed instead of sent as Meta: @ on ⌥L, [ on ⌥5 and | on ⌥7 on a German keyboard. On US layouts ⌥ keys are still Meta.

### Changed

- One tern runs per settings folder. `tern web-01` while tern is open hands the host to the open window, which connects and comes to the front, instead of starting a second process whose saves to `hosts.json` and the vault would overwrite the first's. A tern that holds the folder but does not answer is reported rather than run beside.
- A program on the server setting the clipboard (OSC 52, as tmux and vim copy over SSH) is no longer silent: a toast names the host and how much it copied, at most once a minute per session, and Settings → Terminal → "Programs can copy to the clipboard" turns it off (a blocked attempt says where to allow it). Reading the clipboard stays refused.
- ⌘K clears the scrollback and the screen above the prompt, as in Terminal.app, iTerm2 and Ghostty (Edit → Clear Scrollback; Ctrl+Shift+K on Windows and Linux). The host picker moves to ⌘T, the new-tab shortcut of other Mac apps (Ctrl+Shift+T); a shortcut rebound in Settings keeps its binding.
- VoiceOver reads host rows (name, address, connection state), group headers (name, count, collapsed or expanded), session tabs (title, badges, state, selected) and the sidebar's Sync and empty-state buttons.
- Dragging a host shows a line where it will land instead of tinting the row, and a collapsed group opens after the drag rests on its header for a moment.
- The BCAST and LOG badges are 10 pt and a tab's close button is 20 x 20 pt.
- Empty states show an icon, one line and a primary button: no hosts (sidebar and ⌘K picker), no snippets (Settings page and the snippet picker, which now opens empty instead of showing a toast) and an empty folder in the file browser (#32).
- Scrollback rows keep only the cells they use, so a full history of short lines takes about a sixth of the memory (#89).
- Starting with a wallpaper already rendered no longer decodes the picture, which took about 35 MB of memory at launch; the colours are cached beside it (#33).
- A streaming terminal costs less per frame: a row is shaped only up to its last visible glyph, not to the window's right edge. In a headless run (243 x 59 grid, 30 new lines a frame) a frame took 3.0 ms, now 1.8 ms.
- A terminal that did not change is replayed, not rebuilt, when the window around it redraws (a toast, a fading hover, a streaming split pane): 0.7 ms a frame, now 0.25 ms in the same run.
- The panel opacity over a full-window wallpaper is worked out when the palette, the default scheme or the picture changes, not several times every frame.

### Fixed

- A first sync on a Mac with no vault, with a passphrase that does not open the remote's vault, no longer leaves a new local vault under that passphrase behind; before, the right passphrase then failed with "Wrong vault passphrase."
- When a jump host's key changes, the hint under "connection closed" gives the `ssh-keygen -R` line for that jump host, not for the server behind it.
- A vault passphrase or PIN that finishes checking only after the vault was locked (the Mac slept meanwhile) leaves the vault shut and says so, instead of opening it.
- Unlocking from Settings → Vault no longer replaces a vault another tab opened (and saved to) while the unlock ran; like the terminal's unlock, it keeps the open one.
- A Keychain read still running when the Mac slept or the screen locked (its system dialog left on screen, say) no longer opens the vault once it returns: the lock wins, and the next need asks again.
- A yes to running a saved password, fill or proxy command now holds for the rest of that tab's session even when `approved-commands.json` cannot be written, so an automatic reconnect does not stop at the question again; before, only the proxy question behaved this way. The password command that runs is always the text the question showed, even if the connection was edited while it was on screen.
- A sync pull checks what it downloaded before using it. Each push now seals a manifest beside the files (every item's hash, the vault's included, and a bundle number), so a remote whose `vault.age` was swapped for another, whose `tern-meta.json` was edited, or that was rolled back to a bundle older than one this Mac already synced is refused with a message and nothing is written. A bundle pushed by an older tern is still taken when it matches its meta, but cannot be dated.
- The log folder (`~/Library/Logs/tern`) is readable by you only, and an older log or folder another user could read is made private at launch; `panic.log` is created 0600 like session logs. App logs name hosts and users.
- A keyboard-interactive login (a code or OTP prompt) shows the server's name, instructions and prompt text with control characters escaped (`^[`), so a server cannot move the cursor, retitle the tab, write the clipboard or draw a fake prompt there.
- A saved connection's ProxyCommand runs only once this Mac has approved it, as password commands already did: `hosts.json` also arrives by sync and import, and its ProxyCommand ran through `sh -c` without a word. The first connect asks in the terminal; one saved from the form, or imported from this Mac's ~/.ssh/config, counts as approved, and a `~/.ssh/config` host runs its own as ssh does.
- Saving a password to the vault no longer makes it look locked: the open vault used to be taken out while scrypt ran (about a second, longer during a sync), so another tab asked for the passphrase again and an edit made then could be lost. Every write now goes through one ordered path, and a sync works on a copy, keeping a change made here while it ran. Two saves at once (two tern processes) no longer share a temp file, which could leave a vault that does not open.
- A new host's key can be read at leisure: the 15-second connect timeout no longer counts the time its fingerprint question is open, which used to drop the connection while it was being compared (jump hosts too).
- A file downloaded over SFTP stays in ~/Downloads: a name the server sends with a `/`, `..` or an absolute path is refused instead of written where it points.
- A paste can no longer end bracketed paste early: an end marker hidden inside another (`ESC[20ESC[201~1~`) is removed too, so the rest of the text is not run as typed commands.
- With many tabs the strip scrolled sideways (wheel, two-finger swipe) but nothing showed it: the tabs past the window's edge were out of sight with no hint they were there. The button above lists them, and the tab in front now stays on screen when tabs open, close, move or switch, and when the window is made smaller.
- A dark terminal scheme under the light appearance and a full-window wallpaper no longer turns the terminal a flat grey and hides the picture: the wallpaper's tint leans a scheme toward its own lightness rather than the appearance's, the status line under it takes the palette that suits that background, and small secondary text is plain muted while the panels are see-through, so the panels no longer sit at their most opaque.
- Settings is readable under a wallpaper with a dark terminal scheme in the light appearance: the page sat on the active tab's scheme background, a near-black veil over the picture that washed out its headings and descriptions. It now sits on the app's own surface, like the sidebar.
- Text fields edit by word and line as other macOS fields do: ⌥←/→ moves by word, ⇧⌥←/→ and ⇧⌘←/→ select, ⌘←/→ go to the ends, ⌥⌫ and ⌘⌫ delete, a double click selects a word and a triple click everything. Masked fields count as one word.
- The sidebar behaves like Finder's and Mail's: pointing at a host or a group changes nothing (no hover fill, no "..." button, arrow cursor), only the active or keyboard-selected host is filled, and a host's actions are in the menu a right-click or Control-click opens. The address tooltip shows only where the row cuts the address short.
- The offer to save a password in the vault no longer shares its line with a shell prompt that arrives late: what the server writes while tern asks is shown once the question is answered (#105).
- Keychain unlock no longer reads the Keychain or runs scrypt at launch: the Keychain is asked once, the first time tern needs the vault, and a refusal falls back to the prompt without asking again (#91).
- Clicking a session tab while Settings is open now closes Settings and shows the session, with typing going to its terminal (#63).
- SFTP panel file sizes just under a unit boundary round up to the next unit (e.g. `1.0 MB` instead of `1024 KB`) and sizes just under 10 omit decimals (`10 KB` instead of `10.0 KB`) (#39).
- Plain-text `ftps://` links are now detected and clickable in terminal output (#40).
- Secondary text (sidebar host addresses and section labels) keeps 4.5:1 contrast in both themes, and tern follows macOS Increase contrast (full-strength secondary text, stronger borders) and Reduce transparency (opaque window and panels, no wallpaper showing through).

### Security

- Dependencies updated past RUSTSEC-2026-0383 (`webpki-root-certs` trusted distrusted emSign intermediates), RUSTSEC-2026-0385 and -0387 (timing leaks in `ctutils` and `crypto-bigint`) and RUSTSEC-2026-0386 (`tokio` `AsyncFd` I/O safety), and off the yanked `async-executor` 1.14.0 and `crypto-bigint` 0.7.5.

## [0.1.3] - 2026-10-11

### Fixed

- Host key verification now matches OpenSSH: tern accepts a server when any key recorded for it in `known_hosts` matches, so a stale earlier entry of the same type no longer reports the key as changed and refuses the connection (#36). The changed-key message's line number counts comment and blank lines, so it points at the offending entry as an editor shows it (#37).

## [0.1.2] - 2026-10-06

### Added

- Windows (x86_64) and Linux (x86_64, aarch64) builds, as a **preview**: they build and pass the test suite in CI, but have not yet been run on a real Windows or Linux desktop. On those systems saved secrets go to Windows Credential Manager or the Linux Secret Service, config lives in `%APPDATA%\tern` or `~/.config/tern`, password commands run through `cmd /C` on Windows, the Windows OpenSSH agent pipe is used, and shortcuts use Ctrl+Shift / Ctrl+Alt so plain Ctrl keys still reach the shell (table in `crates/tern/src/keymap.rs`).

### Fixed

- Skipping "Vault passphrase (Enter to skip)" holds for the tab. It was asked again at the server's password prompt, after each refused password, and by every automatic reconnect, which then waited on the question instead of reconnecting.
- Sync's `git` ignores `GIT_DIR` and related variables, so a sync started from inside a git hook cannot act on the wrong repository.

## [0.1.1] - 2026-10-06

### Changed

- New app icon: a flat white bird in flight on near-black, replacing the glossy dusk-sky tern.

## [0.1.0] - 2026-10-06

The first release: a daily-driver SSH client for macOS.

### Highlights

- Saved connections with groups, tags, search, a ⌘K picker and a one-time import from `~/.ssh/config`.
- Logins from SSH keys, a per-connection password command (for example gopass), or the vault. ⌘\ fills `sudo` and other password prompts on request.
- Vault (age, scrypt) for passwords and SSH keys, with PIN unlock, macOS Keychain unlock and an idle lock.
- Terminal: tabs (rename, reorder, restore), split panes, find, links, snippets, broadcast input, session logs, a status line with latency, an SFTP browser and port forwards.
- SSH: ProxyJump, agent forwarding, keep-alive per host, two-factor logins, automatic reconnect, and a hard stop on changed host keys.
- Encrypted sync to your own Git repository, merged per file and per host.
- zeron-style look: frosted panels over a wallpaper (seven public-domain artworks built in), light/dark/system, a sharper mode for 1× monitors, and the Arctic tern icon.
- Signed with a Developer ID and notarised.

### Added

- `tern`: vault PIN (Settings → Vault → Unlock with a PIN). A 4 to 8 digit PIN seals the vault passphrase (age scrypt) in the login Keychain item `tern vault pin`; every unlock prompt asks "Vault PIN" first, with Enter or "Use passphrase instead" for the passphrase. 5 wrong PINs in a row (counted in `vault-pin.json`, not synced) delete the item; changing the vault passphrase removes it.
- `tern`: "Fill password" (⌘\\ by default). Types the saved password plus Enter into the focused pane, only when the cursor line ends in a password prompt (`[sudo] password for me:`, `Password:`, a key passphrase), and only on the key press; a hint toast names the shortcut when such a prompt appears. The password comes from the connection's `sudoPasswordCommand`, else its `passwordCommand`, else the vault entry for the host; commands need the same one-time approval as the password command.

- `tern-ssh`: SSH sessions over russh with ssh-agent, key-file (with passphrase), password and keyboard-interactive login; `~/.ssh/config` hosts and `ProxyCommand`; `known_hosts` checking where a changed key refuses the connection and an unknown key asks first.
- `tern-ssh`: bounded output path. Output is coalesced into 64 KiB chunks over an 8-deep queue, and the session stops reading the socket when the UI falls behind; a 16.9 MB burst peaks at 6 MB RSS. Input reports `Busy` instead of queueing without limit.
- `tern-term`: a GPUI terminal view fed by any byte stream, with xterm-256color and truecolor, text attributes, wide characters, box drawing painted as paths, mouse reporting, selection and bracketed paste.
- `tern-ssh`: password, keyboard-interactive and encrypted-key logins are tested against an in-process russh server, including retries after a wrong answer and cancelling.
- `tern`: the app window, with zeron's chrome: transparent 38 pt titlebar, traffic lights at (14, 14), frosted shell over a blurred background, a 10 pt rounded main panel, bundled Geist fonts and structured JSON logs (`TERN_LOG`). Idle: 66 MB.
- `tern`: a host sidebar listing every concrete `Host` in `~/.ssh/config`; clicking one opens its SSH session in the main panel, with a status dot and the host in the titlebar. `tern <host>` connects at start.
- `tern`: login questions (unknown host key, password, key passphrase, keyboard-interactive) are asked inside the terminal with OpenSSH's wording; Backspace edits, Ctrl-C cancels.
- `tern`: session tabs in the titlebar with status dots; clicking a host with a live tab switches to it. ⌘W closes, ⌘1–9 and ⌘⇧[ / ⌘⇧] or ⌃Tab switch, middle-click closes.
- `tern`: a closed tab says "Press Enter to reconnect"; Enter dials the same host again in the same terminal, keeping its scrollback. Clicking a host whose tab is closed reconnects that tab instead of opening another.
- `tern`: ⌘K host picker: fuzzy search (nucleo, as zeron and helix) over host names and addresses; ↑/↓ or ⌃N/⌃P move, Enter connects, Esc or a click outside closes. Card, header, footer and list height are zeron's command palette values.
- `tern`: shortcuts work before the first tab is open and after the last one closes; the window itself now holds focus when no terminal does.
- `tern`: settings in `~/Library/Application Support/tern/settings.json` (or `$TERN_CONFIG_DIR`), written atomically; a corrupt file falls back to defaults.
- `tern`: ⌘B collapses and expands the sidebar over 200 ms; drag its edge to resize between 224 and 400 pt, double-click the edge to reset to 256. Width and collapsed state persist.
- `tern`: ⌘= / ⌘− / ⌘0 change the terminal font size for every tab (8–32 pt, default 13, as zeron); the grid and the remote PTY follow, and the size persists.
- `tern`: a connecting host's status dot breathes (zeron's 2.4 s pulse), synced between its tab and sidebar row; `"reduceMotion": true` in settings holds it still.
- Docs: idle memory baseline in `docs/perf.md`: 49 MB footprint (87 MB RSS) with one connected tab, about 5 MB over an empty window.
- `tern-vault`: one passphrase-encrypted file for SSH passwords (per `user@host:port`) and key passphrases (per key path), in the age format with an scrypt recipient. A wrong passphrase and a tampered file are both refused, saves are atomic, and plaintext is zeroed after use. Not wired into the app yet.
- `tern`: saved logins. After a password or key passphrase is typed and the login works, tern offers to keep it in the vault, creating one with a repeated passphrase if there is none. Later logins answer from the vault (unlocked once per run, Enter skips), try each saved secret once, and fall back to asking. Unlock and save run off the UI thread.
- `tern-ssh`: `password_server` example, a 127.0.0.1-only server that accepts one password, for checking login flows.
- Docs: README lists what the app does today and what is not done yet.
- `tern`: add, edit and remove connections in the app (⌘N or + in the sidebar). They are kept in `hosts.json`, never in `~/.ssh/config`, and a password typed in the form goes to the vault (created on first use). `~/.ssh/config` hosts stay listed, read-only, with Duplicate to edit; a connection with the same name takes their place.
- `tern`: text fields (adapted from gpui's input example) with a masked mode that never copies its secret.
- `tern`: a crash is written to `~/Library/Logs/tern/panic.log` with a backtrace, even when tern was started from Finder or the Dock.
- `tern`: Settings (⌘, or tern → Settings…) in zeron's layout: Appearance (font size, reduce motion), Terminal (Option as Meta), Connections (list, edit, remove, new), Vault (status, lock now) and About. Escape or Back returns.
- `tern`: 725 terminal colour schemes (iTerm2-Color-Schemes), a searchable picker with a colour strip per row and live preview (Enter keeps, Escape reverts), a default theme in Settings → Appearance, and a theme per host from the sidebar (◐). The panel around the terminal takes the scheme's background.
- `tern`: the offer to save a typed password waits for the login banner to finish and starts on its own line.
- `tern`: Settings → Shortcuts lists every app shortcut; click one and press the new keys (Esc cancels, ⌫ unbinds, Reset restores). A chord already in use is refused with the name of the action that has it. Overrides live in `keymap.json` and apply at once; keys without ⌘, ⌃ or ⌥ always reach the remote shell.
- `tern`: Settings → Sync keeps connections, settings, shortcuts and the vault the same on every Mac through one private GitHub gist. The token comes from `gh` or is pasted once into the macOS Keychain. Everything but the already-encrypted vault is sealed with the vault passphrase before upload. Sync now uploads or downloads, a new Mac just downloads, and when both sides changed it asks which one to keep.
- `tern`: keys keep working after the focused field disappears (for example the passphrase field after a sync).
- `tern`: snackbars in seed-design's style at the top of the window (4 s, paused while the pointer is on them, one at a time, queued): sync results, token saved or removed, connection saved, connection removed with Undo, password saved, vault locked, and a background tab that disconnects (with Show).
- `tern`: tabs shrink like a browser's and the strip scrolls once they no longer fit, with the active tab kept in view; long host names lose their middle, not their distinct end.
- `tern`: connection errors name the target and say what to check ("10.0.0.5:22 refused the connection (is an SSH server running there?)", "could not find host …", "no answer within 15 seconds"); a failing ProxyCommand reports its own first error line.
- `tern`: a refused password says "Permission denied, please try again." before asking again, as OpenSSH does, and running out of attempts reads "permission denied: the server accepted none of the passwords or keys tried".
- `tern`: an empty window shows what to do next, with the current shortcuts for finding a host, a new connection and settings.
- `tern`: logs are also written to `~/Library/Logs/tern/tern.log` (daily, a week kept).
- `tern`: `~/.ssh/config` hosts can be removed from tern's list (× on hover; the file is never written), with Undo and a "Removed from the list" section in Settings → Connections to bring them back.
- `tern`: right-click a host or a tab for its menu: connect, open in a new tab, edit or duplicate, theme, remove; on tabs reconnect, close and close others.
- `tern`: zeron's look: Solar icons throughout, one-line host rows with a status dot and faint address, collapsible Saved and ~/.ssh/config sections, a hover … that opens the row menu, a footer with Sync and Settings, sidebar toggle and + by the traffic lights, icons in menus and the settings nav.
- `tern`: sync through your own private git repository (any remote your git can reach; one click creates `tern-sync` on GitHub with `gh`). Each sync is a commit of the same encrypted files; the gist stays as the fallback when no repository is set.
- `tern`: text fields clip to their box and scroll to keep the cursor in view.
- `tern`: a macOS menu bar (Hide, Hide Others, Show All, Quit, Minimize, Zoom) with ⌘Q, ⌘H, ⌥⌘H and ⌘M.
- `tern-term`: `local_demo <command>` for visual checks; scripted runs never take keyboard focus.
- Project: craft gates (`clippy -D warnings`, file-size and attribution checks, `cargo deny`), CI, architecture diagram, UI spec and measured performance notes.

### Changed

- `tern-ssh`: keyboard-interactive rounds arrive as `Prompt::Challenge` with the server's name, instructions and each prompt's text and echo flag, so 2FA prompts such as "Verification code:" are shown as written (#9).
- `tern-ssh`: the known_hosts location is a `ConnectSpec::known_hosts` field instead of the process-wide `TERN_KNOWN_HOSTS` variable; the `connect` example still reads the variable.

### Fixed

- `tern-ssh`: a `~/.ssh/config` alias with both `HostName` and its own `ProxyCommand` now uses that proxy instead of dialling `HostName` directly; a proxy that dies during the handshake is reported with its exit status (#7).

### Security

- A changed host key is a hard stop with no override.
- Secrets are held as zeroizing `SecretString`s and never logged; session output is redacted from `Debug`.
- RUSTSEC-2023-0071 in `rsa` (via russh) is accepted for now; see [SECURITY.md](SECURITY.md) and #16.

[Unreleased]: https://github.com/itskamol/tern/compare/v0.3.1...main
[0.3.1]: https://github.com/itskamol/tern/releases/tag/v0.3.1
[0.3.0]: https://github.com/itskamol/tern/releases/tag/v0.3.0
[0.2.0]: https://github.com/itskamol/tern/releases/tag/v0.2.0
[0.1.3]: https://github.com/your-moon/tern/releases/tag/v0.1.3
[0.1.2]: https://github.com/your-moon/tern/releases/tag/v0.1.2
[0.1.1]: https://github.com/your-moon/tern/releases/tag/v0.1.1
[0.1.0]: https://github.com/your-moon/tern/releases/tag/v0.1.0
