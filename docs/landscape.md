# tern landscape: similar SSH clients and terminals

Researched 2026-10-06. tern is a native SSH client (Rust + GPUI) for macOS, with Windows and Linux in progress. This document compares it with 29 related tools. Every claim comes from a page opened in a browser (agent-browser) or from the GitHub API for stars, licence and release dates. Where a primary source did not say, the cell reads "not stated". tern's own numbers (RAM, features) come from its owner and were not re-measured here.

## Comparison table

Stars and release dates are from `gh api` on 2026-10-06. "Paid" means a price was read on the vendor page.

| Tool | Platforms | Stack | Price | Open source | Vault / sync | SFTP | Notable |
|---|---|---|---|---|---|---|---|
| tern | macOS (Win/Linux WIP) | Rust + GPUI | Free | GPL-3.0 | age vault + PIN/Keychain; encrypted sync to own Git/gist; gopass/op/pass commands | yes (side panel) | sudo fill on request, broadcast |
| Termius | macOS, Win, Linux, iOS, iPadOS, Android | not stated | Free; Pro $10/mo annual; Team $20; Business $30 | no | local vault free; cloud vault + sync paid; E2E | yes | mobile, teams, Mosh, serial |
| Tabby | Win, macOS, Linux | Electron/TS (repo is TypeScript) | Free | MIT, 74.8k stars, v1.0.237 (2026-09-25) | encrypted container + optional master passphrase; config sync via Tabby Web | yes | plugins, serial, auto-sudo plugin |
| Royal TSX / Royal TS | macOS (TSX), Win, iOS, Android | not stated | Lite free (10 connections); TSX EUR 49 one-time | no | credentials; 1Password/LastPass/KeePass; file/share sync | yes (FTP/SFTP/SCP) | RDP/VNC/SSH, team docs |
| Core Shell | macOS | not stated | Free; Premium from $9.99 | no | macOS Keychain; sync is Premium | not stated | OpenSSH-compatible, tags |
| Secure ShellFish | iOS, macOS | not stated | not stated | no | iCloud Keychain / iCloud sync | yes (Files app) | iOS integration |
| Prompt 3.5 (Panic) | macOS, iOS, iPadOS, visionOS | not stated | one-time price, amount not stated | no | Panic Sync (free) | not stated | Mosh, Eternal Terminal, Clips |
| WindTerm | Win, macOS, Linux | C/C++ (repo language C) | Free, commercial use allowed | partial; 32.4k stars, 2.7.0 (2025-03-11) | session password protection; sync not stated | yes | huge protocol list, no release since 2025-03 |
| Electerm | Win, macOS, Linux, Android, iOS | Electron | Free | MIT, 15.3k stars, v5.5.66 (2026-10-03) | sync to GitHub/Gitee gist, WebDAV, custom server | yes | SSH/RDP/VNC/serial, AI |
| XPipe | Win, macOS, Linux | Java | Free (Community); Homelab $5/mo; Pro $10/mo | core Apache-2.0, 14.6k stars, 24.5 (2026-10-04) | local vault; git sync of vault (free tier); password-manager integration | yes | works on top of installed ssh/docker, launches your terminal |
| MobaXterm | Windows | not stated | Home free (12 sessions); Pro $69 | no | master password | yes (auto) | X server, RDP/VNC |
| SecureCRT | Win, macOS, Linux | not stated | paid, price not stated | no | not stated | with SecureFX | scripting, keyword highlight |
| Xshell | Windows | not stated | free for home/school; paid otherwise (price not stated) | no | auth profiles | yes | quick commands, RDP in tab |
| Bitvise SSH Client | Windows | not stated | Free incl. organisations | no | key pair management | yes | Kerberos/SSPI |
| PuTTY 0.85 | Windows, Unix | not stated | Free | MIT-style licence page not opened | none | psftp/pscp | the baseline |
| Remmina | Linux | GTK/C (repo C) | Free | GPL-2.0 | not stated | not stated | RDP/VNC/SSH/SPICE |
| Snowflake (Muon) | Java desktop | Java | Free | GPL-3.0, 2.2k stars, last release v1.0.4 (2020-02-07), last push 2024-05 | not stated | yes | effectively unmaintained |
| Shellngn | web (cloud or self-host) | not stated | Cloud $3.25-8.25/mo; Pro $39/yr personal, $199/yr commercial | no | server-side | yes | browser SSH/SFTP/RDP |
| Termix | web, desktop, mobile | TypeScript | Free | Apache-2.0 text in LICENSE (GitHub reports "other"), 15.3k stars, 2.9.2 (2026-10-06) | self-hosted server | yes | hosts, tunnels, Docker, automations, AI, sharing |
| Nexterm | web (self-host) | JS | Free | MIT, 5.1k stars, v1.2.2-BETA (2026-07-19) | self-hosted, orgs, OIDC | yes | SSH/VNC/RDP, Proxmox |
| SSHPilot | Linux, macOS | Python (GTK) | Free | GPL-3.0, 1.1k stars, v6.2.9 (2026-10-05) | libsecret, KeePass, Bitwarden/Vaultwarden, pass; backup to Bitwarden or own servers | yes (dual pane) | reads ~/.ssh/config |
| VsTerm | Win, macOS, Linux | Rust, egui/wgpu | not stated | custom ("NOASSERTION"), 359 stars, v1.3.5 (2026-09-29) | OS keyring + encrypted vault | yes | ops panels, ZMODEM |
| OxideTerm | macOS, Win, Linux | Rust + GPUI | Free | GPL-3.0, 1.6k stars, v2.2.1 (2026-10-05) | OS keychain; encrypted cloud sync | yes | closest to tern, see below |
| Warp | macOS, Win, Linux | Rust, own UI framework | Free; Build from $20/mo | AGPL-3.0 (UI crates MIT), 65.4k stars | Warp Drive (cloud) | remote file tree | AI agents, blocks |
| Wave Terminal | macOS, Win, Linux | Go backend | Free | Apache-2.0, 22.4k stars, v0.14.5 (2026-04-16) | local secret store | remote file preview | AI, widgets |
| iTerm2 | macOS | Swift/ObjC | Free | GPL-2.0, 18.1k stars | not an SSH manager | no | tmux integration, profiles |
| Ghostty | macOS, Linux | Zig | Free | MIT, 61.9k stars | none | no | fast terminal, no SSH features |
| Zed | macOS, Linux, Win | Rust + GPUI | Free | 91.3k stars (licence "NOASSERTION") | n/a | via remote project | remote development over SSH |
| Termy | macOS, Linux, Win | Rust + GPUI | Free | MIT, 440 stars, v0.2.80 (2026-09-25) | none | no | terminal, tmux |
| tty7 | macOS, Win, Linux | Rust + GPUI | Free | Apache-2.0, 1.2k stars | not stated | not stated | persistent sessions, agents |

