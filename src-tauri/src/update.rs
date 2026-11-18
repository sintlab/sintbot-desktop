//! Updates of the app itself (the shell). The service inside updates from its
//! console and is not involved: the bundle is replaced and the app reopened
//! while the launchd job keeps running.
//!
//! Checking, downloading, verifying the signature and replacing the bundle all
//! happen here, in Rust, through tauri-plugin-updater. No page is given updater
//! permissions; a page can only ask for the "check, then ask the user" flow the
//! menu item starts.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::Serialize;
use tauri::{AppHandle, Manager, Url};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Error, Update, UpdaterExt};

use crate::layout::Layout;
use crate::{text, Shared, Text};

/// Where to look instead of the release endpoint. For tests and for trying a
/// release before it is published. The signature is still checked against the
/// embedded public key, so this cannot make the app install anything else.
pub const ENDPOINT_ENV: &str = "SINTBOT_DESKTOP_UPDATE_URL";

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A download that delivers nothing for this long is given up.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LOG_LIMIT: u64 = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trigger {
    /// The timer. Silent unless a new version is found, and asks once per version.
    Auto,
    /// The menu item, or the console asking for the same thing. Always answers.
    User,
    /// `--check-update` and `--install-update`: no dialogs.
    Script { install: bool },
}

/// What the build and the environment say about updates, read once at startup.
#[derive(Debug, Default)]
pub struct Source {
    /// A public key is embedded. Without one (a developer build) nothing is checked.
    pub configured: bool,
    /// The endpoint override was refused. Reported instead of checking.
    pub problem: Option<String>,
    /// The endpoint is plain http on this Mac: no proxy, and the archive may come from there too.
    pub local: bool,
}

/// Accepts https anywhere and plain http only from this Mac.
pub fn endpoint_override(value: &str) -> Result<Url, String> {
    let refused = || format!("{ENDPOINT_ENV} must be an https URL, or an http URL on a loopback address");
    let url = Url::parse(value.trim()).map_err(|_| refused())?;
    if url.scheme() == "https" && url.host_str().is_some() || crate::is_loopback(&url) { Ok(url) } else { Err(refused()) }
}

/// Reads the updater settings and applies the endpoint override. The plugin
/// refuses plain http unless told otherwise, and it is told so only here, for
/// an override that was just checked to be a loopback address.
pub fn prepare(config: &mut tauri::Config, endpoint: Option<&str>) -> Source {
    let Some(settings) = config.plugins.0.get_mut("updater").and_then(|value| value.as_object_mut()) else {
        return Source::default();
    };
    let configured = settings.get("pubkey").and_then(|key| key.as_str()).is_some_and(|key| !key.trim().is_empty());
    let mut source = Source { configured, ..Source::default() };
    match endpoint.filter(|value| !value.trim().is_empty()).map(endpoint_override) {
        None => {}
        Some(Err(problem)) => source.problem = Some(problem),
        Some(Ok(url)) => {
            source.local = url.scheme() == "http";
            settings.insert("endpoints".into(), serde_json::json!([url.as_str()]));
            settings.insert("dangerousInsecureTransportProtocol".into(), source.local.into());
        }
    }
    source
}

/// Newer only. An equal or older release is never installed, whatever the endpoint says.
pub fn is_upgrade(current: &Version, candidate: &Version) -> bool { candidate > current }

/// The archive must come over https, except in a test against this Mac.
fn archive_allowed(url: &Url, local: bool) -> bool {
    url.scheme() == "https" || (local && crate::is_loopback(url))
}

#[derive(Debug, PartialEq)]
pub enum Blocker { NotAnApp, Translocated, ReadOnly }

/// `…/SintBot.app` for `…/SintBot.app/Contents/MacOS/sintbot-desktop`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// Why the app cannot be replaced where it is. Opened straight from the disk
/// image, or run by Gatekeeper from a randomized read-only copy, there is
/// nothing to write to; outside a bundle the updater would replace the
/// directory the binary happens to sit in.
fn blocker(exe: &Path, read_only: impl Fn(&Path) -> bool) -> Option<Blocker> {
    let Some(bundle) = bundle_of(exe) else { return Some(Blocker::NotAnApp) };
    if bundle.components().any(|part| part.as_os_str() == "AppTranslocation") { return Some(Blocker::Translocated) }
    read_only(&bundle).then_some(Blocker::ReadOnly)
}

