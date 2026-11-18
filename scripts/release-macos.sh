#!/usr/bin/env bash
# Build, sign, notarize and verify SintBot.app, its disk image and the archive
# installed apps update from. Run on the Mac that holds the Developer ID
# certificate and the update signing key; a development Mac uses
# `npm run build:local` instead.
#
#   scripts/release-macos.sh [service version]      default: sintbot@latest on npm
#
# Required environment:
#   APPLE_SIGNING_IDENTITY   "Developer ID Application: Name (TEAMID)"
#   APPLE_ID, APPLE_PASSWORD (app-specific password), APPLE_TEAM_ID
#     or APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_KEY_PATH
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD   password of the update signing key; set it to
#                            the empty string if the key has none
# The update signing key itself (made once by scripts/updater-keygen.sh) is read from
#   TAURI_SIGNING_PRIVATE_KEY_PATH, default ~/.sintbot-release/updater.key,
#     or TAURI_SIGNING_PRIVATE_KEY (the key text, or a path to it)
# Optional:
#   SINTBOT_UPDATE_NOTES     the "notes" text in latest.json
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
  : "${APPLE_API_KEY_PATH:?Set APPLE_API_KEY_PATH}" "${APPLE_API_ISSUER:?Set APPLE_API_ISSUER}"
  notary=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
fi
[ -z "$(git status --porcelain 2>/dev/null)" ] || echo "Note: the working tree has uncommitted changes" >&2

# Updates. An installed app accepts an update only if it is signed with the
# private key belonging to the public key it embeds, so a release without that
# pair is not built at all.
app_version="$(node -p "require('./src-tauri/tauri.conf.json').version")"
[ -n "$(node -p "require('./src-tauri/tauri.conf.json').plugins.updater.pubkey")" ] ||
  { echo "No update public key in src-tauri/tauri.conf.json. Run scripts/updater-keygen.sh once on this Mac and commit the result." >&2; exit 1; }
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  key_file="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.sintbot-release/updater.key}"
  [ -r "$key_file" ] || { echo "The update signing key is not at $key_file (set TAURI_SIGNING_PRIVATE_KEY_PATH)" >&2; exit 1; }
  update_key="$(cat "$key_file")"
elif [ -f "$TAURI_SIGNING_PRIVATE_KEY" ]; then
  # `tauri build` takes a path here, `tauri signer sign` only the key text.
  update_key="$(cat "$TAURI_SIGNING_PRIVATE_KEY")"
else
  update_key="$TAURI_SIGNING_PRIVATE_KEY"
fi
# Unset, the CLI would stop in the middle of the build to ask for it.
update_key_password="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD?Set TAURI_SIGNING_PRIVATE_KEY_PASSWORD (to the empty string if the key has no password)}"
# Only the two commands that sign get to see the key, not npm and the staging scripts.
unset TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PATH TAURI_SIGNING_PRIVATE_KEY_PASSWORD
with_update_key() { TAURI_SIGNING_PRIVATE_KEY="$update_key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$update_key_password" "$@"; }
sign_update() { with_update_key npx tauri signer sign --app-version "$app_version" "$1" >/dev/null; }

