//! Works out what this session supports: which desktop, which session type,
//! what the screen-capture portal offers. The result is a [`Capabilities`]
//! value; nothing else in the host looks at `XDG_CURRENT_DESKTOP`.
//!
//! [`Capabilities::derive`] is pure (environment values in, capabilities out) so
//! it is tested on every desktop's variables without being on that desktop.
//! [`detect`] gathers the real inputs. Capture support comes from the portal's
//! `AvailableSourceTypes` bitmask (1 monitor, 2 window, 4 virtual), not from the
//! desktop's name.

use super::model::{CaptureCaps, Capabilities, Desktop, LayoutCaps, SessionType};

/// The environment variables the detection reads, captured once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvSnapshot {
    pub xdg_current_desktop: String,
    pub xdg_session_type: String,
    pub wayland_display: bool,
    pub x11_display: bool,
}

impl EnvSnapshot {
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        EnvSnapshot {
            xdg_current_desktop: var("XDG_CURRENT_DESKTOP"),
            xdg_session_type: var("XDG_SESSION_TYPE"),
            wayland_display: !var("WAYLAND_DISPLAY").is_empty(),
            x11_display: !var("DISPLAY").is_empty(),
        }
    }
}

/// Portal `AvailableSourceTypes` bits.
pub const SOURCE_MONITOR: u32 = 1;
pub const SOURCE_WINDOW: u32 = 2;
pub const SOURCE_VIRTUAL: u32 = 4;

fn session_of(env: &EnvSnapshot) -> SessionType {
    match env.xdg_session_type.to_ascii_lowercase().as_str() {
        "wayland" => SessionType::Wayland,
        "x11" => SessionType::X11,
        _ if env.wayland_display => SessionType::Wayland,
        _ if env.x11_display => SessionType::X11,
        _ => SessionType::Unknown,
    }
}

fn desktop_of(env: &EnvSnapshot, session: SessionType) -> Desktop {
    let has = |name: &str| env.xdg_current_desktop.split(':').any(|d| d.eq_ignore_ascii_case(name));
    if has("KDE") {
        Desktop::Kde
    } else if has("GNOME") {
        Desktop::Gnome
    } else if has("COSMIC") {
        Desktop::Cosmic
    } else if session == SessionType::X11 {
        Desktop::X11Generic
    } else {
        Desktop::Unknown
    }
}

impl Capabilities {
    /// `portal_types` is the portal's `AvailableSourceTypes`, or `None` when it
    /// could not be read. In that case Wayland KDE and GNOME are assumed to offer
    /// monitors, windows and virtual monitors (what they do today), any other
    /// Wayland session monitors and windows, and an X11 or unknown session
    /// nothing from a portal.
    ///
    /// Mirror follows what the capturer chain really does, which is more than
    /// the portal: on an X11 session it falls back to reading the screen directly
    /// (XShm), so a missing portal does not rule Mirror out there.
    pub fn derive(env: &EnvSnapshot, portal_types: Option<u32>) -> Capabilities {
        let session = session_of(env);
        let desktop = desktop_of(env, session);
        let mut capture = match portal_types {
            Some(bits) => CaptureCaps {
                portal: true,
                monitor: bits & SOURCE_MONITOR != 0,
                window: bits & SOURCE_WINDOW != 0,
                virtual_: bits & SOURCE_VIRTUAL != 0,
            },
            None if session == SessionType::Wayland => {
                let virtual_ = matches!(desktop, Desktop::Kde | Desktop::Gnome);
                CaptureCaps { portal: true, monitor: true, window: true, virtual_ }
            }
            None => CaptureCaps::default(),
        };
        if session == SessionType::X11 && env.x11_display {
            capture.monitor = true;
        }
        // Any X11 session through RandR (read and events from the server,
        // changes through xrandr; phone-as-main is not built there), KDE
        // through kscreen-doctor (polled), GNOME through DisplayConfig
        // (signal). Nothing else has a layout adapter yet. `detect` narrows
        // this down to what the machine really has.
        let layout = match desktop {
            _ if session == SessionType::X11 => LayoutCaps { primary: false, ..LayoutCaps::ALL },
            Desktop::Kde | Desktop::Gnome => LayoutCaps::ALL,
            _ => LayoutCaps::NONE,
        };
        Capabilities {
            desktop,
            session,
            capture,
            layout,
            // uinput works on every Linux session we run on; a missing
            // /dev/uinput shows up when the injector is built.
            input_ok: session != SessionType::Unknown,
            rung: Capabilities::rung_for(&capture),
            virtual_reason: None,
        }
    }
}

