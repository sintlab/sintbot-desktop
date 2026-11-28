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
    /// The console's software-update card: no dialogs, progress goes to the page.
    /// `install` only after the page has asked the user.
    Page { install: bool },
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

/// The bundle this runs from, when it can be replaced in place (and opened at
/// login from there): not a bare binary, not on a disk image, not a randomized copy.
pub fn settled_bundle(exe: &Path) -> Option<PathBuf> {
    if blocker(exe, on_read_only_volume).is_some() { return None }
    bundle_of(exe)
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
        Trigger::Script { install } | Trigger::Page { install } => install,
        Trigger::User => { asked.insert(version.to_string()); true }
        Trigger::Auto => asked.insert(version.to_string()),
    }
}

/// After a launch at login the app sits in the menu bar, and a newer version
/// found by a check the user did not ask for is only marked in the menu. The
/// question waits until the window is first shown or the menu item is chosen.
#[derive(Debug, Default)]
pub struct Quiet { active: bool, held: Option<String> }

impl Quiet {
    /// Whether this check keeps quiet about `version`, which is then remembered
    /// for the menu. A check the user asked for, or one that installs, never does.
    fn hold(&mut self, trigger: Trigger, version: &str) -> bool {
        if !self.active || !matches!(trigger, Trigger::Auto | Trigger::Page { install: false }) { return false }
        self.held = Some(version.to_string());
        true
    }
    /// The window was shown: from now on as after an ordinary launch. Returns
    /// the version still waiting for its question.
    fn end(&mut self) -> Option<String> {
        self.active = false;
        self.held.take()
    }
}

/// The update menu item. While a version is held after a launch at login it
/// names that version, so choosing it goes on to the question.
fn menu_label(t: &Text, state: &str, held: Option<&str>) -> String {
    match state {
        "checking" => t.u_checking.to_string(),
        "downloading" => t.u_downloading.to_string(),
        "installing" => t.u_installing.to_string(),
        _ => held.map_or_else(|| t.update.to_string(), |version| t.u_held.replace("{new}", version)),
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
    /// The page in the window said it shows the app's row (`console-ready`).
    /// Cleared whenever a page starts loading.
    pub console_ready: AtomicBool,
    /// The window to tell; set once it exists.
    pub app: std::sync::OnceLock<AppHandle>,
    /// Why the last check or installation failed, in the user's language, for the page.
    last_error: Mutex<Option<String>>,
    quiet: Mutex<Quiet>,
}

impl Updates {
    pub fn new(source: Source, layout: &Layout, current: &str) -> Self {
        Updates { source, current: current.to_string(), log_file: layout.app_log_file(),
            log_needs: layout.overridden.then(|| layout.root.clone()),
            busy: AtomicBool::new(false), asked: Mutex::new(HashSet::new()),
            console_ready: AtomicBool::new(false), app: std::sync::OnceLock::new(), last_error: Mutex::new(None),
            quiet: Mutex::new(Quiet::default()) }
    }

    /// This run was started at login: automatic checks keep quiet until the
    /// window is shown.
    pub fn stay_quiet(&self) { self.quiet.lock().unwrap().active = true; }

    /// The window is about to be shown. Returns a version found while the app
    /// kept quiet, which is now to be asked about.
    pub fn window_shown(&self) -> Option<String> { self.quiet.lock().unwrap().end() }

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

/// What the page's app row is told (see the desktop-lite contract): the state,
/// the running and the newer version, and a reason when it did not work.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageDetail { pub state: &'static str, pub current: String, pub latest: Option<String>, pub error: Option<String> }

/// The page's names for the report states. `None`: nothing to tell.
fn page_state(state: &str) -> Option<&'static str> {
    Some(match state {
        "checking" => "checking", "current" => "current", "available" => "available",
        "downloading" => "downloading", "installing" | "installed" => "installing",
        "failed" => "failed", "blocked" => "blocked",
        _ => return None,
    })
}

pub fn page_script(detail: &PageDetail) -> String {
    let json = serde_json::to_string(detail).unwrap_or_else(|_| "null".into());
    format!("(() => {{ const detail = {json}; window.__SINTBOT_APP_UPDATE__ = detail; \
        window.dispatchEvent(new CustomEvent('sintbot-app-update', {{ detail }})); }})();")
}

fn tell_page(shared: &Shared, state: &'static str, version: Option<&str>, error: Option<String>) {
    let up = &shared.update;
    let Some(state) = page_state(state) else { return };
    let Some(app) = up.app.get() else { return };
    let Some(window) = app.get_webview_window("main") else { return };
    let detail = PageDetail { state, current: up.current.clone(), latest: version.map(str::to_string), error };
    let _ = window.eval(page_script(&detail));
}

/// Menu "Check for Updates": a console that shows the app's row gets the
/// question (no dialog); an older console or the status page keeps the dialogs.
pub fn from_menu(app: &AppHandle) {
    let shared = app.state::<Arc<Shared>>().inner().clone();
    // This flow asks (or hands the question to the console) itself; showing
    // the window must not start a second question.
    shared.update.window_shown();
    crate::show_main(app);
    if shared.update.console_ready.load(Ordering::SeqCst) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.eval("window.dispatchEvent(new CustomEvent('sintbot-open-update'));");
        }
        start(app, Trigger::Page { install: false });
    } else {
        start(app, Trigger::User);
    }
}

