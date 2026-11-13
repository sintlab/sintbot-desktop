//! The service is a per-user launchd job with exactly the shape the service's
//! own updater expects (`[node, entry, "start", …]` in
//! `~/Library/LaunchAgents/<label>.plist`), so console upgrades and their
//! rollback keep working unchanged. This module writes that job, and starts
//! and stops it. It never edits a job it did not create.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use plist::{Dictionary, Value};

use crate::layout::{is_service_entry, Layout, DEFAULT_PORT};

pub const MARKER: &str = "SINTBOT_DESKTOP";

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub label: String,
    pub plist: PathBuf,
    pub node: PathBuf,
    pub entry: PathBuf,
    pub port: u16,
    /// Written by this app (carries the marker); anything else is only observed.
    pub managed: bool,
    pub log_file: Option<PathBuf>,
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|items| items.iter()
        .filter_map(|item| item.as_string().map(str::to_string)).collect()).unwrap_or_default()
}

pub fn job_from(dict: &Dictionary, plist: &Path) -> Option<Job> {
    let label = dict.get("Label")?.as_string()?.to_string();
    let args = strings(dict.get("ProgramArguments"));
    if args.len() < 3 || args[2] != "start" || !is_service_entry(Path::new(&args[1])) { return None }
    let port = args.iter().position(|a| a == "--ui-port").and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u16>().ok()).unwrap_or(DEFAULT_PORT);
    let managed = dict.get("EnvironmentVariables").and_then(Value::as_dictionary)
        .and_then(|env| env.get(MARKER)).and_then(Value::as_string) == Some("1");
    Some(Job { label, plist: plist.to_path_buf(), node: PathBuf::from(&args[0]), entry: PathBuf::from(&args[1]),
        port, managed, log_file: dict.get("StandardErrorPath").and_then(Value::as_string).map(PathBuf::from) })
}

pub fn read_job(plist: &Path) -> Option<Job> {
    let value = Value::from_file(plist).ok()?;
    job_from(value.as_dictionary()?, plist)
}

/// A SintBot service someone set up by hand (or an earlier deployment script).
/// One chat app (Feishu, Slack) is served by one connection, so the app adopts it instead of
/// starting a second service.
pub fn find_existing(layout: &Layout) -> Option<Job> {
    let own = layout.plist_path();
    if let Some(job) = read_job(&own) { return Some(job) }
    let mut found: Vec<Job> = fs::read_dir(layout.launch_agents()).into_iter().flatten().filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "plist") && *path != own)
        .filter_map(|path| read_job(&path)).collect();
    found.sort_by(|a, b| a.label.cmp(&b.label));
    // Only a job on the same console port can collide with ours.
    found.into_iter().find(|job| job.port == layout.port)
        // Our own job, set aside by a stop from the app. It remembers which
        // version the service had upgraded itself to.
        .or_else(|| read_job(&layout.parked_plist()).filter(|job| job.managed && job.label == layout.label))
}

/// Stops the job and, for a job this app wrote, moves its file out of
/// LaunchAgents so the next login does not start it again. A job that was not
/// loaded is left where it is: the service's own updater unloads the job for
/// a moment while it switches versions and needs the file in place.
/// Returns whether the file was set aside.
pub fn stop_job(layout: &Layout, job: &Job, timeout: Duration) -> Result<bool, String> {
    let was_loaded = loaded(&job.label).is_some();
    bootout(&job.label, timeout)?;
    if !(was_loaded && job.managed && job.plist == layout.plist_path()) { return Ok(false) }
    park(&job.plist, &layout.parked_plist()).map(|()| true)
}

