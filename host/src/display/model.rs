//! The desktop-neutral vocabulary of the display layer: what a session can do
//! ([`Capabilities`], [`role_support`](Capabilities::role_support)) and what the
//! monitor layout looks like ([`Layout`], [`LayoutOp`]).
//!
//! Everything here is plain data with no I/O, so the same policy code (the
//! reconcile planner, the role gating) runs and is tested the same way on every
//! desktop. Detection lives in [`super::env`], the desktop-specific adapters in
//! `super::backends`.

use super::OutputRegion;
use crate::devices::Role;

/// Which desktop environment the session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Kde,
    Gnome,
    Cosmic,
    /// Any other desktop on an X11 session (Cinnamon, XFCE, MATE, ...).
    X11Generic,
    Windows,
    Unknown,
}

impl Desktop {
    pub fn label(self) -> &'static str {
        match self {
            Desktop::Kde => "KDE Plasma",
            Desktop::Gnome => "GNOME",
            Desktop::Cosmic => "COSMIC",
            Desktop::X11Generic => "This X11 desktop",
            Desktop::Windows => "Windows",
            Desktop::Unknown => "This desktop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Wayland,
    X11,
    Windows,
    Unknown,
}

/// Whether something can be used right now; a refusal always says why, in words
/// fit for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Yes,
    No(String),
}

impl Availability {
    pub fn is_yes(&self) -> bool {
        matches!(self, Availability::Yes)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Availability::Yes => None,
            Availability::No(why) => Some(why),
        }
    }

    fn no(why: &str) -> Self {
        Availability::No(why.to_string())
    }
}

/// What the screen-capture side can hand us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureCaps {
    /// A screen-capture service (the ScreenCast portal, WGC, ...) is present.
    pub portal: bool,
    /// Some capturer can show a host monitor: the portal's, or on an X11 session
    /// the direct screen read.
    pub monitor: bool,
    pub window: bool,
    /// The capture service can create a virtual monitor (portal source type 4).
    pub virtual_: bool,
}

/// What the layout backend can do. All false means "we cannot read or change
/// the layout on this desktop".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutCaps {
    pub query: bool,
    pub events: bool,
    pub apply: bool,
    pub set_mode: bool,
    pub mirror: bool,
    pub primary: bool,
}

impl LayoutCaps {
    pub const NONE: LayoutCaps =
        LayoutCaps { query: false, events: false, apply: false, set_mode: false, mirror: false, primary: false };
    pub const ALL: LayoutCaps =
        LayoutCaps { query: true, events: true, apply: true, set_mode: true, mirror: true, primary: true };
}

/// Which way of getting a screen for the phone is in use (the fallback chain).
/// Shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// The capture service creates the virtual monitor.
    PortalVirtual,
    /// A compositor-specific virtual monitor.
    Compositor,
    /// A privileged dummy output (vkms, RandR).
    Dummy,
    /// An indirect display driver adds the monitor (Windows).
    Driver,
    /// No virtual monitor: the phone can only mirror an existing one.
    MirrorOnly,
    /// Not even capture is available.
    Unavailable,
}

impl Rung {
    pub fn label(self) -> &'static str {
        match self {
            Rung::PortalVirtual => "virtual monitor from the screen-share portal",
            Rung::Compositor => "virtual monitor from the compositor",
            Rung::Dummy => "dummy output",
            Rung::Driver => "virtual monitor from a display driver",
            Rung::MirrorOnly => "mirror only",
            Rung::Unavailable => "no screen capture",
        }
    }
}

/// Everything the host knows about what this session supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub desktop: Desktop,
    pub session: SessionType,
    pub capture: CaptureCaps,
    pub layout: LayoutCaps,
    /// Pen, touch and keyboard injection is possible.
    pub input_ok: bool,
    pub rung: Rung,
    /// Why no virtual monitor can be made, when the platform knows better than
    /// the generic sentence (Windows: which display driver is missing or out of
    /// its supported version range). Only read while `capture.virtual_` is false.
    pub virtual_reason: Option<String>,
}

impl Capabilities {
    /// Nothing supported; the starting point for platforms with no detection.
    pub fn unsupported(desktop: Desktop, session: SessionType) -> Self {
        Capabilities {
            desktop,
            session,
            capture: CaptureCaps::default(),
            layout: LayoutCaps::NONE,
            input_ok: false,
            rung: Rung::Unavailable,
            virtual_reason: None,
        }
    }

    /// The rung implied by the capture bits.
    pub fn rung_for(capture: &CaptureCaps) -> Rung {
        if capture.virtual_ {
            Rung::PortalVirtual
        } else if capture.monitor || capture.window {
            Rung::MirrorOnly
        } else {
            Rung::Unavailable
        }
    }

    /// Whether `role` can work in this session, and if not, why.
    pub fn role_support(&self, role: Role) -> Availability {
        let no_capture = || Availability::no("No screen-capture service is available in this session.");
        match role {
            Role::Mirror => {
                if self.capture.monitor {
                    Availability::Yes
                } else if !self.capture.portal {
                    no_capture()
                } else {
                    Availability::no("The screen-capture service cannot share a monitor here.")
                }
            }
            Role::Extend => self.extend_support(),
            Role::PhonePrimary => match self.extend_support() {
                Availability::Yes if !self.layout.apply => Availability::No(format!(
                    "Making the phone the main screen needs to change the monitor layout, which {} does not allow yet.",
                    self.desktop.label()
                )),
                Availability::Yes if !self.layout.primary => {
                    Availability::No(format!("{} has no way to set the primary monitor.", self.desktop.label()))
                }
                other => other,
            },
            Role::MirrorWindow => {
                if self.capture.window {
                    Availability::Yes
                } else if !self.capture.portal {
                    no_capture()
                } else {
                    Availability::no("The screen-capture service cannot share a single window here.")
                }
            }
            Role::Tablet | Role::InputPad => {
                if self.input_ok {
                    Availability::Yes
                } else {
                    Availability::no("Input injection is not available in this session.")
                }
            }
        }
    }

