//! KWin: bind a uinput touch or tablet device to one output.
//!
//! KWin does not spread an absolute device over the desktop's bounding box the
//! way Mutter does; it maps it onto one output (by default the built-in
//! panel). Which one is the writable `outputName` property of the device's
//! `org.kde.KWin.InputDevice` object at `/org/kde/KWin/InputDevice/<eventN>`.
//! With the device bound to the phone's output, normalised coordinates need
//! no further scaling.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};

const BUS_NAME: &str = "org.kde.KWin";
const INTERFACE: &str = "org.kde.KWin.InputDevice";

/// `UI_GET_SYSNAME(len)`: `_IOC(_IOC_READ, 'U', 44, len)`.
fn ui_get_sysname(len: usize) -> libc::c_ulong {
    const IOC_READ: libc::c_ulong = 2;
    (IOC_READ << 30) | ((len as libc::c_ulong) << 16) | ((b'U' as libc::c_ulong) << 8) | 44
}

/// The `eventN` node of the uinput device behind `file`.
pub fn event_node(file: &File) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), ui_get_sysname(buf.len()), buf.as_mut_ptr()) };
    if rc < 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let sysname = std::str::from_utf8(&buf[..end]).ok()?; // "inputNN"
    let dir = format!("/sys/devices/virtual/input/{sysname}");
    std::fs::read_dir(dir).ok()?.filter_map(Result::ok).find_map(|e| {
        let name = e.file_name().into_string().ok()?;
        name.starts_with("event").then_some(name)
    })
}

