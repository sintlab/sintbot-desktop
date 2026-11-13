# SintBot for macOS

A menu-bar app that removes the terminal from installing and running SintBot. Download the disk image, drag the app to Applications, open it: the window shows the service's connect page, and once a chat platform is connected it shows the console.

The app knows nothing about chat platforms. Which ones can be connected (Feishu today; Slack once the service accepts it again) is decided by the service version inside, and the connect page and console are the service's own.

The app is a shell around the unchanged SintBot service. It draws one page of its own (the status page shown while the service is being installed, started or is stopped). Everything else in its window is the console the service already serves to a browser, so the desktop and web versions are one interface.

## What the app does

| Job | How |
| --- | --- |
| No Node.js needed | Ships the official Node.js build (checksum-verified at build time, unmodified) and unpacks it to `~/.local/share/sintbot/runtime/node-<version>/` on first launch. |
| No `npm install` | Ships the service package and seeds it to `~/.local/share/sintbot/releases/<version>/`, the layout the public installer and the service's own updater use. |
| Runs in the background | Writes a per-user launchd job (`~/Library/LaunchAgents/com.sintbot.daemon.plist`) that starts `node <entry> start --ui --no-open`, restarts it if it exits and starts it at login. |
| Finds Codex and Claude Code | Asks the user's login shell for its `PATH` once and writes it into the job; the bundled runtime is appended as a fallback. |
| Shows the console | The window loads `http://127.0.0.1:4600/` once the service answers. Links that leave the console open in the default browser. |
| Menu bar | Status, open console, start, stop, restart, show log, quit. Closing the window leaves the app in the menu bar; quitting the app leaves the service running. |

Service upgrades need nothing from the app. The job has exactly the shape the service's updater looks for, so "Upgrade now" in the console installs the new version beside the old one, switches the job and rolls back if the groups do not reconnect. A new app release is only needed for a new Node.js runtime or a change to the shell itself.

## What the app never does

- Restart a running service because the app was opened.
- Edit a launchd job it did not write. An existing SintBot job on the same port is adopted as it is: the app shows its console and can start or stop it.
- Start a second service. If SintBot already runs in a terminal (its lock file names a live process), or another program holds the port, the app stops before installing anything and says so.
- Downgrade. If the service upgraded itself past the bundled version, the job keeps the newer one.
- Touch the service's data directory. The log goes to `~/Library/Logs/SintBot/daemon.log`.

## Layout

```
ui/index.html              the status page (plain HTML, no build step)
src-tauri/src/main.rs      window, menu bar, the worker that owns start/stop
src-tauri/src/layout.rs    paths, launchd label, port, development overrides
src-tauri/src/provision.rs runtime and service installation
src-tauri/src/shellenv.rs  login-shell PATH, executor lookup
src-tauri/src/service.rs   launchd job: build, read, start, stop
src-tauri/src/health.rs    what answers on the console port
scripts/fetch-runtime.sh   download and verify Node.js into src-tauri/resources/runtime
scripts/stage-payload.sh   stage a service version into src-tauri/resources/payload
scripts/dev-run.sh         run the built app against an isolated root, data directory, label and port
scripts/release-macos.sh   sign, notarize, staple and verify (release Mac only)
```

## Develop

Needs macOS, Rust and Node.js. No Apple developer account.

```bash
npm install
npm run prepare:runtime                    # once; cached under .cache/
bash scripts/stage-payload.sh ../sintbot   # a local source tree, or a version, or nothing for npm latest
npm run build:local                        # SintBot.app, unsigned
scripts/dev-run.sh                         # opens the app; service on port 4631, label com.sintbot.daemon.dev
scripts/dev-run.sh --stop                  # --start, --restart, --quit drive the running app
scripts/dev-run.sh --clean                 # stop the dev service and delete everything it created
cargo test --manifest-path src-tauri/Cargo.toml
```

To look at a console build inside the real window without any service, start a preview server and run `SINTBOT_DESKTOP_PREVIEW_URL=http://127.0.0.1:<port>/ scripts/dev-run.sh`; in that mode the app starts and stops nothing.

`dev-run.sh` sets `SINTBOT_DESKTOP_ROOT`, `SINTBOT_DESKTOP_DATA_DIR`, `SINTBOT_DESKTOP_LABEL` and `SINTBOT_DESKTOP_PORT`, so nothing is shared with a real installation on the same Mac. Run the app without them only on a Mac where it is meant to own the real service. While the install root exists, the app mirrors the status page into `<root>/desktop-status.json`.

A locally built app is signed ad hoc. It runs on the Mac that built it; copied to another Mac it is blocked by Gatekeeper, which is what the release build is for.

## Release

On the Mac with the Developer ID certificate:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: … (TEAMID)"
export APPLE_ID=… APPLE_PASSWORD=… APPLE_TEAM_ID=…
scripts/release-macos.sh 1.4.18
```

The script stages the published service version, builds, signs with the hardened runtime, notarizes and staples the app and the disk image, then runs the Gatekeeper checks and prints the image's SHA-256.

Not yet verified on a signing Mac:

- **The bundled Node.js binary.** The build leaves it with the Node.js project's own Developer ID signature and entitlements (confirmed locally: the bundler signs only the app's executable, and `codesign --verify --deep --strict` passes). That signature requests `get-task-allow`. If notarization rejects it, rerun with `SINTBOT_RESIGN_NODE=1`, which signs the binary with your identity and `release/node-entitlements.plist`.
- **A quarantined first launch** (downloaded image, app in Applications) on a Mac that has never had SintBot.
- **Login Items.** The job declares the app's bundle identifier so System Settings lists it as SintBot.

## Limits of this version

- Apple silicon only. An Intel build needs the x64 runtime and a universal shell.
- The app does not update itself; replace it with a newer disk image. The service inside updates from the console.
- Codex or Claude Code must already be installed and signed in. The status page shows which were found.
- Stop holds until the next login or Start; there is no "do not start at login" switch yet.
- Deleting the app does not remove the service. Choose "Stop SintBot and Quit" first, then delete `~/Library/LaunchAgents/com.sintbot.daemon.plist`.
- The icon is a placeholder drawn from the SintBot mark (`scripts/make-icons.js`).

## License

Distributed under the [PolyForm Shield License 1.0.0](./LICENSE), the same license as SintBot. The app bundles the SintBot service package (same license) and the official Node.js runtime (its license is shipped next to it as `runtime/LICENSE`).