fn report(shared: &Shared, state: &'static str, version: Option<&str>, error: Option<&str>) {
    let page_error = if state == "failed" { shared.update.last_error.lock().unwrap().clone() } else { None };
    tell_page(shared, state, version, page_error);
    let lang = {
        let mut status = shared.status.lock().unwrap();
        let checked_at = if matches!(state, "checking" | "downloading" | "installing") { status.update.checked_at }
            else { SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|elapsed| elapsed.as_secs()) };
        status.update = Report { state, version: version.map(str::to_string), error: error.map(str::to_string), checked_at };
        status.lang
    };
    let t = text(lang);
    let held = {
        let mut quiet = shared.update.quiet.lock().unwrap();
        // Nothing newer any more: nothing left to mark.
        if state == "current" { quiet.held = None; }
        quiet.held.clone()
    };
    menu_item(shared, menu_label(t, state, held.as_deref()),
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
    let (user, scripted) = (trigger == Trigger::User, matches!(trigger, Trigger::Script { .. } | Trigger::Page { .. }));
    *up.last_error.lock().unwrap() = None;
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
            *up.last_error.lock().unwrap() = Some(format!("{}\n\n{}", t.u_check_failed, failure.body()).trim().to_string());
            if user { tell(app, MessageDialogKind::Error, t.u_check_failed, failure.body()); }
            return Outcome::Failed(format!("the check did not succeed: {}", failure.detail));
        }
    };
    let version = update.version.clone();
    report(shared, "available", Some(&version), None);
    if up.quiet.lock().unwrap().hold(trigger, &version) {
        up.log(&format!("update: {version} is marked in the menu; the question waits until the window is shown"));
        return Outcome::Available(version);
    }
    if !proceeds(&mut up.asked.lock().unwrap(), trigger, &version) { return Outcome::Available(version) }
    let place = tauri::utils::platform::current_exe().ok().map_or(Some(Blocker::NotAnApp), |exe| blocker(&exe, on_read_only_volume));
    if let Some(blocker) = place {
        let (title, body) = if blocker == Blocker::NotAnApp { (t.u_install_failed, t.u_not_app) } else { (t.u_move_title, t.u_move_body) };
        *up.last_error.lock().unwrap() = Some(body.replace("{new}", &version));
        if !scripted { tell(app, MessageDialogKind::Warning, title, body.replace("{new}", &version)); }
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
            *up.last_error.lock().unwrap() = Some(format!("{}\n\n{}", t.u_install_failed, failure.body()).trim().to_string());
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
        Outcome::Blocked(version, _) => {
            report(&shared, "failed", Some(version), Some(&outcome.line(&up.current)));
            let reason = up.last_error.lock().unwrap().clone();
            tell_page(&shared, "blocked", Some(version), reason);
        }
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
        let https = Url::parse("https://github.com/sint908/sintbot-desktop/releases/download/v0.1.4/SintBot_0.1.4_aarch64.app.tar.gz").unwrap();
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
    fn the_page_asks_without_dialogs_and_installs_only_when_told() {
        let mut asked = HashSet::new();
        assert!(!proceeds(&mut asked, Trigger::Page { install: false }, "0.1.9"));
        assert!(proceeds(&mut asked, Trigger::Page { install: true }, "0.1.9"));
        assert!(proceeds(&mut asked, Trigger::Auto, "0.1.9"), "a page check does not use up the question");
    }

    #[test]
    fn the_page_gets_its_own_state_names() {
        assert_eq!(page_state("installed"), Some("installing"));
        assert_eq!(page_state("blocked"), Some("blocked"));
        assert_eq!(page_state("idle"), None);
        assert_eq!(page_state("unconfigured"), None);
        let script = page_script(&PageDetail { state: "available", current: "0.1.8".into(), latest: Some("0.1.9".into()), error: None });
        assert!(script.contains(r#"{"state":"available","current":"0.1.8","latest":"0.1.9","error":null}"#), "{script}");
        assert!(script.contains("sintbot-app-update"));
    }

    fn layout() -> Layout {
        Layout { home: "/Users/t".into(), root: "/Users/t/.local/share/sintbot".into(), data_dir: None,
            label: "com.sintbot.daemon".into(), port: 4600, overridden: false }
    }

    #[test]
    fn after_a_launch_at_login_a_new_version_is_only_marked_in_the_menu() {
        let updates = Updates::new(Source { configured: true, ..Source::default() }, &layout(), "0.2.0");
        updates.stay_quiet();
        let mut asked = HashSet::new();
        // The automatic check finds 0.2.1 while the window has never been shown:
        // no question, the menu names the version, and the question is not used up.
        assert!(updates.quiet.lock().unwrap().hold(Trigger::Auto, "0.2.1"));
        assert!(asked.is_empty());
        let held = updates.quiet.lock().unwrap().held.clone();
        assert_eq!(menu_label(&crate::ZH, "available", held.as_deref()), "安装新版本 0.2.1…");
        assert_eq!(menu_label(&crate::EN, "available", held.as_deref()), "Install Version 0.2.1…");
        // A later periodic check, window still unopened: the same.
        assert!(updates.quiet.lock().unwrap().hold(Trigger::Auto, "0.2.2"));
        // The console loaded in the hidden window checks too, and marks the menu the same way.
        assert!(updates.quiet.lock().unwrap().hold(Trigger::Page { install: false }, "0.2.2"));
        // The user opens the window: that version is asked about, as after an ordinary launch.
        assert_eq!(updates.window_shown(), Some("0.2.2".into()));
        assert!(proceeds(&mut asked, Trigger::Auto, "0.2.2"), "asked once the window is shown");
        assert!(!updates.quiet.lock().unwrap().hold(Trigger::Auto, "0.2.3"), "and from then on as usual");
        assert_eq!(updates.window_shown(), None);
    }

    #[test]
    fn what_the_user_asks_for_is_never_held() {
        let updates = Updates::new(Source { configured: true, ..Source::default() }, &layout(), "0.2.0");
        updates.stay_quiet();
        let mut quiet = updates.quiet.lock().unwrap();
        assert!(!quiet.hold(Trigger::User, "0.2.1"), "the menu item asks as always");
        assert!(!quiet.hold(Trigger::Page { install: true }, "0.2.1"), "the console's install button installs");
        assert!(!quiet.hold(Trigger::Script { install: false }, "0.2.1"));
        assert!(!quiet.hold(Trigger::Script { install: true }, "0.2.1"));
        assert_eq!(quiet.held, None);
    }

    #[test]
    fn an_ordinary_launch_is_unchanged() {
        let updates = Updates::new(Source { configured: true, ..Source::default() }, &layout(), "0.2.0");
        let mut asked = HashSet::new();
        // Not quiet: the automatic check goes on to its question, once per version.
        assert!(!updates.quiet.lock().unwrap().hold(Trigger::Auto, "0.2.1"));
        assert!(proceeds(&mut asked, Trigger::Auto, "0.2.1"));
        assert_eq!(updates.window_shown(), None, "showing the window asks nothing extra");
        assert_eq!(menu_label(&crate::EN, "available", None), crate::EN.update);
        assert_eq!(menu_label(&crate::ZH, "checking", None), crate::ZH.u_checking);
        assert_eq!(menu_label(&crate::EN, "downloading", Some("0.2.1")), crate::EN.u_downloading, "progress wins over the mark");
    }

    #[test]
    fn outcome_lines() {
        assert_eq!(Outcome::Available("0.1.4".into()).line("0.1.3"), "update available: 0.1.4 (running 0.1.3)");
        assert_eq!(Outcome::Current.line("0.1.3"), "up to date: 0.1.3 is the latest version");
        assert!(Outcome::Unconfigured.ok() && Outcome::Current.ok() && Outcome::Installed("0.1.4".into()).ok());
        assert!(!Outcome::Failed("x".into()).ok() && !Outcome::Blocked("0.1.4".into(), Blocker::ReadOnly).ok());
    }
}