/// The desktop of this process's session, from the environment alone (no
/// portal query, so it is cheap and never blocks).
pub fn current_desktop() -> Desktop {
    let env = EnvSnapshot::from_process();
    desktop_of(&env, session_of(&env))
}

/// The session type of this process, from the environment alone. X11-only
/// paths decide on this, never on `DISPLAY` (which Xwayland sets on every
/// Wayland session too).
pub fn current_session() -> SessionType {
    session_of(&EnvSnapshot::from_process())
}

/// What this session supports, from the real environment.
pub fn detect() -> Capabilities {
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(windows)]
    {
        super::windows_caps::detect()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Capabilities::unsupported(Desktop::Unknown, SessionType::Unknown)
    }
}

#[cfg(target_os = "linux")]
fn detect_linux() -> Capabilities {
    let env = EnvSnapshot::from_process();
    let mut caps = Capabilities::derive(&env, portal_source_types());
    if caps.session == SessionType::X11 {
        apply_x11_probe(&mut caps, super::randr::state().ok().as_ref(), super::randr::xrandr_available());
    } else if caps.desktop == Desktop::Kde && !super::kscreen::available() {
        caps.layout = LayoutCaps::NONE;
    }
    caps
}

/// Narrows an X11 session's capabilities to what RandR and the `xrandr` tool
/// really offer: layout caps from both, and a virtual monitor (the dummy-output
/// rung) only where a spare RandR output exists. A portal that already offers
/// virtual monitors keeps its rung.
#[cfg(target_os = "linux")]
fn apply_x11_probe(caps: &mut Capabilities, state: Option<&super::randr::RandrState>, xrandr: bool) {
    caps.layout = super::backends::x11::caps_for(state.is_some(), xrandr);
    if caps.capture.virtual_ {
        return;
    }
    let spare = match state {
        Some(s) if xrandr => super::x11_virtual::probe(s),
        Some(_) => Err("the xrandr tool is not installed".into()),
        None => Err("the X server's RandR extension cannot be read".into()),
    };
    match spare {
        Ok(output) => {
            log::info!("X11: {output} can be the phone's virtual monitor");
            caps.capture.virtual_ = true;
            caps.rung = super::model::Rung::Dummy;
        }
        Err(why) => log::info!("X11: no virtual monitor ({why}); Mirror only"),
    }
}

