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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// mDNS, for `_displayswarm._tcp` discovery.
pub const MDNS_PORT: u16 = 5353;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firewall {
    Ufw,
    Firewalld,
}

impl Firewall {
    pub fn name(self) -> &'static str {
        match self {
            Firewall::Ufw => "ufw",
            Firewall::Firewalld => "firewalld",
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
    }
}

/// What to paste in a terminal when `pkexec` is unavailable or refused.
pub fn manual_commands(fw: Firewall, port: u16) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_zero_yields_no_commands() {
        assert!(commands(Firewall::Ufw, 0).is_empty());
        assert!(commands(Firewall::Firewalld, 0).is_empty());
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