fn on_read_only_volume(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `statfs` fills
    // `info` completely when it returns 0, the only case in which it is read.
    unsafe { libc::statfs(c_path.as_ptr(), info.as_mut_ptr()) == 0 && info.assume_init().f_flags & libc::MNT_RDONLY as u32 != 0 }
}

/// Whether this trigger goes on to the question (or, for a script, to the
/// installation). The automatic check asks once per version per run; a "Later"
/// from the menu counts too.
fn proceeds(asked: &mut HashSet<String>, trigger: Trigger, version: &str) -> bool {
    match trigger {
        Trigger::Script { install } => install,
        Trigger::User => { asked.insert(version.to_string()); true }
        Trigger::Auto => asked.insert(version.to_string()),
    }
}

/// The part of the status page snapshot (and `desktop-status.json`) about updates.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// unconfigured, idle, checking, current, available, downloading, installing, installed, failed
    pub state: &'static str,
    /// The newer version, once one was found.
    pub version: Option<String>,
    pub error: Option<String>,
    /// When the last check finished, in seconds since 1970.
    pub checked_at: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Unconfigured,
    Busy,
    Current,
    /// Found and left alone: only a check was asked for, the user chose "Later",
    /// or the automatic check had asked about this version before.
    Available(String),
    Blocked(String, Blocker),
    Installed(String),
    Failed(String),
}

impl Outcome {
    /// One line for the log and for `--check-update` on the terminal.
    pub fn line(&self, current: &str) -> String {
        match self {
            Outcome::Unconfigured => "updates are not configured in this build".into(),
            Outcome::Busy => "an update check is already in progress".into(),
            Outcome::Current => format!("up to date: {current} is the latest version"),
            Outcome::Available(version) => format!("update available: {version} (running {current})"),
            Outcome::Blocked(version, Blocker::NotAnApp) => format!("update available: {version}, but this copy does not run from an app bundle and cannot update itself"),
            Outcome::Blocked(version, _) => format!("update available: {version}, but the app runs from a read-only location; move SintBot to Applications first"),
            Outcome::Installed(version) => format!("installed {version} (was {current})"),
            Outcome::Failed(detail) => format!("failed: {detail}"),
        }
    }
    pub fn ok(&self) -> bool { !matches!(self, Outcome::Failed(_) | Outcome::Busy | Outcome::Blocked(..)) }
}

pub struct Updates {
    pub source: Source,
    current: String,
    log_file: PathBuf,
    /// A development root is not created just to hold this log.
    log_needs: Option<PathBuf>,
    busy: AtomicBool,
    asked: Mutex<HashSet<String>>,
}

impl Updates {
    pub fn new(source: Source, layout: &Layout, current: &str) -> Self {
        Updates { source, current: current.to_string(), log_file: layout.app_log_file(),
            log_needs: layout.overridden.then(|| layout.root.clone()),
            busy: AtomicBool::new(false), asked: Mutex::new(HashSet::new()) }
    }

    pub fn initial_report(&self) -> Report {
        Report { state: if self.source.configured { "idle" } else { "unconfigured" }, version: None, error: None, checked_at: None }
    }

    /// The app's own log, next to the service log. The automatic check says
    /// nothing on screen when it fails, so this is where to look.
    pub fn log(&self, line: &str) {
        eprintln!("SintBot: {line}");
        if self.log_needs.as_ref().is_some_and(|root| !root.is_dir()) { return }
        let Some(dir) = self.log_file.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() { return }
        if std::fs::metadata(&self.log_file).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
            let _ = std::fs::rename(&self.log_file, self.log_file.with_extension("log.1"));
        }
        let stamp = time::OffsetDateTime::now_utc().replace_nanosecond(0).ok()
            .and_then(|now| now.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_file) {
            let _ = writeln!(file, "{stamp} {line}");
        }
    }
}

