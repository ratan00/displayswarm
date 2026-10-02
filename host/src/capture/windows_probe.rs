//! What this Windows session can capture and inject, decided from a handful of
//! facts the Win32 probe in [`crate::display::windows_caps`] gathers. The
//! decision itself is plain data in, [`Capabilities`] out, so it is unit-tested
//! on every platform.

use crate::display::model::{CaptureCaps, Capabilities, Desktop, LayoutCaps, Rung, SessionType};

/// Windows 10 1803: Windows.Graphics.Capture (`Direct3D11CaptureFramePool`).
pub const WGC_MIN_BUILD: u32 = 17134;
/// Windows 10 1809: `CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`.
pub const POINTER_INJECTION_MIN_BUILD: u32 = 17763;

/// Facts about the running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Windows build number (`dwBuildNumber`), 0 when it could not be read.
    pub build: u32,
    /// `GraphicsCaptureSession::IsSupported()`.
    pub wgc_supported: bool,
    /// A DXGI factory exists and enumerates at least one output.
    pub dxgi_output: bool,
    /// Monitors attached to the desktop.
    pub monitors: usize,
    /// The process runs in an interactive (non-zero) session: a service in
    /// session 0 can neither capture nor inject into the user's desktop.
    pub interactive_session: bool,
    /// Whether a display driver can add a monitor for the phone
    /// ([`crate::display::virtual_monitor::select`]): `Err` carries the reason
    /// Extend is refused.
    pub virtual_monitor: Result<(), String>,
}

/// The capabilities for `p`. A virtual monitor needs a display driver
/// (`p.virtual_monitor`) and something to capture it with; there is no layout
/// backend yet, so every layout bit stays false and Phone-primary is refused
/// with the usual reason.
pub fn capabilities(p: &Probe) -> Capabilities {
    let has_screen = p.interactive_session && p.monitors > 0;
    let wgc = has_screen && p.wgc_supported && p.build >= WGC_MIN_BUILD;
    let monitor = wgc || (has_screen && p.dxgi_output);
    let capture = CaptureCaps {
        portal: monitor,
        monitor,
        // DXGI duplication is monitors only; windows need WGC.
        window: wgc,
        virtual_: monitor && p.virtual_monitor.is_ok(),
    };
    let virtual_reason = match &p.virtual_monitor {
        Err(why) if monitor => Some(why.clone()),
        _ => None,
    };
    Capabilities {
        desktop: Desktop::Windows,
        session: SessionType::Windows,
        capture,
        layout: LayoutCaps::NONE,
        input_ok: p.interactive_session && p.build >= POINTER_INJECTION_MIN_BUILD,
        rung: if capture.virtual_ { Rung::Driver } else { Capabilities::rung_for(&capture) },
        virtual_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Role;

    fn win11() -> Probe {
        Probe {
            build: 22631,
            wgc_supported: true,
            dxgi_output: true,
            monitors: 1,
            interactive_session: true,
            virtual_monitor: Err(crate::display::virtual_monitor::NO_DRIVER_REASON.into()),
        }
    }

    #[test]
    fn a_normal_session_mirrors_and_injects_but_cannot_extend() {
        let c = capabilities(&win11());
        assert_eq!((c.desktop, c.session), (Desktop::Windows, SessionType::Windows));
        assert!(c.capture.monitor && c.capture.window && !c.capture.virtual_);
        assert_eq!(c.layout, LayoutCaps::NONE);
        assert!(c.input_ok);
        assert_eq!(c.rung, Rung::MirrorOnly);
        assert!(c.role_support(Role::Mirror).is_yes());
        assert!(c.role_support(Role::MirrorWindow).is_yes());
        assert!(c.role_support(Role::Tablet).is_yes());
        let extend = c.role_support(Role::Extend);
        assert!(!extend.is_yes(), "no virtual display driver installed");
        assert!(extend.reason().unwrap().contains("Virtual Display Driver"), "{extend:?}");
        assert!(!c.role_support(Role::PhonePrimary).is_yes());
    }

    #[test]
    fn with_a_usable_driver_extend_works_but_phone_primary_still_needs_a_layout_backend() {
        let c = capabilities(&Probe { virtual_monitor: Ok(()), ..win11() });
        assert!(c.capture.virtual_);
        assert_eq!(c.rung, Rung::Driver);
        assert!(c.role_support(Role::Extend).is_yes());
        let primary = c.role_support(Role::PhonePrimary);
        assert!(primary.reason().unwrap().contains("layout"), "{primary:?}");
    }

    #[test]
    fn an_unusable_driver_gives_its_own_reason() {
        let why = "Virtual Display Driver 1.0 (built 2024-10-27) is older than the builds DisplaySwarm supports.";
        let c = capabilities(&Probe { virtual_monitor: Err(why.into()), ..win11() });
        assert_eq!(c.role_support(Role::Extend).reason(), Some(why));
        // Without capture the capture reason wins, whatever the driver says.
        let svc = capabilities(&Probe { interactive_session: false, virtual_monitor: Ok(()), ..win11() });
        assert!(!svc.capture.virtual_);
        assert!(svc.role_support(Role::Extend).reason().unwrap().contains("screen-capture"));
    }

    #[test]
    fn without_wgc_dxgi_still_mirrors_but_not_a_window() {
        let c = capabilities(&Probe { wgc_supported: false, ..win11() });
        assert!(c.capture.monitor && !c.capture.window);
        assert!(c.role_support(Role::Mirror).is_yes());
        assert!(!c.role_support(Role::MirrorWindow).is_yes());
    }

    #[test]
    fn an_old_build_ignores_wgc_and_cannot_inject_pointers() {
        let c = capabilities(&Probe { build: 16299, ..win11() });
        assert!(c.capture.monitor && !c.capture.window, "falls back to DXGI");
        assert!(!c.input_ok);
        assert!(!c.role_support(Role::Tablet).is_yes());
        // 1803 has WGC but no synthetic pointers; 1809 has both.
        let b = capabilities(&Probe { build: WGC_MIN_BUILD, ..win11() });
        assert!(b.capture.window && !b.input_ok);
        assert!(capabilities(&Probe { build: POINTER_INJECTION_MIN_BUILD, ..win11() }).input_ok);
    }

    #[test]
    fn session_zero_and_headless_machines_offer_nothing() {
        let svc = capabilities(&Probe { interactive_session: false, ..win11() });
        assert_eq!(svc.rung, Rung::Unavailable);
        assert!(!svc.input_ok && !svc.role_support(Role::Mirror).is_yes());
        let headless = capabilities(&Probe { monitors: 0, ..win11() });
        assert_eq!(headless.rung, Rung::Unavailable);
        assert!(headless.role_support(Role::Mirror).reason().is_some());
    }
}
