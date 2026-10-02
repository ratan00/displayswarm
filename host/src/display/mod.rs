//! Automatic virtual-display provisioning.
//!
//! # The requirement
//!
//! *"If I connect one phone it becomes one display; if I add a second phone it
//! becomes another display."*
//!
//! That needs a **kernel** DRM driver. There is no user-space way to create a
//! display: a display is a KMS pipeline (plane -> CRTC -> encoder -> connector)
//! owned by a kernel driver, and DRM exposes no "create connector" uAPI. So
//! DisplaySwarm configures [`vkms`], the software-only KMS driver, and lets the
//! compositor discover the result. `vkms` supports several independent
//! configfs instances, each with its own connector, which is exactly what makes
//! N phones map onto N displays.
//!
//! # The three layers
//!
//! | layer | file | privilege | responsibility |
//! |-------|------|-----------|----------------|
//! | policy | this file | none | decide what should exist, and expose the API `server.rs` and the UI call |
//! | lifecycle | [`vkms`] | none | instance naming, ordered configfs operation plans, read-only probing |
//! | escalation | [`privilege`] | root via `pkexec` | run the plans as root, with a validated argv and a manual fallback |
//!
//! The lifecycle layer builds an ordered list of filesystem operations and never
//! performs it; the escalation layer is the only thing that ever runs as root.
//! That split is what makes the whole sequence testable without root, and it
//! keeps the root-side surface down to four subcommands.
//!
//! # Where `server.rs` should call this
//!
//! `server.rs` is owned elsewhere, so this module exposes the API and documents
//! the two call sites. Both are in [`crate::server::run_server`]'s session
//! handler. **Do not wire this into the UI only** - the UI button is for a user
//! who wants to provision ahead of time; the automatic path is what makes
//! "plug in a phone, get a display" work.
//!
//! ## 1. On client connect
//!
//! In `spawn_client_session` (or wherever the transport is turned into a
//! session), give the session a stable identity **and** call
//! [`ensure_display_for`]. The identity must survive a reconnect, or the second
//! connection will provision a second display: prefer the AOA device serial for
//! USB, and the peer address for TCP ([`tcp_client_id`]).
//!
//! ```ignore
//! // in src/server.rs
//! use displayswarm_host::display;
//!
//! // `spawn_client_session` gains a `client_id: String` parameter so the
//! // identity outlives a single connection. AOA passes the device serial,
//! // which is stable across reconnects:
//! //     spawn_client_session(..., aoa_stream_serial.clone());
//! // TCP passes the peer address, see `display::tcp_client_id`:
//! //     spawn_client_session(..., display::tcp_client_id(peer_addr.ip()));
//!
//! tokio::spawn(async move {
//!     // Provision the display BEFORE the capturer is built: the capturer
//!     // enumerates outputs, so a display that appears afterwards would be
//!     // missing until the next session. `ensure_display_for` is idempotent,
//!     // so a reconnect reuses the instance it already created.
//!     //
//!     // It is synchronous (it may wait on a polkit dialog) and returns a
//!     // structured error rather than panicking, so run it where blocking is
//!     // acceptable and treat failure as "stream the phone's own screen".
//!     let display_handle = display::ensure_display_for(&client_id);
//!     let (display_handle, display_note) = match display_handle {
//!         Ok(handle) => {
//!             let note = handle.describe();
//!             log::info!("Display ready: {note}");
//!             (Some(handle), note)
//!         }
//!         Err(err) => {
//!             // A failure here is NOT fatal. Fall back to the phone's own
//!             // screen and tell the user exactly what to run to fix it:
//!             log::warn!("No virtual display: {}", err.summary());
//!             (None, err.summary())
//!         }
//!     };
//!
//!     // ... existing session body, then forward `display_note` into the
//!     // status_callback so the UI shows it:
//!     status_callback(
//!         "Client Connected".into(),
//!         label.into(),
//!         display_note,
//!     );
//!
//!     if let Err(e) = handle_client_stream(
//!         reader, writer, &config, &mut stop_rx, display_handle.as_ref(),
//!     ).await {
//!         log::warn!("{} disconnected: {:?}", label, e);
//!     }
//! });
//! ```
//!
//! ## 2. On client disconnect
//!
//! In `handle_client_stream`, the handle passed down above must be dropped on
//! **every** exit path (it is not `Copy`; a local binding dropped at the end of
//! the function covers the error and cancellation paths too):
//!
//! ```ignore
//! // in src/server.rs, at the top of `handle_client_stream`:
//! display_handle: Option<&display::DisplayHandle>,
//! // ...
//! at the end of the function, before `Ok(())`:
//! if let Some(handle) = display_handle {
//!     display::release_display(handle)?;
//! }
//! Ok(())
//! ```
//!
//! Whether the instance is actually *removed* is the handle's business, not
//! the caller's: see [`DisplayRequest::teardown_on_release`], which defaults to
//! `false` because removing a display prompts for root again and yanks a
//! screen out from under the compositor on every disconnect. Set it to `true` if
//! you want the display to disappear with the phone, and accept the extra
//! authentication prompt.
//!
//! ## 3. What `main.rs` must add
//!
//! The UI button is wired in `main.rs`, next to `main_window.on_fps_changed(...)`.
//! `main.rs` must not do the work itself; it registers a one-line callback and
//! [`on_request_display_setup`] does the rest off the UI thread.
//!
//! ```ignore
//! // in src/main.rs, alongside the other on_* handlers
//! let ui_weak_display = ui_handle.clone();
//! main_window.on_setup_display(move || {
//!     displayswarm_host::display::on_request_display_setup(move |report| {
//!         let ui = ui_weak_display.clone();
//!         let _ = slint::invoke_from_event_loop(move || {
//!             if let Some(ui) = ui.upgrade() {
//!                 ui.set_display_status_text(report.status_text().into());
//!             }
//!         });
//!     });
//! });
//! ```
//!
//! `on_request_display_setup` spawns a worker thread (it may block on a polkit
//! dialog for as long as the user takes to type their password) and calls the
//! callback exactly once, so `report` always arrives - success, failure, or
//! cancellation - and the UI can never hang.
//!
//! # N phones -> N displays
//!
//! The display layer is built for N from the start; the *host session* is
//! single-client today, and that is what limits it, not this module.
//!
//! * Each client identity maps deterministically to its own instance name
//!   ([`vkms::instance_name_for`]), so phone A always gets instance A and phone
//!   B always gets instance B.
//! * Each instance is a separate configfs group, so each gets a separate DRM
//!   card and connector, and the compositor turns each into a separate screen.
//!   KWin adopts new vkms cards with no configuration; verified on the
//!   development machine, where `modprobe vkms` alone produced
//!   `Number of Screens: 2` with `Virtual-1 Enabled 1 Geometry 1920,0,1920x1080`.
//! * The first phone to connect *adopts* the display that `modprobe vkms` already
//!   creates, rather than adding a second one on top of it. Only phones after
//!   the first get a new instance, which is what makes the count line up.
//!
//! To go multi-session, the host needs per-client capturers, encoders and
//! injectors, and the compositor has to allow the extra outputs - the display
//! provisioning itself is already the easy part.

