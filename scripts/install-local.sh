#!/usr/bin/env bash
# Builds tern.app via scripts/bundle-app.sh, verifies signing, installs it to /Applications/tern.app,
# smoke-checks `tern mcp` tools/list, and lists any already-running tern / tern mcp processes.
#
#   scripts/install-local.sh
set -euo pipefail
cd "$(dirname "$0")/.."

dest=/Applications/tern.app
src=target/release/bundle/tern.app

had_local_sign=0
if [ -d "$dest" ] && codesign -dvv "$dest" 2>&1 | grep -q 'Authority=tern local'; then
  had_local_sign=1
fi

scripts/bundle-app.sh

if [ "$had_local_sign" -eq 1 ] && [ -z "${TERN_SIGN_ID:-}" ]; then
  if ! codesign -dvv "$src" 2>&1 | grep -q 'Authority=tern local'; then
    echo "install-local: refusing to replace \"tern local\"-signed $dest with an ad-hoc bundle (Keychain access would reset; run outside sandbox)" >&2
    exit 1
  fi
fi

rm -rf "$dest"
ditto "$src" "$dest"
codesign --verify --deep --strict "$dest"
codesign -dvv "$dest" 2>&1 | grep -E '^(Identifier|Authority|Signature)=' || true

python3 scripts/mcp-smoke.py --bin "$dest/Contents/MacOS/tern" --tools-only

running=$(pgrep -fl "$dest/Contents/MacOS/tern" || true)
if [ -n "$running" ]; then
  echo "note: existing tern / tern mcp processes still run the previous binary until restarted/reconnected:"
  printf '%s\n' "$running" | sed 's/^/  /'
fi
