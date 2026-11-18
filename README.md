# SintBot for macOS

A menu-bar app that removes the terminal from installing and running SintBot. Download the disk image, drag the app to Applications, open it: the window shows the service's connect page, and once a chat platform is connected it shows the console.

The app knows nothing about chat platforms. Which ones can be connected (Feishu, WeCom and Slack) is decided by the service version inside, and the connect page and console are the service's own.

The app is a shell around the unchanged SintBot service. It draws one page of its own (the status page shown while the service is being installed, started or is stopped). Everything else in its window is the console the service already serves to a browser, so the desktop and web versions are one interface.

## What the app does

| Job | How |
| --- | --- |
| No Node.js needed | Ships the official Node.js build (checksum-verified at build time, unmodified) and unpacks it to `~/.local/share/sintbot/runtime/node-<version>/` on first launch. |
| No `npm install` | Ships the service package and seeds it to `~/.local/share/sintbot/releases/<version>/`, the layout the public installer and the service's own updater use. |
| Runs in the background | Writes a per-user launchd job (`~/Library/LaunchAgents/com.sintbot.daemon.plist`) that starts `node <entry> start --ui --no-open`, restarts it if it exits and starts it at login. |
| Finds Codex and Claude Code | Asks the user's login shell for its `PATH` once and writes it into the job; the bundled runtime is appended as a fallback. |
| Shows the console | The window loads `http://127.0.0.1:4600/` once the service answers. Links that leave the console open in the default browser. |
| Menu bar | Status, open console, start, stop, restart, show log, check for updates, quit, remove the background service. Closing the window leaves the app in the menu bar; quitting the app leaves the service running. |
| Updates itself | Looks for a newer app on the GitHub Release, asks, then downloads a signed archive, replaces itself and reopens. The service keeps running throughout. See "App updates". |
| Follows the system appearance | The status page uses the console's light and dark palettes and follows the system setting. |

Service upgrades need nothing from the app. The job has exactly the shape the service's updater looks for, so "Upgrade now" in the console installs the new version beside the old one, switches the job and rolls back if the groups do not reconnect. A new app release is only needed for a new Node.js runtime or a change to the shell itself.

## App updates