#[cfg(target_os = "linux")]
pub mod privilege;
#[cfg(target_os = "linux")]
pub mod vkms;

/// Read-only checks against the live machine, `#[ignore]`d by default.
///
/// Run with `cargo test --lib -- --ignored --nocapture live_`. They are
/// unprivileged and never create or destroy an instance, so they are safe to run
/// on a developer's desktop, but they are not deterministic, so they are kept out
/// of the default run.
#[cfg(all(test, target_os = "linux"))]
mod live_tests;

pub mod backend;
pub mod backends;
pub mod env;
pub mod model;
pub mod reconcile;
pub mod virtual_monitor;
#[cfg(windows)]
pub mod windows_caps;
// Plain logic plus a fake; the Win32 side is `windows_vdd_sys`.
pub mod windows_vdd;
#[cfg(windows)]
pub mod windows_vdd_sys;

#[cfg(target_os = "linux")]
pub mod kscreen;
pub mod layout;
#[cfg(target_os = "linux")]
pub mod mutter;
#[cfg(target_os = "linux")]
pub mod primary;
#[cfg(target_os = "linux")]
pub mod randr;
#[cfg(target_os = "linux")]
pub mod x11_virtual;
// Backend-neutral (it only gates the KDE adapter inside); `server.rs` calls it
// on every platform.
pub mod watch;

