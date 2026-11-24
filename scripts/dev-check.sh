#!/usr/bin/env bash
# Runs a debug build as its own app beside the real tern, for checking a change by eye
# (docs/testing.md, "End-to-end is by eye"), against two local demo SSH servers.
#
#   scripts/dev-check.sh                    build, seed the config if missing, start the servers, launch
#   scripts/dev-check.sh --fresh            the same over a new, empty config
#   scripts/dev-check.sh keys "<script>"    the same, then replay the script into the window
#   scripts/dev-check.sh keys "<script>" D  the same against the config directory D (made by hand,
#                                           for a state the seed does not have, such as a vault)
#   scripts/dev-check.sh shot <out.png>     capture the on-screen TernDev window bounds to <out.png>
#   scripts/dev-check.sh stop               quit the app and the servers
#
# A keys script is how a check is driven without touching the screen: keystrokes, typing, waits,
# clicks at window points and clicks with modifiers held. Its words are listed at the top of
# crates/tern/src/devkeys.rs. Each `keys` run starts the app again, so a script is the whole
# path to the state under check, e.g. "alt-cmd-o sleep-1500 click:700,220 cmd-click:700,260".
#
# What it sets up, all under target/dev-check/:
# - TernDev.app, bundle id dev.tern.check: a real bundle, so window tools and screenshots can
#   find it, and it never shares a window, lock or bundle id with an installed tern;
# - config/: TERN_CONFIG_DIR with hosts `demo` (password hunter2), `demo-cmd` (a password
#   command, asked about before it first runs) and `demo-2` (a second server, for anything that
#   moves between two hosts); the first two are reopened as idle tabs;
# - two servers on 127.0.0.1, user test, password hunter2: port 2299 serving sftp/ and port 2298
#   serving sftp-2/, each with a few files in home/; their pids are in servers.pid;
# - known_hosts: TERN_KNOWN_HOSTS, so ~/.ssh/known_hosts is never touched.
# The window opens at 80,80 on the main display, 1100x700, so screenshots find it whole. It
# opens on the desktop Space: while a full-screen app has the screen, it is there but not in view
# (window tools find no window, and the frame stats count almost no frames, since nothing is
# drawn). Leave full screen, or switch to the desktop, before checking anything;
# `swift scripts/onscreen-windows.swift` says what holds the screen and where tern's windows are.
# TERN_DEV_WINDOW, TERN_DEV_WINDOW_AT and TERN_DEV_KEYS work in debug builds only (TERN_DEV_KEYS
# also in a release built with `--features dev-keys`). A GPUI window takes no keystrokes sent to
# it in the background: drive it with `keys`, or with input to the screen.
# A debug build keeps its Keychain items apart too ("tern vault (debug)").
set -euo pipefail
cd "$(dirname "$0")/.."

target_dir=${CARGO_TARGET_DIR:-target}
root=target/dev-check
app="$root/TernDev.app"
config="$PWD/$root/config"
pids="$root/servers.pid"
port=2299
port2=2298
keys=""

stop_servers() {
  [ -f "$pids" ] || return 0
  while read -r pid; do kill "$pid" 2>/dev/null || true; done < "$pids"
  rm -f "$pids"
}

case "${1:-}" in
  stop)
    pkill -f "$app/Contents/MacOS/tern" || true
    stop_servers
    echo "stopped"
    exit 0
    ;;
  shot)
    out=${2:?usage: scripts/dev-check.sh shot <output.png>}
    mkdir -p "$(dirname "$out")"
    bounds=$(swift scripts/onscreen-windows.swift TernDev \
      | awk '/TernDev at [0-9-]+,[0-9-]+ size [0-9]+x[0-9]+, in view/ {
          gsub(/,$/, "", $3); split($3, pos, ","); split($5, sz, "x");
          printf "%s,%s,%s,%s\n", pos[1], pos[2], sz[1], sz[2]; exit
        }')
    if [ -z "$bounds" ]; then
      echo "dev-check: no in-view TernDev window found (run swift scripts/onscreen-windows.swift)" >&2
      exit 1
    fi
    screencapture -x -R "$bounds" "$out"
    echo "$out ($bounds)"
    exit 0
    ;;
  keys)
    keys=${2:?usage: scripts/dev-check.sh keys "<script>" [config directory]}
    if [ -n "${3:-}" ]; then config=$(cd "$3" && pwd); fi
    ;;
esac

pkill -f "$app/Contents/MacOS/tern" || true
cargo build -q -p tern
cargo build -q -p tern-ssh --example password_server

mkdir -p "$app/Contents/MacOS"
cp "$target_dir/debug/tern" "$app/Contents/MacOS/tern"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.tern.check</string>
  <key>CFBundleName</key><string>TernDev</string>
  <key>CFBundleExecutable</key><string>tern</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign -s - --force "$app" 2>/dev/null

if [ "${1:-}" = --fresh ]; then
  rm -rf "$config"
fi
if [ ! -d "$config" ]; then
  mkdir -p "$config"
  printf '{"appearance":"dark","reopenTabs":true,"showStatusLine":true}\n' > "$config/settings.json"
  cat > "$config/hosts.json" <<JSON
{"version":1,"connections":[
  {"name":"demo","host":"127.0.0.1","port":$port,"user":"test"},
  {"name":"demo-cmd","host":"127.0.0.1","port":$port,"user":"test","passwordCommand":"printf hunter2"},
  {"name":"demo-2","host":"127.0.0.1","port":$port2,"user":"test"}
]}
JSON
  printf '{"tabs":[{"kind":"ssh","alias":"demo"},{"kind":"ssh","alias":"demo-cmd"}],"active":0}\n' \
    > "$config/tabs.json"
fi

serve() { # port, served folder, log
  lsof -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1 && return 0
  if [ ! -d "$2/home" ]; then
    mkdir -p "$2/home/logs"
    for f in notes.txt report.csv todo.md; do printf 'port %s: %s\n' "$1" "$f" > "$2/home/$f"; done
    printf 'started\n' > "$2/home/logs/app.log"
  fi
  "$target_dir/debug/examples/password_server" --demo --sftp "$2" "$1" hunter2 > "$3" 2>&1 &
  echo $! >> "$pids"
}
serve "$port" "$PWD/$root/sftp" "$root/demo.log"
serve "$port2" "$PWD/$root/sftp-2" "$root/demo-2.log"
sleep 1

launch=(--env TERN_CONFIG_DIR="$config" --env TERN_KNOWN_HOSTS="$(dirname "$config")/known_hosts"
  --env TERN_DEV_WINDOW=1100x700 --env TERN_DEV_WINDOW_AT=80,80)
if [ -n "$keys" ]; then launch+=(--env TERN_DEV_KEYS="$keys"); fi
open -n "${launch[@]}" "$app"

echo "app:       $app (bundle id dev.tern.check)"
echo "config:    $config"
echo "servers:   127.0.0.1:$port (demo, demo-cmd) and :$port2 (demo-2), user test, password hunter2"
echo "stop with: scripts/dev-check.sh stop"
