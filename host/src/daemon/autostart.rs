//! "Start on login": enables the systemd user service when it is installed,
//! otherwise falls back to an XDG autostart entry. On Windows it is a value
//! under the user's `Run` key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`),
//! which is what Settings > Apps > Startup lists.

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
    if cfg!(windows) {
        return false;
    }
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
    #[cfg(windows)]
    return run_key::get().is_some();
    #[allow(unreachable_code)]
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
    #[cfg(windows)]
    return if enabled { run_key::set(&run_key::command_line(daemon_exe)) } else { run_key::remove() };
    #[allow(unreachable_code)]
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

/// The per-user `Run` key value that starts the daemon at sign-in.
#[cfg(any(windows, test))]
mod run_key {
    /// Value name under the `Run` key.
    #[cfg(windows)]
    pub const VALUE: &str = "DisplaySwarm";

    /// The command line stored: the quoted executable path (paths with spaces
    /// are common under `Program Files`), and no console.
    pub fn command_line(exe: &str) -> String {
        format!("\"{}\"", exe.trim_matches('"'))
    }

    #[cfg(windows)]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[cfg(windows)]
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    #[cfg(windows)]
    pub fn get() -> Option<String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
        let (key, value) = (wide(RUN), wide(VALUE));
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if r.is_err() {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        Some(s.trim_end_matches('\0').to_string())
    }

    #[cfg(windows)]
    pub fn set(command: &str) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
        let (key, value, data) = (wide(RUN), wide(VALUE), wide(command));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        }
        .ok()
        .map_err(|e| format!("could not write the Run key: {e}"))
    }

    #[cfg(windows)]
    pub fn remove() -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
        use windows::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};
        let (key, value) = (wide(RUN), wide(VALUE));
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(value.as_ptr())) };
        if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("could not remove the Run key value: {}", r.to_hresult()))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_key_command_is_quoted_once() {
        use super::run_key::command_line;
        assert_eq!(
            command_line(r"C:\Program Files\DisplaySwarm\displayswarm-hostd.exe"),
            r#""C:\Program Files\DisplaySwarm\displayswarm-hostd.exe""#
        );
        assert_eq!(command_line(r#""C:\x.exe""#), r#""C:\x.exe""#);
    }

    #[test]
    fn desktop_entry_names_the_daemon() {
        let e = super::desktop_entry("/usr/bin/displayswarm-hostd");
        assert!(e.contains("Exec=/usr/bin/displayswarm-hostd\n"));
        assert!(e.starts_with("[Desktop Entry]\n"));
    }
}