work="$(mktemp -d "${TMPDIR:-/tmp}/sintbot-release.XXXXXX")"
mount_dir="$work/mount"
cleanup() {
  hdiutil detach "$mount_dir" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

npm ci --no-audit --no-fund
# Before the long part: the key opens with the password and belongs to the
# public key in the configuration. The Tauri CLI only warns about a mismatch.
printf 'sintbot updater key check\n' > "$work/probe"
sign_update "$work/probe" && node scripts/updater-manifest.js "$work/probe" "$app_version" ||
  { echo "The update signing key cannot be used, or does not belong to the public key in src-tauri/tauri.conf.json" >&2; exit 1; }
bash scripts/fetch-runtime.sh
bash scripts/stage-payload.sh "${1:-latest}"
node_bin=src-tauri/resources/runtime/bin/node
if [ "${SINTBOT_RESIGN_NODE:-0}" = 1 ]; then
  codesign --force --options runtime --timestamp --entitlements release/node-entitlements.plist \
    --sign "$APPLE_SIGNING_IDENTITY" "$node_bin"
  "$node_bin" -e 'new Function("return 1")()' || { echo "Re-signed Node.js cannot run" >&2; exit 1; }
fi

# The Tauri CLI signs the app with the hardened runtime, notarizes and staples it
# when the variables above are present, then builds the disk image from it. With
# createUpdaterArtifacts it finally archives that same app as SintBot.app.tar.gz
# and signs the archive with the update key. The option is given here and not in
# tauri.conf.json so that `npm run build:local` needs no key.
with_update_key npx tauri build --bundles app,dmg --config '{"bundle":{"createUpdaterArtifacts":true}}'
dmgs=(src-tauri/target/release/bundle/dmg/*_"${app_version}"_*.dmg)
[ "${#dmgs[@]}" = 1 ] && [ -f "${dmgs[0]}" ] ||
  { echo "Expected exactly one DMG in the bundle directory" >&2; exit 1; }
dmg="${dmgs[0]}"
codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$dmg"
xcrun notarytool submit "$dmg" "${notary[@]}" --wait
xcrun stapler staple "$dmg"

# The DMG bundler may clean the intermediate .app. Verify the actual shipped
# copy, mounted read-only, rather than relying on that intermediate directory.
mkdir "$mount_dir"
hdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$mount_dir" >/dev/null
app="$mount_dir/SintBot.app"
codesign --verify --deep --strict --verbose=2 "$app"
spctl -a -t exec -vv "$app"
spctl -a -t open --context context:primary-signature -vv "$dmg"
xcrun stapler validate "$app"
xcrun stapler validate "$dmg"
codesign -dvv "$app/Contents/Resources/runtime/bin/node" 2>&1 | grep -E 'Authority=Developer ID Application|TeamIdentifier'

# The update archive. An installed app unpacks it over itself, so the app inside
# must be the finished one: signed, notarized and stapled. Tauri's bundler makes
# the archive after all of that (see the README), and this checks the result
# instead of trusting the order: the archive is unpacked and the app inside is
# examined the way the disk image's copy was.
bundle_dir=src-tauri/target/release/bundle/macos
archive="$bundle_dir/SintBot.app.tar.gz"
[ -f "$archive" ] && [ -f "$archive.sig" ] ||
  { echo "The build produced no update archive with a signature in $bundle_dir" >&2; exit 1; }
check_archived_app() {
  rm -rf "$work/unpacked" && mkdir "$work/unpacked" && tar -xzf "$archive" -C "$work/unpacked" &&
    [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$work/unpacked/SintBot.app/Contents/Info.plist")" = "$app_version" ] &&
    codesign --verify --deep --strict --verbose=2 "$work/unpacked/SintBot.app" &&
    spctl -a -t exec -vv "$work/unpacked/SintBot.app" &&
    xcrun stapler validate "$work/unpacked/SintBot.app"
}
if ! check_archived_app; then
  # Not expected. Make the archive again from the app the disk image was built
  # from, and sign it again: the signature covers the archive's bytes.
  echo "The archived app is not the signed, notarized and stapled $app_version; archiving $bundle_dir/SintBot.app again" >&2
  codesign --verify --deep --strict "$bundle_dir/SintBot.app"
  spctl -a -t exec "$bundle_dir/SintBot.app"
  xcrun stapler validate "$bundle_dir/SintBot.app" || xcrun stapler staple "$bundle_dir/SintBot.app"
  rm -f "$archive" "$archive.sig"
  COPYFILE_DISABLE=1 tar -czf "$archive" -C "$bundle_dir" SintBot.app
  sign_update "$archive"
  check_archived_app
fi

# What goes on the GitHub Release. latest.json is only written after the
# archive's signature verified against the public key the apps embed.
out="dist/v$app_version"
asset="SintBot_${app_version}_aarch64.app.tar.gz"
rm -rf "$out" && mkdir -p "$out"
cp "$dmg" "$out/"
cp "$archive" "$out/$asset"
cp "$archive.sig" "$out/$asset.sig"
node scripts/updater-manifest.js "$out/$asset" "$app_version" --out "$out/latest.json" --notes "${SINTBOT_UPDATE_NOTES:-}"

printf '\nService %s · Node.js %s\n' "$(cat src-tauri/resources/payload/VERSION)" "$(cat src-tauri/resources/runtime/VERSION)"
shasum -a 256 "$dmg"
cat <<NOTE

Upload to the GitHub Release with the tag v$app_version:
  $out/$(basename "$dmg")
  $out/$asset
  $out/latest.json
Installed apps read releases/latest/download/latest.json, so they are offered
$app_version as soon as that release is published as the latest one (not as a
draft or a pre-release), and they download the archive from the v$app_version tag.
NOTE
