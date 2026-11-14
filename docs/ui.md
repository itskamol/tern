# UI spec

tern's UI is modeled on zeron (https://github.com/zeronsh/zeron, MIT). Its feature list,
`docs/research/feature-inventory.md`, and its native GPUI code (`crates/ui/src/`) are the reference.
Reference screenshot: `apps/landing/public/assets/app-screenshot.jpg`.

## No-slop rules

- **Every number names its source, never eyeballing.** In order: zeron's source; then another named
  design system, adapted with an attribution header (the snackbar is from seed-design); then a value
  worked out from tern's own tokens, written as the sum (the Files panel width is the sidebar's,
  224–400, default 256). A number picked in a prototype is not a source. Each value below names
  where it came from. When the inventory (written for the old Electron app) and the native code
  disagree, the native code wins.
- **Nothing generic.** No stock gradients, no emoji in chrome, no default gpui-component look left
  untouched, no placeholder buttons that do nothing. A control ships working or doesn't ship.
- **Copy the implementation, not just the look.** Motion, sizing and the theme layer stack are taken
  from zeron's `motion.rs` / `theme.rs` / `shell.rs` and adapted, with the attribution header.
- **Checked by eye.** Each feature is accepted only after a screenshot of the running app is put next to
  zeron's (or the named source's) and looked at. Numbers that matter are measured from the running app.

## Window shell

