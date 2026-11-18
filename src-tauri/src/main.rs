// SintBot for macOS. The app is a thin shell: it installs a Node.js runtime and
// the service package, keeps the service running as a launchd job, and shows
// the service's own web console in a window. Everything the user configures
// lives in that console, which is the same page a browser gets.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod health;
mod layout;
mod provision;
mod service;
mod shellenv;
mod update;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use health::Http;
use layout::Layout;
use service::Job;

const BUNDLE_ID: &str = "com.sintbot.desktop";
const TICK: Duration = Duration::from_secs(2);

/// Runs in every page of the window, including the console served by the local
/// service. Links that leave the console open in the default browser; the
/// navigation handler below does the opening.
const INIT_JS: &str = r#"(() => {
  if (window.__SINTBOT_DESKTOP__) return;
  window.__SINTBOT_DESKTOP__ = true;
  const external = url => { location.href = 'sintbot-open:' + encodeURIComponent(url) };
  document.addEventListener('click', event => {
    const link = event.target && event.target.closest && event.target.closest('a[href]');
    if (!link || event.defaultPrevented) return;
    let url; try { url = new URL(link.getAttribute('href'), location.href) } catch { return }
    if (!/^https?:$/.test(url.protocol)) return;
    if (link.target === '_blank' || url.origin !== location.origin) { event.preventDefault(); external(url.href) }
  }, true);
  const open = window.open;
  window.open = function (url) {
    if (!url) return open.apply(window, arguments);
    try { external(new URL(url, location.href).href) } catch {}
    return null;
  };
})();"#;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase { Preparing, Starting, Setup, Running, Stopping, Stopped, Failed }

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    phase: Phase,
    /// Why the last start did not happen. Cleared by the next attempt.
    error: Option<String>,
    app_version: String,
    node_version: Option<String>,
    service_version: Option<String>,
    codex: Option<String>,
    claude: Option<String>,
    label: String,
    pid: Option<u32>,
    http: Http,
    url: String,
    adopted: bool,
    development: bool,
    log_file: String,
    log_tail: Vec<String>,
    lang: &'static str,
    update: update::Report,
}

enum Cmd {
    Ensure, Stop, Restart, StopAndQuit,
    /// `quiet`: driven by a script, so no dialogs.
    RemoveService { quiet: bool },
}

struct Shared {
    status: Mutex<Status>,
    tx: Mutex<Sender<Cmd>>,
    launcher: Mutex<Option<Url>>,
    tray: Mutex<Option<TrayItems>>,
    update: update::Updates,
}

struct TrayItems {
    status: MenuItem<tauri::Wry>,
    start: MenuItem<tauri::Wry>,
    stop: MenuItem<tauri::Wry>,
    restart: MenuItem<tauri::Wry>,
    update: MenuItem<tauri::Wry>,
}

struct Text {
    open: &'static str, browser: &'static str, start: &'static str, stop: &'static str, restart: &'static str,
    logs: &'static str, quit: &'static str, stop_quit: &'static str, remove: &'static str,
    remove_title: &'static str, remove_body: &'static str, confirm_remove: &'static str,
    removed_title: &'static str, removed_body: &'static str, remove_adopted: &'static str,
    stop_title: &'static str, stop_body: &'static str, restart_body: &'static str, confirm_stop: &'static str,
    confirm_restart: &'static str, cancel: &'static str,
    s_preparing: &'static str, s_starting: &'static str, s_setup: &'static str, s_running: &'static str,
    s_stopping: &'static str, s_stopped: &'static str, s_failed: &'static str,
    // Updates of the app. `{new}` and `{cur}` stand for the versions.
    update: &'static str, u_checking: &'static str, u_downloading: &'static str, u_installing: &'static str,
    u_found_title: &'static str, u_found_body: &'static str, u_install: &'static str, u_later: &'static str,
    u_current_title: &'static str, u_current_body: &'static str, u_check_failed: &'static str, u_install_failed: &'static str,
    u_network: &'static str, u_signature: &'static str, u_unchanged: &'static str,
    u_move_title: &'static str, u_move_body: &'static str, u_not_app: &'static str, u_unconfigured: &'static str,
}