use std::net::IpAddr;

/// A monitor's rectangle in the host's global desktop layout, in logical
/// pixels (what the compositor and the portal report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "linux")]
pub use vkms::{CardInfo, ConnectorInfo, DisplayProbe, FsOp, InstanceInfo};

#[cfg(target_os = "linux")]
use privilege::HelperCommand;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong while provisioning a display.
///
/// Every variant carries what the user needs in order to finish the job without
/// the app: [`DisplayError::manual_commands`] returns the exact `sudo` commands
/// to run in a terminal, so a missing `pkexec` or a dismissed dialog degrades to
/// a copy-paste job rather than to a dead end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayError {
    /// The client identity cannot be turned into a configfs instance name.
    InvalidClientId { reason: String },
    /// A filesystem operation failed. Root was held, so this is a kernel or
    /// configfs problem, not a permission one.
    Io { context: String, detail: String },
    /// The privileged helper could not be staged or run at all - `pkexec`
    /// missing, or the staged script does not match [`privilege::HELPER_SCRIPT`].
    HelperUnavailable { detail: String, manual: String },
    /// The user dismissed the polkit dialog, or polkit refused the request.
    AuthorisationDenied { detail: String, manual: String },
    /// The helper ran and failed - including the case where the kernel's configfs
    /// layout is one this build does not know how to provision, which is
    /// reported rather than half-attempted.
    HelperFailed { detail: String, manual: String },
    /// This desktop has no way to do `what` (a [`backend::NoLayout`] session).
    Unsupported { what: String },
}

impl DisplayError {
    /// The exact commands a user can run with `sudo` to do by hand what the
    /// app could not do for them, or `None` when the failure has no manual
    /// equivalent.
    pub fn manual_commands(&self) -> Option<&str> {
        match self {
            DisplayError::InvalidClientId { .. } | DisplayError::Io { .. } | DisplayError::Unsupported { .. } => None,
            DisplayError::HelperUnavailable { manual, .. }
            | DisplayError::AuthorisationDenied { manual, .. }
            | DisplayError::HelperFailed { manual, .. } => Some(manual),
        }
    }

    /// One line suitable for a log or a status bar.
    pub fn summary(&self) -> String {
        match self {
            DisplayError::InvalidClientId { reason } => format!("Display: {reason}"),
            DisplayError::Io { context, detail } => format!("Display: {context} failed: {detail}"),
            DisplayError::HelperUnavailable { detail, .. } => {
                format!("Display: {detail}")
            }
            DisplayError::AuthorisationDenied { detail, .. } => {
                format!("Display: {detail}. Run the commands below with sudo.")
            }
            DisplayError::HelperFailed { detail, .. } => format!("Display: {detail}"),
            DisplayError::Unsupported { what } => format!("Display: {what} is not supported on this desktop"),
        }
    }
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for DisplayError {}

// ---------------------------------------------------------------------------
// Requests and handles
// ---------------------------------------------------------------------------

/// What to provision, and what to do with it afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRequest {
    /// Stable per-device identity. See [`ensure_display_for`].
    pub client_id: String,
    /// Create a new instance even if a virtual display already exists.
    ///
    /// This is the N-phone switch. The *first* phone adopts the display
    /// `modprobe vkms` already provides; every phone after that must set this,
    /// otherwise it would adopt the first phone's screen and the user would see
    /// one display for two phones.
    pub force_new_instance: bool,
    /// Remove the instance when the session ends.
    ///
    /// Defaults to `false`. Removing it needs root again, so a teardown on
    /// every disconnect means a second polkit dialog per session and yanks a
    /// screen out from under the compositor. Set it to `true` only if the
    /// display should disappear with the phone and that is worth the prompt.
    pub teardown_on_release: bool,
}

