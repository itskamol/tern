# Memory and performance

## Where the memory goes

| Item | Cost | Source |
| --- | --- | --- |
| One terminal cell | 24 bytes | `alacritty_terminal` 0.26 `term/cell.rs:303` |
| One scrollback row | the cells it uses + 1 (vendored `alacritty_terminal`, #89); upstream allocates columns × 24 B even when empty | `vendor/alacritty_terminal/TERN-PATCH.md` |
| Scrollback growth | lazy, in chunks of ≥1,000 rows; freed when shrunk >1,000 below capacity | `grid/storage.rs:111,133` |
| Full scrollback, 200 cols | 10,000 lines of ~25 characters: 46.3 MiB upstream, 7.8 MiB trimmed. Full-width lines still cost columns × 24 B | counting allocator, release build, 2026-10-14, #89 |
| Scrolling cost of trimming | 300k lines fed: 135 ms upstream, 273 ms trimmed (+0.5 µs a line); 20 resizes of a full history: 55 ms, 3 ms | same run |

## SSH session buffers (tern-ssh)

| Buffer | Bound | Source |
| --- | --- | --- |
| russh per-channel queue | 16 messages (russh default 100) | `session.rs` `CHANNEL_BUFFER` |
| Chunk being coalesced | < 64 KiB + one packet | `session.rs` `MAX_CHUNK` |
| Event queue to the UI | 8 chunks | `session.rs` `EVENT_QUEUE` |
| Input queue from the UI | 64 commands, full → `InputError::Busy` | `lib.rs` `COMMAND_QUEUE` |

When the UI falls behind, tern stops reading the channel, russh's queue fills, and russh stops
reading the socket, so the server slows down instead of tern's memory growing.

So the default scrollback is **5,000 lines**, configurable. A fresh tab pays only for what it has printed.

## Paint

| Item | Cost | Source |
| --- | --- | --- |
| Viewport snapshot (`Terminal::lines`), 200×50 | 22 µs/frame, 240 KB transient (freed each frame) | release-build timing, 2026-10-05, #3 |

That is 0.13% of a 60 fps frame, with no steady-state memory, so it stays (#3 closed wontfix).

### Frame cost while a terminal streams (2026-11-05)

Release build on an M-series Mac, no GPU and no window: `crates/tern-term/examples/frame_cost.rs`
draws a 243×59 terminal through GPUI's `TestApp` with the real CoreText shaper. `TERN_FRAME_STATS=1`
logs the same per-second figures from the app (`frame_stats`).

| Frame | Before | After | What changed |
| --- | --- | --- | --- |
| Streaming, 30 new log lines a frame | 3.0 ms | 1.8 ms | rows shaped to their last glyph, not to the right edge |
| Terminal idle, parent re-renders | 1.8 ms | 0.25 ms | the same, and the terminal a cached child |
| Parse, mixed ANSI, 20 MiB in 64 KiB chunks | 110–140 MiB/s | unchanged | `parse_throughput` (ignored test) |

Before the change a streaming frame was 57 µs of grid snapshot, 1.16 ms in `shape_line`, 0.22 ms
building the runs, and the rest layout and painting; the snapshot and the parser are not where the
time goes. The headless run cannot show GPU time (not measured at all) or the real `Shell::render`,
which `TERN_FRAME_STATS=1` measures in the app.

## Budget

Idle memory with one connected tab, release build: RSS from `scripts/measure-rss.sh` (30 s
window, after connecting) and `footprint -p` (phys_footprint, what Activity Monitor shows; RSS
also counts shared framework pages).
The first measurement sets the baseline; every release records a new row and may not regress >10%.

| Date | Commit | Scenario | RSS |
| --- | --- | --- | --- |
| 2026-10-05 | tern-ssh (uncommitted → first commit) | `examples/connect` release, `seq 1 2000000` (16.9 MB) from strong-b, fast reader | 6 MB peak |
| 2026-10-05 | same | same, reader stalled 6 s (backpressure) | 6 MB peak |
| 2026-10-05 | tern-term | `local_demo` release, one idle local shell, 900×560 window (GPUI + fonts + Metal, no SSH) | 82 MB |
| 2026-10-05 | tern (window shell, #10) | `tern` release, empty main window 1320×880, no session | 66 MB |
| — | — | app with one idle tab | not yet measured |
| 2026-10-05 | 2fbea6e | no tab | 84.4 MB RSS · 44 MB footprint |
| 2026-10-05 | 2fbea6e | one tab, connected to a LAN host, idle shell | 86.6 MB RSS (peak 91.7) · 49 MB footprint |
| 2026-11-11 | 0.1.3 release (`/Applications/tern.app`), M1 Pro | no tab, empty config, 30 s | 101.6 MB RSS · 68 MB footprint |
| 2026-11-11 | 0.2.0 release (`target/release/bundle`), M1 Pro | same, two runs | 100.8 / 101.2 MB RSS · 70 / 88 MB footprint |

Baseline, set by the second row: **49 MB footprint / 87 MB RSS**. Measured on an M4 Pro,
macOS 26.6.2, with the screen locked, so no frames were being composited. A connected tab
costs about 5 MB over an empty window. phys_footprint_peak reached 153 MB during start-up
and connection; that transient is not explained yet (likely shader and font atlas set-up).

## 2026-10-06 — 0.1.0 release build, wallpaper on

| Build | Scene | Result |
| --- | --- | --- |
| 0.1.0 release (`/Applications/tern.app`) | window open, Great Wave wallpaper (full window + blurred backdrop), no session, idle 12 s | 83 MB phys_footprint · 127 MB RSS · 186 MB footprint peak |

The start-up peak (F7) is the wallpaper: the picture is decoded at full size, then scaled,
blurred and cached (`wallpaper.rs` `prepare`), and the decode buffers are freed after.

## 2026-10-06 — against other terminal and SSH apps

Method: macOS `footprint -p <pid>`, `phys_footprint` summed over every process whose
executable lives in the app's bundle (Electron apps run several). Apps not already running
were launched hidden (`open -g -j`), left 20 s, measured and quit. iTerm2 and Ghostty were
already running with the owner's sessions and were measured as they were, so they are upper
bounds rather than idle figures.

| App | phys_footprint | Processes | State |
| --- | --- | --- | --- |
| tern 0.1.0 | 83 MB | 1 | fresh, idle, wallpaper on (107 MB with the owner's tabs open) |
| Terminal.app | 30 MB | 1 | fresh |
| iTerm2 | 200 MB | 1 | running, with sessions |
| Ghostty | 339 MB | 1 | running, with sessions |
| Termius | 627 MB | 7 | fresh, idle |
| Tabby | 752 MB | 5 | fresh, idle |

### SSH clients, same protocol (supersedes the rows above for tern, Tabby, Termius)

Each app launched fresh, `phys_footprint` summed over its processes every 15 s for 120 s.
Connected = one tab logged in to `password_server --open --demo` on 127.0.0.1:2399 (the
`--open` flag lets any client in with the `none` method, so no password is typed into any app).
Release build of 0.1.0, notarised bundle.

| App, state | 15s | 30s | 45s | 60s | 75s | 90s | 105s | 120s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| tern idle, no wallpaper | 67 | 67 | 67 | 67 | 67 | 91 | 86 | 86 |
| tern idle, wallpaper | 124 | 129 | 124 | 129 | 124 | 124 | 107 | 108 |
| tern connected, no wallpaper | 180 | 181 | 180 | 82 | 76 | 76 | 76 | 76 |
| tern connected, wallpaper | 131 | 125 | 125 | 125 | 125 | 125 | 125 | 125 |
| Tabby idle (empty profile) | 555 | 477 | 485 | 474 | 469 | 469 | 470 | 471 |
| Tabby connected | 835 | 811 | 806 | 794 | 587 | 577 | 579 | 580 |
| Termius idle | 632 | 599 | 594 | 589 | 587 | 583 | 581 | 583 |

Notes:
- The first minute is noisy for every app (start-up allocations, the wallpaper decode); the
  README quotes the 120 s value.
- Tabby connected ran with the owner's profile, so two of their old tabs were restored
  (not connected). The empty profile did not connect from the command line, so it gives
  only the idle figure.
- Termius has no connected figure: its quick-connect could not be driven by script.
- The lead is over Electron apps, not over native ones: OxideTerm, another GPUI SSH client,
  reports 81 MB idle for itself (not measured here).

## 2026-10-14 — AutoFill helper opt-out (unmeasured)

`Info.plist` sets `NSAutoFillRequiresTextContentTypeForOneTimeCodeOnMac`, the documented opt-out
from one-time-code AutoFill that AppKit attaches to every text input from macOS 26. tern never
needs it (2FA goes through the SSH keyboard-interactive prompt). #82 reports that this starts a
separate "AutoFill (tern)" helper process; that is unconfirmed here: on 2026-10-14 no helper that
could be tied to tern appeared with or without the key (the system's SafariPlatformSupport
helpers outlive the app). No memory change is claimed; record before/after numbers from
`scripts/measure-rss.sh` here if it is ever measured.

## 2026-10-14 — wallpaper cache hit decodes nothing (#33)

Before: every launch with a wallpaper read and fully decoded the source, thumbnailed it, and
opened the cached 1600 px PNG only to test that it opened. The lead measured about 35 MB of extra
peak RSS at launch for a 1920x1038 source (131-139 MB with the wallpaper, 100-103 MB without).

Now: the cache name comes from the source's file length, and the accent and backdrop colours are
stored in `<hash>.meta.json` next to the picture. A hit checks the PNG signature and IEND trailer
of both PNGs and parses the sidecar; no pixel is decoded. A miss, a truncated file or an old
sidecar takes the previous path, which also writes the sidecar. The after-figure is measured by the
lead, not recorded here.

## 2026-11-12 — window drawables capped at 2 (#20, unmeasured)

GPUI makes the window's `CAMetalLayer` with `maximumDrawableCount` 3 (`MetalRenderer::configure_layer`),
once, and never replaces the layer or resets the count on resize, full screen or display change.
`drawables::apply` sets it to 2 after the window opens and again on every bounds change (a no-op
while the count holds), so the window never holds more than 2 window-sized IOSurfaces: one buffer
less, about 18.6 MB at 1320x880 and about 31 MB full screen. The same-build A/B in #8 held
IOSurface at 36 MB in 4 of 4 idle runs, against 54 MB in 3 of 8 for stock.

Still to record here, with the window in front (`footprint -p`): IOSurface during the flood demo
and a window resize never above 2 x the window size, and frame stats during a flood, a resize and
a divider drag inside the limits above.

## 2026-11-12 — memory budget check at release (#22)

`scripts/measure-memory.sh` replaces `scripts/measure-idle.sh` as the release step
(`RELEASING.md`, step 5). It copies the bundle for each run, opens the copy over its own
`TERN_CONFIG_DIR`, waits 30 s, reads `footprint -p`, and prints the median and range of 3 runs per
scene against the target and the ceiling from #12. It exits 1 when a median is over its ceiling
and 2 when it refused a reading.

| Scene | How | Target | Ceiling |
| --- | --- | --- | --- |
| No tab | nothing restored, no wallpaper | 70 MB | 80 MB |
| One connected tab | `tern demo-cmd`: local demo server, approved password command, pre-seeded known hosts | 75 MB | 85 MB |
| Start-up peak, no vault | `phys_footprint_peak` of the runs above, pooled | 90 MB | 100 MB |
| Start-up peak, vault | `phys_footprint_peak`, a throwaway vault opened at start-up | 380 MB | 400 MB |

Refused, never guessed: a locked screen, a copy that is not the front app (compared by pid, at
every 2 s check and at the reading), a window that is not 1320x880, a tab that did not connect, and
a vault scene whose peak is under the 256 MiB scrypt block (the vault did not open).

The vault scene cannot open a vault with a shipped binary and no typing: the Keychain is the
person's and typed input needs an Accessibility grant a script does not have. That scene runs a copy
whose binary is built from the same tree with `--features dev-keys`, which compiles in the
`TERN_DEV_KEYS` replay (`devkeys.rs`) that types the passphrase of a vault written by
`crates/tern/examples/throwaway_vault.rs` into the prompt of a host with a vault key. The feature is
off by default and the script refuses a bundle whose binary carries the replay, so the other scenes
and every shipped build are as shipped.

Readings, release bundle of the branch over `integration/memory` at c613f3e (wallpaper removed,
drawables capped at 2), screen unlocked, copy in front, medians of 3 with the range, in MB:

| Scene | Target | Ceiling | Median | Range |
| --- | --- | --- | --- | --- |
| No tab | 70 | 80 | 70 | 70-71 |
| One connected tab | 75 | 85 | **82** | 81-82 |
| Start-up peak, no vault | 90 | 100 | 78 | 78-82 |
| Start-up peak, vault | 380 | 400 | 336 | 336 |

Every scene is within its ceiling and the check passes. The one connected tab is over its target
(82 against 75), so the ticket's "at or under target" is not met for that scene; it is reported
here and not tuned away. `footprint` prints whole megabytes, so ranges are coarse. The vault row
is from the dev-keys variant described above.