const ZH: Text = Text {
    open: "打开控制台", browser: "在浏览器中打开", start: "启动机器人", stop: "停止机器人", restart: "重启机器人",
    logs: "查看日志", quit: "退出（机器人继续在后台运行）", stop_quit: "停止机器人并退出", remove: "移除后台服务…",
    remove_title: "移除 SintBot 的后台服务？",
    remove_body: "机器人会停止，登录时也不再启动，然后 App 退出。配置、聊天与任务记录、已安装的版本和运行环境都留在原处，不会删除。再次打开 SintBot 会重新装上后台服务。",
    confirm_remove: "移除并退出", removed_title: "后台服务已移除",
    removed_body: "配置、记录、已安装的版本和运行环境都还在。要删除 App，把 SintBot 拖进废纸篓即可。",
    remove_adopted: "这个后台服务不是由本 App 创建的，App 不会改动或删除它。请按当初安装它的方式移除。",
    stop_title: "停止 SintBot？", stop_body: "正在进行的任务会被中断。在你重新启动之前，群里的消息不会有回应，下次登录也不会自动启动。",
    restart_body: "正在进行的任务会被中断，随后服务重新启动。", confirm_stop: "停止", confirm_restart: "重启", cancel: "取消",
    s_preparing: "正在准备…", s_starting: "正在启动…", s_setup: "等待接入聊天平台", s_running: "运行中",
    s_stopping: "正在停止…", s_stopped: "已停止", s_failed: "未能启动",
    update: "检查更新…", u_checking: "正在检查更新…", u_downloading: "正在下载更新…", u_installing: "正在安装更新…",
    u_found_title: "SintBot App 有新版本",
    u_found_body: "新版本 {new} 可以安装了（当前是 {cur}）。安装后 App 会自动重新打开；机器人一直在后台运行，不会中断。",
    u_install: "安装并重新打开", u_later: "以后再说",
    u_current_title: "已是最新版本", u_current_body: "SintBot App {cur} 已经是最新版本。",
    u_check_failed: "未能检查更新", u_install_failed: "更新没有装上",
    u_network: "连不上更新服务器。请检查网络后再试，也可以到 github.com/sintlab/sintbot-desktop/releases 手动下载新版本。",
    u_signature: "下载的更新没有通过签名校验，已经丢弃。当前版本没有改动。", u_unchanged: "当前版本没有改动。",
    u_move_title: "请先把 SintBot 移到“应用程序”文件夹",
    u_move_body: "新版本 {new} 可以安装了，但 SintBot 现在是从磁盘映像或临时位置打开的，无法在这里更新。请退出 App，把 SintBot 拖进“应用程序”文件夹，再从那里打开并更新。",
    u_not_app: "新版本 {new} 可以安装了，但这个 SintBot 不是从 App 包里运行的，无法自行更新。",
    u_unconfigured: "这个版本没有配置更新。",
};
const EN: Text = Text {
    open: "Open Console", browser: "Open in Browser", start: "Start SintBot", stop: "Stop SintBot", restart: "Restart SintBot",
    logs: "Show Log", quit: "Quit (SintBot keeps running)", stop_quit: "Stop SintBot and Quit", remove: "Remove Background Service…",
    remove_title: "Remove the SintBot background service?",
    remove_body: "SintBot stops, no longer starts at login, and the app quits. Settings, chat and task records, installed versions and the runtime stay where they are; nothing is deleted. Opening SintBot again installs the background service again.",
    confirm_remove: "Remove and Quit", removed_title: "Background service removed",
    removed_body: "Settings, records, installed versions and the runtime are still there. To delete the app, drag SintBot to the Trash.",
    remove_adopted: "This background service was not created by this app, so the app will not change or delete it. Remove it the way it was installed.",
    stop_title: "Stop SintBot?", stop_body: "Running tasks are interrupted. Groups get no reply until you start it again, and it does not start at the next login.",
    restart_body: "Running tasks are interrupted, then the service starts again.", confirm_stop: "Stop", confirm_restart: "Restart", cancel: "Cancel",
    s_preparing: "Preparing…", s_starting: "Starting…", s_setup: "Waiting for chat setup", s_running: "Running",
    s_stopping: "Stopping…", s_stopped: "Stopped", s_failed: "Could not start",
    update: "Check for Updates…", u_checking: "Checking for Updates…", u_downloading: "Downloading Update…", u_installing: "Installing Update…",
    u_found_title: "A new version of the SintBot app is available",
    u_found_body: "Version {new} is ready to install (you have {cur}). The app reopens by itself afterwards; SintBot keeps running in the background and is not interrupted.",
    u_install: "Install and Restart", u_later: "Later",
    u_current_title: "You're up to date", u_current_body: "SintBot {cur} is the latest version of the app.",
    u_check_failed: "Could not check for updates", u_install_failed: "The update was not installed",
    u_network: "The update server could not be reached. Check the network and try again, or download the new version from github.com/sintlab/sintbot-desktop/releases.",
    u_signature: "The downloaded update did not pass the signature check and was discarded. The installed version is unchanged.",
    u_unchanged: "The installed version is unchanged.",
    u_move_title: "Move SintBot to the Applications folder first",
    u_move_body: "Version {new} is ready to install, but SintBot was opened from a disk image or a temporary location and cannot be updated there. Quit the app, drag SintBot to the Applications folder, then open it from there and update.",
    u_not_app: "Version {new} is ready to install, but this copy of SintBot does not run from an app bundle and cannot update itself.",
    u_unconfigured: "Updates are not configured in this build.",
};