fn park(own: &Path, parked: &Path) -> Result<(), String> {
    let dir = parked.parent().ok_or("parked job file has no directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    fs::rename(own, parked).map_err(|e| format!("the service stopped, but its job file could not be set aside ({e}); it will start again at the next login"))
}

/// After the job file is back in LaunchAgents the parked copy is stale.
pub fn discard_parked(layout: &Layout) {
    let parked = layout.parked_plist();
    if read_job(&parked).is_some_and(|job| job.managed) { let _ = fs::remove_file(parked); }
}

/// Deletes the job files this app wrote, in LaunchAgents and parked, and
/// nothing else: a file without the marker is never removed. Returns what was
/// deleted.
pub fn remove_job_files(layout: &Layout) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    for file in [layout.plist_path(), layout.parked_plist()] {
        if !read_job(&file).is_some_and(|job| job.managed && job.label == layout.label) { continue }
        fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        removed.push(file);
    }
    Ok(removed)
}

pub struct JobSpec<'a> {
    pub layout: &'a Layout,
    pub node: &'a Path,
    pub entry: &'a Path,
    pub path_env: &'a str,
    pub bundle_id: &'a str,
}

pub fn build_plist(spec: &JobSpec) -> Dictionary {
    let layout = spec.layout;
    let mut args: Vec<String> = vec![spec.node.to_string_lossy().into(), spec.entry.to_string_lossy().into(),
        "start".into(), "--ui".into(), "--no-open".into()];
    if layout.port != DEFAULT_PORT { args.extend(["--ui-port".to_string(), layout.port.to_string()]) }
    if let Some(dir) = &layout.data_dir {
        args.extend(["--data-dir".to_string(), dir.to_string_lossy().into(),
            "--config".to_string(), dir.join("config.json").to_string_lossy().into()]);
    }
    let mut env = Dictionary::new();
    env.insert("PATH".into(), Value::String(spec.path_env.into()));
    env.insert("HOME".into(), Value::String(layout.home.to_string_lossy().into()));
    env.insert("LANG".into(), Value::String("en_US.UTF-8".into()));
    env.insert(MARKER.into(), Value::String("1".into()));
    if layout.overridden {
        env.insert("SINTBOT_RELEASES_DIR".into(), Value::String(layout.releases_dir().to_string_lossy().into()));
    }
    let log = layout.log_file().to_string_lossy().to_string();
    let mut dict = Dictionary::new();
    dict.insert("Label".into(), Value::String(layout.label.clone()));
    dict.insert("ProgramArguments".into(), Value::Array(args.into_iter().map(Value::String).collect()));
    dict.insert("EnvironmentVariables".into(), Value::Dictionary(env));
    dict.insert("WorkingDirectory".into(), Value::String(layout.home.to_string_lossy().into()));
    dict.insert("RunAtLoad".into(), Value::Boolean(true));
    dict.insert("KeepAlive".into(), Value::Boolean(true));
    dict.insert("StandardOutPath".into(), Value::String(log.clone()));
    dict.insert("StandardErrorPath".into(), Value::String(log));
    // Declare the owning app; macOS may still group this legacy agent under
    // the Node.js signing identity in System Settings › Login Items.
    dict.insert("AssociatedBundleIdentifiers".into(), Value::Array(vec![Value::String(spec.bundle_id.into())]));
    dict
}

pub fn write_plist(path: &Path, dict: &Dictionary) -> Result<(), String> {
    let parent = path.parent().ok_or("job file has no directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let tmp = path.with_extension(format!("plist.tmp-{}", std::process::id()));
    Value::Dictionary(dict.clone()).to_file_xml(&tmp).map_err(|e| format!("writing job: {e}"))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| format!("installing job: {e}"))
}

pub fn uid() -> Result<u32, String> {
    let out = Command::new("/usr/bin/id").arg("-u").output().map_err(|e| format!("id: {e}"))?;
    String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|_| "cannot read the user id".to_string())
}

fn launchctl(args: &[&str]) -> Result<(bool, String), String> {
    let out = Command::new("/bin/launchctl").args(args).stdin(Stdio::null()).output()
        .map_err(|e| format!("launchctl: {e}"))?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loaded { pub pid: Option<u32> }

pub fn parse_pid(printed: &str) -> Option<u32> {
    printed.lines().map(str::trim).find_map(|line| line.strip_prefix("pid = ")).and_then(|pid| pid.trim().parse().ok())
}

/// `None` when launchd does not know the job.
pub fn loaded(label: &str) -> Option<Loaded> {
    let target = format!("gui/{}/{label}", uid().ok()?);
    match launchctl(&["print", &target]) {
        Ok((true, printed)) => Some(Loaded { pid: parse_pid(&printed) }),
        _ => None,
    }
}

pub fn bootstrap(plist: &Path) -> Result<(), String> {
    let domain = format!("gui/{}", uid()?);
    let (ok, output) = launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])?;
    if ok { Ok(()) } else { Err(format!("launchd refused the job: {}", output.trim())) }
}

