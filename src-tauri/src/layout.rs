//! Where everything lives on disk. The defaults match the public installer and
//! the service's own updater; the environment overrides exist so a development
//! build can run next to a real installation without sharing anything with it.

use std::env;
use std::path::{Path, PathBuf};

pub const DEFAULT_LABEL: &str = "com.sintbot.daemon";
pub const DEFAULT_PORT: u16 = 4600;
pub const ENTRY_SUFFIX: &str = "lib/node_modules/sintbot/bin/sintbot-v2.js";

#[derive(Clone, Debug)]
pub struct Layout {
    pub home: PathBuf,
    /// Install root: runtime/, releases/, tools/. Default `~/.local/share/sintbot`.
    pub root: PathBuf,
    /// Only set by `SINTBOT_DESKTOP_DATA_DIR`; otherwise the service uses its own default.
    pub data_dir: Option<PathBuf>,
    pub label: String,
    pub port: u16,
    /// True when any override is in effect; the status page says so.
    pub overridden: bool,
}

fn absolute(var: &str) -> Result<Option<PathBuf>, String> {
    match env::var_os(var) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() && path.parent().is_some() {
                Ok(Some(path))
            } else {
                Err(format!("{var} must be an absolute path below the filesystem root"))
            }
        }
    }
}

impl Layout {
    pub fn from_env() -> Result<Self, String> {
        let home = env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute())
            .ok_or_else(|| "HOME is not set".to_string())?;
        let root = absolute("SINTBOT_DESKTOP_ROOT")?;
        let data_dir = absolute("SINTBOT_DESKTOP_DATA_DIR")?;
        let label = match env::var("SINTBOT_DESKTOP_LABEL") {
            Ok(value) if !value.is_empty() => {
                let ok = value.len() <= 128
                    && value.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                    && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
                if !ok {
                    return Err("SINTBOT_DESKTOP_LABEL is not a valid launchd label".into());
                }
                Some(value)
            }
            _ => None,
        };
        let port = match env::var("SINTBOT_DESKTOP_PORT") {
            Ok(value) if !value.is_empty() => Some(
                value.parse::<u16>().ok().filter(|p| *p > 0)
                    .ok_or_else(|| "SINTBOT_DESKTOP_PORT must be a port number".to_string())?,
            ),
            _ => None,
        };
        let overridden = root.is_some() || data_dir.is_some() || label.is_some() || port.is_some();
        Ok(Layout {
            root: root.unwrap_or_else(|| home.join(".local/share/sintbot")),
            data_dir,
            label: label.unwrap_or_else(|| DEFAULT_LABEL.to_string()),
            port: port.unwrap_or(DEFAULT_PORT),
            home,
            overridden,
        })
    }

    pub fn runtime_root(&self) -> PathBuf { self.root.join("runtime") }
    pub fn runtime_dir(&self, node_version: &str) -> PathBuf {
        self.runtime_root().join(format!("node-{node_version}"))
    }
    pub fn releases_dir(&self) -> PathBuf { self.root.join("releases") }
    pub fn release_entry(&self, version: &str) -> PathBuf {
        self.releases_dir().join(version).join(ENTRY_SUFFIX)
    }
    /// Executors the app installs itself go here; it is appended to the service PATH.
    pub fn tools_bin(&self) -> PathBuf { self.root.join("tools/bin") }
    pub fn state_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| self.home.join(".sbot"))
    }
    /// Outside the state directory on purpose: the first-run scan refuses a
    /// state directory that already holds files, and the app must not be the
    /// one to put them there.
    pub fn log_file(&self) -> PathBuf {
        if self.overridden {
            self.root.join("logs/daemon.log")
        } else {
            self.home.join("Library/Logs/SintBot/daemon.log")
        }
    }
    /// The app's own log (update checks and installations), next to the service log.
    pub fn app_log_file(&self) -> PathBuf { self.log_file().with_file_name("desktop.log") }
    pub fn launch_agents(&self) -> PathBuf { self.home.join("Library/LaunchAgents") }
    pub fn plist_path(&self) -> PathBuf { self.launch_agents().join(format!("{}.plist", self.label)) }
    /// Where the job file waits while the service is stopped from the app.
    /// launchd loads everything in LaunchAgents at login, so a job that must
    /// stay stopped cannot sit there.
    pub fn parked_plist(&self) -> PathBuf { self.root.join("launchd").join(format!("{}.plist", self.label)) }
    pub fn console_url(&self) -> String { format!("http://127.0.0.1:{}/", self.port) }
}

pub fn is_service_entry(path: &Path) -> bool {
    path.to_string_lossy().ends_with("/sintbot/bin/sintbot-v2.js")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_detection_matches_installer_and_release_layouts() {
        assert!(is_service_entry(Path::new("/Users/a/.local/share/sintbot/releases/1.4.18/lib/node_modules/sintbot/bin/sintbot-v2.js")));
        assert!(is_service_entry(Path::new("/Users/a/.local/share/sintbot/lib/node_modules/sintbot/bin/sintbot-v2.js")));
        assert!(!is_service_entry(Path::new("/opt/homebrew/bin/sbot")));
        assert!(!is_service_entry(Path::new("/x/notsintbot/bin/sintbot-v2.js.bak")));
    }
}
