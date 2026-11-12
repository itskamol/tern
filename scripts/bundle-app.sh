#!/usr/bin/env bash
# Builds a release tern and assembles target/release/bundle/tern.app.
#
#   scripts/bundle-app.sh
#
# Signing: the local "tern local" identity when present (else ad-hoc), so the bundle runs on this Mac. With TERN_SIGN_ID (a "Developer ID
# Application: ..." identity) it is signed with the hardened runtime instead; with
# TERN_NOTARY_PROFILE too (a `xcrun notarytool store-credentials` profile) it is notarised and
# stapled. Without them those steps are skipped, with a message.
#
# Plain shell rather than cargo-bundle: the bundle is one binary, one plist, one icon and the
# licence files (fonts, icons and themes are compiled in with include_bytes!), and a script
# adds signing and notarisation, which cargo-bundle does not do.
set -euo pipefail
cd "$(dirname "$0")/.."

BUNDLE_ID="mn.tern.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

cargo build --release -p tern

app=target/release/bundle/tern.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/tern "$app/Contents/MacOS/tern"
cp packaging/macos/tern.icns "$app/Contents/Resources/tern.icns"
# The licences travel with the binary: tern's own (GPL-3.0, whose terms ask for it), what it
# copied (NOTICE.md), and every linked crate's notice (scripts/third-party.py).
cp LICENSE "$app/Contents/Resources/LICENSE"
cp NOTICE.md "$app/Contents/Resources/NOTICE.md"
python3 -I scripts/third-party.py "$app/Contents/Resources/THIRD-PARTY.txt"

# The oldest macOS the binary loads on is whatever rustc linked it for; read it back instead of
# guessing (gpui itself supports 10.15, rustc's arm64 floor is 11.0).
minos=$(otool -l "$app/Contents/MacOS/tern" | awk '/LC_BUILD_VERSION/ {f=1} f && /minos/ {print $2; exit}')
minos=${minos:-11.0}

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>tern</string>
  <key>CFBundleIconFile</key><string>tern</string>
  <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>tern</string>
  <key>CFBundleDisplayName</key><string>tern</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>${minos}</string>
  <key>NSAutoFillRequiresTextContentTypeForOneTimeCodeOnMac</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist"
[ "$(plutil -extract NSAutoFillRequiresTextContentTypeForOneTimeCodeOnMac raw "$app/Contents/Info.plist")" = true ] \
  || { echo "Info.plist lost the AutoFill opt-out key" >&2; exit 1; }

if [ -n "${TERN_SIGN_ID:-}" ]; then
  codesign --force --deep --options runtime --timestamp -s "$TERN_SIGN_ID" "$app"
  echo "signed with: $TERN_SIGN_ID"
else
  # A local self-signed identity ("tern local", see scripts/make-local-identity.sh) gives every
  # build the same designated requirement, so the Keychain's "Always Allow" survives updates;
  # an ad-hoc signature is a new fingerprint each build and macOS asks for the password again.
  if security find-identity -p codesigning 2>/dev/null | grep -q '"tern local"'; then
    codesign --force --deep -s "tern local" "$app"
    echo "signed with the local identity \"tern local\" (set TERN_SIGN_ID for a Developer ID)"
  else
    codesign --force --deep -s - "$app"
    echo "ad-hoc signed: run scripts/make-local-identity.sh once so Keychain access survives updates"
  fi
fi

if [ -n "${TERN_SIGN_ID:-}" ] && [ -n "${TERN_NOTARY_PROFILE:-}${TERN_NOTARY_KEY:-}" ]; then
  zip=$(mktemp -d)/tern-notarize.zip
  ditto -c -k --keepParent "$app" "$zip"
  scripts/notarize.sh "$zip"
  xcrun stapler staple "$app"
  echo "notarised and stapled"
else
  echo "not notarised (needs TERN_SIGN_ID and TERN_NOTARY_PROFILE or TERN_NOTARY_KEY; skipping that)"
fi

codesign --verify --deep --strict "$app"
echo "built $app ($version)"
