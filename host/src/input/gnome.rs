//! GNOME: map the uinput pen and touchscreen onto one monitor.
//!
//! Mutter spreads an absolute uinput device over the desktop's bounding box
//! unless the device has an `output` setting. The schemas are relocatable, one
//! instance per `VID:PID` of the device:
//!
//! ```text
//! org.gnome.desktop.peripherals.tablet      /org/gnome/desktop/peripherals/tablets/056a:f0f0/        output
//! org.gnome.desktop.peripherals.touchscreen /org/gnome/desktop/peripherals/touchscreens/056a:0301/   output
//! ```
//!
//! `output` is `as` and holds the monitor's EDID triple `[vendor, product,
//! serial]` as `GetCurrentState` reports it. (Not the connector: the key has
//! no slot for it, and `['','','']` means "map automatically".) Mutter watches
//! the key and remaps the device when it changes.
//!
//! The keys are written with the `gsettings` CLI, which goes through dconf, and
//! reset (not set to empty) when the mapping ends so the user's dconf is left
//! as we found it. Anything that fails leaves the caller on its fallback of
//! scaling into the desktop's bounding box.

use std::process::Command;

use crate::display::mutter::Monitor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tablet,
    Touchscreen,
}

impl Kind {
    fn schema(self) -> &'static str {
        match self {
            Kind::Tablet => "org.gnome.desktop.peripherals.tablet",
            Kind::Touchscreen => "org.gnome.desktop.peripherals.touchscreen",
        }
    }

    fn path_dir(self) -> &'static str {
        match self {
            Kind::Tablet => "tablets",
            Kind::Touchscreen => "touchscreens",
        }
    }
}

/// A monitor's EDID identity, the value of the `output` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputId {
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

impl OutputId {
    pub fn of(m: &Monitor) -> Self {
        Self { vendor: m.vendor.clone(), product: m.product.clone(), serial: m.serial.clone() }
    }

    /// `['vendor', 'product', 'serial']` as a GVariant text literal.
    pub fn gvariant(&self) -> String {
        let q = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
        format!("[{}, {}, {}]", q(&self.vendor), q(&self.product), q(&self.serial))
    }

    /// All-empty is Mutter's "automatic" value, so it names no monitor.
    fn is_blank(&self) -> bool {
        self.vendor.is_empty() && self.product.is_empty() && self.serial.is_empty()
    }
}

/// `schema:path` as `gsettings` takes a relocatable schema. `vid`/`pid` are
/// lowercase hex like the kernel prints them.
pub fn schema_with_path(kind: Kind, vid: u16, pid: u16) -> String {
    format!("{}:/org/gnome/desktop/peripherals/{}/{vid:04x}:{pid:04x}/", kind.schema(), kind.path_dir())
}

/// `gsettings` arguments that map the device to `id`, or reset the mapping.
pub fn gsettings_args(kind: Kind, vid: u16, pid: u16, id: Option<&OutputId>) -> Vec<String> {
    let target = schema_with_path(kind, vid, pid);
    match id {
        Some(id) => vec!["set".into(), target, "output".into(), id.gvariant()],
        None => vec!["reset".into(), target, "output".into()],
    }
}

fn run(args: &[String], env: &[(&str, &str)]) -> Result<(), String> {
    let out = Command::new("gsettings")
        .args(args)
        .envs(env.iter().copied())
        .output()
        .map_err(|e| format!("gsettings: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Maps the device `vid:pid` onto the monitor `id`.
pub fn map_output(kind: Kind, vid: u16, pid: u16, id: &OutputId) -> Result<(), String> {
    if id.is_blank() {
        return Err("the monitor reports no EDID identity to map to".into());
    }
    run(&gsettings_args(kind, vid, pid, Some(id)), &[])
}

/// Removes the mapping (back to Mutter's automatic one).
pub fn clear_output(kind: Kind, vid: u16, pid: u16) -> Result<(), String> {
    run(&gsettings_args(kind, vid, pid, None), &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> OutputId {
        OutputId { vendor: "MetaVendor".into(), product: "Virtual remote monitor".into(), serial: "0x0000002".into() }
    }

    #[test]
    fn value_is_a_three_string_array() {
        assert_eq!(id().gvariant(), "['MetaVendor', 'Virtual remote monitor', '0x0000002']");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        let id = OutputId { vendor: "O'Neil".into(), product: r"a\b".into(), serial: String::new() };
        assert_eq!(id.gvariant(), r"['O\'Neil', 'a\\b', '']");
    }

    #[test]
    fn paths_use_lowercase_vid_pid() {
        assert_eq!(
            schema_with_path(Kind::Tablet, 0x056a, 0xf0f0),
            "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/056a:f0f0/"
        );
        assert_eq!(
            schema_with_path(Kind::Touchscreen, 0x056a, 0x0301),
            "org.gnome.desktop.peripherals.touchscreen:/org/gnome/desktop/peripherals/touchscreens/056a:0301/"
        );
    }

    #[test]
    fn set_and_reset_arguments() {
        let set = gsettings_args(Kind::Tablet, 0x056a, 0xf0f0, Some(&id()));
        assert_eq!(set[0], "set");
        assert_eq!(set[2], "output");
        assert_eq!(set[3], id().gvariant());
        let reset = gsettings_args(Kind::Touchscreen, 0x056a, 0x0301, None);
        assert_eq!(reset[0], "reset");
        assert_eq!(reset.len(), 3);
    }

    #[test]
    fn a_monitor_without_identity_is_refused() {
        let blank = OutputId { vendor: String::new(), product: String::new(), serial: String::new() };
        assert!(map_output(Kind::Tablet, 0x056a, 0xf0f0, &blank).is_err());
    }

    /// Runs the real `gsettings` against its in-memory backend (the user's
    /// dconf is untouched): the relocatable schema/path syntax and the value
    /// must be accepted. `cargo test --lib live_gsettings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_gsettings_accepts_the_keys() {
        let mem = [("GSETTINGS_BACKEND", "memory")];
        for (kind, pid) in [(Kind::Tablet, 0xf0f0), (Kind::Touchscreen, 0x0301)] {
            run(&gsettings_args(kind, 0x056a, pid, Some(&id())), &mem).expect("set");
            run(&gsettings_args(kind, 0x056a, pid, None), &mem).expect("reset");
        }
        println!("gsettings accepted set and reset for both schemas");
    }
}
