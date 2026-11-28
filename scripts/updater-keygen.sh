#!/usr/bin/env bash
# Create the key pair that signs updates of the app. Run it ONCE, on the
# release Mac.
#
#   scripts/updater-keygen.sh [private key file]
#       default: $SINTBOT_UPDATER_KEY, or ~/.sintbot-release/updater.key
#
# The private key is written outside the repository with mode 0600. The public
# key goes into src-tauri/tauri.conf.json (plugins.updater.pubkey), which every
# build embeds: an installed app only accepts updates signed with the private
# key that belongs to it. An existing key is never overwritten.
#
# The Tauri CLI asks for a password to protect the private key, and again when
# the new key is tried out at the end. CI=true skips the questions and makes a
# key without a password.
set -euo pipefail
start_dir="$PWD"
cd "$(dirname "$0")/.."
key="${1:-${SINTBOT_UPDATER_KEY:-$HOME/.sintbot-release/updater.key}}"
case "$key" in /*) ;; *) key="$start_dir/$key" ;; esac
conf=src-tauri/tauri.conf.json
case "$key" in "$PWD"/*) echo "Keep the private key outside the repository: $key" >&2; exit 1 ;; esac
if [ -e "$key" ] || [ -e "$key.pub" ]; then
  echo "$key (or $key.pub) already exists and is left as it is." >&2
  echo "Apps that were released with its public key can only be updated with this key." >&2
  exit 1
fi
if [ -n "$(node -p "require('./$conf').plugins.updater.pubkey")" ]; then
  echo "$conf already holds a public key. Released apps accept updates from its private key only;" >&2
  echo "replacing it cuts them off from updates. Empty plugins.updater.pubkey by hand if that is intended." >&2
  exit 1
fi
[ -d node_modules/@tauri-apps/cli ] || npm ci --no-audit --no-fund

umask 077
mkdir -p "$(dirname "$key")"
npx tauri signer generate --write-keys "$key"
chmod 600 "$key"
node -e '
  const fs = require("fs"), [file, keyFile] = process.argv.slice(1);
  const config = JSON.parse(fs.readFileSync(file, "utf8"));
  config.plugins.updater.pubkey = fs.readFileSync(keyFile, "utf8").trim();
  fs.writeFileSync(file, JSON.stringify(config, null, 2) + "\n");
' "$conf" "$key.pub"

# Try the pair out: sign something with the private key and check it against
# the public key the app will embed.
probe="$(mktemp -d "${TMPDIR:-/tmp}/sintbot-keygen.XXXXXX")"
trap 'rm -rf "$probe"' EXIT
printf 'sintbot updater key check\n' > "$probe/probe"
env -u TAURI_SIGNING_PRIVATE_KEY -u TAURI_SIGNING_PRIVATE_KEY_PATH \
  npx tauri signer sign --private-key-path "$key" --app-version 0.0.0 "$probe/probe" >/dev/null
node scripts/updater-manifest.js "$probe/probe" 0.0.0

cat <<NOTE

Private key: $key (mode 0600). It is not in the repository and must never be.
Public key:  written to $conf. Commit that change.

BACK UP THE PRIVATE KEY NOW, offline, together with its password if you set one.
If it is lost, no update can be signed any more, and every installed app has to
be replaced by hand with a newly downloaded disk image.

To build a release:
  export TAURI_SIGNING_PRIVATE_KEY_PATH="$key"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="..."     # the empty string if the key has no password
  scripts/release-macos.sh
NOTE