The app (the shell, not the service) updates through [tauri-plugin-updater](https://v2.tauri.app/plugin/updater/).

- **When.** 30 seconds after launch and every 6 hours after that, and from "Check for Updates…" in the menu. The automatic check says nothing unless a newer version exists, asks about a version once per run, and writes failures to the log only. The menu item always answers: a newer version, "You're up to date", or the reason the check failed.
- **Asking first.** A dialog names the new version and offers "Install and Restart" or "Later". Nothing is downloaded before that.
- **Installing.** The app downloads `SintBot_<version>_aarch64.app.tar.gz` from the release, checks its signature against the public key built into the app, swaps the bundle and reopens. Only a newer version is ever installed, and the signature must have been made for exactly the version the release announces (`requireSignedVersion`), so an older signed archive cannot be passed off as a new one. The background service is a launchd job; replacing and reopening the app does not stop or restart it.
- **Where it works.** The app must sit somewhere it can be replaced. Opened straight from the disk image, or run by Gatekeeper from a randomized read-only copy, it says "Move SintBot to the Applications folder first" instead of failing halfway. In `/Applications` owned by another user, macOS asks for an administrator password.
- **Where it looks.** `https://github.com/sintlab/sintbot-desktop/releases/latest/download/latest.json`, which points at the archive under the release's own tag. When GitHub cannot be reached the check gives up after 20 seconds; a download that stalls for a minute is abandoned and the installed app is left as it was.
- **Pages get no say.** All of this runs in Rust. No capability grants the updater's commands to any page, and the console (remote content from the local service) has no access to the app at all. A console can navigate to `sintbot-app:check-update`, which does what the menu item does (the user is still asked); `window.__SINTBOT_DESKTOP_UPDATE__` is `true` in shells that have it.
- **Scripts.** `SintBot --check-update` and `SintBot --install-update` drive the running app without dialogs (the second installs and reopens). The answer goes to the app's log, `~/Library/Logs/SintBot/desktop.log`, and to the `update` entry of `desktop-status.json`. With no app running the same flags answer on the terminal and exit, without opening the app or starting the service.
- **A build without a key** (every developer build) has no updates: the menu item is disabled and `--check-update` says "updates are not configured in this build".

Versions up to 0.1.3 cannot do any of this. The first release with the updater has to be installed by hand once, from the disk image; from then on the app offers the next one itself.

## Stop, login and removal

- **A running service starts again at login.** The job file sits in `~/Library/LaunchAgents`, which launchd loads at every login, with `RunAtLoad` and `KeepAlive` set.
- **A stopped service stays stopped.** "Stop SintBot" and "Stop SintBot and Quit" unload the job and move its file to `~/.local/share/sintbot/launchd/`, so the next login starts nothing. Starting it (the menu, the status page, or opening the app) moves the file back. "Restart" does not move it.
- **Remove Background Service…** stops the service and deletes the job file the app wrote, then quits. Settings, records, installed versions and the runtime are left in place; opening the app again installs the job again. To uninstall completely, remove the service this way, drag the app to the Trash, and delete `~/.local/share/sintbot` and the data directory yourself if you no longer want them.
- Both only ever act on a job the app wrote (it carries the `SINTBOT_DESKTOP` marker). An adopted job is unloaded by Stop but its file is not moved, and removal refuses it.

There is no "do not start at login" switch for a service that is left running, and the job file cannot provide one. With `KeepAlive` set, launchd starts the job whenever it loads it, whatever `RunAtLoad` says; a job that does not start on load would also not start when the service's updater loads the new version, because that is the same launchd operation on the same file. Disabling the job (`launchctl disable`, which is also what the switch in System Settings › Login Items does) keeps it from loading at login, but then the updater's reload fails too and a console upgrade leaves the service down. A real switch needs the service's updater to enable the job before it loads it and restore the setting afterwards.

## What the app never does

- Restart a running service because the app was opened.
- Edit, move or delete a launchd job it did not write. An existing SintBot job on the same port is adopted as it is: the app shows its console and can start or stop it.
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
src-tauri/src/update.rs    updates of the app itself: check, ask, verify, replace, reopen
scripts/fetch-runtime.sh   download and verify Node.js into src-tauri/resources/runtime
scripts/stage-payload.sh   stage a service version into src-tauri/resources/payload
scripts/dev-run.sh         run the built app against an isolated root, data directory, label and port
scripts/release-macos.sh   sign, notarize, staple and verify; build the update archive and latest.json (release Mac only)
scripts/updater-keygen.sh  create the update signing key pair (release Mac, once)
scripts/updater-manifest.js verify a signed update archive and write latest.json
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
scripts/dev-run.sh --remove-service        # stop the dev service and delete its job; keeps the dev root and data
scripts/dev-run.sh --clean                 # stop the dev service and delete everything it created
cargo test --manifest-path src-tauri/Cargo.toml
node --test scripts/updater-manifest.test.js
```

To look at a console build inside the real window without any service, start a preview server and run `SINTBOT_DESKTOP_PREVIEW_URL=http://127.0.0.1:<port>/ scripts/dev-run.sh`; in that mode the app starts and stops nothing.

`dev-run.sh` sets `SINTBOT_DESKTOP_ROOT`, `SINTBOT_DESKTOP_DATA_DIR`, `SINTBOT_DESKTOP_LABEL` and `SINTBOT_DESKTOP_PORT`, so nothing is shared with a real installation on the same Mac. Run the app without them only on a Mac where it is meant to own the real service. While the install root exists, the app mirrors the status page into `<root>/desktop-status.json`.

A locally built app is signed ad hoc. It runs on the Mac that built it; copied to another Mac it is blocked by Gatekeeper, which is what the release build is for.

`npm run build:local` needs no update key and builds an app without updates. To try an update on a development Mac, use a throwaway key pair kept outside the repository and do not commit what it changes: `CI=true scripts/updater-keygen.sh /tmp/try/updater.key` writes its public key into `src-tauri/tauri.conf.json`; build once as the old version; build the new one with `TAURI_SIGNING_PRIVATE_KEY=/tmp/try/updater.key TAURI_SIGNING_PRIVATE_KEY_PASSWORD= npx tauri build --bundles app --config '{"version":"9.9.9","bundle":{"createUpdaterArtifacts":true}}'`; write a `latest.json` for the archive with `scripts/updater-manifest.js … --url http://127.0.0.1:<port>/…` and serve both from a local web server. Then run a copy of the old app with `SINTBOT_DESKTOP_APP=<copy>/SintBot.app SINTBOT_DESKTOP_UPDATE_URL=http://127.0.0.1:<port>/latest.json scripts/dev-run.sh` and `--check-update`, `--install-update`. `SINTBOT_DESKTOP_UPDATE_URL` must be https, or http on a loopback address; either way the archive still has to carry a signature from the embedded key. Under `dev-run.sh` there are no automatic checks.

## Release

On the Mac with the Developer ID certificate and the update signing key:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: … (TEAMID)"
export APPLE_API_KEY="<key ID>" APPLE_API_ISSUER="<issuer ID>"
export APPLE_API_KEY_PATH="/secure/path/AuthKey_<key ID>.p8"
export TAURI_SIGNING_PRIVATE_KEY_PATH="$HOME/.sintbot-release/updater.key"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="…"     # the empty string if the key has no password
scripts/release-macos.sh 1.4.20
```

The script stages the published service version, builds, signs with the hardened runtime, notarizes and staples the app and the disk image, then validates the App inside the read-only mounted image, runs Gatekeeper checks and prints the image's SHA-256. Keep the API private key outside the repository. Apple ID app-specific password credentials remain an alternative supported by the script.

It then prepares the update: the build archives the finished app as `SintBot.app.tar.gz` and signs the archive with the update key; the script unpacks that archive and puts the app inside through the same signature, Gatekeeper and stapling checks, verifies the archive's signature against the public key in `tauri.conf.json`, and writes `latest.json`. Everything to upload ends up in `dist/v<version>/`: the disk image, `SintBot_<version>_aarch64.app.tar.gz` and `latest.json`. Publish them on the release tagged `v<version>` and make it the latest release; a draft or pre-release is not offered to anyone.

### The update signing key

Updates are signed with a minisign key pair that is separate from the Apple certificate. The public key is in `src-tauri/tauri.conf.json` (`plugins.updater.pubkey`) and is built into every release; the private key exists only on the release Mac.

```bash
scripts/updater-keygen.sh          # once; writes ~/.sintbot-release/updater.key (0600) and the public key into tauri.conf.json
git commit -m "…" src-tauri/tauri.conf.json
```

Back the private key (and its password) up offline straight away. If it is lost, no further update can be signed and every installed app has to be replaced by hand with a new disk image. It never goes into the repository; the script refuses a path inside it and never overwrites an existing key. The release script refuses to build when the public key is missing, when the private key or its password is not available, and when the two do not belong together.

What the release script relies on, checked against the Tauri CLI 2.12.4 sources:

- **The archive holds the stapled app.** The bundler signs, notarizes and staples `SintBot.app`, builds the disk image from it, and only after every bundle is finished archives that same directory. The script does not depend on this: it unpacks the archive and checks the app inside, and if that app is not the stapled one it archives the stapled app again and signs the new archive (`tauri signer sign --app-version`), since the signature covers the archive's bytes.
- **`createUpdaterArtifacts` is switched on by the release script**, not in `tauri.conf.json`, so `npm run build:local` needs no key.
- **The signature carries the version.** `tauri build` records it in the signature's trusted comment, which `requireSignedVersion` makes the app insist on.
- **A wrong key is only a warning for the CLI.** The script turns it into an error before the build and again before `latest.json` is written.

Signing validation and remaining checks:

- **The bundled Node.js binary.** The Apple silicon v0.1.0 candidate passed App and DMG notarization and Gatekeeper with Node.js 22.23.5 retaining its original signature. Recheck on each runtime change; `SINTBOT_RESIGN_NODE=1` remains an explicit fallback if Apple rejects a future binary.
- **A quarantined first launch** (downloaded image, app in Applications) on a Mac that has never had SintBot.
- **Login Items.** On the signing Mac, System Settings groups the development job under **Node.js Foundation**, even with the app bundle identifier declared. The displayed name on a clean installation remains to be checked.
- **The first real update.** Updating has been exercised with locally built, unsigned apps only. After the first release with the updater is installed by hand, check the next one end to end from `/Applications`: the dialog, the reopened app passing Gatekeeper, and the service's process ID unchanged.

## Limits of this version

- Apple silicon only. An Intel build needs the x64 runtime and a universal shell.
- Versions up to 0.1.3 do not update themselves; replace them once with a newer disk image. The service inside updates from the console.
- Codex or Claude Code must already be installed and signed in. The status page shows which were found.
- A service left running starts again at login; there is no switch for that yet (see "Stop, login and removal").
- Deleting the app alone does not remove the service. Choose "Remove Background Service…" first.
- The Mac is not kept awake while a task runs, and the app does not announce finished tasks. Service 1.4.20 provides `GET /api/shell/activity`, answered without a token only to loopback requests that carry no proxy headers and send `X-Sbot-Local: 1`, returning counts only (`running`, `active`, `waiting`, `queued`, `held`); the app does not use it yet.
- The icon is a placeholder drawn from the SintBot mark (`scripts/make-icons.js`).

## License

Distributed under the [PolyForm Shield License 1.0.0](./LICENSE), the same license as SintBot. The app bundles the SintBot service package (same license) and the official Node.js runtime (its license is shipped next to it as `runtime/LICENSE`).