| Feature | Value | Source |
| --- | --- | --- |
| Default / min size | 1320×880, min 900×600 | inventory §1.1 |
| Titlebar | transparent, custom-drawn strip, app owns drag | `crates/ui/src/lib.rs:393` |
| Traffic lights | at (14, 14), 38px titlebar row | `lib.rs:398` (code wins over inventory's 14,15) |
| Fullscreen | traffic lights hidden → chrome cluster reflows left | inventory §1.1 |
| Window background | macOS `Blurred` (material `Selection`), frosted shell, no picture; opaque under Reduce transparency and elsewhere | `theme.rs:1130`, `lib.rs:423` |
| Glass | blur 44, saturate 1.8, brightness 1.18 | inventory §1.12 |
| Type | Geist (UI), Geist Mono (terminal) | inventory §1.12 |
| Surfaces | `bg` → `surface` (+1 step) → `surface_raised` → `overlay`; hairline white borders | `theme.rs:610-640` |

## Sidebar (hosts instead of sessions)

| Feature | Value | Source |
| --- | --- | --- |
| Width | drag-resize 224–400px, default 256, persisted (inventory §1.3 says 208; native code wins) | `settings.rs` `SIDEBAR_MIN`/`MAX`/`DEFAULT` |
| Collapse | width → 0 over 200ms ease-out; main area goes full-bleed (radius/border melt) | inventory §1.3 |
| Separator | double-click resets width; arrow keys nudge ±16px | inventory §1.3 |
| Rows | host alias, `user@host`, live status dot (connected / connecting / error) | §1.6, adapted |
| Grouping | flat or grouped (ssh config file / tag), persisted | §1.6 |
| Row menu | Connect, Connect in new tab, Edit, Duplicate, Delete | §1.6 Rename/Archive/Delete, adapted |
| Re-sort | rows glide to new position, 260ms cubic-bezier(0.22,1,0.36,1) | §1.6, `motion.rs` |
| Edges | fade masks, no visible scrollbar | §1.6 |

## Tabs and terminal

| Feature | Value | Source |
| --- | --- | --- |
| Tabs | in the titlebar strip; drag-reorder with 150ms sliding transforms (`TAB_SLIDE`) | §1.10, `motion.rs` |
| Tab actions | middle-click close, `+` new connection, Cmd+1…9, Cmd+Shift+[ / ] | §1.10, adapted |
| Tab state | title from OSC title, status dot, "[disconnected: reason]" banner + Reconnect | §1.10 "[process exited N]", adapted |
| Input coalescing | 12ms | §1.10 |
| Resize debounce | 80ms | §1.10 |
| Terminal bg | #090909 + full ANSI palette from the theme | §1.10 |
| Reconnect | backoff, keeps scrollback | §1.10 |

## Overlays and motion

| Feature | Value | Source |
| --- | --- | --- |
| Menus / popovers | `menu-in` 0.14s, scale 0.96 + translateY −2, origin tracks anchor | §1.12 |
| Dialogs (password, host key) | `dialog-in` 0.18s, scale 0.96→1 | §1.12 |
| Entrances | `fade-in` 0.5s cubic-bezier(0.16,1,0.3,1), translateY 4→0 | §1.12 |
| Quick fades | 0.15s | §1.12 |
| Reduced motion | honours the OS setting; all motion off | §1.12 |
| Connecting indicator | zeron-pulse cell wave, 2.4s, staggered opacity 0.08→1 | §1.12 |

## Files panel

| Feature | Value | Source |
| --- | --- | --- |
| Width | the sidebar's range and default, drag from the left edge, double-click resets, persisted as `filesPanelWidth` | the sidebar's, above |
| Header | 34px, host name 13px | the Files tab's panel header |
| Row | 26px, name 12px, icon 15px, padding 8px, gap 6px | the Files tab's row (`files_tab_view.rs`), padding and gap trimmed for the narrower panel |
| Size column | 52px, 11px text | the Files tab's 64px less 12px, so a name keeps 83px at the default width |
| Modified column | 72px, 11px text; left out below 256px of panel | the Files tab's 92px less 20px; 256 − 16 − 15 − 52 − 72 − 3×6 = 83px of name, and below that the name would have under 70px |
| Breadcrumb | 22px parts, 12px text, the front clips | worked out from the row height less 4px |
| Footer | 11px text, Download button 26px | the Files tab's footer |
| Send to… button | 20px, 13px arrow, over the row's end 4 from the edge and 3 from the top, on the row's own colour; shown while the row is hovered or picked | the row's 26px less 3 above and below; Solar's arrow-up turned right (the set's arrow-right) |
| Send to… menu | the shell's context menu: 216 wide, 4 inset, 2 gap, 12 radius, at most 480 high; items 8×6 padding, 7 radius, 16px icon, 13px name over an 11px folder; a 1px hairline above Move | `shell_menu.rs` (`render_menu`) |
| Dragged rows | the Files tab's drag ghost ("Copy notes.txt", "Move 3 items" while ⌥ is held); over a session pane, the host drop zone's 0.22 accent tint and 1px accent edge on a clear layer over the whole pane, so the terminal never reflows; over a tab, the tab drag's 0.25 accent tint | the Files tab's `Dragged`; `drop_zone` in `shell_split.rs`; the tab strip's drag-over |
| Clipboard banner | under the path bar, 8 from the panel's sides and 4 above and below, 8 gap, 8×6 padding, 7 radius, the Finder drop tint behind it; 14px copy icon, 12px name over an 11px route, Paste as the footer's 26px primary button, a 20px Forget button with a 12px cross on a 5 radius hover; Paste only where the rows are not already | the clash row's frame and gap; the Send to… button |
| Move and "for the rest" boxes | 12px, 3 radius, accent when on | the clash sheet's 14px box, less 2 to sit beside 11–12px text |
| Transfer list | the Files tab's: 26px header with Cancel all, rows 12×4 padding, a 3px bar, 11px detail and route; at most 180 high, then it scrolls; under the panel's footer | the Files tab's transfer list (moved to `transfer_rows.rs`, shared by both) |
| Clash in a row | 8 inset, 7 radius, warning at 0.08 behind it; Skip, Keep both, Replace as the footer's 26px buttons, wrapping when narrow | the panel's Finder drop tint (accent at 0.08); the footer's buttons |

## Messages (ADR 0002)

Every message is one of three kinds, and the code shows none without its kind: `Shell::activity`,
`Shell::alert` (the only way to a snackbar) and a Decision card.

| Feature | Value | Source |
| --- | --- | --- |
| Activity in the status line | the line's own 11 pt, beside the host, for 4 s | zeron `SESSION_FOOTER` text (`statusline.rs`); seed-design's snackbar time |
| Activity by the titlebar (Settings, Files tab, status line off, no tab) | 11 pt muted, at most the sidebar's default width (256), left of the Activity button | the status line's 11 pt; the sidebar's default width |
| Activity button | 24 pt control, 16 pt icon, at the titlebar's right end; held at the hover tint while its list is open | zeron's titlebar control (`chrome.rs`) |
| Activity list | 340 wide, 6 inset, 12 radius, 13 pt rows, 11 pt time; at most 320 high, newest first, 200 kept | tern's port forwards popover; the Files tab's source menu |
| Work in progress (update download, copies) | an Activity replaced in place until the work ends; never added to the list | — |
| Alert | seed-design snackbar: 464 wide (16 less than the window when narrower), at least 44 high, 10 inset, 8 radius, 4 s (paused while held), enter 150 ms / exit 100 ms, at most one action; top right under the titlebar, 8 from the edge | seed-design `snackbar.css` (`shell_toast.rs` header) |
| Decision | a card that stays until answered: 340 wide, 12 radius, 13 pt, inset and gap 8, the connection form's 28 pt buttons; above any snackbar | the Activity list's card; `SPACE_SM` |
| A name already taken by a copy | asked in the copy's row in the Files tab or the Files panel; a sheet over the window only while neither is on screen | issue #23, story 30 |

## Keyboard

| Feature | Value | Source |
| --- | --- | --- |
| Toggle sidebar | Cmd+S | §1.4 |
| Host picker / palette | Cmd+K, fuzzy search over hosts | zeron `pickers.rs`, adapted |
| Customisable | click-to-record combo, conflict detection, per-row Reset, Restore defaults | §1.4 |

## Later (not in the first version)

- Theme library with VS Code theme import (zeron `crates/theme/src/vscode.rs`).
