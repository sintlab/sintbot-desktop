#!/usr/bin/env bash
# Build, sign, notarize and verify SintBot.app and its disk image. Run on the
# Mac that holds the Developer ID certificate; a development Mac uses
# `npm run build:local` instead.
#
#   scripts/release-macos.sh [service version]      default: sintbot@latest on npm
#
# Required environment:
#   APPLE_SIGNING_IDENTITY   "Developer ID Application: Name (TEAMID)"
#   APPLE_ID, APPLE_PASSWORD (app-specific password), APPLE_TEAM_ID
#     or APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_KEY_PATH
# Optional:
#   SINTBOT_RESIGN_NODE=1    sign the bundled Node.js binary with the identity above
#                            instead of keeping the Node.js project's signature. Use it
#                            if notarization objects to that binary (it requests
#                            get-task-allow); release/node-entitlements.plist keeps the
#                            entitlements V8 needs and drops that one.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY to the Developer ID Application identity}"
security find-identity -v -p codesigning | grep -F "$APPLE_SIGNING_IDENTITY" >/dev/null ||
  { echo "The signing identity is not in the keychain" >&2; exit 1; }
if [ -z "${APPLE_API_KEY:-}" ]; then
  : "${APPLE_ID:?Set APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID (or the APPLE_API_* variables)}"
  : "${APPLE_PASSWORD:?}" "${APPLE_TEAM_ID:?}"
  notary=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
else
  notary=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
fi
[ -z "$(git status --porcelain 2>/dev/null)" ] || echo "Note: the working tree has uncommitted changes" >&2

npm ci --no-audit --no-fund
bash scripts/fetch-runtime.sh
bash scripts/stage-payload.sh "${1:-latest}"
node_bin=src-tauri/resources/runtime/bin/node
if [ "${SINTBOT_RESIGN_NODE:-0}" = 1 ]; then
  codesign --force --options runtime --timestamp --entitlements release/node-entitlements.plist \
    --sign "$APPLE_SIGNING_IDENTITY" "$node_bin"
  "$node_bin" -e 'new Function("return 1")()' || { echo "Re-signed Node.js cannot run" >&2; exit 1; }
fi

# The Tauri CLI signs the app with the hardened runtime, notarizes and staples it
# when the variables above are present, then builds the disk image from it.
npx tauri build --bundles app,dmg
app=src-tauri/target/release/bundle/macos/SintBot.app
dmg="$(ls src-tauri/target/release/bundle/dmg/*.dmg)"
codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$dmg"
xcrun notarytool submit "$dmg" "${notary[@]}" --wait
xcrun stapler staple "$dmg"

codesign --verify --deep --strict --verbose=2 "$app"
spctl -a -t exec -vv "$app"
spctl -a -t open --context context:primary-signature -vv "$dmg"
xcrun stapler validate "$app"
xcrun stapler validate "$dmg"
codesign -dvv "$app/Contents/Resources/runtime/bin/node" 2>&1 | grep -E 'Authority=Developer ID Application|TeamIdentifier'
printf '\nService %s · Node.js %s\n' "$(cat src-tauri/resources/payload/VERSION)" "$(cat src-tauri/resources/runtime/VERSION)"
shasum -a 256 "$dmg"
