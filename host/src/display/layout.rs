//! Picks the [`LayoutBackend`] for this session: any X11 session through
//! RandR ([`super::randr`]), KDE Plasma through [`super::kscreen`]
//! (`kscreen-doctor`), GNOME through [`super::mutter`] (D-Bus), and
//! [`NoLayout`] everywhere else, including every non-Linux
//! platform. The rest of the host asks the backend (or
//! [`Capabilities`](super::model::Capabilities)) and never the desktop's name.

use super::backend::LayoutBackend;
use std::sync::Arc;

/// The layout backend for this session.
pub fn current() -> Arc<dyn LayoutBackend> {
    #[cfg(target_os = "linux")]
    {
        current_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(super::backend::NoLayout)
    }
}

/// X11 sessions through RandR whatever the desktop (KDE's and GNOME's Wayland
/// tools do not apply there); on Wayland, KDE through `kscreen-doctor` when it
/// is installed and GNOME through Mutter's DisplayConfig. Every other Wayland
/// desktop has no layout adapter yet.
#[cfg(target_os = "linux")]
fn current_linux() -> Arc<dyn LayoutBackend> {
    use super::backend::NoLayout;
    use super::backends::{gnome::GnomeLayout, kde::KdeLayout, x11::X11Layout};
    use super::model::{Desktop, SessionType};
    if super::env::current_session() == SessionType::X11 {
        return Arc::new(X11Layout);
    }
    match super::env::current_desktop() {
        Desktop::Kde if super::kscreen::available() => Arc::new(KdeLayout),
        Desktop::Gnome => Arc::new(GnomeLayout),
        _ => Arc::new(NoLayout),
    }
}
