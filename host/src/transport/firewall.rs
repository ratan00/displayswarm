//! Opening the Wi-Fi port in the host firewall, through `pkexec`.
//!
//! A default-deny firewall (ufw, firewalld) silently drops phones' connections
//! to the listen port and the mDNS discovery packets, so a phone shows a plain
//! connect timeout. This detects those two firewalls and can add the rules.
//!
//! Same rules as `display::privilege`: no shell, `pkexec` gets an explicit argv
//! with the firewall tool by absolute path, and the only variable input is the
//! port, a non-zero `u16` printed by us. The tool set is closed: ufw and
//! firewall-cmd, `allow`/`add-port` only.
//!
//! On Windows the firewall is Windows Defender Firewall, which is always on.
//! One inbound allow rule for the daemon's executable covers the listen port,
//! mDNS and the per-session UDP audio ports, scoped to private and domain
//! networks. `netsh` runs elevated through the UAC prompt
//! (`ShellExecuteExW` with the `runas` verb), again with a fixed argument list
//! whose only variable part is our own executable's path.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// mDNS, for `_displayswarm._tcp` discovery.
pub const MDNS_PORT: u16 = 5353;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firewall {
    Ufw,
    Firewalld,
    /// Windows Defender Firewall.
    Windows,
}

impl Firewall {
    pub fn name(self) -> &'static str {
        match self {
            Firewall::Ufw => "ufw",
            Firewall::Firewalld => "firewalld",
            Firewall::Windows => "Windows Defender Firewall",
        }
    }
}

fn find_tool(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(Path::new).find(|p| p.is_file()).map(Path::to_path_buf)
}

fn ufw_path() -> Option<PathBuf> {
    find_tool(&["/usr/bin/ufw", "/usr/sbin/ufw", "/sbin/ufw"])
}

fn firewall_cmd_path() -> Option<PathBuf> {
    find_tool(&["/usr/bin/firewall-cmd", "/usr/sbin/firewall-cmd"])
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The active firewall that can block phones, if any. Needs no privileges.
pub fn detect() -> Option<Firewall> {
    if cfg!(windows) {
        return Some(Firewall::Windows);
    }
    if ufw_path().is_some() && unit_active("ufw") {
        Some(Firewall::Ufw)
    } else if firewall_cmd_path().is_some() && unit_active("firewalld") {
        Some(Firewall::Firewalld)
    } else {
        None
    }
}

/// The commands (each run through `pkexec`) that open `port`/tcp and mDNS.
/// Empty for port 0 or when the tool is missing.
pub fn commands(fw: Firewall, port: u16) -> Vec<Vec<OsString>> {
    if port == 0 {
        return Vec::new();
    }
    let tcp = format!("{port}/tcp");
    let mdns = format!("{MDNS_PORT}/udp");
    let argv = |tool: PathBuf, args: Vec<String>| {
        let mut v: Vec<OsString> = vec![tool.into()];
        v.extend(args.into_iter().map(OsString::from));
        v
    };
    match fw {
        Firewall::Ufw => match ufw_path() {
            Some(t) => vec![
                argv(t.clone(), vec!["allow".into(), tcp]),
                argv(t, vec!["allow".into(), mdns]),
            ],
            None => Vec::new(),
        },
        Firewall::Firewalld => match firewall_cmd_path() {
            Some(t) => vec![
                argv(t.clone(), vec!["--permanent".into(), format!("--add-port={tcp}"), format!("--add-port={mdns}")]),
                argv(t, vec!["--reload".into()]),
            ],
            None => Vec::new(),
        },
        Firewall::Windows => match std::env::current_exe() {
            Ok(exe) => vec![argv(netsh_path(), windows_rule_args(&exe.to_string_lossy()))],
            Err(_) => Vec::new(),
        },
    }
}

/// The name of the Windows firewall rule this adds.
pub const WINDOWS_RULE: &str = "DisplaySwarm";

fn netsh_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("netsh.exe")
}

/// `netsh` arguments for an inbound allow rule for `program` on private and
/// domain networks. A public network (a café's Wi-Fi) stays closed; on a home
/// network Windows has marked public, the user switches it to private.
pub fn windows_rule_args(program: &str) -> Vec<String> {
    vec![
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={WINDOWS_RULE}"),
        "dir=in".into(),
        "action=allow".into(),
        format!("program={program}"),
        "enable=yes".into(),
        "profile=private,domain".into(),
    ]
}