fn report(shared: &Shared, state: &'static str, version: Option<&str>, error: Option<&str>) {
    let lang = {
        let mut status = shared.status.lock().unwrap();
        let checked_at = if matches!(state, "checking" | "downloading" | "installing") { status.update.checked_at }
            else { SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|elapsed| elapsed.as_secs()) };
        status.update = Report { state, version: version.map(str::to_string), error: error.map(str::to_string), checked_at };
        status.lang
    };
    let t = text(lang);
    menu_item(shared, match state { "checking" => t.u_checking, "downloading" => t.u_downloading, "installing" => t.u_installing, _ => t.update }.to_string(),
        !matches!(state, "checking" | "downloading" | "installing" | "installed"));
}

fn menu_item(shared: &Shared, label: String, enabled: bool) {
    if let Some(items) = shared.tray.lock().unwrap().as_ref() {
        let _ = items.update.set_text(label);
        let _ = items.update.set_enabled(enabled);
    }
}

struct Failure { lead: &'static str, detail: String }

impl Failure {
    fn of(error: &Error, t: &'static Text) -> Self {
        let lead = match error {
            Error::Reqwest(_) | Error::Network(_) | Error::ReleaseNotFound => t.u_network,
            Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) | Error::SignedVersionMismatch { .. }
                | Error::MissingSignedVersion => t.u_signature,
            // The message names where the previous app was kept.
            Error::PreviousAppNotRestored(_) => "",
            _ => t.u_unchanged,
        };
        // reqwest keeps the reason (timed out, refused, DNS) in the source chain.
        let mut detail = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            let cause_text = cause.to_string();
            if !detail.contains(&cause_text) { detail = format!("{detail}: {cause_text}"); }
            source = cause.source();
        }
        Failure { lead, detail }
    }
    fn body(&self) -> String { format!("{}\n\n{}", self.lead, self.detail).trim().to_string() }
}

fn tell(app: &AppHandle, kind: MessageDialogKind, title: &str, body: String) {
    app.dialog().message(body).title(title).kind(kind).blocking_show();
}

fn check(app: &AppHandle, source: &Source, t: &'static Text) -> Result<Option<Update>, Failure> {
    let mut builder = app.updater_builder()
        .timeout(CHECK_TIMEOUT)
        .version_comparator(|current, release| is_upgrade(&current, &release.version))
        .configure_client(|client| client.connect_timeout(CONNECT_TIMEOUT).read_timeout(STALL_TIMEOUT));
    if source.local { builder = builder.no_proxy(); }
    let updater = builder.build().map_err(|error| Failure::of(&error, t))?;
    let found = tauri::async_runtime::block_on(updater.check()).map_err(|error| Failure::of(&error, t))?;
    match found {
        Some(update) if !archive_allowed(&update.download_url, source.local) =>
            Err(Failure { lead: t.u_unchanged, detail: format!("the update archive is not served over https: {}", update.download_url) }),
        found => Ok(found),
    }
}

fn install(shared: &Shared, mut update: Update, t: &'static Text) -> Result<(), Error> {
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    let (mut received, mut shown) = (0u64, None);
    tauri::async_runtime::block_on(update.download_and_install(
        |chunk, total| {
            received += chunk as u64;
            let percent = total.filter(|total| *total > 0).map(|total| (received * 100 / total).min(100));
            if percent != shown {
                shown = percent;
                menu_item(shared, percent.map_or(t.u_downloading.to_string(), |p| format!("{} {p}%", t.u_downloading)), false);
            }
        },
        || report(shared, "installing", Some(&update.version), None),
    ))
}

