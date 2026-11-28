//! A launchd job starts with a bare PATH, and so does an app opened from the
//! Finder. The executors (`codex`, `claude`) live wherever the user's shell
//! puts them, so the service PATH is taken from a login shell once and written
//! into the job.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BEGIN: &str = "__SINTBOT_PATH_BEGIN__";
const END: &str = "__SINTBOT_PATH_END__";

fn login_shell() -> PathBuf {
    std::env::var_os("SHELL").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

fn ask_shell(shell: &Path, timeout: Duration) -> Option<String> {
    let fish = shell.file_name().is_some_and(|n| n == "fish");
    let script = if fish {
        format!("printf '\\n{BEGIN}%s{END}\\n' (string join : $PATH)")
    } else {
        format!("printf '\\n{BEGIN}%s{END}\\n' \"$PATH\"")
    };
    let mut child = Command::new(shell).args(["-ilc", &script])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || { let mut text = String::new(); let _ = stdout.read_to_string(&mut text); text });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => { let _ = child.kill(); let _ = child.wait(); break }
        }
    }
    parse_marked(&reader.join().ok()?)
}

fn parse_marked(output: &str) -> Option<String> {
    let start = output.rfind(BEGIN)? + BEGIN.len();
    let end = output[start..].find(END)? + start;
    Some(output[start..end].to_string())
}

fn push_unique(list: &mut Vec<PathBuf>, dir: PathBuf) {
    if dir.is_absolute() && dir.is_dir() && !list.contains(&dir) { list.push(dir) }
}

/// Directories of the user's login-shell PATH that exist, followed by the
/// usual install locations in case the shell could not be asked.
pub fn login_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = ask_shell(&login_shell(), Duration::from_secs(8)) {
        for part in path.split(':') { push_unique(&mut dirs, PathBuf::from(part)) }
    }
    for fallback in [PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin"), home.join(".local/bin"),
        PathBuf::from("/usr/bin"), PathBuf::from("/bin"), PathBuf::from("/usr/sbin"), PathBuf::from("/sbin")] {
        push_unique(&mut dirs, fallback);
    }
    dirs
}

pub fn find_tool(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|dir| dir.join(name)).find(|candidate| {
        std::fs::metadata(candidate).is_ok_and(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        })
    })
}

/// The user's own tools come first; the bundled runtime is the fallback that
/// lets a Mac without Node.js run executors installed through npm.
pub fn service_path(dirs: &[PathBuf], extra: &[PathBuf]) -> String {
    let mut all: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().to_string()).collect();
    for dir in extra {
        let text = dir.to_string_lossy().to_string();
        if !all.contains(&text) { all.push(text) }
    }
    all.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_survives_shell_noise() {
        let noisy = format!("Last login: today\nwelcome\n{BEGIN}/a/bin:/b/bin{END}\nbye\n");
        assert_eq!(parse_marked(&noisy).as_deref(), Some("/a/bin:/b/bin"));
        assert_eq!(parse_marked("no markers"), None);
    }

    #[test]
    fn service_path_appends_extras_once() {
        let dirs = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
        assert_eq!(service_path(&dirs, &[PathBuf::from("/x/bin"), PathBuf::from("/bin")]), "/usr/bin:/bin:/x/bin");
    }
}
