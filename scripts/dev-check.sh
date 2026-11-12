#!/usr/bin/env bash
# Runs a debug build as its own app beside the real tern, for checking a change by eye
# (docs/testing.md, "End-to-end is by eye"), against a local demo SSH server.
#
#   scripts/dev-check.sh          build, seed the config if missing, start the server, launch
#   scripts/dev-check.sh --fresh  the same over a new, empty config
#   scripts/dev-check.sh stop     quit the app and the server
#
# What it sets up, all under target/dev-check/:
# - TernDev.app, bundle id dev.tern.check: a real bundle, so window tools and screenshots can
#   find it, and it never shares a window, lock or bundle id with an installed tern;
# - config/: TERN_CONFIG_DIR with hosts `demo` (password hunter2) and `demo-cmd` (a password
#   command, asked about before it first runs), both reopened as idle tabs;
# - known_hosts: TERN_KNOWN_HOSTS, so ~/.ssh/known_hosts is never touched.
# The window opens at 80,80 on the main display, 1100x700, so screenshots find it whole.
# A debug build keeps its Keychain items apart too ("tern vault (debug)").
set -euo pipefail
cd "$(dirname "$0")/.."

root=target/dev-check
app="$root/TernDev.app"
config="$PWD/$root/config"
port=2299

if [ "${1:-}" = stop ]; then
  pkill -f "$app/Contents/MacOS/tern" || true
  pkill -f "password_server --demo $port" || true
  echo "stopped"
  exit 0
fi

pkill -f "$app/Contents/MacOS/tern" || true
cargo build -q -p tern
cargo build -q -p tern-ssh --example password_server

mkdir -p "$app/Contents/MacOS"
cp target/debug/tern "$app/Contents/MacOS/tern"
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
  {"name":"demo-cmd","host":"127.0.0.1","port":$port,"user":"test","passwordCommand":"printf hunter2"}
]}
JSON
  printf '{"tabs":[{"kind":"ssh","alias":"demo"},{"kind":"ssh","alias":"demo-cmd"}],"active":0}\n' \
    > "$config/tabs.json"
fi

if ! lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
  target/debug/examples/password_server --demo "$port" hunter2 > "$root/demo.log" 2>&1 &
  sleep 1
fi

open -n --env TERN_CONFIG_DIR="$config" --env TERN_KNOWN_HOSTS="$PWD/$root/known_hosts" \
  --env TERN_DEV_WINDOW=1100x700 --env TERN_DEV_WINDOW_AT=80,80 "$app"

echo "app:       $app (bundle id dev.tern.check)"
echo "config:    $config"
echo "server:    127.0.0.1:$port, user test, password hunter2 (log: $root/demo.log)"
echo "stop with: scripts/dev-check.sh stop"