fn flow(app: &AppHandle, shared: &Shared, trigger: Trigger) -> Outcome {
    let up = &shared.update;
    let t = text(shared.status.lock().unwrap().lang);
    let (user, scripted) = (trigger == Trigger::User, matches!(trigger, Trigger::Script { .. }));
    if !up.source.configured {
        if user { tell(app, MessageDialogKind::Info, t.update.trim_end_matches('…'), t.u_unconfigured.into()); }
        return Outcome::Unconfigured;
    }
    report(shared, "checking", None, None);
    let checked = match &up.source.problem {
        Some(problem) => Err(Failure { lead: "", detail: problem.clone() }),
        None => check(app, &up.source, t),
    };
    let update = match checked {
        Ok(Some(update)) => update,
        Ok(None) => {
            if user { tell(app, MessageDialogKind::Info, t.u_current_title, t.u_current_body.replace("{cur}", &up.current)); }
            return Outcome::Current;
        }
        Err(failure) => {
            if user { tell(app, MessageDialogKind::Error, t.u_check_failed, failure.body()); }
            return Outcome::Failed(format!("the check did not succeed: {}", failure.detail));
        }
    };
    let version = update.version.clone();
    report(shared, "available", Some(&version), None);
    if !proceeds(&mut up.asked.lock().unwrap(), trigger, &version) { return Outcome::Available(version) }
    let place = tauri::utils::platform::current_exe().ok().map_or(Some(Blocker::NotAnApp), |exe| blocker(&exe, on_read_only_volume));
    if let Some(blocker) = place {
        if !scripted {
            let (title, body) = if blocker == Blocker::NotAnApp { (t.u_install_failed, t.u_not_app) } else { (t.u_move_title, t.u_move_body) };
            tell(app, MessageDialogKind::Warning, title, body.replace("{new}", &version));
        }
        return Outcome::Blocked(version, blocker);
    }
    if !scripted {
        let body = t.u_found_body.replace("{new}", &version).replace("{cur}", &up.current);
        let agreed = app.dialog().message(body).title(t.u_found_title).kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(t.u_install.to_string(), t.u_later.to_string())).blocking_show();
        if !agreed { return Outcome::Available(version) }
    }
    up.log(&format!("update: downloading {version} from {}", update.download_url));
    report(shared, "downloading", Some(&version), None);
    match install(shared, update, t) {
        Ok(()) => Outcome::Installed(version),
        Err(error) => {
            let failure = Failure::of(&error, t);
            if !scripted { tell(app, MessageDialogKind::Error, t.u_install_failed, failure.body()); }
            Outcome::Failed(format!("{version} was not installed: {}", failure.detail))
        }
    }
}

/// One check at a time, from whichever trigger. Blocks until it is over,
/// including the time the question stays on screen.
pub fn run(app: &AppHandle, trigger: Trigger) -> Outcome {
    let shared = app.state::<Arc<Shared>>().inner().clone();
    let up = &shared.update;
    if up.busy.swap(true, Ordering::SeqCst) {
        if trigger != Trigger::Auto { up.log(&format!("update: {}", Outcome::Busy.line(&up.current))); }
        return Outcome::Busy;
    }
    let outcome = flow(app, &shared, trigger);
    up.log(&format!("update: {}", outcome.line(&up.current)));
    match &outcome {
        Outcome::Unconfigured => {}
        Outcome::Current => report(&shared, "current", None, None),
        Outcome::Available(version) => report(&shared, "available", Some(version), None),
        Outcome::Blocked(version, _) => report(&shared, "failed", Some(version), Some(&outcome.line(&up.current))),
        // Stays busy: the app is about to be reopened, and nothing else may start in between.
        Outcome::Installed(version) => { report(&shared, "installed", Some(version), None); return outcome }
        Outcome::Failed(detail) => report(&shared, "failed", None, Some(detail)),
        Outcome::Busy => unreachable!("handled above"),
    }
    up.busy.store(false, Ordering::SeqCst);
    outcome
}

/// Runs the flow off the main thread. After an installation the app reopens
/// through LaunchServices; the service is a launchd job and is not touched.
pub fn start(app: &AppHandle, trigger: Trigger) {
    let app = app.clone();
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        if let Outcome::Installed(_) = run(&app, trigger) { app.restart() }
    });
}

/// Shortly after launch, then every few hours. Not in a development run: an
/// app pointed at an isolated root is a local build, and its question would
/// pop up on a Mac where something else is being tested.
pub fn schedule(app: &AppHandle, development: bool) {
    let shared = app.state::<Arc<Shared>>().inner().clone();
    if !shared.update.source.configured || development { return }
    let app = app.clone();
    let _ = std::thread::Builder::new().name("update-timer".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            if let Outcome::Installed(_) = run(&app, Trigger::Auto) { app.restart() }
            std::thread::sleep(CHECK_EVERY);
        }
    });
}

