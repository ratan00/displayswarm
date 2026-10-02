//! "Start on login": enables the systemd user service when it is installed,
//! otherwise falls back to an XDG autostart entry.

use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const UNIT: &str = "displayswarm-hostd.service";

fn systemctl(args: &[&str]) -> Option<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .ok()
}

/// Whether systemd knows the user unit (installed by `dist/install.sh`).
pub fn unit_installed() -> bool {
    systemctl(&["cat", UNIT]).map(|o| o.status.success()).unwrap_or(false)
}

/// Starts the daemon through systemd; false when that is not possible.
pub fn systemd_start() -> bool {
    unit_installed() && systemctl(&["start", UNIT]).map(|o| o.status.success()).unwrap_or(false)
}

fn desktop_entry_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("displayswarm-hostd.desktop"))
}

/// The autostart entry's text for a daemon at `exe`.
pub fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=DisplaySwarm\nComment=Phone as a display and input device\n\
         Exec={exe}\nIcon=video-display\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

pub fn is_enabled() -> bool {
    if unit_installed() {
        if let Some(o) = systemctl(&["is-enabled", UNIT]) {
            if String::from_utf8_lossy(&o.stdout).trim() == "enabled" {
                return true;
            }
        }
    }
    desktop_entry_path().map(|p| p.exists()).unwrap_or(false)
}

/// Turns start-on-login on or off. `daemon_exe` is the path to record in the
/// fallback autostart entry.
pub fn set_enabled(enabled: bool, daemon_exe: &str) -> Result<(), String> {
    if unit_installed() {
        let verb = if enabled { "enable" } else { "disable" };
        let out = systemctl(&[verb, UNIT]).ok_or("could not run systemctl")?;
        if !out.status.success() {
            return Err(format!("systemctl {verb} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        // A leftover fallback entry would start a second copy.
        if let Some(p) = desktop_entry_path() {
            let _ = std::fs::remove_file(p);
        }
        return Ok(());
    }
    let path = desktop_entry_path().ok_or("no home directory")?;
    if enabled {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, desktop_entry(daemon_exe)).map_err(|e| e.to_string())
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_entry_names_the_daemon() {
        let e = super::desktop_entry("/usr/bin/displayswarm-hostd");
        assert!(e.contains("Exec=/usr/bin/displayswarm-hostd\n"));
        assert!(e.starts_with("[Desktop Entry]\n"));
    }
}