    /// A virtual monitor from the portal, or on X11 from a spare RandR output
    /// (the capture is then the direct screen read, no portal needed).
    fn extend_support(&self) -> Availability {
        if self.capture.virtual_ && (self.capture.portal || self.capture.monitor) {
            Availability::Yes
        } else if !self.capture.portal && !self.capture.monitor {
            Availability::no("No screen-capture service is available in this session.")
        } else if self.session == SessionType::X11 {
            Availability::no(
                "A virtual monitor on X11 needs a spare RandR output (like VIRTUAL1) and the xrandr tool, and this session lacks one of them; Mirror is available instead.",
            )
        } else {
            if let Some(why) = &self.virtual_reason {
                return Availability::No(why.clone());
            }
            Availability::No(format!(
                "{} cannot create a virtual monitor for the phone; Mirror is available instead.",
                self.desktop.label()
            ))
        }
    }
}

/// A monitor's video mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (60000 = 60 Hz).
    pub refresh_mhz: u32,
}

/// One monitor. `name` is the key: KDE's numeric ids change per session, the
/// connector name does not.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub name: String,
    /// Where it sits in the desktop, in logical pixels; `None` while disabled.
    pub region: Option<OutputRegion>,
    pub enabled: bool,
    /// The `name` of the output this one replicates, if it is a mirror.
    pub mirror_of: Option<String>,
    pub primary: bool,
    /// The laptop's own panel.
    pub builtin: bool,
    pub mode: Option<Mode>,
}

/// The monitor layout at one moment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub outputs: Vec<Output>,
}

impl Layout {
    pub fn output(&self, name: &str) -> Option<&Output> {
        self.outputs.iter().find(|o| o.name == name)
    }
}

/// One change to the layout. A batch is applied together.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutOp {
    Enable { name: String },
    MirrorNone { name: String },
    SetPrimary { name: String },
    Move { name: String, x: i32, y: i32 },
    SetMode { name: String, mode: Mode },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(capture: CaptureCaps, layout: LayoutCaps, input_ok: bool) -> Capabilities {
        Capabilities {
            desktop: Desktop::Gnome,
            session: SessionType::Wayland,
            capture,
            layout,
            input_ok,
            rung: Capabilities::rung_for(&capture),
            virtual_reason: None,
        }
    }

    const FULL: CaptureCaps = CaptureCaps { portal: true, monitor: true, window: true, virtual_: true };

    #[test]
    fn full_session_supports_every_role() {
        let c = caps(FULL, LayoutCaps::ALL, true);
        for r in Role::ALL {
            assert_eq!(c.role_support(r), Availability::Yes, "{r:?}");
        }
    }

    #[test]
    fn mirror_needs_capture_only() {
        let mirror_only = CaptureCaps { portal: true, monitor: true, window: false, virtual_: false };
        let c = caps(mirror_only, LayoutCaps::NONE, false);
        assert!(c.role_support(Role::Mirror).is_yes());
        assert!(!c.role_support(Role::Extend).is_yes());
        assert!(!c.role_support(Role::MirrorWindow).is_yes());
        assert!(!c.role_support(Role::Tablet).is_yes());
        assert_eq!(c.rung, Rung::MirrorOnly);
    }

    #[test]
    fn phone_primary_needs_extend_plus_apply_and_primary() {
        let no_apply = LayoutCaps { apply: false, ..LayoutCaps::ALL };
        assert!(caps(FULL, no_apply, true).role_support(Role::PhonePrimary).reason().unwrap().contains("layout"));
        let no_primary = LayoutCaps { primary: false, ..LayoutCaps::ALL };
        assert!(caps(FULL, no_primary, true).role_support(Role::PhonePrimary).reason().unwrap().contains("primary"));
        let no_virtual = CaptureCaps { virtual_: false, ..FULL };
        let why = caps(no_virtual, LayoutCaps::ALL, true).role_support(Role::PhonePrimary);
        assert!(why.reason().unwrap().contains("virtual monitor"), "the Extend reason wins: {why:?}");
    }

    #[test]
    fn every_refusal_carries_a_reason() {
        let c = Capabilities::unsupported(Desktop::Unknown, SessionType::Unknown);
        for r in Role::ALL {
            let a = c.role_support(r);
            assert!(!a.is_yes(), "{r:?}");
            assert!(!a.reason().unwrap().is_empty(), "{r:?}");
        }
    }

    #[test]
    fn a_platform_reason_replaces_the_generic_extend_refusal() {
        let no_virtual = CaptureCaps { virtual_: false, ..FULL };
        let mut c = caps(no_virtual, LayoutCaps::NONE, true);
        c.virtual_reason = Some("Install the driver.".into());
        assert_eq!(c.role_support(Role::Extend).reason(), Some("Install the driver."));
        assert_eq!(c.role_support(Role::PhonePrimary).reason(), Some("Install the driver."));
        // Ignored once a virtual monitor can be made.
        c.capture.virtual_ = true;
        assert!(c.role_support(Role::Extend).is_yes());
    }

    #[test]
    fn no_portal_says_so() {
        let c = Capabilities::unsupported(Desktop::X11Generic, SessionType::X11);
        assert!(c.role_support(Role::Mirror).reason().unwrap().contains("screen-capture service"));
    }
}
