//! First-launch installation of the two things a new Mac lacks: a Node.js
//! runtime and the SintBot service package. Both are unpacked from the app
//! bundle into the install root, side by side with anything already there, and
//! only moved into place once they have been run successfully.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::layout::{Layout, ENTRY_SUFFIX};

#[derive(Clone, Debug)]
pub struct Seed {
    pub resources: PathBuf,
    pub node_version: String,
    pub service_version: String,
}

#[derive(Clone, Debug)]
pub struct Runtime {
    pub dir: PathBuf,
    pub node: PathBuf,
}

fn read_version(file: &Path) -> Result<String, String> {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let version = text.trim().to_string();
    let ok = !version.is_empty() && version.len() <= 64
        && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        && !version.starts_with('.');
    if ok { Ok(version) } else { Err(format!("{} does not hold a version", file.display())) }
}

impl Seed {
    pub fn read(resources: &Path) -> Result<Seed, String> {
        Ok(Seed {
            resources: resources.to_path_buf(),
            node_version: read_version(&resources.join("runtime/VERSION"))?,
            service_version: read_version(&resources.join("payload/VERSION"))?,
        })
    }
}

fn run(program: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program).args(args).stdin(Stdio::null()).current_dir("/")
        .output().map_err(|e| format!("{}: {e}", program.display()))?;
    if !output.status.success() {
        return Err(format!("{} {} failed: {}", program.display(), args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn untar(archive: &Path, into: &Path) -> Result<(), String> {
    fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    run(Path::new("/usr/bin/tar"), &["-xzf", &archive.to_string_lossy(), "-C", &into.to_string_lossy()]).map(|_| ())
}

fn fresh_staging(parent: &Path, mode: u32) -> Result<PathBuf, String> {
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(mode));
    let staging = parent.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    Ok(staging)
}

/// A directory that failed halfway is kept for inspection, never deleted.
fn set_aside(path: &Path) -> Result<(), String> {
    if !path.exists() { return Ok(()) }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let aside = path.with_file_name(format!("{}.incomplete-{stamp}",
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    fs::rename(path, &aside).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn ensure_runtime(layout: &Layout, seed: &Seed) -> Result<Runtime, String> {
    let dir = layout.runtime_dir(&seed.node_version);
    let node = dir.join("bin/node");
    if dir.join(".complete").is_file() && node.is_file() {
        return Ok(Runtime { dir, node });
    }
    let staging = fresh_staging(&layout.runtime_root(), 0o755)?;
    let staged_node = staging.join("bin/node");
    fs::create_dir_all(staging.join("bin")).map_err(|e| e.to_string())?;
    fs::copy(seed.resources.join("runtime/bin/node"), &staged_node).map_err(|e| format!("copying node: {e}"))?;
    fs::set_permissions(&staged_node, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    untar(&seed.resources.join("runtime/npm.tgz"), &staging.join("lib/node_modules"))?;
    // The service's updater installs new versions with the `npm` next to the
    // node binary it runs under.
    std::os::unix::fs::symlink("../lib/node_modules/npm/bin/npm-cli.js", staging.join("bin/npm"))
        .map_err(|e| format!("linking npm: {e}"))?;
    // Files copied out of a downloaded app inherit its quarantine flag.
    let _ = Command::new("/usr/bin/xattr").args(["-dr", "com.apple.quarantine"]).arg(&staging)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let reported = run(&staged_node, &["--version"])?;
    if reported != seed.node_version {
        return Err(format!("bundled Node.js reports {reported}, expected {}", seed.node_version));
    }
    run(&staged_node, &[&staging.join("bin/npm").to_string_lossy(), "--version"])?;
    fs::write(staging.join(".complete"), &seed.node_version).map_err(|e| e.to_string())?;
    set_aside(&dir)?;
    fs::rename(&staging, &dir).map_err(|e| format!("installing runtime: {e}"))?;
    Ok(Runtime { node: dir.join("bin/node"), dir })
}

/// Seeds the bundled service version unless that exact version is already
/// installed. Returns its entry script. Newer versions the service installed
/// itself are left alone; choosing between them is the caller's decision.
pub fn ensure_release(layout: &Layout, seed: &Seed, runtime: &Runtime) -> Result<PathBuf, String> {
    let entry = layout.release_entry(&seed.service_version);
    if entry.is_file() {
        return Ok(entry);
    }
    let staging = fresh_staging(&layout.releases_dir(), 0o700)?;
    untar(&seed.resources.join("payload/sintbot.tgz"), &staging)?;
    let staged_entry = staging.join(ENTRY_SUFFIX);
    let reported = run(&runtime.node, &[&staged_entry.to_string_lossy(), "version"])?;
    if reported != seed.service_version {
        return Err(format!("bundled service reports {reported}, expected {}", seed.service_version));
    }
    let target = layout.releases_dir().join(&seed.service_version);
    set_aside(&target)?;
    fs::rename(&staging, &target).map_err(|e| format!("installing service: {e}"))?;
    Ok(entry)
}

/// Version of the package an entry script belongs to.
pub fn entry_version(entry: &Path) -> Option<String> {
    let package = entry.parent()?.parent()?.join("package.json");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(package).ok()?).ok()?;
    if json.get("name")?.as_str()? != "sintbot" { return None }
    Some(json.get("version")?.as_str()?.to_string())
}

fn numeric(version: &str) -> (u64, u64, u64, bool) {
    let (core, pre) = match version.split_once('-') { Some((c, _)) => (c, true), None => (version, false) };
    let mut parts = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0), !pre)
}

/// True when `candidate` is a strictly newer release than `than`.
pub fn is_newer(candidate: &str, than: &str) -> bool { numeric(candidate) > numeric(than) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_order() {
        assert!(is_newer("1.4.19", "1.4.18"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(!is_newer("1.4.18", "1.4.18"));
        assert!(is_newer("1.4.18", "1.4.18-dev.1"));
        assert!(!is_newer("1.4.17", "1.4.18"));
    }

    #[test]
    fn version_files_reject_paths() {
        let dir = std::env::temp_dir().join(format!("sintbot-desktop-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("VERSION");
        for bad in ["", "../x", "1.0/2", ".hidden"] {
            fs::write(&file, bad).unwrap();
            assert!(read_version(&file).is_err(), "{bad}");
        }
        fs::write(&file, "v22.23.5\n").unwrap();
        assert_eq!(read_version(&file).unwrap(), "v22.23.5");
        fs::remove_dir_all(&dir).unwrap();
    }
}
