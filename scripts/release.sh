#!/usr/bin/env bash
# Builds, bundles, zips and packs tern into a drag-to-Applications dmg for a GitHub release,
# fills the Homebrew cask and prints the `gh release create` command. It publishes nothing: no
# tag, no release, no push.
#
#   scripts/release.sh
#
# TERN_SIGN_ID (a "Developer ID Application: ..." identity): the app is signed with the hardened
# runtime by bundle-app.sh and the dmg is signed too. With TERN_NOTARY_PROFILE as well (an
# `xcrun notarytool store-credentials` profile): the app is notarised and stapled (bundle-app.sh),
# the dmg is built from the stapled app, notarised and stapled, and both are checked with spctl.
# Without them those steps are skipped, with a message.
#
# The dmg follows zeron's scripts/package-macos.sh (MIT): dmgbuild writes the .DS_Store
# (background, icon view, icon positions) directly, no Finder scripting, over a hidpi tiff.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
tag="v$version"
repo=$(sed -n 's|^repository = "https://github.com/\(.*\)"|\1|p' Cargo.toml | head -n 1)
[ -n "$repo" ] || { echo "no GitHub repository in Cargo.toml" >&2; exit 1; }

scripts/bundle-app.sh

out=target/release/bundle
app="$out/tern.app"
zip="$out/tern-$version-macos-arm64.zip"
dmg="$out/tern-$version-macos-arm64.dmg"
rm -f "$zip" "$dmg"
# ditto keeps the bundle's signature and resource forks intact, which plain zip does not. The zip
# is made from the stapled app, so the Homebrew cask installs a bundle with its ticket.
ditto -c -k --keepParent "$app" "$zip"
sha=$(shasum -a 256 "$zip" | awk '{print $1}')

# dmgbuild lives in a venv under target/ at a pinned version, so the system Python is untouched.
# dmgbuild 1.6.7 needs Python 3.10+; macOS's own python3 is 3.9, so take the newest found.
venv=target/dmgbuild-venv
if ! "$venv/bin/python" -c 'import dmgbuild' 2>/dev/null; then
  py=$(command -v python3.13 || command -v python3.12 || command -v python3.11 || command -v python3.10 || true)
  [ -n "$py" ] || { echo "dmgbuild needs Python 3.10 or newer (brew install python)" >&2; exit 1; }
  rm -rf "$venv"
  "$py" -m venv "$venv"
  "$venv/bin/pip" install --quiet "dmgbuild==1.6.7"
fi

# Pair the 1x/2x renders (scripts/dmg-background.py) into a hidpi tiff for retina displays.
bg="$out/dmg-background.tiff"
tiffutil -cathidpicheck packaging/macos/dmg-background.png packaging/macos/dmg-background@2x.png \
  -out "$bg" >/dev/null 2>&1

APP="$app" DMG="$dmg" BG="$bg" "$venv/bin/python" - <<'PY'
import os
import dmgbuild

app = os.environ["APP"]
dmgbuild.build_dmg(
    filename=os.environ["DMG"],
    volume_name="tern",
    settings={
        "format": "UDZO",
        "files": [app],
        "symlinks": {"Applications": "/Applications"},
        "icon": os.path.join(app, "Contents/Resources/tern.icns"),
        "background": os.environ["BG"],
        "show_status_bar": False,
        "show_tab_view": False,
        "show_toolbar": False,
        "show_pathbar": False,
        "show_sidebar": False,
        "default_view": "icon-view",
        # Window and icon geometry must match scripts/dmg-background.py.
        "window_rect": ((200, 120), (660, 400)),
        "icon_size": 104,
        "text_size": 12,
        "icon_locations": {"tern.app": (165, 170), "Applications": (495, 170)},
    },
)
PY
rm -f "$bg"

notarise=false
if [ -n "${TERN_SIGN_ID:-}" ]; then
  codesign -s "$TERN_SIGN_ID" --timestamp "$dmg"
  echo "signed dmg with: $TERN_SIGN_ID"
  [ -z "${TERN_NOTARY_PROFILE:-}${TERN_NOTARY_KEY:-}" ] || notarise=true
else
  echo "dmg not signed (needs TERN_SIGN_ID; skipping that)"
fi

if $notarise; then
  scripts/notarize.sh "$dmg"
  xcrun stapler staple "$dmg"
  spctl -a -t open --context context:primary-signature -vv "$dmg"
  spctl -a -vv "$app"
  echo "dmg notarised and stapled"
else
  echo "dmg not notarised (needs TERN_SIGN_ID and TERN_NOTARY_PROFILE or TERN_NOTARY_KEY; skipping that)"
fi

cask="$out/tern.rb"
sed -e "s|@REPO@|$repo|g" -e "s/@VERSION@/$version/" -e "s/@SHA256@/$sha/" \
  packaging/homebrew/tern.rb > "$cask"

# The filled cask passes Homebrew's own linter before it goes to the tap: a cask Homebrew cannot
# read breaks `brew install` for everyone using the tap.
if command -v brew >/dev/null; then
  check_tap=local/tern-release-check
  brew untap "$check_tap" >/dev/null 2>&1 || true
  brew tap-new --no-git "$check_tap" >/dev/null
  mkdir -p "$(brew --repository)/Library/Taps/local/homebrew-tern-release-check/Casks"
  cp "$cask" "$(brew --repository)/Library/Taps/local/homebrew-tern-release-check/Casks/tern.rb"
  if ! brew style --cask "$check_tap/tern"; then
    brew untap "$check_tap" >/dev/null 2>&1 || true
    echo "the cask fails brew style; fix packaging/homebrew/tern.rb" >&2
    exit 1
  fi
  brew untap "$check_tap" >/dev/null 2>&1 || true
  echo "cask passes brew style"
else
  echo "cask not checked (no brew)"
fi

echo
echo "dmg:    $dmg"
echo "zip:    $zip"
echo "sha256: $sha (zip, for the cask)"
echo "cask:   $cask (copy to the tap's Casks/tern.rb)"
echo
echo "Not run. When ready, with $tag tagged and pushed:"
echo
echo "  gh release create $tag \"$dmg\" \"$zip\" --repo $repo --title \"tern $version\" --notes-file <(awk '/^## \\[$version\\]/{f=1;next} /^## \\[/{f=0} f' CHANGELOG.md)"