/// Sets `outputName` of the KWin input device `event` ("" unbinds it). KWin
/// registers a new device asynchronously, so this retries for up to a second.
pub fn bind_to_output(event: &str, output: &str) -> Result<(), String> {
    let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let path = format!("/org/kde/KWin/InputDevice/{event}");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let res = Proxy::new(&conn, BUS_NAME, path.as_str(), INTERFACE)
            .and_then(|p| p.set_property("outputName", output).map_err(zbus::Error::from));
        match res {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(format!("{path}: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// A normalised rectangle `(x, y, width, height)` of a device's surface.
pub type Area = (f64, f64, f64, f64);
pub const FULL_AREA: Area = (0.0, 0.0, 1.0, 1.0);

/// The part of a `phone_w` x `phone_h` surface to use so that it maps onto
/// `out_w` x `out_h` without stretching: the largest centred rectangle of the
/// output's aspect. Aspects within 2% count as equal (the whole surface).
pub fn keep_aspect_area(phone_w: u32, phone_h: u32, out_w: u32, out_h: u32) -> Area {
    if phone_w == 0 || phone_h == 0 || out_w == 0 || out_h == 0 {
        return FULL_AREA;
    }
    let ratio = (out_w as f64 / out_h as f64) / (phone_w as f64 / phone_h as f64);
    if (ratio - 1.0).abs() < 0.02 {
        FULL_AREA
    } else if ratio < 1.0 {
        // The output is narrower than the phone: use a central band.
        (0.5 - ratio / 2.0, 0.0, ratio, 1.0)
    } else {
        let h = 1.0 / ratio;
        (0.0, 0.5 - h / 2.0, 1.0, h)
    }
}

/// The part of an `out_w` x `out_h` output the whole `phone_w` x `phone_h`
/// surface should cover so nothing stretches: the largest centred rectangle
/// of the phone's aspect. The fallback for devices without `inputArea`.
pub fn letterbox_output_area(phone_w: u32, phone_h: u32, out_w: u32, out_h: u32) -> Area {
    if phone_w == 0 || phone_h == 0 || out_w == 0 || out_h == 0 {
        return FULL_AREA;
    }
    let ratio = (out_w as f64 / out_h as f64) / (phone_w as f64 / phone_h as f64);
    if (ratio - 1.0).abs() < 0.02 {
        FULL_AREA
    } else if ratio > 1.0 {
        (0.5 - 0.5 / ratio, 0.0, 1.0 / ratio, 1.0)
    } else {
        (0.0, 0.5 - ratio / 2.0, 1.0, ratio)
    }
}

/// Whether KWin lets the device's `inputArea` be set.
pub fn supports_input_area(event: &str) -> bool {
    let Ok(conn) = Connection::session() else { return false };
    let path = format!("/org/kde/KWin/InputDevice/{event}");
    Proxy::new(&conn, BUS_NAME, path.as_str(), INTERFACE)
        .and_then(|p| p.get_property::<bool>("supportsInputArea"))
        .unwrap_or(false)
}

/// Sets the device's `inputArea`.
pub fn set_input_area(event: &str, area: Area) -> Result<(), String> {
    set_area(event, "inputArea", area)
}

/// Sets the device's `outputArea` (the part of the output it covers).
pub fn set_output_area(event: &str, area: Area) -> Result<(), String> {
    set_area(event, "outputArea", area)
}

fn set_area(event: &str, property: &str, area: Area) -> Result<(), String> {
    use zbus::zvariant::{StructureBuilder, Value};
    let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let path = format!("/org/kde/KWin/InputDevice/{event}");
    let st = StructureBuilder::new().add_field(area.0).add_field(area.1).add_field(area.2).add_field(area.3).build().map_err(|e| e.to_string())?;
    Proxy::new(&conn, BUS_NAME, path.as_str(), INTERFACE)
        .and_then(|p| p.set_property(property, Value::Structure(st)).map_err(zbus::Error::from))
        .map_err(|e| format!("{path}: {e}"))
}

/// Reads `outputName` back (for checks and logs).
pub fn bound_output(event: &str) -> Result<String, String> {
    let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let path = format!("/org/kde/KWin/InputDevice/{event}");
    Proxy::new(&conn, BUS_NAME, path.as_str(), INTERFACE)
        .and_then(|p| p.get_property::<String>("outputName"))
        .map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysname_ioctl_number_matches_the_kernel_macro() {
        // UI_GET_SYSNAME(64) from linux/uinput.h: _IOC(_IOC_READ, 'U', 44, 64).
        assert_eq!(ui_get_sysname(64), 0x8040_552c);
    }

    #[test]
    fn matching_aspects_use_the_whole_surface() {
        assert_eq!(keep_aspect_area(1080, 2400, 1080, 2400), FULL_AREA);
        assert_eq!(keep_aspect_area(1600, 720, 1920, 864), FULL_AREA);
        assert_eq!(keep_aspect_area(0, 10, 10, 10), FULL_AREA);
    }

    #[test]
    fn a_narrower_output_takes_a_central_band_of_the_phone() {
        let (x, y, w, h) = keep_aspect_area(2000, 1000, 1000, 1000);
        assert!((x - 0.25).abs() < 1e-9 && y == 0.0 && (w - 0.5).abs() < 1e-9 && h == 1.0);
    }

    #[test]
    fn letterbox_covers_the_matching_part_of_the_output() {
        assert_eq!(letterbox_output_area(1080, 2400, 1080, 2400), FULL_AREA);
        // Portrait phone on a landscape monitor: a centred band of the output.
        let (x, y, w, h) = letterbox_output_area(1080, 2400, 1920, 1080);
        assert!(y == 0.0 && h == 1.0 && (x - (0.5 - w / 2.0)).abs() < 1e-9);
        assert!(((w * 1920.0) / 1080.0 - 1080.0 / 2400.0).abs() < 1e-9);
        // Landscape phone on a portrait monitor: a centred strip.
        let (_, y, w, h) = letterbox_output_area(2000, 1000, 1000, 1000);
        assert!(w == 1.0 && (h - 0.5).abs() < 1e-9 && (y - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_wider_output_takes_a_central_strip_of_the_phone() {
        // Portrait 1080x2400 phone onto a 1920x1080 monitor.
        let (x, y, w, h) = keep_aspect_area(1080, 2400, 1920, 1080);
        assert!(x == 0.0 && w == 1.0);
        assert!((h - (1080.0 / 2400.0) / (1920.0 / 1080.0)).abs() < 1e-9);
        assert!((y - (0.5 - h / 2.0)).abs() < 1e-9);
        // The strip has the output's aspect.
        assert!(((w * 1080.0) / (h * 2400.0) - 1920.0 / 1080.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod live_tests {
    /// `cargo test --release --lib live_kwin_device_classes -- --ignored --nocapture`:
    /// creates the uinput devices and prints how KWin classifies each.
    #[test]
    #[ignore]
    fn live_kwin_device_classes() {
        let inj = crate::input::linux::LinuxUinputInjector::new(1600, 720).expect("uinput");
        // KWin registers new devices asynchronously; 800 ms was sometimes short.
        std::thread::sleep(std::time::Duration::from_millis(2000));
        let conn = zbus::blocking::Connection::session().unwrap();
        for event in inj.digitizer_event_nodes() {
            let path = format!("/org/kde/KWin/InputDevice/{event}");
            let p = zbus::blocking::Proxy::new(&conn, super::BUS_NAME, path.as_str(), super::INTERFACE).unwrap();
            let name: String = p.get_property("name").unwrap();
            let tool: bool = p.get_property("tabletTool").unwrap();
            let touch: bool = p.get_property("touch").unwrap();
            let out: String = p.get_property("outputName").unwrap();
            println!("{event}: {name:?} tabletTool={tool} touch={touch} output={out:?}");
        }
    }

    /// `cargo test --lib live_kwin_tablet_binding -- --ignored --nocapture`:
    /// binds the pen to an output, sets a keep-aspect input area when KWin
    /// allows it, and reads everything back. Sends no pen events.
    #[test]
    #[ignore]
    fn live_kwin_tablet_binding() {
        use crate::display::{kscreen, OutputRegion};
        use crate::input::InputInjector;
        let state = kscreen::state().expect("kscreen-doctor");
        let out = state.outputs.iter().find(|o| o.enabled && o.region().is_some()).expect("an enabled output");
        let region: OutputRegion = out.region().unwrap();
        let mut inj = crate::input::linux::LinuxUinputInjector::new(1080, 2400).expect("uinput");
        std::thread::sleep(std::time::Duration::from_millis(800));
        inj.set_output_region(Some(region));
        let conn = zbus::blocking::Connection::session().unwrap();
        for event in inj.digitizer_event_nodes() {
            let path = format!("/org/kde/KWin/InputDevice/{event}");
            let p = zbus::blocking::Proxy::new(&conn, super::BUS_NAME, path.as_str(), super::INTERFACE).unwrap();
            let name: String = p.get_property("name").unwrap();
            let bound = super::bound_output(&event).unwrap();
            let supports = super::supports_input_area(&event);
            println!("{event} {name:?}: output={bound:?} (want {:?}) supportsInputArea={supports}", out.name);
            assert_eq!(bound, out.name);
            if supports {
                let want = super::keep_aspect_area(1080, 2400, region.width, region.height);
                let got: (f64, f64, f64, f64) = p.get_property("inputArea").unwrap();
                println!("  inputArea={got:?} (want {want:?})");
            } else if name.ends_with("Pen") {
                let want = super::letterbox_output_area(1080, 2400, region.width, region.height);
                let got: (f64, f64, f64, f64) = p.get_property("outputArea").unwrap();
                println!("  outputArea={got:?} (want {want:?})");
                assert!((got.0 - want.0).abs() < 1e-6 && (got.3 - want.3).abs() < 1e-6);
            }
        }
        inj.set_output_region(None);
        for event in inj.digitizer_event_nodes() {
            assert_eq!(super::bound_output(&event).unwrap(), "", "{event} unbound again");
        }
    }
}
