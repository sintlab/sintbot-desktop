#!/usr/bin/env bash
# Download the official Node.js build for this Mac's architecture, verify it
# against the published SHA-256 list, and stage it for bundling:
#   src-tauri/resources/runtime/bin/node   the binary, unmodified (it keeps the
#                                          Node.js project's Developer ID signature)
#   src-tauri/resources/runtime/npm.tgz    npm, as one archive (thousands of small
#                                          files and symlinks do not belong in a bundle)
# The app unpacks both into the user's runtime directory on first launch.
set -euo pipefail
cd "$(dirname "$0")/.."
NODE_VERSION="${SINTBOT_NODE_VERSION:-v22.23.5}"
case "${SINTBOT_NODE_ARCH:-$(uname -m)}" in
  arm64) arch=arm64 ;;
  x86_64|x64) arch=x64 ;;
  *) echo "Unsupported architecture" >&2; exit 1 ;;
esac
name="node-$NODE_VERSION-darwin-$arch"
cache=".cache/node"; out="src-tauri/resources/runtime"
mkdir -p "$cache"
base="${SINTBOT_NODE_MIRROR:-https://nodejs.org/dist}/$NODE_VERSION"
[ -f "$cache/SHASUMS256-$NODE_VERSION.txt" ] || curl -fsSL --retry 3 -o "$cache/SHASUMS256-$NODE_VERSION.txt" "$base/SHASUMS256.txt"
[ -f "$cache/$name.tar.gz" ] || curl -fL --retry 3 -o "$cache/$name.tar.gz" "$base/$name.tar.gz"
want="$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$cache/SHASUMS256-$NODE_VERSION.txt")"
got="$(shasum -a 256 "$cache/$name.tar.gz" | awk '{ print $1 }')"
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "Node.js archive checksum mismatch" >&2; rm -f "$cache/$name.tar.gz"; exit 1; }
rm -rf "$out.tmp"; mkdir -p "$out.tmp"
tar -xzf "$cache/$name.tar.gz" -C "$out.tmp" --strip-components 1 "$name/bin/node" "$name/lib/node_modules/npm" "$name/LICENSE"
tar -czf "$out.tmp/npm.tgz" -C "$out.tmp/lib/node_modules" npm
rm -rf "$out.tmp/lib"
printf '%s\n' "$NODE_VERSION" > "$out.tmp/VERSION"
printf '%s\n' "$arch" > "$out.tmp/ARCH"
rm -rf "$out"; mv "$out.tmp" "$out"
echo "Node.js $NODE_VERSION ($arch) staged in $out ($(du -sh "$out" | awk '{ print $1 }'))"
