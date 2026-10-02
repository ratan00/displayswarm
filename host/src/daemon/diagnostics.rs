//! Environment checks behind the UI's Diagnostics tab. Everything here blocks
//! (D-Bus round trips, opening a hardware encoder): call from a blocking task.

use crate::display::model::Desktop;
use crate::ipc::proto::{DiagItem, DiagStatus};

fn item(name: &str, status: DiagStatus, detail: impl Into<String>) -> DiagItem {
    DiagItem { name: name.into(), status, detail: detail.into() }
}

/// The desktop backend's short name, as shown in the UI.
pub fn backend_name() -> &'static str {
    if crate::display::env::current_session() == crate::display::model::SessionType::X11 {
        return "randr";
    }
    match crate::display::env::current_desktop() {
        Desktop::Kde => "kscreen",
        Desktop::Gnome => "mutter",
        _ => "none",
    }
}

/// Opens the default encoder once (720p) and returns the backend that came up,
/// e.g. `h264_vaapi` or `libx264`. Opening a hardware encoder takes a moment.
pub fn probe_encoder() -> String {
    crate::encoder::create_default_encoder(1280, 720, 30, 4000).backend_name()
}

/// Runs every check. `encoder` is the cached result of [`probe_encoder`].
pub fn run(encoder: Option<&str>, tray_mode: &str) -> Vec<DiagItem> {
    let mut out = Vec::new();

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    out.push(item(
        "Desktop backend",
        DiagStatus::Ok,
        format!("{} (desktop {:?}, session {:?})", backend_name(), desktop, session),
    ));

    out.push(portal_check());
    out.push(pipewire_check());

    out.push(match encoder {
        Some(e) if e == "none" => item("Video encoder", DiagStatus::Fail, "No H.264 encoder could be opened"),
        Some(e) if e.to_ascii_lowercase().contains("x264") => {
            item("Video encoder", DiagStatus::Warn, format!("{e} (software; no hardware encoder was usable)"))
        }
        Some(e) => item("Video encoder", DiagStatus::Ok, format!("{e} (hardware)")),
        None => item("Video encoder", DiagStatus::Warn, "still probing"),
    });
    out.push(vaapi_check());
    out.push(uinput_check());
    out.push(usb_check());

    out.push(match tray_mode {
        "tray" => item("Tray", DiagStatus::Ok, "StatusNotifierItem registered"),
        "notifications" => item(
            "Tray",
            DiagStatus::Warn,
            "No StatusNotifierHost (on GNOME install the AppIndicator extension); using notifications",
        ),
        _ => item("Tray", DiagStatus::Warn, "disabled"),
    });

    let unit = super::autostart::unit_installed();
    out.push(item(
        "systemd user service",
        if unit { DiagStatus::Ok } else { DiagStatus::Warn },
        if unit {
            format!("{} installed", super::autostart::UNIT)
        } else {
            format!("{} not installed (run host/dist/install.sh)", super::autostart::UNIT)
        },
    ));
    out.push(match crate::devices::DeviceStore::default_path() {
        Some(p) => item("Device store", DiagStatus::Ok, p.display().to_string()),
        None => item("Device store", DiagStatus::Warn, "no config directory; roles are not remembered"),
    });
    out
}

fn portal_check() -> DiagItem {
    let name = "Screen-capture portal";
    let conn = match zbus::blocking::Connection::session() {
        Ok(c) => c,
        Err(e) => return item(name, DiagStatus::Fail, format!("no session bus: {e}")),
    };
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    );
    match proxy.and_then(|p| p.get_property::<u32>("version")) {
        Ok(v) => item(name, DiagStatus::Ok, format!("org.freedesktop.portal.ScreenCast version {v}")),
        Err(e) => item(name, DiagStatus::Fail, format!("ScreenCast portal unavailable: {e}")),
    }
}

fn pipewire_check() -> DiagItem {
    let name = "PipeWire";
    let remote = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    let path = if remote.starts_with('/') {
        std::path::PathBuf::from(&remote)
    } else {
        std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default()).join(&remote)
    };
    if path.exists() {
        item(name, DiagStatus::Ok, format!("socket {}", path.display()))
    } else {
        item(name, DiagStatus::Fail, format!("no PipeWire socket at {}", path.display()))
    }
}

fn vaapi_check() -> DiagItem {
    let name = "VAAPI render node";
    let nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("renderD")).unwrap_or(false))
                .map(|p| p.display().to_string())
                .collect()
        })
        .unwrap_or_default();
    if nodes.is_empty() {
        return item(name, DiagStatus::Warn, "no /dev/dri/renderD* node; only software encoding is possible");
    }
    let usable: Vec<&String> = nodes
        .iter()
        .filter(|n| std::fs::OpenOptions::new().read(true).write(true).open(n).is_ok())
        .collect();
    if usable.is_empty() {
        item(name, DiagStatus::Warn, format!("{} exists but is not accessible (video/render group?)", nodes.join(", ")))
    } else {
        item(name, DiagStatus::Ok, usable.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
    }
}

fn uinput_check() -> DiagItem {
    let name = "uinput (pen, touch, keyboard)";
    match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => item(name, DiagStatus::Ok, "/dev/uinput is writable"),
        Err(e) => item(name, DiagStatus::Fail, format!("/dev/uinput: {e} (add a udev rule or join the input group)")),
    }
}

fn usb_check() -> DiagItem {
    use nusb::MaybeFuture;
    let name = "USB (AOA)";
    match nusb::list_devices().wait() {
        Ok(devs) => item(name, DiagStatus::Ok, format!("{} USB devices visible", devs.count())),
        Err(e) => item(name, DiagStatus::Fail, format!("cannot enumerate USB devices: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_always_produce_the_core_items() {
        // Outcomes depend on the machine; the set of checks does not.
        let items = run(Some("libx264"), "none");
        for want in ["Desktop backend", "Screen-capture portal", "PipeWire", "Video encoder", "Tray"] {
            assert!(items.iter().any(|i| i.name == want), "missing {want}");
        }
        let enc = items.iter().find(|i| i.name == "Video encoder").unwrap();
        assert_eq!(enc.status, DiagStatus::Warn);
    }
}
