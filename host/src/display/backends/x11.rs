//! Any X11 session's layout through RandR, behind [`LayoutBackend`]: Cinnamon,
//! XFCE, MATE, and KDE or GNOME when they run on X11 (their Wayland tools do
//! not apply there, RandR always does).
//!
//! [`super::super::randr`] is the tool-level code; this adapter only
//! translates. Reading and change events need nothing but the X server;
//! changing the layout needs the `xrandr` tool. Making the phone the primary
//! screen is not built on X11 yet, so `primary` stays off and that role is
//! refused with a reason (the `SetPrimary` op itself works; the watcher uses it).

use crate::display::backend::{LayoutBackend, LayoutEvents};
use crate::display::model::{Layout, LayoutCaps, LayoutOp};
use crate::display::randr;
use crate::display::{DisplayError, OutputRegion};

pub struct X11Layout;

/// What the backend can do, given whether RandR answers and `xrandr` exists.
pub fn caps_for(readable: bool, xrandr: bool) -> LayoutCaps {
    if !readable {
        return LayoutCaps::NONE;
    }
    LayoutCaps { query: true, events: true, apply: xrandr, set_mode: xrandr, mirror: xrandr, primary: false }
}

impl LayoutBackend for X11Layout {
    fn kind(&self) -> &'static str {
        "x11"
    }

    fn caps(&self) -> LayoutCaps {
        caps_for(true, randr::xrandr_available())
    }

    fn snapshot(&self) -> Result<Layout, DisplayError> {
        Ok(randr::layout_from_state(&randr::state()?))
    }

    fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError> {
        if !randr::xrandr_available() {
            return Err(DisplayError::Unsupported { what: "changing the monitor layout without xrandr".into() });
        }
        let (args, modes) = randr::plan_ops(&randr::state()?, ops)
            .map_err(|detail| DisplayError::Io { context: "change the X11 monitor layout".into(), detail })?;
        randr::apply(&args)?;
        for (name, mode) in modes {
            randr::set_output_mode(&name, mode)?;
        }
        Ok(())
    }

    fn events(&self) -> Option<Box<dyn LayoutEvents>> {
        match randr::RandrEvents::open() {
            Ok(ev) => Some(Box::new(ev)),
            Err(e) => {
                log::warn!("X11: cannot watch RandR changes ({})", e.summary());
                None
            }
        }
    }

    /// X maps an absolute input device over the whole root window, which can be
    /// larger than the monitors' bounding box, so that is the box input is
    /// scaled into.
    fn desktop_bounds(&self) -> Option<OutputRegion> {
        let (width, height) = randr::state().ok()?.screen;
        Some(OutputRegion { x: 0, y: 0, width, height })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_follow_randr_and_the_tool() {
        assert_eq!(caps_for(false, true), LayoutCaps::NONE);
        let read_only = caps_for(true, false);
        assert!(read_only.query && read_only.events && !read_only.apply && !read_only.set_mode);
        let full = caps_for(true, true);
        assert!(full.apply && full.mirror && full.set_mode);
        assert!(!full.primary, "phone as main screen is not built on X11");
    }
}