impl DisplayRequest {
    /// A request for a session that should provision if needed and keep the
    /// result afterwards.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            force_new_instance: false,
            teardown_on_release: false,
        }
    }

    /// A request that will definitely add a display, whatever already exists.
    pub fn additional_display(client_id: impl Into<String>) -> Self {
        Self {
            force_new_instance: true,
            ..Self::new(client_id)
        }
    }
}

/// A provisioned display.
///
/// Dropping it does **not** tear the instance down: see
/// [`DisplayRequest::teardown_on_release`] and [`release_display`]. That is
/// deliberate, because teardown needs root and an accidental `drop` must not be
/// able to trigger an authentication prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayHandle {
    /// The client this display was provisioned for.
    pub client_id: String,
    /// The configfs instance name, e.g. `vmon-phone-a05f`.
    pub instance: String,
    /// How the display came to exist, which is what the UI reports.
    pub state: ProvisioningState,
    /// Whether [`release_display`] should destroy the instance.
    pub teardown_on_release: bool,
    /// DRM cards seen when the request was made, for best-effort attribution of
    /// the card a new instance registered.
    pub cards_before: Vec<String>,
}

impl DisplayHandle {
    /// One line for a log or the status bar, naming the instance and how it got
    /// there.
    pub fn describe(&self) -> String {
        match self.state {
            ProvisioningState::Created => format!("{} (new virtual display)", self.instance),
            ProvisioningState::AlreadyProvisioned => {
                format!("{} (already provisioned)", self.instance)
            }
            ProvisioningState::AdoptedDefault => {
                format!("{} (reusing the existing vkms display)", self.instance)
            }
        }
    }
}

/// How a [`DisplayHandle`] came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningState {
    /// A new configfs instance was created and enabled.
    Created,
    /// Our instance for this client was already there and enabled. Reconnection
    /// is idempotent because of this.
    AlreadyProvisioned,
    /// No instance was created: the display `modprobe vkms` produces on its own
    /// was adopted.
    ///
    /// This is the case that stops one phone getting two screens on a host where
    /// someone has merely run `modprobe vkms`. The named instance is *reserved*
    /// for this client but does not exist on disk, so
    /// [`DisplayHandle::instance`] is not a directory until
    /// [`DisplayRequest::force_new_instance`] is used.
    AdoptedDefault,
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

/// Reads the current display state. Read-only, needs no privilege, and is safe
/// to call from the UI thread.
#[cfg(target_os = "linux")]
pub fn probe() -> DisplayProbe {
    vkms::read_probe()
}