/// One command-line string for `ShellExecuteExW`, every argument quoted the way
/// `CommandLineToArgvW` reads it back (a path with spaces stays one argument).
pub fn windows_command_line(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') {
                // netsh reads `key="value with spaces"`.
                match a.split_once('=') {
                    Some((k, v)) => format!("{k}=\"{v}\""),
                    None => format!("\"{a}\""),
                }
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What to paste in a terminal when `pkexec` is unavailable or refused.
pub fn manual_commands(fw: Firewall, port: u16) -> String {
    if fw == Firewall::Windows {
        return commands(fw, port)
            .iter()
            .map(|c| {
                let args: Vec<String> = c.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
                format!("netsh {}  (in a terminal opened with \"Run as administrator\")", windows_command_line(&args))
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    commands(fw, port)
        .iter()
        .map(|c| format!("sudo {}", c.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Opens the port, showing polkit's dialog. Blocks until the user answers.
pub fn open_port(port: u16) -> Result<String, String> {
    let fw = detect().ok_or("No active firewall (ufw or firewalld) was found; nothing to open")?;
    let cmds = commands(fw, port);
    if cmds.is_empty() {
        return Err(format!("Cannot open port {port} in {}", fw.name()));
    }
    let manual = manual_commands(fw, port);
    #[cfg(windows)]
    if fw == Firewall::Windows {
        return elevated::open(&cmds[0], &manual);
    }
    for cmd in &cmds {
        let out = Command::new("pkexec")
            .args(cmd)
            .output()
            .map_err(|e| format!("Could not run pkexec ({e}). Run this yourself:\n{manual}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "{} was not changed ({}). Run this yourself:\n{manual}",
                fw.name(),
                err.trim().lines().last().unwrap_or("cancelled or refused")
            ));
        }
    }
    log::info!("firewall: opened {port}/tcp and {MDNS_PORT}/udp in {}", fw.name());
    Ok(format!("Opened port {port} (and mDNS) in {}", fw.name()))
}

#[cfg(windows)]
mod elevated {
    use super::*;
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use ::windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use ::windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use ::windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Whether our rule is already there (no elevation needed to look).
    fn rule_exists() -> bool {
        Command::new(netsh_path())
            .args(["advfirewall", "firewall", "show", "rule", &format!("name={WINDOWS_RULE}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Runs `argv` (netsh and its arguments) elevated, waiting for it.
    pub fn open(argv: &[OsString], manual: &str) -> Result<String, String> {
        if rule_exists() {
            return Ok(format!("{} already allows DisplaySwarm", Firewall::Windows.name()));
        }
        let file = wide(&argv[0].to_string_lossy());
        let args: Vec<String> = argv[1..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let params = wide(&windows_command_line(&args));
        let verb = wide("runas");
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
            // ERROR_CANCELLED: the user said no to the UAC prompt.
            return Err(if e.code().0 as u32 & 0xFFFF == 1223 {
                format!("The firewall was not changed (the administrator prompt was declined). Or run this yourself:\n{manual}")
            } else {
                format!("Could not run netsh ({e}). Run this yourself:\n{manual}")
            });
        }
        let process = info.hProcess;
        if process.is_invalid() {
            return Err(format!("netsh did not start. Run this yourself:\n{manual}"));
        }
        let mut code = 1u32;
        unsafe {
            if WaitForSingleObject(process, INFINITE) == WAIT_OBJECT_0 {
                let _ = GetExitCodeProcess(process, &mut code);
            }
            let _ = CloseHandle(process);
        }
        if code != 0 {
            return Err(format!("netsh failed (exit code {code}). Run this yourself:\n{manual}"));
        }
        log::info!("firewall: added the inbound rule {WINDOWS_RULE:?} to {}", Firewall::Windows.name());
        Ok(format!("{} now allows DisplaySwarm on private networks", Firewall::Windows.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_zero_yields_no_commands() {
        assert!(commands(Firewall::Ufw, 0).is_empty());
        assert!(commands(Firewall::Firewalld, 0).is_empty());
    }

    #[test]
    fn windows_rule_is_one_program_rule_on_private_networks() {
        let args = windows_rule_args(r"C:\Program Files\DisplaySwarm\displayswarm-hostd.exe");
        assert_eq!(&args[..4], ["advfirewall", "firewall", "add", "rule"]);
        assert!(args.contains(&"profile=private,domain".to_string()), "never public networks");
        assert!(args.contains(&"action=allow".to_string()) && args.contains(&"dir=in".to_string()));
        let line = windows_command_line(&args);
        assert!(line.contains(r#"program="C:\Program Files\DisplaySwarm\displayswarm-hostd.exe""#), "{line}");
        assert!(line.starts_with("advfirewall firewall add rule name=DisplaySwarm "), "{line}");
    }

    #[test]
    fn ufw_commands_are_fixed_argv() {
        for c in commands(Firewall::Ufw, 9999) {
            let s: Vec<String> = c.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            assert!(s[0].ends_with("/ufw"));
            assert_eq!(s[1], "allow");
            assert!(s[2] == "9999/tcp" || s[2] == "5353/udp");
            assert_eq!(s.len(), 3);
        }
    }
}