## Dedicated SSH clients

**Termius** is the commercial reference. The free Starter plan has a local vault, SSH, SFTP, AI autocomplete and port forwarding; the cloud vault, mobile/desktop sync, session logs and snippet automation start at Pro ($10/month annually) ([pricing](https://termius.com/pricing)). Its feature matrix also lists Mosh, serial, FIDO2 and jump hosts, and the security page says synced data is end-to-end encrypted with a client-side key ([security](https://termius.com/security)). Complaints: Trustpilot shows 2.4/5 from 17 reviews, with one reviewer reporting lost data and another objecting to serial being a paid feature ([Trustpilot](https://www.trustpilot.com/review/termius.com)). Small sample, unclaimed profile.

**Tabby** is the open-source cross-platform option: SSH client with connection manager, SFTP, X11 and port forwarding, jump hosts, agent forwarding, serial, an optional encrypted password container with a master passphrase, plugins, and multi-pane input ([tabby.sh](https://tabby.sh)). The repo has a `tabby-auto-sudo-password` plugin that "offers to automatically paste saved sudo password in SSH sessions" ([repo](https://github.com/Eugeny/tabby)). Complaints from its tracker: open memory-leak reports, e.g. a leak per closed tab ([#11718](https://github.com/Eugeny/tabby/issues/11718)) and macOS SFTP memory ([#11572](https://github.com/Eugeny/tabby/issues/11572)); Mosh support has been requested since 2019 ([#593](https://github.com/Eugeny/tabby/issues/593)).

**Royal TSX / Royal TS** targets IT teams: RDP, VNC, SSH, SFTP, web, with terminals based on iTerm2, credential sharing without sharing secrets, 1Password/LastPass/KeePass credential sources, and documents synced via a network share or cloud storage. Lite is free up to 10 connections; the full macOS licence is EUR 49 with no subscription ([features](https://www.royalapps.com/ts/mac/features)). It runs on Windows, macOS, iOS and Android ([royalapps.com](https://www.royalapps.com/store)).

**Core Shell** is a small macOS app: OpenSSH-compatible (certificates, ProxyJump), Keychain integration, tags, auto reconnect. Free tier has unlimited hosts; importing/exporting, automatic syncing and AppleScript automation are Premium, "from $9.99" (unit not stated) ([coreshell.app](https://coreshell.app)).

**Secure ShellFish** is iOS/macOS-focused: server directories appear in Files and Finder, config sync via iCloud Keychain, snippets via iCloud, security keys and Secure Enclave keys, cloud server launch ([secureshellfish.app](https://secureshellfish.app)). Pricing not read.

**Prompt 3.5 (Panic)** runs on Mac, iPhone, iPad and visionOS, with Mosh and Eternal Terminal, Clips (synced snippets), free Panic Sync for servers/keys/passwords, and Face ID/Secure Enclave protection ([panic.com/prompt](https://panic.com/prompt/)). Price amount not stated on that page.

**WindTerm** (C/C++) is free for commercial use; released source is Apache-2.0 but the project describes itself as only partly open ([README](https://github.com/kingToolbox/WindTerm)). It lists SSH, ControlMaster, ProxyJump, agent forwarding, all three port-forward modes, SFTP/SCP, tmux integration, a command palette and sync input. The latest release is 2.7.0 from 2025-03-11, so it looks slow-moving.

**Electerm** (Electron) covers SSH/SFTP/FTP/telnet/serial/RDP/VNC, with bookmark sync to GitHub or Gitee gist, WebDAV or its own cloud, and AI features ([README](https://github.com/electerm/electerm)). Recent tracker items include a memory leak with auto reconnect, since closed ([#4284](https://github.com/electerm/electerm/issues/4284)).

**XPipe** is a connection hub that wraps your installed ssh/docker CLIs and launches your own terminal. It retrieves secrets from a password manager rather than storing them, can sync the vault through your own git repository, dynamically elevates with sudo, and fills password prompts ([README](https://github.com/xpipe-io/xpipe)). Community is free; Homelab is $5/month and Professional $10/month ([pricing](https://xpipe.io/pricing)). It is the closest in spirit to tern's password-command plus own-Git-sync design.

**MobaXterm** (Windows): SSH, Telnet, RDP, VNC, Mosh, X server, auto SFTP browser, multi-execution. Home is free but capped (12 sessions, 2 tunnels); Pro is $69 ([features](https://mobaxterm.mobatek.net/features.html), [download](https://mobaxterm.mobatek.net/download.html)).

**SecureCRT** (Windows, macOS, Linux) emphasises tabbed sessions, scripting and keyword highlighting; evaluation is free ([vandyke.com](https://www.vandyke.com/products/securecrt/)). Price not found (the pricing URLs I tried returned 404).

**Xshell** offers a session manager, quick commands, triggers, auth profiles and RDP in tabs ([xshell.com](https://www.xshell.com/en/xshell/)); the vendor says free licences exist for home/school use. Platform not stated on the page read (it is Windows software to my knowledge, unverified here).

**Bitvise SSH Client** is a free Windows client with a graphical SFTP client, remote-desktop forwarding and Kerberos/SSPI sign-on ([bitvise.com](https://www.bitvise.com/ssh-client)). **PuTTY** 0.85 is a free SSH/Telnet client for Windows and Unix ([putty page](https://www.chiark.greenend.org.uk/~sgtatham/putty/)). **Remmina** is a Linux remote-desktop client with RDP, SSH, SPICE, VNC, X2Go and HTTP plugins ([remmina.org](https://remmina.org)); it is GPL-2.0 per the GitHub mirror `FreeRDP/Remmina` (last push 2026-02-08).

**Snowflake / Muon** (Java, GPL-3.0) had its last release in February 2020 and last push in May 2024 ([repo](https://github.com/subhra74/snowflake)); treat as dormant. I found no named successor.

**Self-hosted web managers.** Shellngn offers a paid cloud and a self-hosted Pro licence ($39/yr personal, $199/yr commercial) ([pricing](https://shellngn.com/pricing/)). Termix is a free self-hosted platform with SSH, RDP/VNC, tunnels, Docker, automations, role-based sharing and an optional AI assistant, positioned as a Termius alternative ([README](https://github.com/Termix-SSH/Termix)). Nexterm is MIT, does SSH/VNC/RDP/SFTP with OIDC and organisations ([README](https://github.com/gnmyt/Nexterm)). All need a server, unlike tern.

**SSHPilot** (Python/GTK, Linux and macOS) reads `~/.ssh/config`, has a dual-pane SFTP manager, snippets, plugins, and storage backends including libsecret, KeePass, Bitwarden/Vaultwarden and pass ([README](https://github.com/mfat/sshpilot)). **VsTerm** is a Rust/egui SSH manager with an OS keyring plus encrypted vault, built-in SFTP/ZMODEM and ops panels; its licence is non-standard ([README via GitHub API](https://github.com/vesaaa/vsterm), read through `gh api`, not the browser).

## Terminals with SSH workflows

**Warp** is now open source (client AGPL-3.0, UI crates MIT, on its own UI framework, not GPUI) ([repo](https://github.com/warpdotdev/warp)). Its SSH support installs a small companion server on the remote host for file tree, code review and agent tools ([docs](https://docs.warp.dev/terminal/warpify/ssh)). Free tier exists; Build is from $20/month for AI credits ([pricing](https://www.warp.dev/pricing)). Complaint: a highly-upvoted issue about mandatory login and no offline mode ([#900](https://github.com/warpdotdev/warp/issues/900)). It has no saved-host manager comparable to tern's.

**Wave Terminal** (Apache-2.0) has durable SSH sessions with auto reconnect, remote file preview and editing, an AI widget (OpenAI, Claude, Ollama and others), and secret storage in native backends ([README](https://github.com/wavetermdev/waveterm)). Open issues include SSH agent forwarding as a per-connection option ([#2718](https://github.com/wavetermdev/waveterm/issues/2718)). The README I read did not name its UI stack.

**iTerm2** (GPL-2.0, macOS) offers split panes, profiles, search, hotkey window and tmux integration ([features](https://iterm2.com/features.html)). It is a terminal, not a host manager or vault.

**Ghostty** runs on macOS and Linux with native UI and GPU rendering; Windows is "planned" ([features](https://ghostty.org/docs/features)). No SSH management.

**Zed** supports remote development: you give it an ssh command, it uploads a server to the host and opens a remote project ([docs](https://zed.dev/docs/remote-development)). It is an editor, not an SSH client.

## GPUI-based

- **OxideTerm** is the direct competitor. It describes itself as a free native SSH client and remote workspace "drawn directly on the GPU with GPUI", GPL-3.0, for macOS, Windows and Linux (desktop only, no mobile) ([README](https://github.com/AnalyseDeCircuit/oxideterm)). It lists SSH, Mosh, Telnet, serial, RDP/VNC plugins, SFTP, port forwarding, ~/.ssh/config import, split panes, broadcast groups, tmux -CC, session logs, grace-period reconnect, themes and background images, encrypted cloud sync and a bring-your-own-key AI assistant. Credentials sit in the OS keychain. Its README gives idle memory of 81.3 MB (macOS) and 23.5 MB (Windows), the maintainer's own figures. It shipped v2.2.1 on 2026-10-05. I found no mention of password-command integrations or a sudo-fill feature on its README (not stated, not proof of absence).
- **Termy** (MIT) is a GPUI terminal with tabs, splits, tasks and optional tmux sessions; no SSH manager ([README](https://github.com/termylabs/termy)).
- **tty7** (Apache-2.0) is a GPUI terminal with persistent background sessions, agent awareness and remote development over its own SSH stack ([README](https://github.com/l0ng-ai/tty7)).
- Smaller: `elonehoo/xssh` is a self-described GPUI SSH demo with 0 stars; `zortax/gpui-terminal` is a terminal component (49 stars) (GitHub search results, not opened further).

## Where tern stands

**What tern does that few others do** (only claims the table supports):
- Password-manager commands (gopass/op/pass) plus an age vault: XPipe and SSHPilot pull secrets from external managers, Royal TS from 1Password/LastPass/KeePass, but I found none that lists a generic "run a command to fetch the password" with gopass/pass alongside its own age vault.
- Sync to the user's own Git repo or gist, end-to-end encrypted: XPipe (git vault sync) and Electerm (gist/WebDAV) are close; Termius sync is E2E but goes to its servers; OxideTerm's cloud sync target is not stated.
- On-request sudo fill (⌘\): Tabby has a plugin and XPipe elevates with sudo; a first-class built-in with an explicit user trigger is uncommon among the others.
- A free, GPL, native macOS app with this whole bundle. Closest overlap is OxideTerm (same framework and licence), which has a wider protocol list but no stated vault-with-PIN or password-command story.
- Memory: ~76-125 MB versus Tabby ~580 MB and Termius ~583 MB are the owner's measurements; OxideTerm reports 81 MB for itself, so tern's lead is over Electron apps, not over other native ones.

**What tern lacks that users of the others expect:**
- Mobile apps (Termius, Prompt, ShellFish, Royal TS, Termix, Electerm have them).
- Team sharing and shared vaults (Termius Team/Business, Royal TS, Termix, Nexterm, Shellngn Pro).
- RDP/VNC (Royal TS, MobaXterm, Xshell, Remmina, Electerm, OxideTerm, Termix, Nexterm).
- Mosh (Termius, Prompt, WindTerm, MobaXterm, OxideTerm) and serial (Termius, Tabby, WindTerm, MobaXterm, Electerm, OxideTerm).
- AI assistance (Warp, Wave, Termix, Electerm, OxideTerm, Termius autocomplete).
- A working Windows build today (Tabby, WindTerm, MobaXterm, Termius, OxideTerm, Warp all ship it).

**Feature ideas, ranked by how many competitors have them** (counts from this document):
1. Mobile companion or at least read-only sync: Termius, Prompt, ShellFish, Royal TS, Electerm, Termix (6). Large effort; lower priority than parity items.
2. Windows and Linux builds: Tabby, WindTerm, Termius, OxideTerm, Warp, Electerm, XPipe all ship them. In progress for tern (branch `cross-platform`). (tern already imports `~/.ssh/config`, so that is not a gap.)
3. Serial and Mosh: serial in 6+ tools, Mosh in 5. Mosh matters for the flaky-network use case tern's auto-reconnect already targets.
4. tmux control mode (-CC): iTerm2, OxideTerm, Termy, WindTerm (4); fits the existing split-pane and reconnect work.
5. Optional BYOK AI (explain error, generate command) with approval gates: Warp, Wave, Termix, Electerm, OxideTerm (5). Contentious; make it off by default.

## Not verified

- Termius, Core Shell, ShellFish, Royal TS, MobaXterm, SecureCRT, Xshell, Bitvise, PuTTY: tech stack not stated on pages read.
- SecureCRT and Xshell prices, Prompt's price, ShellFish pricing: not found.
- Termius feature-matrix tick marks (which tier has Mosh, serial, etc.) did not render in the text I read; tier claims are limited to the pricing blurbs.
- Tabby's Electron stack is inferred from the TypeScript repo, not stated on a page read. Wave's stack not stated.
- RAM figures for tern, Tabby and Termius are the owner's and were not reproduced.
- Trustpilot sample for Termius is 17 reviews, not representative.
- VsTerm and the Tabby/Warp/Wave/XPipe/Electerm issue links and repo stats were obtained through the GitHub API; the issue titles were not opened in the browser.
- Windows/Linux platform lists for several tools come from product pages' general statements; Xshell, MobaXterm and Bitvise were not confirmed as Windows-only beyond what is quoted.

## Sources (all accessed 2026-10-06)

Opened in browser: https://termius.com/pricing , https://termius.com/security , https://www.trustpilot.com/review/termius.com , https://tabby.sh , https://github.com/Eugeny/tabby , https://www.royalapps.com/ts/mac/features , https://www.royalapps.com/store , https://coreshell.app , https://secureshellfish.app , https://panic.com/prompt/ , https://github.com/kingToolbox/WindTerm , https://github.com/electerm/electerm , https://electerm.org , https://xpipe.io/pricing , https://github.com/xpipe-io/xpipe , https://mobaxterm.mobatek.net/features.html , https://mobaxterm.mobatek.net/download.html , https://www.vandyke.com/products/securecrt/ , https://www.xshell.com/en/xshell/ , https://www.bitvise.com/ssh-client , https://www.chiark.greenend.org.uk/~sgtatham/putty/ , https://remmina.org , https://shellngn.com/pricing/ , https://github.com/Termix-SSH/Termix , https://github.com/gnmyt/Nexterm , https://github.com/mfat/sshpilot , https://www.warp.dev/pricing , https://docs.warp.dev/terminal/warpify/ssh , https://github.com/warpdotdev/warp , https://github.com/wavetermdev/waveterm , https://iterm2.com/features.html , https://ghostty.org , https://ghostty.org/docs/features , https://zed.dev/docs/remote-development , https://github.com/AnalyseDeCircuit/oxideterm , https://github.com/termylabs/termy , https://github.com/l0ng-ai/tty7

GitHub API (`gh api`, `gh search`): repo metadata for Eugeny/tabby, wavetermdev/waveterm, warpdotdev/warp, ghostty-org/ghostty, zed-industries/zed, kingToolbox/WindTerm, electerm/electerm, xpipe-io/xpipe, subhra74/snowflake, gnmyt/Nexterm, mfat/sshpilot, vesaaa/vsterm (README), Termix-SSH/Termix, gnachman/iTerm2, FreeRDP/Remmina, termylabs/termy, AnalyseDeCircuit/oxideterm, l0ng-ai/tty7; issue searches for tabby, warp, waveterm, xpipe, electerm; repo search for GPUI projects.

WebSearch (to find URLs only): Termius complaints query.