/// `SintBot --check-update` or `--install-update` with no app running: answer
/// on the terminal and leave, without opening the app or starting the service.
pub fn headless(app: &AppHandle, install: bool) {
    let app = app.clone();
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        let outcome = run(&app, Trigger::Script { install });
        println!("SintBot: {}", outcome.line(&app.package_info().version.to_string()));
        // A check that finds no updates configured has its answer; an installation has not happened.
        let done = outcome.ok() && !(install && outcome == Outcome::Unconfigured);
        app.exit(if done { 0 } else { 1 });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version { Version::parse(text).unwrap() }

    #[test]
    fn only_newer_versions_are_updates() {
        for (current, candidate) in [("0.1.3", "0.1.4"), ("0.1.9", "0.1.10"), ("0.1.3", "1.0.0"), ("0.1.4-beta.1", "0.1.4")] {
            assert!(is_upgrade(&version(current), &version(candidate)), "{current} -> {candidate}");
        }
        for (current, candidate) in [("0.1.3", "0.1.3"), ("0.1.4", "0.1.3"), ("0.1.10", "0.1.9"), ("0.1.4", "0.1.4-beta.1"), ("1.0.0", "0.9.9")] {
            assert!(!is_upgrade(&version(current), &version(candidate)), "{current} -> {candidate}");
        }
    }

    #[test]
    fn endpoint_override_is_https_or_loopback_http() {
        for accepted in ["https://example.com/latest.json", "http://127.0.0.1:8765/latest.json", "http://localhost:8765/x", "http://[::1]:8765/x"] {
            assert!(endpoint_override(accepted).is_ok(), "{accepted}");
        }
        for refused in ["http://example.com/latest.json", "http://203.0.113.5/latest.json", "http://127.0.0.1.example.com/x",
            "file:///tmp/latest.json", "ftp://127.0.0.1/x", "latest.json", ""] {
            assert!(endpoint_override(refused).is_err(), "{refused}");
        }
    }

    fn config(updater: serde_json::Value) -> tauri::Config {
        serde_json::from_value(serde_json::json!({ "identifier": "com.example.test", "plugins": { "updater": updater } })).unwrap()
    }

    fn setting(config: &tauri::Config, key: &str) -> serde_json::Value {
        config.plugins.0["updater"].get(key).cloned().unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn a_build_without_a_public_key_is_not_configured() {
        let release = serde_json::json!(["https://example.com/latest.json"]);
        let mut unsigned = config(serde_json::json!({ "pubkey": "", "endpoints": release }));
        assert!(!prepare(&mut unsigned, None).configured);
        let mut missing: tauri::Config = serde_json::from_value(serde_json::json!({ "identifier": "com.example.test" })).unwrap();
        assert!(!prepare(&mut missing, Some("http://127.0.0.1:1/x")).configured);
        let mut signed = config(serde_json::json!({ "pubkey": "abc", "endpoints": release }));
        let source = prepare(&mut signed, None);
        assert!(source.configured && !source.local && source.problem.is_none());
        assert_eq!(setting(&signed, "endpoints"), release);
        assert_eq!(setting(&signed, "dangerousInsecureTransportProtocol"), serde_json::Value::Null);
    }

    #[test]
    fn plain_http_is_allowed_only_for_a_loopback_override() {
        let release = serde_json::json!(["https://example.com/latest.json"]);
        let mut local = config(serde_json::json!({ "pubkey": "abc", "endpoints": release }));
        let source = prepare(&mut local, Some("http://127.0.0.1:8765/latest.json"));
        assert!(source.local && source.problem.is_none());
        assert_eq!(setting(&local, "endpoints"), serde_json::json!(["http://127.0.0.1:8765/latest.json"]));
        assert_eq!(setting(&local, "dangerousInsecureTransportProtocol"), serde_json::json!(true));

        let mut secure = config(serde_json::json!({ "pubkey": "abc", "endpoints": release }));
        let source = prepare(&mut secure, Some("https://staging.example.com/latest.json"));
        assert!(!source.local);
        assert_eq!(setting(&secure, "dangerousInsecureTransportProtocol"), serde_json::json!(false));

        // A refused override is reported; it neither opens plain http nor falls back silently.
        let mut refused = config(serde_json::json!({ "pubkey": "abc", "endpoints": release }));
        let source = prepare(&mut refused, Some("http://example.com/latest.json"));
        assert!(source.problem.is_some() && !source.local);
        assert_eq!(setting(&refused, "endpoints"), release);
        assert_eq!(setting(&refused, "dangerousInsecureTransportProtocol"), serde_json::Value::Null);
    }

    #[test]
    fn the_archive_comes_over_https_unless_testing_locally() {
        let https = Url::parse("https://github.com/sintlab/sintbot-desktop/releases/download/v0.1.4/SintBot_0.1.4_aarch64.app.tar.gz").unwrap();
        let loopback = Url::parse("http://127.0.0.1:8765/SintBot.app.tar.gz").unwrap();
        let plain = Url::parse("http://example.com/SintBot.app.tar.gz").unwrap();
        assert!(archive_allowed(&https, false) && archive_allowed(&https, true));
        assert!(archive_allowed(&loopback, true) && !archive_allowed(&loopback, false));
        assert!(!archive_allowed(&plain, true) && !archive_allowed(&plain, false));
    }

    #[test]
    fn places_the_app_cannot_be_replaced_in() {
        let writable = |_: &Path| false;
        assert_eq!(blocker(Path::new("/Applications/SintBot.app/Contents/MacOS/sintbot-desktop"), writable), None);
        assert_eq!(bundle_of(Path::new("/Applications/SintBot.app/Contents/MacOS/sintbot-desktop")), Some(PathBuf::from("/Applications/SintBot.app")));
        assert_eq!(blocker(Path::new("/private/var/folders/ab/cd/T/AppTranslocation/1F2E/d/SintBot.app/Contents/MacOS/sintbot-desktop"), writable),
            Some(Blocker::Translocated));
        assert_eq!(blocker(Path::new("/Volumes/SintBot/SintBot.app/Contents/MacOS/sintbot-desktop"), |_| true), Some(Blocker::ReadOnly));
        // `cargo run` and `tauri dev`: the updater would replace the target directory.
        assert_eq!(blocker(Path::new("/src/sintbot-desktop/src-tauri/target/release/sintbot-desktop"), writable), Some(Blocker::NotAnApp));
        assert_eq!(blocker(Path::new("/x/SintBot.app/Contents/Helpers/sintbot-desktop"), writable), Some(Blocker::NotAnApp));
    }

    #[test]
    fn read_only_volumes_are_recognised() {
        // The sealed system volume is mounted read-only; the temporary directory is not.
        assert!(on_read_only_volume(Path::new("/System/Library")));
        assert!(!on_read_only_volume(&std::env::temp_dir()));
        assert!(!on_read_only_volume(Path::new("/nonexistent/SintBot.app")));
    }

    #[test]
    fn the_automatic_check_asks_once_per_version() {
        let mut asked = HashSet::new();
        assert!(proceeds(&mut asked, Trigger::Auto, "0.1.4"));
        assert!(!proceeds(&mut asked, Trigger::Auto, "0.1.4"));
        assert!(proceeds(&mut asked, Trigger::User, "0.1.4"), "the menu item always asks");
        assert!(proceeds(&mut asked, Trigger::Auto, "0.1.5"), "a newer version is asked about again");
        // "Later" from the menu also counts, and a script's check never uses up the question.
        let mut asked = HashSet::new();
        assert!(!proceeds(&mut asked, Trigger::Script { install: false }, "0.1.4"));
        assert!(proceeds(&mut asked, Trigger::Script { install: true }, "0.1.4"));
        assert!(proceeds(&mut asked, Trigger::User, "0.1.4"));
        assert!(!proceeds(&mut asked, Trigger::Auto, "0.1.4"));
    }

    #[test]
    fn outcome_lines() {
        assert_eq!(Outcome::Available("0.1.4".into()).line("0.1.3"), "update available: 0.1.4 (running 0.1.3)");
        assert_eq!(Outcome::Current.line("0.1.3"), "up to date: 0.1.3 is the latest version");
        assert!(Outcome::Unconfigured.ok() && Outcome::Current.ok() && Outcome::Installed("0.1.4".into()).ok());
        assert!(!Outcome::Failed("x".into()).ok() && !Outcome::Blocked("0.1.4".into(), Blocker::ReadOnly).ok());
    }
}
