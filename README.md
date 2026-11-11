<p align="center">
  <img src="packaging/macos/icon.svg" width="128" alt="tern icon: a white bird in flight on near-black">
</p>

<h1 align="center">tern</h1>

<p align="center">
  A native SSH client for macOS, written in Rust on <a href="https://github.com/zed-industries/zed/tree/main/crates/gpui">GPUI</a>.<br>
  Saved hosts, SFTP, splits and snippets, with passwords from your own password manager.
<br>
  <a href="https://tern.carrot-soft.tech">Website: https://tern.carrot-soft.tech</a>
</p>

<p align="center">
  <img src="docs/screenshots/demo.gif" alt="⌘K finds web-01, tern connects, runs docker ps, then ⌘\ fills the sudo password" width="900">
</p>

```sh
brew install --cask itskamol/tern/tern
```

Or download the DMG from [Releases](https://github.com/itskamol/tern/releases). These builds are signed with a local certificate, not notarised: Homebrew clears the quarantine flag for you; a DMG copy needs `xattr -dr com.apple.quarantine /Applications/tern.app` once. tern then updates itself.

## Why tern

tern does what Termius and Tabby do for a list of servers (saved hosts, ⌘T to any of them, SFTP, splits, snippets and port forwards), but as one small native process. One connected tab takes 76 MB where Tabby takes 580 MB ([how I measured it](docs/perf.md)).

Passwords come from your SSH key, from a password-manager command such as `gopass show -o work/ssh/web-01`, or from tern's age-encrypted vault. At a `sudo` prompt, ⌘\\ types the password. It never types one on its own.

Hosts, snippets and settings sync to a private Git repository you own, encrypted on your Mac first. No account, no subscription.

## A closer look

| | |
| --- | --- |
| <img src="docs/screenshots/docker.jpg" alt="sudo docker ps with the sudo password filled by ⌘\"><br>**⌘\\ fills `sudo`**: the password comes from your password manager, typed only when you ask and only at a password prompt. | <img src="docs/screenshots/snippets.jpg" alt="The snippets picker filtering docker commands"><br>**Snippets (⇧⌘S)**: saved commands. ↵ marks the ones that run when picked; the rest are typed for you to finish. |
| <img src="docs/screenshots/split.jpg" alt="Two panes side by side with find in scrollback"><br>**Splits and find**: ⌘D and ⌘⇧D split a tab; ⌘F searches the scrollback. | <img src="docs/screenshots/sftp.jpg" alt="The SFTP side panel listing a remote home folder"><br>**Files tab (⇧⌘O)**: two panels, the Mac and a host; drag files and folders between them, or drop files from Finder to upload. |
| <img src="docs/screenshots/settings.jpg" alt="Settings, Appearance, with the wallpaper gallery"><br>**Wallpapers and fonts**: seven public-domain artworks built in, or any image; light, dark or follow macOS; any monospace font. | <img src="docs/screenshots/light.jpg" alt="tern in light appearance over Van Gogh's Wheat Field"><br>**Light and dark**: the window takes its colours from the wallpaper, and body text stays at least 4.5:1 against it. |

## Also in the box

- **Connections:** groups and tags, a one-time import from `~/.ssh/config`, ProxyJump chains, local, remote and SOCKS forwards, agent forwarding, and two-factor logins.
- **Staying connected:** automatic reconnect after a network drop, but never after you type `exit`. A changed host key stops the connection and shows the exact `ssh-keygen -R` line to fix one you trust.
- **Terminal:** tabs that reopen on launch, broadcast input to chosen tabs, ⌘-click links (including OSC 8), session logging, and a status line with latency.
- **Vault:** passwords and SSH keys in an age-encrypted file that unlocks with a PIN or the Keychain and locks when you're idle or the Mac sleeps or locks its screen (macOS). A password command that arrives through sync must be approved once on each Mac.
- **Sync:** runs on its own, merges per host when both Macs changed, and only asks about a real conflict.

Every shortcut can be rebound in Settings. The full list, with the Windows and Linux keys, is in [docs/keyboard.md](docs/keyboard.md).

## Security

- Host keys are checked against `~/.ssh/known_hosts`. A changed key refuses the connection. A key you accept is recorded with the host name hashed when `~/.ssh/config` says `HashKnownHosts yes`, as `ssh` does.
- Secrets live in the vault (age, scrypt) or in your password manager. They are never written to logs, and never typed into the terminal unless you press ⌘\\.
- Sync data is encrypted before it leaves your Mac. The repository sees `.age` files and a plain `tern-meta.json` (hash, time and a bundle number, nothing else). A pull checks the downloaded files against a manifest sealed inside them, so a remote whose files were swapped, edited or rolled back to an older copy is refused.
- Losing a Mac doesn't lose your setup: see [docs/recovery.md](docs/recovery.md). Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md).

## Other platforms and building

Windows and Linux builds are on the [Releases](https://github.com/your-moon/tern/releases) page as a preview: they pass CI but haven't been run on a real desktop yet, so reports are welcome. To build tern yourself on any platform, see [docs/building.md](docs/building.md). To contribute, start with [CONTRIBUTING.md](CONTRIBUTING.md).

## Feedback

tern is young (0.1.3), and I use it every day. If it's missing the one thing that keeps you on Termius, or something breaks, [open an issue](https://github.com/your-moon/tern/issues). I read all of them. Small fixes are very welcome too: the issues labelled [`good first issue`](https://github.com/your-moon/tern/labels/good%20first%20issue) are a good place to start.

## Credits

tern copies and adapts code from well-maintained projects rather than reinventing it, and each copied file names its origin ([NOTICE.md](NOTICE.md)): [zeron](https://github.com/zeronsh/zeron)'s UI (MIT), [russh](https://github.com/Eugeny/russh) (Apache-2.0), [alacritty_terminal](https://github.com/alacritty/alacritty) (Apache-2.0) and Zed's GPUI. Icons are Solar by 480 Design (CC BY 4.0), fonts are Geist (OFL), terminal themes come from [iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes), and the wallpapers are from [The Met Open Access](https://www.metmuseum.org/about-the-met/policies-and-documents/open-access) (CC0).

## License

[GPL-3.0](LICENSE).
