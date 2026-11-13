#!/usr/bin/env bash
# Stage the SintBot service package the app seeds on first launch: one archive
# (src-tauri/resources/payload/sintbot.tgz) holding the same layout the public
# installer produces (lib/node_modules/sintbot with its dependencies).
#   scripts/stage-payload.sh                 # sintbot@latest from the npm registry
#   scripts/stage-payload.sh 1.4.18          # an exact published version
#   scripts/stage-payload.sh /path/to/tree   # a local source tree (npm pack is run on it)
#   scripts/stage-payload.sh /path/x.tgz     # a packed tarball
set -euo pipefail
cd "$(dirname "$0")/.."
src="${1:-latest}"
out="$PWD/src-tauri/resources/payload"
runtime="$PWD/src-tauri/resources/runtime"
[ -x "$runtime/bin/node" ] && [ -f "$runtime/npm.tgz" ] || { echo "Run scripts/fetch-runtime.sh first" >&2; exit 1; }
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
# Build with the exact runtime that will run the service, not the developer's.
mkdir -p "$work/rt/bin" "$work/rt/lib/node_modules"
cp "$runtime/bin/node" "$work/rt/bin/node"
tar -xzf "$runtime/npm.tgz" -C "$work/rt/lib/node_modules"
ln -s ../lib/node_modules/npm/bin/npm-cli.js "$work/rt/bin/npm"
export PATH="$work/rt/bin:/usr/bin:/bin:/usr/sbin:/sbin"
if [ -d "$src" ]; then
  (cd "$src" && npm pack --ignore-scripts --silent --pack-destination "$work" >/dev/null)
  spec="$(ls "$work"/*.tgz)"
elif [ -f "$src" ]; then spec="$(cd "$(dirname "$src")" && pwd)/$(basename "$src")"
else spec="sintbot@$src"; fi
rm -rf "$out.tmp"; mkdir -p "$out.tmp"
npm install --registry="${SINTBOT_NPM_REGISTRY:-https://registry.npmjs.org}" --global --prefix "$out.tmp/prefix" \
  --omit=dev --ignore-scripts --no-audit --no-fund "$spec" >/dev/null
entry="$out.tmp/prefix/lib/node_modules/sintbot/bin/sintbot-v2.js"
version="$(node "$entry" version)"
node -e 'const p=require(process.argv[1]);if(p.name!=="sintbot"||p.version!==process.argv[2])process.exit(1)' \
  "$out.tmp/prefix/lib/node_modules/sintbot/package.json" "$version"
[ -z "$(find "$out.tmp/prefix/lib" -type f -perm +111 -exec file {} + | grep -i 'mach-o' || true)" ] ||
  { echo "The service package contains native binaries; they must be shipped and signed individually" >&2; exit 1; }
tar -czf "$out.tmp/sintbot.tgz" -C "$out.tmp/prefix" lib
rm -rf "$out.tmp/prefix"
printf '%s\n' "$version" > "$out.tmp/VERSION"
rm -rf "$out"; mv "$out.tmp" "$out"
echo "SintBot $version staged in $out ($(du -sh "$out" | awk '{ print $1 }'))"
