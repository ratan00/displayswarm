//! The Windows monitor layout: read through `EnumDisplayDevicesW` /
//! `EnumDisplaySettingsExW`, changed through `ChangeDisplaySettingsExW`.
//!
//! What it is used for:
//! * the Tablet role maps the pen onto the primary monitor
//!   ([`LayoutBackend::primary_region`]); without a backend it spanned the whole
//!   virtual desktop, which on two monitors put the pen on the wrong screen;
//! * Phone-primary makes the phone's virtual monitor the primary one
//!   ([`make_primary`]), and the guard puts the old primary back.
//!
//! Windows requires the primary monitor at the desktop origin (0,0), so making
//! another one primary moves every monitor by the same offset
//! ([`plan_primary`]); the relative arrangement does not change. The planning
//! is plain data and unit-tested on every platform.

use super::model::{Layout, LayoutCaps, Output};
#[cfg(windows)]
use super::model::LayoutOp;
use super::OutputRegion;

/// The new top-left of every monitor when `name` becomes primary: everything
/// shifted so `name` sits at (0,0). `None` if `name` is not an enabled output.
pub fn plan_primary(layout: &Layout, name: &str) -> Option<Vec<(String, i32, i32)>> {
    let target = layout.output(name).filter(|o| o.enabled)?.region?;
    Some(
        layout
            .outputs
            .iter()
            .filter(|o| o.enabled)
            .filter_map(|o| o.region.map(|r| (o.name.clone(), r.x - target.x, r.y - target.y)))
            .collect(),
    )
}

/// The enabled output whose rectangle is `region` (the phone's monitor as the
/// capture reported it).
pub fn output_at(layout: &Layout, region: OutputRegion) -> Option<String> {
    layout.outputs.iter().find(|o| o.enabled && o.region == Some(region)).map(|o| o.name.clone())
}

/// What this backend can do: read, set the primary, move monitors. Mode
/// changes and mirroring stay with the display driver's own tools.
pub const CAPS: LayoutCaps =
    LayoutCaps { query: true, events: false, apply: true, set_mode: false, mirror: false, primary: true };

#[cfg(windows)]
pub use win::{make_primary, WindowsLayout, WindowsPrimaryGuard};

