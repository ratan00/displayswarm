//! Windows USB driver for the Android accessory interface.
//!
//! # Why a driver is needed
//!
//! After the AOA switch the phone re-enumerates as `18D1:2D00` (accessory) or
//! `18D1:2D01` (accessory + adb). The accessory interface is vendor class 0xFF
//! and the phone's gadget reports no MS OS descriptors for it (only its adb
//! function does), so Windows matches no driver and `nusb` has nothing to open.
//! It must be bound to WinUSB.
//!
//! # Options considered
//!
//! * Microsoft OS compatible ID (`USB\MS_COMP_WINUSB`): would need the phone's
//!   kernel to report it for `f_accessory`; it does not and we cannot change it.
//! * An unsigned INF through `UpdateDriverForPlugAndPlayDevices`/`pnputil`:
//!   64-bit Windows 10/11 rejects third-party INFs without a signed catalog
//!   (even when they only `Include=winusb.inf`) unless test signing is on, which
//!   we must not ask users to do.
//! * Zadig/libwdi: same mechanism as the chosen one, but interactive and manual.
//! * **Chosen**: an INF that includes the inbox `winusb.inf`
//!   (`packaging/windows/usb-driver/displayswarm-usb.inf`), a catalog generated
//!   at install time with `New-FileCatalog`, signed with a freshly generated
//!   self-signed code-signing certificate whose private key is non-exportable and
//!   deleted right after signing, and that certificate (public part only) added
//!   to the machine's Root and TrustedPublisher stores so Windows accepts the
//!   catalog. One elevated PowerShell run, one UAC prompt, no SDK/WDK needed.
//!   `SetupVerifyInfFile` on the generated catalog reports the INF/catalog match
//!   and fails only with `CERT_E_UNTRUSTEDROOT` before the certificate is trusted.
//!
//! # Security scope
//!
//! Trusting a self-signed root is sensitive, so: the certificate is named
//! `DisplaySwarm USB driver (...key destroyed after signing)`, is code-signing
//! only, and its key never leaves the install run, so nobody (including us) can
//! sign anything else with it. The package contains no binaries; the kernel
//! driver is Microsoft's `winusb.sys`. Only `VID_18D1&PID_2D00` and
//! `VID_18D1&PID_2D01&MI_00` are matched; on 2D01 the adb interface keeps its
//! own driver. `--uninstall-usb-driver` removes the package and the certificate.

/// Command-line flag that installs the driver (elevated).
pub const INSTALL_FLAG: &str = "--install-usb-driver";
/// Command-line flag that removes the driver and its certificate (elevated).
pub const UNINSTALL_FLAG: &str = "--uninstall-usb-driver";

/// One-line hint shown when an accessory phone has no usable driver.
pub const DRIVER_HINT: &str =
    "Windows has no driver for the phone's accessory interface; run `displayswarm-hostd --install-usb-driver` once";

/// Handles the driver CLI flags. `Some(exit code)` if `arg` was one of them.
pub fn handle_flag(arg: &str) -> Option<i32> {
    let uninstall = match arg {
        INSTALL_FLAG => false,
        UNINSTALL_FLAG => true,
        _ => return None,
    };
    #[cfg(windows)]
    {
        Some(match run_elevated(uninstall) {
            Ok(log) => {
                println!("{log}");
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        })
    }
    #[cfg(not(windows))]
    {
        let _ = uninstall;
        eprintln!("{arg} is only needed on Windows");
        Some(1)
    }
}

#[cfg(windows)]
const INF: &str = include_str!("../../../packaging/windows/usb-driver/displayswarm-usb.inf");
#[cfg(windows)]
const SCRIPT: &str = include_str!("../../../packaging/windows/usb-driver/install.ps1");

/// Extracts the package to a temporary directory and runs `install.ps1` in an
/// elevated Windows PowerShell (one UAC prompt). Returns the script's log.
#[cfg(windows)]
fn run_elevated(uninstall: bool) -> Result<String, String> {
    use std::process::Command;

    let dir = std::env::temp_dir().join(format!("displayswarm-usb-driver-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"));
    write("displayswarm-usb.inf", INF)?;
    write("install.ps1", SCRIPT)?;
    let log = dir.join("install.log");

    // Paths come from the temp dir (no quotes or newlines), so single quotes suffice.
    let quote = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
    let inner = format!(
        "-NoProfile -ExecutionPolicy Bypass -File {} -Dir {} -LogFile {}{}",
        quote(&dir.join("install.ps1")),
        quote(&dir),
        quote(&log),
        if uninstall { " -Uninstall" } else { "" }
    );
    let outer = format!(
        "$p = Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList \"{}\"; exit $p.ExitCode",
        inner.replace('"', "`\"")
    );
    println!("Asking for administrator rights to {} the USB driver...", if uninstall { "remove" } else { "install" });
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &outer])
        .status()
        .map_err(|e| format!("cannot start PowerShell: {e}"))?;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    if status.success() {
        Ok(format!("{}\nDone. Unplug and replug the phone if it does not connect.", text.trim()))
    } else {
        Err(format!("USB driver {} failed ({status}). Log:\n{}", if uninstall { "removal" } else { "installation" }, text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_flags_are_ignored() {
        assert_eq!(handle_flag("--no-tray"), None);
    }

    #[cfg(windows)]
    #[test]
    fn packaged_inf_matches_accessory_ids() {
        assert!(INF.contains("USB\\VID_18D1&PID_2D00"));
        assert!(INF.contains("USB\\VID_18D1&PID_2D01&MI_00"));
        assert!(!INF.contains("2D01&MI_01"), "adb interface must keep its own driver");
        assert!(SCRIPT.contains("-DeleteKey"));
    }
}