/// Asks launchd to stop the job (SIGTERM, the service's graceful stop) and
/// waits until it is gone.
pub fn bootout(label: &str, timeout: Duration) -> Result<(), String> {
    if loaded(label).is_none() { return Ok(()) }
    let target = format!("gui/{}/{label}", uid()?);
    let (ok, output) = launchctl(&["bootout", &target])?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if loaded(label).is_none() { return Ok(()) }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(if ok { "the service did not stop in time".to_string() } else { format!("launchd could not stop the job: {}", output.trim()) })
}

pub fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill").args(["-0", &pid.to_string()]).stdin(Stdio::null())
        .stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// PID in the service's single-writer lock, when that process is still alive.
pub fn lock_holder(state_dir: &Path) -> Option<u32> {
    let pid = fs::read_to_string(state_dir.join("daemon.lock")).ok()?.trim().parse::<u32>().ok()?;
    pid_alive(pid).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout { home: "/Users/t".into(), root: "/Users/t/.local/share/sintbot".into(), data_dir: None,
            label: "com.sintbot.daemon".into(), port: 4600, overridden: false }
    }

    #[test]
    fn written_job_has_the_shape_the_service_updater_reads() {
        let layout = layout();
        let entry = layout.release_entry("1.4.18");
        let dict = build_plist(&JobSpec { layout: &layout, node: Path::new("/rt/bin/node"), entry: &entry,
            path_env: "/usr/bin:/bin", bundle_id: "com.sintbot.desktop" });
        let args = strings(dict.get("ProgramArguments"));
        assert_eq!(args, ["/rt/bin/node", entry.to_str().unwrap(), "start", "--ui", "--no-open"]);
        assert_eq!(dict.get("KeepAlive"), Some(&Value::Boolean(true)));
        let job = job_from(&dict, &layout.plist_path()).unwrap();
        assert!(job.managed);
        assert_eq!((job.port, job.entry.clone(), job.label.as_str()), (4600, entry, "com.sintbot.daemon"));
        assert_eq!(job.log_file.as_deref(), Some(Path::new("/Users/t/Library/Logs/SintBot/daemon.log")));
    }

    #[test]
    fn overrides_reach_the_service_command_line() {
        let layout = Layout { data_dir: Some("/tmp/d".into()), port: 4631, overridden: true, label: "x.dev".into(), ..layout() };
        let dict = build_plist(&JobSpec { layout: &layout, node: Path::new("/n"), entry: &layout.release_entry("1.0.0"),
            path_env: "/bin", bundle_id: "b" });
        let args = strings(dict.get("ProgramArguments"));
        assert_eq!(&args[5..], ["--ui-port", "4631", "--data-dir", "/tmp/d", "--config", "/tmp/d/config.json"]);
        assert_eq!(job_from(&dict, Path::new("/p")).unwrap().port, 4631);
    }

    #[test]
    fn other_programs_are_not_mistaken_for_the_service() {
        let mut dict = Dictionary::new();
        dict.insert("Label".into(), Value::String("com.sbot.daemon".into()));
        dict.insert("ProgramArguments".into(), Value::Array(["/opt/homebrew/bin/node", "/opt/homebrew/bin/sbot", "start", "--ui"]
            .iter().map(|s| Value::String(s.to_string())).collect()));
        assert_eq!(job_from(&dict, Path::new("/p")), None);
    }

    fn scratch(name: &str) -> Layout {
        let home = std::env::temp_dir().join(format!("sintbot-desktop-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        Layout { root: home.join("root"), home, data_dir: None, label: "com.sintbot.test.unit".into(), port: 4699, overridden: true }
    }

    fn own_job(layout: &Layout, version: &str) -> Dictionary {
        build_plist(&JobSpec { layout, node: Path::new("/n"), entry: &layout.release_entry(version), path_env: "/bin", bundle_id: "b" })
    }

    #[test]
    fn a_parked_job_is_found_and_keeps_its_entry() {
        let layout = scratch("park");
        assert_eq!(find_existing(&layout), None);
        write_plist(&layout.plist_path(), &own_job(&layout, "1.5.0")).unwrap();
        park(&layout.plist_path(), &layout.parked_plist()).unwrap();
        assert!(!layout.plist_path().exists(), "nothing is left for launchd to load at login");
        let job = find_existing(&layout).unwrap();
        assert!(job.managed);
        assert_eq!((job.plist.clone(), job.entry.clone()), (layout.parked_plist(), layout.release_entry("1.5.0")));
        // Back in LaunchAgents, that file wins and the parked copy is dropped.
        write_plist(&layout.plist_path(), &own_job(&layout, "1.6.0")).unwrap();
        assert_eq!(find_existing(&layout).unwrap().entry, layout.release_entry("1.6.0"));
        discard_parked(&layout);
        assert!(!layout.parked_plist().exists());
        fs::remove_dir_all(&layout.home).unwrap();
    }

    #[test]
    fn a_job_on_the_same_port_wins_over_a_parked_one() {
        let layout = scratch("adopt");
        write_plist(&layout.parked_plist(), &own_job(&layout, "1.5.0")).unwrap();
        let mut foreign = own_job(&layout, "1.4.0");
        foreign.insert("Label".into(), Value::String("com.example.sintbot".into()));
        foreign.insert("EnvironmentVariables".into(), Value::Dictionary(Dictionary::new()));
        write_plist(&layout.launch_agents().join("com.example.sintbot.plist"), &foreign).unwrap();
        let job = find_existing(&layout).unwrap();
        assert_eq!((job.label.as_str(), job.managed), ("com.example.sintbot", false));
        fs::remove_dir_all(&layout.home).unwrap();
    }

    #[test]
    fn removal_only_deletes_files_this_app_wrote() {
        let layout = scratch("remove");
        // Someone else's file under our name: no marker, so it stays.
        let mut theirs = own_job(&layout, "1.4.0");
        theirs.insert("EnvironmentVariables".into(), Value::Dictionary(Dictionary::new()));
        write_plist(&layout.plist_path(), &theirs).unwrap();
        write_plist(&layout.parked_plist(), &own_job(&layout, "1.5.0")).unwrap();
        assert_eq!(remove_job_files(&layout).unwrap(), [layout.parked_plist()]);
        assert!(layout.plist_path().exists());
        write_plist(&layout.plist_path(), &own_job(&layout, "1.5.0")).unwrap();
        assert_eq!(remove_job_files(&layout).unwrap(), [layout.plist_path()]);
        assert_eq!(remove_job_files(&layout).unwrap(), Vec::<PathBuf>::new());
        fs::remove_dir_all(&layout.home).unwrap();
    }

    #[test]
    fn stopping_a_job_that_is_not_loaded_leaves_its_file_in_place() {
        // The state the service's updater creates between unloading the old
        // version and loading the new one.
        let layout = scratch("updater");
        write_plist(&layout.plist_path(), &own_job(&layout, "1.5.0")).unwrap();
        let job = read_job(&layout.plist_path()).unwrap();
        assert_eq!(stop_job(&layout, &job, Duration::from_secs(1)), Ok(false));
        assert!(layout.plist_path().exists() && !layout.parked_plist().exists());
        fs::remove_dir_all(&layout.home).unwrap();
    }

    #[test]
    fn pid_is_read_from_launchctl_print() {
        assert_eq!(parse_pid("\tstate = running\n\tpid = 4123\n"), Some(4123));
        assert_eq!(parse_pid("\tstate = waiting\n"), None);
    }
}
