#!/usr/bin/env bash
# Run the locally built app against an isolated install root, data directory,
# launchd label and port, so it can sit next to a real installation.
#   scripts/dev-run.sh                 start the app (or show its window)
#   scripts/dev-run.sh --stop|--start|--restart|--quit   drive the running app
#   scripts/dev-run.sh --check-update|--install-update   look for a newer app; install it without asking and reopen
#   scripts/dev-run.sh --remove-service   stop the dev service, delete its launchd job, quit; keeps the dev root and data
#   scripts/dev-run.sh --clean         stop the dev service and delete everything it created
# SINTBOT_DESKTOP_APP runs another copy of the app than the one in the build
# directory (an update replaces the copy it runs from). SINTBOT_DESKTOP_UPDATE_URL
# points the update check at another latest.json. SINTBOT_DESKTOP_LOGIN_ITEM_SIMULATED=1
# offers "Open SintBot at Login" against a file in the dev root instead of the
# system's setting; SINTBOT_DESKTOP_LAUNCHED_AT_LOGIN=1 starts as at login.
set -euo pipefail
cd "$(dirname "$0")/.."
dev="${SINTBOT_DESKTOP_DEV_DIR:-$PWD/.cache/dev}"
export SINTBOT_DESKTOP_ROOT="$dev/root" SINTBOT_DESKTOP_DATA_DIR="$dev/data"
export SINTBOT_DESKTOP_LABEL="${SINTBOT_DESKTOP_LABEL:-com.sintbot.daemon.dev}" SINTBOT_DESKTOP_PORT="${SINTBOT_DESKTOP_PORT:-4631}"
app="${SINTBOT_DESKTOP_APP:-$PWD/src-tauri/target/release/bundle/macos/SintBot.app}/Contents/MacOS/sintbot-desktop"
if [ "${1:-}" = --clean ]; then
  "$app" --quit >/dev/null 2>&1 || true
  launchctl bootout "gui/$(id -u)/$SINTBOT_DESKTOP_LABEL" 2>/dev/null || true
  rm -f "$HOME/Library/LaunchAgents/$SINTBOT_DESKTOP_LABEL.plist"
  rm -rf "$dev"
  echo "Removed the development service and $dev"
  exit 0
fi
[ -x "$app" ] || { echo "Build first: npm run build:local" >&2; exit 1; }
mkdir -p "$dev"
exec "$app" "$@"