fn system_lang() -> &'static str {
    let out = Command::new("/usr/bin/defaults").args(["read", "-g", "AppleLanguages"]).stdin(Stdio::null()).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let first = text.split('"').nth(1).unwrap_or("");
    if first.starts_with("zh") { "zh" } else { "en" }
}

/// The window is painted in the page's background colour before the page
/// loads, so a dark desktop does not get a white flash.
fn system_dark() -> bool {
    Command::new("/usr/bin/defaults").args(["read", "-g", "AppleInterfaceStyle"]).stdin(Stdio::null()).output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "Dark")
}

fn text(lang: &str) -> &'static Text { if lang == "zh" { &ZH } else { &EN } }

fn phase_label(t: &Text, phase: Phase) -> &'static str {
    match phase {
        Phase::Preparing => t.s_preparing, Phase::Starting => t.s_starting, Phase::Setup => t.s_setup,
        Phase::Running => t.s_running, Phase::Stopping => t.s_stopping, Phase::Stopped => t.s_stopped,
        Phase::Failed => t.s_failed,
    }
}

fn tail(file: &Path, lines: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut handle) = std::fs::File::open(file) else { return Vec::new() };
    let size = handle.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = handle.seek(SeekFrom::Start(size.saturating_sub(8 * 1024)));
    let mut bytes = Vec::new();
    let _ = handle.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].iter().map(|line| line.chars().take(400).collect()).collect()
}

fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") { let _ = window.hide(); }
    // Menu-bar only while the window is closed: no Dock icon for a background helper.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

fn open_with_system(target: &str) {
    let _ = Command::new("/usr/bin/open").arg(target).stdin(Stdio::null()).spawn();
}

fn is_loopback(url: &Url) -> bool {
    url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

/// The window only ever shows the bundled status page or the local console.
fn allow_navigation(url: &Url) -> bool {
    match url.scheme() {
        "tauri" | "about" | "blob" | "data" => true,
        "http" if is_loopback(url) => true,
        "sintbot-open" => {
            let encoded = url.as_str().trim_start_matches("sintbot-open:");
            let target: String = percent_decode(encoded);
            if Url::parse(&target).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) { open_with_system(&target) }
            false
        }
        "http" | "https" | "mailto" => { open_with_system(url.as_str()); false }
        _ => false,
    }
}

/// `sintbot-app:<request>`: a page asking the app to do something of its own.
/// The only request is `check-update`, which does what the menu item does; the
/// user is still asked before anything is installed.
fn app_request(url: &Url) -> Option<&str> {
    (url.scheme() == "sintbot-app").then(|| url.path())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&input[i + 1..i + 3], 16) { out.push(value); i += 3; continue }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

struct Worker {
    app: AppHandle,
    shared: Arc<Shared>,
    layout: Layout,
    resources: PathBuf,
    job: Option<Job>,
    busy: Option<Phase>,
    error: Option<String>,
    down_since: Option<Instant>,
    tools: (Option<String>, Option<String>),
    versions: (Option<String>, Option<String>),
}

impl Worker {
    fn publish(&self, phase: Phase, pid: Option<u32>, http: Http) {
        let port = self.job.as_ref().map(|j| j.port).unwrap_or(self.layout.port);
        let log_file = self.job.as_ref().and_then(|j| j.log_file.clone()).unwrap_or_else(|| self.layout.log_file());
        let snapshot = {
            let mut status = self.shared.status.lock().unwrap();
            status.phase = phase;
            status.error = self.error.clone();
            status.pid = pid;
            status.http = http;
            status.url = format!("http://127.0.0.1:{port}/");
            status.label = self.job.as_ref().map(|j| j.label.clone()).unwrap_or_else(|| self.layout.label.clone());
            status.adopted = self.job.as_ref().is_some_and(|j| !j.managed);
            status.node_version = self.versions.0.clone();
            status.service_version = self.job.as_ref().and_then(|j| provision::entry_version(&j.entry)).or(self.versions.1.clone());
            status.codex = self.tools.0.clone();
            status.claude = self.tools.1.clone();
            status.log_tail = if matches!(phase, Phase::Running | Phase::Setup) { Vec::new() } else { tail(&log_file, 14) };
            status.log_file = log_file.to_string_lossy().to_string();
            status.clone()
        };
        self.update_tray(&snapshot);
        self.write_status_file(&snapshot);
    }