/// The ScreenCast portal's `AvailableSourceTypes`, or `None` when the portal
/// cannot be reached.
#[cfg(target_os = "linux")]
pub fn portal_source_types() -> Option<u32> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .ok()?;
    proxy.get_property::<u32>("AvailableSourceTypes").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Role;

    fn env(desktop: &str, session: &str) -> EnvSnapshot {
        EnvSnapshot {
            xdg_current_desktop: desktop.into(),
            xdg_session_type: session.into(),
            wayland_display: session == "wayland",
            x11_display: session == "x11",
        }
    }

    #[test]
    fn desktops_are_recognised_from_the_variable_list() {
        assert_eq!(Capabilities::derive(&env("KDE", "wayland"), Some(7)).desktop, Desktop::Kde);
        assert_eq!(Capabilities::derive(&env("ubuntu:GNOME", "wayland"), Some(7)).desktop, Desktop::Gnome);
        assert_eq!(Capabilities::derive(&env("COSMIC", "wayland"), Some(3)).desktop, Desktop::Cosmic);
        assert_eq!(Capabilities::derive(&env("X-Cinnamon", "x11"), None).desktop, Desktop::X11Generic);
        assert_eq!(Capabilities::derive(&env("X-Cinnamon", "wayland"), Some(3)).desktop, Desktop::Unknown);
        assert_eq!(Capabilities::derive(&env("", ""), None).desktop, Desktop::Unknown);
    }

    #[test]
    fn capture_comes_from_the_portal_bits_not_the_desktop() {
        let kde_no_virtual = Capabilities::derive(&env("KDE", "wayland"), Some(SOURCE_MONITOR | SOURCE_WINDOW));
        assert!(kde_no_virtual.capture.monitor && kde_no_virtual.capture.window && !kde_no_virtual.capture.virtual_);
        assert!(!kde_no_virtual.role_support(Role::Extend).is_yes());
        let cosmic_virtual = Capabilities::derive(&env("COSMIC", "wayland"), Some(7));
        assert!(cosmic_virtual.capture.virtual_, "the bitmask wins over the desktop name");
        let none = Capabilities::derive(&env("GNOME", "wayland"), Some(0));
        assert!(none.capture.portal && !none.capture.monitor);
        assert_eq!(none.rung, super::super::model::Rung::Unavailable);
    }

    #[test]
    fn unreadable_portal_falls_back_to_what_works_today() {
        let kde = Capabilities::derive(&env("KDE", "wayland"), None);
        assert!(kde.capture.virtual_ && kde.role_support(Role::PhonePrimary).is_yes());
        let cosmic = Capabilities::derive(&env("COSMIC", "wayland"), None);
        assert!(cosmic.role_support(Role::Mirror).is_yes());
        assert!(!cosmic.role_support(Role::Extend).is_yes());
        let x11 = Capabilities::derive(&env("XFCE", "x11"), None);
        assert!(x11.role_support(Role::Mirror).is_yes(), "X11 sessions mirror by reading the screen directly");
        assert!(!x11.capture.portal && !x11.capture.window && !x11.capture.virtual_);
        assert!(!x11.role_support(Role::Extend).is_yes() && !x11.role_support(Role::MirrorWindow).is_yes());
        assert_eq!(x11.rung, super::super::model::Rung::MirrorOnly);
        assert!(x11.role_support(Role::Tablet).is_yes());
        let no_display = Capabilities::derive(&EnvSnapshot::default(), None);
        assert!(!no_display.role_support(Role::Mirror).is_yes());
    }

    #[test]
    fn session_type_falls_back_to_display_variables() {
        let mut e = env("KDE", "");
        e.wayland_display = true;
        assert_eq!(Capabilities::derive(&e, Some(7)).session, SessionType::Wayland);
        let mut e = env("XFCE", "");
        e.x11_display = true;
        assert_eq!(Capabilities::derive(&e, None).session, SessionType::X11);
    }

    #[test]
    fn every_x11_session_uses_randr_whatever_the_desktop() {
        for d in ["X-Cinnamon", "XFCE", "MATE", "KDE", "GNOME"] {
            let c = Capabilities::derive(&env(d, "x11"), None);
            assert!(c.layout.query && c.layout.events && c.layout.apply, "{d}");
            assert!(!c.layout.primary, "{d}: phone as main is not built on X11");
            assert!(!c.role_support(Role::PhonePrimary).is_yes(), "{d}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_probe_offers_extend_only_with_a_spare_output_and_xrandr() {
        use crate::display::randr::{fixtures, parse_verbose};
        let spare = parse_verbose(fixtures::PANEL_AND_SPARE).unwrap();
        let base = Capabilities::derive(&env("XFCE", "x11"), None);

        let mut c = base.clone();
        apply_x11_probe(&mut c, Some(&spare), true);
        assert!(c.capture.virtual_ && c.role_support(Role::Extend).is_yes());
        assert_eq!(c.rung, super::super::model::Rung::Dummy);
        assert!(c.role_support(Role::Mirror).is_yes());

        let mut c = base.clone();
        apply_x11_probe(&mut c, Some(&spare), false);
        assert!(!c.capture.virtual_ && !c.layout.apply && c.layout.query);
        assert!(c.role_support(Role::Extend).reason().unwrap().contains("RandR"));

        let mut none = spare.clone();
        none.outputs.retain(|o| o.connected);
        let mut c = base.clone();
        apply_x11_probe(&mut c, Some(&none), true);
        assert!(!c.role_support(Role::Extend).is_yes() && c.role_support(Role::Mirror).is_yes());
        assert_eq!(c.rung, super::super::model::Rung::MirrorOnly);

        let mut c = base;
        apply_x11_probe(&mut c, None, true);
        assert_eq!(c.layout, LayoutCaps::NONE);
        assert!(!c.capture.virtual_);
    }

    #[test]
    fn only_kde_and_gnome_have_a_layout_adapter() {
        assert_eq!(Capabilities::derive(&env("KDE", "wayland"), Some(7)).layout, LayoutCaps::ALL);
        assert_eq!(Capabilities::derive(&env("GNOME", "wayland"), Some(7)).layout, LayoutCaps::ALL);
        assert_eq!(Capabilities::derive(&env("COSMIC", "wayland"), Some(3)).layout, LayoutCaps::NONE);
    }
}