#[cfg(windows)]
mod win {
    use super::*;
    use crate::display::backend::{LayoutBackend, LayoutEvents};
    use crate::display::DisplayError;
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, CDS_NORESET, CDS_SET_PRIMARY,
        CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISP_CHANGE_SUCCESSFUL, DM_POSITION,
        ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
    };

    const ATTACHED_TO_DESKTOP: u32 = 0x1;
    const PRIMARY_DEVICE: u32 = 0x4;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn text(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn current_mode(name: &str) -> Option<DEVMODEW> {
        let w = wide(name);
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        unsafe { EnumDisplaySettingsExW(PCWSTR(w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm, ENUM_DISPLAY_SETTINGS_FLAGS(0)) }
            .as_bool()
            .then_some(dm)
    }

    /// The layout as Windows reports it now.
    pub fn read() -> Layout {
        let mut outputs = Vec::new();
        for i in 0.. {
            let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
                break;
            }
            let name = text(&dd.DeviceName);
            let enabled = dd.StateFlags & ATTACHED_TO_DESKTOP != 0;
            let (region, mode) = match current_mode(&name).filter(|_| enabled) {
                Some(dm) => {
                    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
                    let region = OutputRegion { x: pos.x, y: pos.y, width: dm.dmPelsWidth, height: dm.dmPelsHeight };
                    let mode = super::super::model::Mode {
                        width: dm.dmPelsWidth,
                        height: dm.dmPelsHeight,
                        refresh_mhz: dm.dmDisplayFrequency * 1000,
                    };
                    (Some(region), Some(mode))
                }
                None => (None, None),
            };
            if !enabled && region.is_none() && dd.StateFlags == 0 {
                continue; // an adapter slot with nothing behind it
            }
            outputs.push(Output {
                name,
                region,
                enabled,
                mirror_of: None,
                primary: dd.StateFlags & PRIMARY_DEVICE != 0,
                builtin: false,
                mode,
            });
        }
        Layout { outputs }
    }

    /// Moves the monitors to `positions`, making `primary` the primary one, as
    /// one change.
    fn set_positions(positions: &[(String, i32, i32)], primary: &str) -> Result<(), DisplayError> {
        for (name, x, y) in positions {
            let mut dm = current_mode(name)
                .ok_or_else(|| DisplayError::Io { context: format!("read {name}"), detail: "no current mode".into() })?;
            dm.dmFields = DM_POSITION;
            dm.Anonymous1.Anonymous2.dmPosition = ::windows::Win32::Foundation::POINTL { x: *x, y: *y };
            let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
            if name == primary {
                flags |= CDS_SET_PRIMARY;
            }
            let w = wide(name);
            let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(w.as_ptr()), Some(&dm), None, flags, None) };
            if r != DISP_CHANGE_SUCCESSFUL {
                return Err(DisplayError::Io { context: format!("move {name}"), detail: format!("ChangeDisplaySettingsEx returned {}", r.0) });
            }
        }
        // Apply everything staged above at once.
        let r = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
        if r != DISP_CHANGE_SUCCESSFUL {
            return Err(DisplayError::Io { context: "apply the monitor layout".into(), detail: format!("ChangeDisplaySettingsEx returned {}", r.0) });
        }
        Ok(())
    }

    pub struct WindowsLayout;

    impl LayoutBackend for WindowsLayout {
        fn kind(&self) -> &'static str {
            "windows"
        }
        fn caps(&self) -> LayoutCaps {
            CAPS
        }
        fn snapshot(&self) -> Result<Layout, DisplayError> {
            Ok(read())
        }
        fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError> {
            let layout = read();
            for op in ops {
                match op {
                    LayoutOp::SetPrimary { name } => {
                        let plan = plan_primary(&layout, name)
                            .ok_or_else(|| DisplayError::Io { context: "set the primary monitor".into(), detail: format!("{name} is not on the desktop") })?;
                        set_positions(&plan, name)?;
                    }
                    LayoutOp::Move { name, x, y } => {
                        let primary = layout.outputs.iter().find(|o| o.primary).map(|o| o.name.clone()).unwrap_or_default();
                        set_positions(&[(name.clone(), *x, *y)], &primary)?;
                    }
                    LayoutOp::Enable { .. } | LayoutOp::MirrorNone { .. } => {}
                    other => {
                        return Err(DisplayError::Unsupported { what: format!("{other:?} on Windows") });
                    }
                }
            }
            Ok(())
        }
        fn events(&self) -> Option<Box<dyn LayoutEvents>> {
            None
        }
    }

    /// Puts the previous primary monitor back (and every monitor where it was).
    pub struct WindowsPrimaryGuard {
        previous: Vec<(String, i32, i32)>,
        previous_primary: String,
        region: OutputRegion,
    }

    impl WindowsPrimaryGuard {
        pub fn region(&self) -> OutputRegion {
            self.region
        }
    }

    impl Drop for WindowsPrimaryGuard {
        fn drop(&mut self) {
            // Monitors that are gone (the phone's, removed first) are skipped.
            let now = read();
            let still: Vec<_> = self
                .previous
                .iter()
                .filter(|(n, _, _)| now.output(n).is_some_and(|o| o.enabled))
                .cloned()
                .collect();
            if let Err(e) = set_positions(&still, &self.previous_primary) {
                log::warn!("Could not put the previous primary monitor back: {}", e.summary());
            }
        }
    }

    /// Makes the monitor at `region` primary. `panel_off` has no Windows
    /// equivalent here and is ignored with a note.
    pub fn make_primary(region: OutputRegion, panel_off: bool) -> Result<WindowsPrimaryGuard, DisplayError> {
        if panel_off {
            log::info!("Turning the PC's own screen off is not supported on Windows; it stays on");
        }
        let layout = read();
        let name = output_at(&layout, region)
            .ok_or_else(|| DisplayError::Io { context: "find the phone's monitor".into(), detail: format!("no monitor at {region:?}") })?;
        let previous_primary = layout.outputs.iter().find(|o| o.primary).map(|o| o.name.clone()).unwrap_or_default();
        let previous: Vec<_> =
            layout.outputs.iter().filter(|o| o.enabled).filter_map(|o| o.region.map(|r| (o.name.clone(), r.x, r.y))).collect();
        let plan = plan_primary(&layout, &name).expect("the output was just found");
        set_positions(&plan, &name)?;
        log::info!("{name} is now the primary monitor (was {previous_primary})");
        Ok(WindowsPrimaryGuard {
            previous,
            previous_primary,
            region: OutputRegion { x: 0, y: 0, ..region },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, x: i32, y: i32, w: u32, h: u32, primary: bool) -> Output {
        Output {
            name: name.into(),
            region: Some(OutputRegion { x, y, width: w, height: h }),
            enabled: true,
            mirror_of: None,
            primary,
            builtin: false,
            mode: None,
        }
    }

    #[test]
    fn making_a_monitor_primary_shifts_everything_to_put_it_at_the_origin() {
        let layout = Layout {
            outputs: vec![out(r"\\.\DISPLAY1", 0, 0, 1920, 1080, true), out(r"\\.\DISPLAY2", 1920, 200, 2400, 1080, false)],
        };
        let plan = plan_primary(&layout, r"\\.\DISPLAY2").unwrap();
        assert_eq!(plan, vec![(r"\\.\DISPLAY1".to_string(), -1920, -200), (r"\\.\DISPLAY2".to_string(), 0, 0)]);
        assert!(plan_primary(&layout, r"\\.\DISPLAY9").is_none());
        assert_eq!(output_at(&layout, OutputRegion { x: 1920, y: 200, width: 2400, height: 1080 }).as_deref(), Some(r"\\.\DISPLAY2"));
        assert_eq!(output_at(&layout, OutputRegion { x: 0, y: 0, width: 1, height: 1 }), None);
    }

    #[test]
    fn a_disabled_monitor_is_neither_moved_nor_chosen() {
        let mut off = out(r"\\.\DISPLAY3", 0, 0, 800, 600, false);
        off.enabled = false;
        off.region = None;
        let layout = Layout { outputs: vec![out(r"\\.\DISPLAY1", 0, 0, 1920, 1080, true), off] };
        assert_eq!(plan_primary(&layout, r"\\.\DISPLAY1").unwrap().len(), 1);
        assert!(plan_primary(&layout, r"\\.\DISPLAY3").is_none());
    }
}

#[cfg(all(test, windows))]
mod live_tests {
    #[test]
    #[ignore = "needs a desktop session"]
    fn reads_the_monitors_and_one_primary() {
        let layout = super::win_read_for_tests();
        eprintln!("{layout:?}");
        let enabled: Vec<_> = layout.outputs.iter().filter(|o| o.enabled).collect();
        assert!(!enabled.is_empty());
        assert_eq!(enabled.iter().filter(|o| o.primary).count(), 1);
        let primary = enabled.iter().find(|o| o.primary).unwrap();
        assert_eq!((primary.region.unwrap().x, primary.region.unwrap().y), (0, 0), "Windows keeps the primary at the origin");
    }
}

#[cfg(all(test, windows))]
fn win_read_for_tests() -> Layout {
    win::read()
}