    /// The same snapshot the status page shows, for scripts and bug reports.
    /// Only when the install root already exists: reporting must not be the
    /// thing that creates it.
    fn write_status_file(&self, status: &Status) {
        if !self.layout.root.is_dir() { return }
        let Ok(json) = serde_json::to_string_pretty(status) else { return };
        let file = self.layout.root.join("desktop-status.json");
        if std::fs::read_to_string(&file).is_ok_and(|current| current == json) { return }
        let tmp = self.layout.root.join(format!(".desktop-status.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() { let _ = std::fs::rename(&tmp, &file); }
    }

    fn update_tray(&self, status: &Status) {
        let t = text(status.lang);
        let mut line = phase_label(t, status.phase).to_string();
        if let (Phase::Running | Phase::Setup, Some(version)) = (status.phase, &status.service_version) {
            line = format!("{line} · v{version}");
        }
        let (can_start, can_stop) = match status.phase {
            Phase::Stopped | Phase::Failed => (true, status.pid.is_some()),
            Phase::Running | Phase::Setup | Phase::Starting => (false, true),
            Phase::Preparing | Phase::Stopping => (false, false),
        };
        if let Some(items) = self.shared.tray.lock().unwrap().as_ref() {
            let _ = items.status.set_text(line);
            let _ = items.start.set_enabled(can_start);
            let _ = items.stop.set_enabled(can_stop);
            let _ = items.restart.set_enabled(can_stop);
        }
    }

    /// Console when the service answers, status page when it has been away for
    /// a while. A console upgrade restarts the service and the console page
    /// follows that itself, so a short gap must not pull it away.
    fn steer(&mut self, status_http: Http, loaded: bool, url: &str) {
        let Some(window) = self.app.get_webview_window("main") else { return };
        let Ok(current) = window.url() else { return };
        let on_console = is_loopback(&current);
        if status_http != Http::Down {
            self.down_since = None;
            if !on_console { if let Ok(target) = Url::parse(url) { let _ = window.navigate(target); } }
            return;
        }
        if !on_console { self.down_since = None; return }
        let since = *self.down_since.get_or_insert_with(Instant::now);
        let patience = if loaded { Duration::from_secs(150) } else { Duration::from_secs(20) };
        if since.elapsed() >= patience {
            if let Some(launcher) = self.shared.launcher.lock().unwrap().clone() { let _ = window.navigate(launcher); }
            self.down_since = None;
        }
    }

    fn refresh(&mut self) {
        if self.busy.is_none() { self.job = service::find_existing(&self.layout).or(self.job.take()); }
        let loaded = self.job.as_ref().and_then(|job| service::loaded(&job.label));
        let pid = loaded.and_then(|l| l.pid);
        let port = self.job.as_ref().map(|j| j.port).unwrap_or(self.layout.port);
        let http = if pid.is_some() { health::probe(port) } else { Http::Down };
        let phase = match (self.busy, http, loaded.is_some()) {
            (Some(Phase::Stopping), _, true) => Phase::Stopping,
            (Some(Phase::Preparing), _, _) => Phase::Preparing,
            (_, Http::Console, _) => Phase::Running,
            (_, Http::Setup, _) => Phase::Setup,
            (_, Http::Down, true) => Phase::Starting,
            (_, Http::Down, false) => if self.error.is_some() { Phase::Failed } else { Phase::Stopped },
        };
        if matches!(phase, Phase::Running | Phase::Setup) { self.error = None; }
        self.publish(phase, pid, http);
        let url = format!("http://127.0.0.1:{port}/");
        self.steer(http, loaded.is_some(), &url);
    }

    fn say(&self, zh: String, en: String) -> String {
        if self.shared.status.lock().unwrap().lang == "zh" { zh } else { en }
    }

    fn start(&mut self) -> Result<(), String> {
        let layout = self.layout.clone();
        let existing = service::find_existing(&layout);
        if let Some(job) = existing.as_ref().filter(|job| !job.managed) {
            // Someone else's job: start it as it is, never rewrite it.
            self.job = Some(job.clone());
            if service::loaded(&job.label).is_none() { service::bootstrap(&job.plist)?; }
            return Ok(());
        }
        let seed = provision::Seed::read(&self.resources)?;
        self.versions = (Some(seed.node_version.clone()), Some(seed.service_version.clone()));
        let dirs = shellenv::login_dirs(&layout.home);
        let mut search = dirs.clone();
        search.push(layout.tools_bin());
        self.tools = (shellenv::find_tool(&search, "codex").map(|p| p.to_string_lossy().to_string()),
            shellenv::find_tool(&search, "claude").map(|p| p.to_string_lossy().to_string()));
        if let Some(job) = existing.as_ref() {
            self.job = Some(job.clone());
            // A loaded job is left exactly as it runs; opening the app must
            // never restart a working service.
            if service::loaded(&job.label).is_some() { return Ok(()) }
        }
        if layout.plist_path().exists() && service::read_job(&layout.plist_path()).is_none() {
            let file = layout.plist_path().display().to_string();
            return Err(self.say(format!("{file} 已存在，但它启动的不是 SintBot；请先把它移走。"),
                format!("{file} exists but does not start SintBot; move it aside first.")));
        }
        // Checked before anything is installed: on a Mac where SintBot (or
        // something else on its port) already runs, the app changes nothing.
        if let Some(pid) = service::lock_holder(&layout.state_dir()) {
            return Err(self.say(format!("SintBot 已经在这个 App 之外运行（进程 {pid}）。先在它所在的终端按 Ctrl-C 停掉，再点启动。"),
                format!("SintBot is already running outside this app (process {pid}). Stop it there first (Ctrl-C in its terminal), then start again.")));
        }
        if health::port_in_use(layout.port) {
            let port = layout.port;
            return Err(self.say(format!("端口 {port} 正被其他程序占用。退出那个程序后再点启动。"),
                format!("Port {port} is in use by another program. Quit that program, then start again.")));
        }
        let runtime = provision::ensure_runtime(&layout, &seed)?;
        let seeded = provision::ensure_release(&layout, &seed, &runtime)?;
        // Keep a version the service upgraded itself to; never downgrade it.
        let entry = existing.as_ref().map(|job| job.entry.clone())
            .filter(|entry| entry.is_file() && entry.starts_with(layout.releases_dir()))
            .filter(|entry| provision::entry_version(entry).is_some_and(|v| provision::is_newer(&v, &seed.service_version)))
            .unwrap_or(seeded);
        let path_env = shellenv::service_path(&dirs, &[layout.tools_bin(), runtime.dir.join("bin")]);
        if let Some(parent) = layout.log_file().parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
        let dict = service::build_plist(&service::JobSpec { layout: &layout, node: &runtime.node, entry: &entry,
            path_env: &path_env, bundle_id: BUNDLE_ID });
        service::write_plist(&layout.plist_path(), &dict)?;
        service::discard_parked(&layout);
        self.job = service::read_job(&layout.plist_path());
        service::bootstrap(&layout.plist_path())
    }

    /// `hold`: the user asked for the service to be off, so it must also stay
    /// off at the next login. A restart stops without that.
    fn stop(&mut self, hold: bool) -> Result<(), String> {
        let Some(job) = self.job.clone() else { return Ok(()) };
        if !hold { return service::bootout(&job.label, Duration::from_secs(90)) }
        if service::stop_job(&self.layout, &job, Duration::from_secs(90))? {
            self.job = service::find_existing(&self.layout);
        }
        Ok(())
    }

    /// Stops the service and deletes the job this app wrote. Nothing else is
    /// removed. `Ok(false)`: the job belongs to someone else and was not touched.
    fn remove_service(&mut self) -> Result<bool, String> {
        let job = service::find_existing(&self.layout).or(self.job.take());
        if job.as_ref().is_some_and(|job| !job.managed) { return Ok(false) }
        if let Some(job) = &job { service::bootout(&job.label, Duration::from_secs(90))?; }
        service::remove_job_files(&self.layout)?;
        self.job = None;
        Ok(true)
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Cmd::Ensure) => {
                    self.error = None;
                    self.busy = Some(Phase::Preparing);
                    self.publish(Phase::Preparing, None, Http::Down);
                    if let Err(error) = self.start() { self.error = Some(error); }
                    self.busy = None;
                }
                Ok(Cmd::Stop) => {
                    self.busy = Some(Phase::Stopping);
                    self.publish(Phase::Stopping, None, Http::Down);
                    self.error = self.stop(true).err();
                    self.busy = None;
                }
                Ok(Cmd::Restart) => {
                    self.busy = Some(Phase::Stopping);
                    self.publish(Phase::Stopping, None, Http::Down);
                    self.error = self.stop(false).err();
                    if self.error.is_none() {
                        self.busy = Some(Phase::Preparing);
                        if let Err(error) = self.start() { self.error = Some(error); }
                    }
                    self.busy = None;
                }
                Ok(Cmd::StopAndQuit) => {
                    self.busy = Some(Phase::Stopping);
                    self.publish(Phase::Stopping, None, Http::Down);
                    match self.stop(true) {
                        Ok(()) => { self.app.exit(0); return; }
                        Err(error) => {
                            // Do not report a successful stop by disappearing while
                            // launchd may still be running the service.
                            self.error = Some(error.clone());
                            self.busy = None;
                            show_main(&self.app);
                            self.app.dialog().message(error).title(text(self.shared.status.lock().unwrap().lang).s_failed)
                                .kind(MessageDialogKind::Error).show(|_| {});
                        }
                    }
                }
                Ok(Cmd::RemoveService { quiet }) => {
                    let t = text(self.shared.status.lock().unwrap().lang);
                    self.busy = Some(Phase::Stopping);
                    self.publish(Phase::Stopping, None, Http::Down);
                    let outcome = self.remove_service();
                    self.busy = None;
                    match outcome {
                        Ok(true) if quiet => { self.app.exit(0); return; }
                        Ok(true) => {
                            // The worker ends here: a refresh would be free to
                            // find nothing, but nothing may start it again.
                            let app = self.app.clone();
                            self.publish(Phase::Stopped, None, Http::Down);
                            self.app.dialog().message(t.removed_body).title(t.removed_title)
                                .kind(MessageDialogKind::Info).show(move |_| app.exit(0));
                            return;
                        }
                        Ok(false) => {
                            if quiet { eprintln!("SintBot: {}", EN.remove_adopted); }
                            else { self.app.dialog().message(t.remove_adopted).title(t.remove_title)
                                .kind(MessageDialogKind::Info).show(|_| {}); }
                        }
                        Err(error) => {
                            self.error = Some(error.clone());
                            if quiet { eprintln!("SintBot: {error}"); }
                            else {
                                show_main(&self.app);
                                self.app.dialog().message(error).title(t.remove_title)
                                    .kind(MessageDialogKind::Error).show(|_| {});
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.refresh();
        }
    }
}

fn send(app: &AppHandle, cmd: Cmd) {
    let shared = app.state::<Arc<Shared>>();
    let _ = shared.tx.lock().unwrap().send(cmd);
}

fn confirm_then(app: &AppHandle, body: &'static str, ok: &'static str, cmd: fn() -> Cmd) {
    let lang = app.state::<Arc<Shared>>().status.lock().unwrap().lang;
    confirm_titled(app, text(lang).stop_title, body, ok, cmd)
}

fn confirm_titled(app: &AppHandle, title: &'static str, body: &'static str, ok: &'static str, cmd: fn() -> Cmd) {
    let lang = app.state::<Arc<Shared>>().status.lock().unwrap().lang;
    let t = text(lang);
    let handle = app.clone();
    app.dialog().message(body).title(title).kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(ok.to_string(), t.cancel.to_string()))
        .show(move |confirmed| if confirmed { send(&handle, cmd()) });
}

/// Opening the app again shows the window. With a flag it drives the running
/// app instead (`SintBot --stop`), which is how scripts and tests control it.
fn second_launch(app: &AppHandle, args: &[String]) {
    match args.iter().skip(1).find(|arg| arg.starts_with("--")).map(String::as_str) {
        Some("--start") => send(app, Cmd::Ensure),
        Some("--stop") => send(app, Cmd::Stop),
        Some("--restart") => send(app, Cmd::Restart),
        Some("--remove-service") => send(app, Cmd::RemoveService { quiet: true }),
        Some("--check-update") => update::start(app, update::Trigger::Script { install: false }),
        Some("--install-update") => update::start(app, update::Trigger::Script { install: true }),
        Some("--quit") => app.exit(0),
        _ => show_main(app),
    }
}

#[tauri::command]
fn status(shared: tauri::State<Arc<Shared>>) -> Status { shared.status.lock().unwrap().clone() }

#[tauri::command]
fn service_action(app: AppHandle, action: String) -> Result<(), String> {
    let lang = app.state::<Arc<Shared>>().status.lock().unwrap().lang;
    let t = text(lang);
    match action.as_str() {
        "start" => send(&app, Cmd::Ensure),
        "stop" => confirm_then(&app, t.stop_body, t.confirm_stop, || Cmd::Stop),
        "restart" => confirm_then(&app, t.restart_body, t.confirm_restart, || Cmd::Restart),
        _ => return Err("unknown action".into()),
    }
    Ok(())
}

#[tauri::command]
fn open_logs(shared: tauri::State<Arc<Shared>>) {
    let file = shared.status.lock().unwrap().log_file.clone();
    if Path::new(&file).is_file() { open_with_system(&file) }
}

fn build_tray(app: &AppHandle, lang: &'static str) -> tauri::Result<TrayItems> {
    let t = text(lang);
    let item = |id: &str, label: &str, enabled: bool| MenuItem::with_id(app, id, label, enabled, None::<&str>);
    let status = item("status", t.s_preparing, false)?;
    let open = item("open", t.open, true)?;
    let browser = item("browser", t.browser, true)?;
    let start = item("start", t.start, false)?;
    let stop = item("stop", t.stop, false)?;
    let restart = item("restart", t.restart, false)?;
    let logs = item("logs", t.logs, true)?;
    let update = item("update", t.update, app.state::<Arc<Shared>>().update.source.configured)?;
    let remove = item("remove", t.remove, true)?;
    let quit = item("quit", t.quit, true)?;
    let stop_quit = item("stop_quit", t.stop_quit, true)?;
    let menu = Menu::with_items(app, &[&status, &PredefinedMenuItem::separator(app)?, &open, &browser,
        &PredefinedMenuItem::separator(app)?, &start, &stop, &restart, &logs,
        &PredefinedMenuItem::separator(app)?, &update,
        &PredefinedMenuItem::separator(app)?, &quit, &stop_quit,
        &PredefinedMenuItem::separator(app)?, &remove])?;
    TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            let t = text(lang);
            match event.id().as_ref() {
                "open" => show_main(app),
                "browser" => {
                    let url = app.state::<Arc<Shared>>().status.lock().unwrap().url.clone();
                    open_with_system(&url);
                }
                "start" => send(app, Cmd::Ensure),
                "stop" => confirm_then(app, t.stop_body, t.confirm_stop, || Cmd::Stop),
                "restart" => confirm_then(app, t.restart_body, t.confirm_restart, || Cmd::Restart),
                "logs" => {
                    let file = app.state::<Arc<Shared>>().status.lock().unwrap().log_file.clone();
                    if Path::new(&file).is_file() { open_with_system(&file) }
                }
                "update" => update::start(app, update::Trigger::User),
                "quit" => app.exit(0),
                "stop_quit" => confirm_then(app, t.stop_body, t.confirm_stop, || Cmd::StopAndQuit),
                "remove" => {
                    // Someone else's job: say so instead of asking to confirm
                    // something the app will not do.
                    if app.state::<Arc<Shared>>().status.lock().unwrap().adopted {
                        app.dialog().message(t.remove_adopted).title(t.remove_title).kind(MessageDialogKind::Info).show(|_| {});
                    } else {
                        confirm_titled(app, t.remove_title, t.remove_body, t.confirm_remove, || Cmd::RemoveService { quiet: false });
                    }
                }
                _ => {}
            }
        })
        .build(app)?;
    Ok(TrayItems { status, start, stop, restart, update })
}

fn main() {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => { eprintln!("SintBot: {error}"); std::process::exit(2) }
    };
    let lang = system_lang();
    let mut context = tauri::generate_context!();
    let app_version = context.package_info().version.to_string();
    let updates = update::Updates::new(
        update::prepare(context.config_mut(), std::env::var(update::ENDPOINT_ENV).ok().as_deref()), &layout, &app_version);
    let update_configured = updates.source.configured;
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        status: Mutex::new(Status {
            phase: Phase::Preparing, error: None, app_version,
            node_version: None, service_version: None, codex: None, claude: None, label: layout.label.clone(),
            pid: None, http: Http::Down, url: layout.console_url(), adopted: false, development: layout.overridden,
            log_file: layout.log_file().to_string_lossy().to_string(), log_tail: Vec::new(), lang,
            update: updates.initial_report(),
        }),
        tx: Mutex::new(tx), launcher: Mutex::new(None), tray: Mutex::new(None), update: updates,
    });
    let worker_shared = shared.clone();
    let mut rx = Some(rx);
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| second_launch(app, &args)))
        .plugin(tauri_plugin_dialog::init());
    // Only Rust calls the updater. No capability grants its commands to a page,
    // and a build without a public key does not load it at all.
    if update_configured { builder = builder.plugin(tauri_plugin_updater::Builder::new().build()); }
    builder
        .manage(shared)
        .invoke_handler(tauri::generate_handler![status, service_action, open_logs])
        .setup(move |app| {
            // Reaching setup means no other instance took the launch over. A
            // control flag then has nothing to drive: do what it asks without
            // opening the app.
            match std::env::args().skip(1).find(|arg| arg.starts_with("--")).as_deref() {
                Some("--quit") => std::process::exit(0),
                Some("--stop") => {
                    let stopped = match service::find_existing(&layout) {
                        Some(job) => service::stop_job(&layout, &job, Duration::from_secs(90)).map(|_| ()),
                        None => Ok(()),
                    };
                    if let Err(error) = &stopped { eprintln!("SintBot: {error}"); }
                    std::process::exit(if stopped.is_ok() { 0 } else { 1 });
                }
                Some("--remove-service") => {
                    let removed = match service::find_existing(&layout) {
                        Some(job) if !job.managed => Err(EN.remove_adopted.to_string()),
                        Some(job) => service::bootout(&job.label, Duration::from_secs(90))
                            .and_then(|()| service::remove_job_files(&layout)).map(|_| ()),
                        None => Ok(()),
                    };
                    if let Err(error) = &removed { eprintln!("SintBot: {error}"); }
                    std::process::exit(if removed.is_ok() { 0 } else { 1 });
                }
                Some(flag @ ("--check-update" | "--install-update")) => {
                    #[cfg(target_os = "macos")]
                    let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
                    update::headless(app.handle(), flag == "--install-update");
                    return Ok(());
                }
                _ => {}
            }
            let handle = app.handle().clone();
            let items = build_tray(&handle, lang)?;
            *worker_shared.tray.lock().unwrap() = Some(items);
            // Development aid: show a console served by something else (a preview
            // server with sample data) in the real window, and manage no service.
            let preview = std::env::var("SINTBOT_DESKTOP_PREVIEW_URL").ok()
                .and_then(|value| Url::parse(&value).ok()).filter(is_loopback);
            let first_page = match &preview {
                Some(url) => WebviewUrl::External(url.clone()),
                None => WebviewUrl::App("index.html".into()),
            };
            let (r, g, b) = if system_dark() { (0x13, 0x17, 0x1c) } else { (0xf6, 0xf5, 0xf1) };
            let bridge = handle.clone();
            let window = WebviewWindowBuilder::new(app, "main", first_page)
                .title("SintBot").inner_size(1080.0, 720.0).min_inner_size(820.0, 560.0)
                .background_color(tauri::window::Color(r, g, b, 255))
                .initialization_script(INIT_JS)
                // Lets a console offer its own "check for updates" button only where it works.
                .initialization_script(if update_configured { "window.__SINTBOT_DESKTOP_UPDATE__ = true;" } else { "" })
                .on_navigation(move |url| match app_request(url) {
                    Some("check-update") => { update::start(&bridge, update::Trigger::User); false }
                    Some(_) => false,
                    None => allow_navigation(url),
                })
                // The native file-drop handler claims every drag on macOS and never hands it to
                // the page, so the console's HTML5 drag reordering got no dragover/drop. Nothing
                // in the app takes native file drops.
                .disable_drag_drop_handler()
                .build()?;
            *worker_shared.launcher.lock().unwrap() = window.url().ok();
            let resources = std::env::var_os("SINTBOT_DESKTOP_RESOURCES").map(PathBuf::from)
                .map(Ok).unwrap_or_else(|| app.path().resource_dir())?;
            let worker = Worker { app: handle.clone(), shared: worker_shared.clone(), layout: layout.clone(), resources,
                job: None, busy: None, error: None, down_since: None, tools: (None, None), versions: (None, None) };
            let rx = rx.take().expect("setup runs once");
            if preview.is_none() {
                std::thread::Builder::new().name("service".into()).spawn(move || worker.run(rx))?;
                send(&handle, Cmd::Ensure);
            }
            worker_shared.update.log(&format!("SintBot for macOS {} started", handle.package_info().version));
            update::schedule(&handle, layout.overridden);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Closing the window leaves the app in the menu bar.
                api.prevent_close();
                hide_main(window.app_handle());
            }
        })
        .build(context)
        .expect("SintBot could not start")
        .run(|app, event| match event {
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main(app),
            RunEvent::ExitRequested { api, code, .. } => { if code.is_none() { api.prevent_exit() } }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_stays_on_local_pages() {
        for allowed in ["tauri://localhost/index.html", "http://127.0.0.1:4600/", "http://localhost:4631/x", "about:blank"] {
            assert!(allow_navigation(&Url::parse(allowed).unwrap()), "{allowed}");
        }
        for refused in ["file:///etc/passwd", "javascript:alert(1)", "ftp://example.com/", "sintbot-app:check-update"] {
            assert!(!allow_navigation(&Url::parse(refused).unwrap()), "{refused}");
        }
    }

    #[test]
    fn pages_can_only_ask_for_an_update_check() {
        assert_eq!(app_request(&Url::parse("sintbot-app:check-update").unwrap()), Some("check-update"));
        assert_eq!(app_request(&Url::parse("sintbot-app:install-update").unwrap()), Some("install-update"));
        assert_eq!(app_request(&Url::parse("sintbot-open:https%3A%2F%2Fsintbot.com").unwrap()), None);
        assert_eq!(app_request(&Url::parse("http://127.0.0.1:4600/sintbot-app:check-update").unwrap()), None);
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("https%3A%2F%2Fsintbot.com%2Fzh%2F"), "https://sintbot.com/zh/");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