/// The client id for a TCP peer: stable for as long as the device keeps its
/// address, which is exactly the lifetime of a session.
///
/// A USB phone that re-enumerates gets a *different* id, so the AOA serial must
/// be preferred for USB - see the `server.rs` snippet in the module docs.
pub fn tcp_client_id(peer: IpAddr) -> String {
    format!("tcp-{peer}")
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// Provisions a virtual display for `client_id` if one is not already there.
///
/// This is the function `server.rs` calls on client connect. It is
/// **synchronous** and may block on a polkit authentication dialog, so call it
/// from a worker rather than from the middle of an async task; see the `server.rs`
/// snippet in the module docs.
///
/// # Policy
///
/// 1. `vkms` not loaded, or configfs not mounted? Run
///    [`HelperCommand::EnsureModule`] (one `pkexec` prompt).
/// 2. The driver's configfs layout is one this build does not know how to
///    provision (pre-6.19, where `possible_crtcs` was a symlink attribute rather
///    than a directory)? Stop and say so, rather than half-configure an instance
///    that can never light up.
/// 3. Our instance for this client already exists and is enabled? Return
///    [`ProvisioningState::AlreadyProvisioned`]. No prompt, no work - this is
///    what makes a reconnect idempotent instead of a second display.
/// 4. A virtual display already exists and `force_new_instance` is false?
///    **Adopt it**: [`ProvisioningState::AdoptedDefault`], and do not create
///    anything. Loading `vkms` is enough to get a working display, so a host
///    that has merely run `modprobe vkms` already satisfies "one phone, one
///    display"; creating a `vmon-*` instance on top would give it two.
/// 5. Otherwise create and enable the instance ([`HelperCommand::Create`]) and
///    return [`ProvisioningState::Created`].
///
/// # Verification
///
/// Success is not "the helper exited 0". After enabling, `/sys/class/drm` is
/// re-read and diffed against a snapshot taken before, because an instance whose
/// pipeline the kernel rejects leaves a `vmon-*` directory behind and produces
/// **no output at all** - exactly the confusing half-configured state this
/// module exists to avoid. A missing card is reported as a warning rather than a
/// failure (udev may still be enumerating it), and a virtual one is logged with
/// its connectors.
///
/// # Failures
///
/// A failure is never fatal to streaming. The [`DisplayError`] carries the
/// manual `sudo` commands, so the user can finish the job in a terminal and the
/// session continues with the phone's own screen.
#[cfg(target_os = "linux")]
pub fn ensure_display_for(client_id: &str) -> Result<DisplayHandle, DisplayError> {
    ensure_display(&DisplayRequest::new(client_id))
}

/// [`ensure_display_for`] with the full policy knobs. See [`DisplayRequest`].
#[cfg(target_os = "linux")]
pub fn ensure_display(request: &DisplayRequest) -> Result<DisplayHandle, DisplayError> {
    let instance = vkms::instance_name_for(&request.client_id)?;
    let mut probe = vkms::read_probe();

    // Step 1: the prerequisites. Cheap to check, and the common case on a host
    // where someone has never set a virtual display up is that it is needed.
    if !probe.vkms_loaded || !probe.configfs_available {
        log::info!(
            "Provisioning a virtual display for {}: vkms loaded={}, configfs available={}",
            request.client_id,
            probe.vkms_loaded,
            probe.configfs_available
        );
        privilege::run(&HelperCommand::EnsureModule)?;
        probe = vkms::read_probe();
        if !probe.vkms_loaded {
            return Err(DisplayError::Io {
                context: "loading vkms".to_string(),
                detail: "the privileged helper reported success but /sys/module/vkms is still \
                         absent"
                    .to_string(),
            });
        }
        if !probe.configfs_available {
            return Err(DisplayError::Io {
                context: "mounting configfs".to_string(),
                detail: format!("{} is still missing", vkms::VKMS_CONFIGFS_ROOT),
            });
        }
    }

    // Step 2: a layout this build does not know how to provision. A clear
    // refusal beats a half-configured instance that never lights up.
    if let Some(unsupported) = vkms::legacy_layout() {
        log::error!("Cannot provision a virtual display: {unsupported}");
        return Err(DisplayError::HelperFailed {
            detail: format!("this kernel's vkms configfs layout is not supported: {unsupported}"),
            manual: HelperCommand::EnsureModule.manual_commands().join("\n"),
        });
    }

    let cards_before: Vec<String> = probe.cards.iter().map(|c| c.card.clone()).collect();

    // Step 3: already ours. Idempotent reconnect.
    if probe.our_instance(&instance).is_some() {
        log::info!("Virtual display {} is already provisioned for {}", instance, request.client_id);
        return Ok(DisplayHandle {
            client_id: request.client_id.clone(),
            instance,
            state: ProvisioningState::AlreadyProvisioned,
            teardown_on_release: request.teardown_on_release,
            cards_before,
        });
    }

    // Step 4: a virtual display already exists, so adopt it rather than making
    // a second one.
    if !request.force_new_instance && probe.implicit_default_display() {
        log::info!(
            "Adopting the existing vkms display for {} instead of creating {}",
            request.client_id,
            instance
        );
        return Ok(DisplayHandle {
            client_id: request.client_id.clone(),
            instance,
            state: ProvisioningState::AdoptedDefault,
            teardown_on_release: request.teardown_on_release,
            cards_before,
        });
    }

    // Step 5: create it. This is the one path that prompts for root on a host
    // that has no virtual display yet.
    log::info!("Creating virtual display instance {} for {}", instance, request.client_id);
    privilege::run(&HelperCommand::Create {
        instance: instance.clone(),
    })?;

    if !vkms::instance_exists(&instance) {
        return Err(DisplayError::Io {
            context: format!("creating vkms instance {instance}"),
            detail: "the privileged helper reported success but the instance is not in \
                     configfs"
                .to_string(),
        });
    }

    // The kernel does not expose which instance owns which card, so this is a
    // diff against the pre-create snapshot. A missing card is a warning, not a
    // failure: udev may still be enumerating it, and the user can press the
    // button again.
    let after = vkms::read_probe();
    let new_cards: Vec<&vkms::CardInfo> = after
        .cards
        .iter()
        .filter(|card| !cards_before.contains(&card.card))
        .collect();
    match new_cards.as_slice() {
        [card] if card.is_virtual() => {
            let connectors: Vec<&str> = card.connectors.iter().map(|c| c.name.as_str()).collect();
            log::info!(
                "Virtual display {} registered as {} with connectors {:?}",
                instance,
                card.card,
                connectors
            );
        }
        [] => log::warn!(
            "Instance {} was enabled but no new DRM card has appeared yet; the compositor may \
             not have noticed",
            instance
        ),
        many => log::info!(
            "{} new DRM cards appeared; the kernel does not say which is {}",
            many.len(),
            instance
        ),
    }

    Ok(DisplayHandle {
        client_id: request.client_id.clone(),
        instance,
        state: ProvisioningState::Created,
        teardown_on_release: request.teardown_on_release,
        cards_before,
    })
}

/// Releases a display when a session ends.
///
/// Only destroys the instance if the request asked for it
/// ([`DisplayRequest::teardown_on_release`]) *and* this handle actually created
/// or found a DisplaySwarm-managed instance. An
/// [`ProvisioningState::AdoptedDefault`] handle is never torn down: the display
/// it adopted belongs to the loaded module, not to the phone, and removing it
/// would take away a screen the user had before DisplaySwarm was involved.
#[cfg(target_os = "linux")]
pub fn release_display(handle: &DisplayHandle) -> Result<(), DisplayError> {
    if !handle.teardown_on_release {
        log::debug!("Keeping {} after the session ended (teardown_on_release is off)", handle.instance);
        return Ok(());
    }
    if handle.state == ProvisioningState::AdoptedDefault {
        log::info!(
            "Not removing the adopted vkms display; it belongs to the module, not to {}",
            handle.instance
        );
        return Ok(());
    }
    log::info!("Removing virtual display instance {}", handle.instance);
    privilege::run(&HelperCommand::Destroy {
        instance: handle.instance.clone(),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The UI hook
// ---------------------------------------------------------------------------

/// What the UI button produced. Always delivered, never an `Err` the UI has to
/// unwrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySetupReport {
    pub ok: bool,
    /// One line, for the status field.
    pub summary: String,
    /// The `sudo` commands to run by hand, when the app could not do it for the
    /// user. Shown verbatim so it can be copied.
    pub manual_fallback: Option<String>,
}

impl DisplaySetupReport {
    /// The text to put in the UI's status field: the summary, plus the manual
    /// commands when there are any, separated by a blank line.
    pub fn status_text(&self) -> String {
        match &self.manual_fallback {
            Some(manual) if !manual.trim().is_empty() => {
                format!("{}\n\nRun these in a terminal:\n{}", self.summary, manual)
            }
            _ => self.summary.clone(),
        }
    }
}

/// Runs display provisioning off the UI thread and delivers the report to
/// `callback`, exactly once.
///
/// # What `main.rs` must add
///
/// `main.rs` owns the Slint window, so this module cannot register the callback
/// itself - the lib crate does not have the `MainWindow` type. `main.rs` adds
/// one registration, next to the existing `main_window.on_fps_changed(...)`:
///
/// ```ignore
/// // in src/main.rs
/// let ui_weak_display = ui_handle.clone();
/// main_window.on_setup_display(move || {
///     displayswarm_host::display::on_request_display_setup(move |report| {
///         let ui = ui_weak_display.clone();
///         let _ = slint::invoke_from_event_loop(move || {
///             if let Some(ui) = ui.upgrade() {
///                 ui.set_display_status_text(report.status_text().into());
///             }
///         });
///     });
/// });
/// ```
///
/// # Why the worker thread
///
/// Provisioning can block for as long as the user takes to read and answer a
/// polkit dialog, and the UI thread must keep drawing throughout. The callback is
/// invoked on the worker thread, so it must hop to the event loop itself -
/// `slint::invoke_from_event_loop`, as the snippet above does. This mirrors the
/// `status_callback` shape already used in `main.rs`.
///
/// # Guaranteed delivery
///
/// The callback runs whether the operation succeeded, failed, or was cancelled:
/// a UI that only updated on success would sit there looking broken, and a user
/// who dismissed the password dialog needs to be told what to do instead.
#[cfg(target_os = "linux")]
pub fn on_request_display_setup<F>(callback: F)
where
    F: FnOnce(DisplaySetupReport) + Send + 'static,
{
    std::thread::spawn(move || {
        // No client id: the button provisions a host-level display, so the
        // display survives disconnects and the button stays idempotent.
        let report = run_setup(None);
        callback(report);
    });
}

/// The body of [`on_request_display_setup`], without the thread, so the logic is
/// testable and reusable from a non-UI call site.
///
/// Uses the connected client as the identity when the caller supplies one, so
/// the button reuses the instance the session already created instead of making
/// a second display. With no client, a stable host-level identity is used, so
/// the display survives a reboot and the button is idempotent.
#[cfg(target_os = "linux")]
pub fn run_setup(client_id: Option<&str>) -> DisplaySetupReport {
    let identity = match client_id {
        Some(id) => format!("session-{id}"),
        None => "host".to_string(),
    };

    match ensure_display(&DisplayRequest::new(identity)) {
        Ok(handle) => {
            let probe = vkms::read_probe();
            DisplaySetupReport {
                ok: true,
                summary: format!("{} | {}", handle.describe(), probe.summary()),
                manual_fallback: None,
            }
        }
        Err(err) => {
            log::warn!("Display setup failed: {}", err.summary());
            DisplaySetupReport {
                ok: false,
                summary: err.summary(),
                manual_fallback: err.manual_commands().map(|s| s.to_string()),
            }
        }
    }
}

/// Off Linux there is no vkms to provision.
#[cfg(not(target_os = "linux"))]
pub fn run_setup(_client_id: Option<&str>) -> DisplaySetupReport {
    DisplaySetupReport {
        ok: false,
        summary: "Virtual display setup is not available on this platform.".to_string(),
        manual_fallback: None,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn test_tcp_client_id_is_stable_and_sanitisable() {
        let id = tcp_client_id("192.168.1.5".parse().unwrap());
        assert_eq!(id, "tcp-192.168.1.5");
        let name = vkms::instance_name_for(&id).unwrap();
        assert!(vkms::is_valid_instance_name(&name), "{name:?}");
        assert!(name.starts_with(vkms::INSTANCE_PREFIX));

        // An IPv6 peer, whose colons have to be sanitised away.
        let v6 = tcp_client_id("fe80::1".parse().unwrap());
        let name6 = vkms::instance_name_for(&v6).unwrap();
        assert!(vkms::is_valid_instance_name(&name6), "{name6:?}");
        assert!(!name6.contains(':'));

        // Two peers get two displays, which is the multi-phone requirement.
        let a = vkms::instance_name_for(&tcp_client_id("192.168.1.5".parse().unwrap())).unwrap();
        let b = vkms::instance_name_for(&tcp_client_id("192.168.1.6".parse().unwrap())).unwrap();
        assert_ne!(a, b);
    }

    /// The two knobs [`DisplayRequest`] has, and what they default to. A default
    /// that surprises the caller here costs a polkit dialog or an extra screen.
    #[test]
    fn test_display_request_defaults() {
        let req = DisplayRequest::new("usb-0a05f-0642");
        assert_eq!(req.client_id, "usb-0a05f-0642");
        assert!(!req.force_new_instance, "the first phone must adopt, not add a screen");
        assert!(!req.teardown_on_release, "teardown must not prompt for root on disconnect");

        let extra = DisplayRequest::additional_display("usb-4b1f-1180");
        assert!(extra.force_new_instance, "a second phone must add a display");
        assert!(!extra.teardown_on_release);
    }

    /// N phones have to be able to hold N distinct instances, and the mapping
    /// has to survive a reconnect so the same phone keeps the same display.
    #[test]
    fn test_n_phones_map_to_n_instances() {
        let phones = [
            "usb-0a05f-0642",
            "usb-4b1f-1180",
            "usb-2c9d-3311",
            "usb-77aa-0f0f",
        ];
        let mut names = std::collections::BTreeSet::new();
        for phone in phones {
            let name = vkms::instance_name_for(phone).unwrap();
            assert!(vkms::is_our_instance(&name), "{name:?} is not recognisably ours");
            assert!(names.insert(name.clone()), "{phone} collided on {name:?}");
        }
        assert_eq!(names.len(), phones.len());
        // Deterministic, so a reconnect lands on the same instance.
        for phone in phones {
            assert_eq!(
                vkms::instance_name_for(phone).unwrap(),
                vkms::instance_name_for(phone).unwrap()
            );
        }
    }

    /// The status text is what a user sees when provisioning fails, and it has
    /// to contain the commands that would make it work.
    #[test]
    fn test_status_text_includes_the_manual_fallback() {
        let with = DisplaySetupReport {
            ok: false,
            summary: "Display: the system authentication dialog was dismissed.".to_string(),
            manual_fallback: Some("sudo mkdir /x\nsudo echo 1 > /y".to_string()),
        };
        let text = with.status_text();
        assert!(text.contains("dismissed"), "{text}");
        assert!(text.contains("sudo mkdir /x"), "{text}");

        let without = DisplaySetupReport {
            ok: true,
            summary: "vmon-host (new virtual display)".to_string(),
            manual_fallback: None,
        };
        assert_eq!(without.status_text(), "vmon-host (new virtual display)");
    }

    /// A handle must never tear down a display it did not create: the adopted
    /// one belongs to the module and was there before the phone connected.
    #[test]
    fn test_release_never_removes_an_adopted_display() {
        let handle = DisplayHandle {
            client_id: "host".into(),
            instance: "vmon-host".into(),
            state: ProvisioningState::AdoptedDefault,
            teardown_on_release: true,
            cards_before: vec!["card2".into()],
        };
        // Returns Ok without running a privileged helper, so this is safe to
        // call from a test on a machine that is not root.
        assert_eq!(release_display(&handle), Ok(()));
        assert!(handle.describe().contains("reusing"));
    }

    /// With teardown off, releasing must not even try. Also safe unprivileged.
    #[test]
    fn test_release_is_a_no_op_when_teardown_is_off() {
        let handle = DisplayHandle {
            client_id: "host".into(),
            instance: "vmon-host".into(),
            state: ProvisioningState::Created,
            teardown_on_release: false,
            cards_before: vec![],
        };
        assert_eq!(release_display(&handle), Ok(()));
        assert!(handle.describe().contains("new virtual display"));
    }

    /// Errors have to be actionable. A failure with no manual equivalent is
    /// only acceptable when it is the user's own input that was rejected.
    #[test]
    fn test_error_fallbacks() {
        let rejected = DisplayError::InvalidClientId {
            reason: "bad id".into(),
        };
        assert!(rejected.manual_commands().is_none());
        assert!(rejected.summary().contains("bad id"));

        let denied = DisplayError::AuthorisationDenied {
            detail: "dismissed".into(),
            manual: "sudo modprobe vkms".into(),
        };
        assert_eq!(denied.manual_commands(), Some("sudo modprobe vkms"));
        assert!(denied.summary().contains("dismissed"));
        assert!(denied.summary().contains("sudo"), "the summary must point at the fix");

        // `Display` and `summary` must agree, since the UI and the log use
        // different ones.
        assert_eq!(denied.to_string(), denied.summary());
    }

    /// `DisplayError` is a real `Error`, so it composes with the rest of the
    /// crate's `Box<dyn Error>` signatures.
    #[test]
    fn test_error_is_a_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(DisplayError::HelperFailed {
            detail: "boom".into(),
            manual: "sudo true".into(),
        });
        assert!(err.to_string().contains("boom"));
    }
}
