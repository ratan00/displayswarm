//! XDG Desktop Portal `ScreenCast` handshake.
//!
//! Isolated from PipeWire on purpose: this is pure D-Bus and can be tested
//! without a compositor, which made diagnosing "session granted but zero
//! frames" tractable.
//!
//! # The sequence
//!
//! 1. `CreateSession` on `org.freedesktop.portal.Desktop` at
//!    `/org/freedesktop/portal/desktop`, with a random `handle_token`. The
//!    method returns an `org.freedesktop.portal.Request` object path; the
//!    answer arrives asynchronously on that object's `Response` signal.
//! 2. Wait for `Response` on the request path. The body is `(u32, a{sv})`:
//!    `0` = success, `1` = the user dismissed the dialog, anything else is a
//!    portal error. `results["session_handle"]` is the session object path.
//! 3. `SelectSources` with `types = 1` (MONITOR) and `multiple = false`, then
//!    wait for *its* `Response`. This is the step that actually shows the
//!    "share a monitor?" dialog, so skipping the wait is a race. It also
//!    carries the two options that let that dialog be skipped on later runs:
//!    `persist_mode` and `restore_token` (see [`PERSIST_MODE_UNTIL_REVOKED`]).
//! 4. `Start` with an empty parent window, then wait for `Response`.
//!    `results["streams"]` is `a(ua{sv})`; the `u32` of the first entry is the
//!    PipeWire node id the stream must be connected to. The response may also
//!    carry a fresh `restore_token`, which is stored for the next run.
//! 5. `OpenPipeWireRemote`, which hands back a *restricted* PipeWire socket.
//!
//! # Skipping the dialog after the first run
//!
//! `SelectSources` takes a `persist_mode` and a `restore_token`. Asking for
//! [`PERSIST_MODE_UNTIL_REVOKED`] makes the portal issue a restore token in the
//! `Start` response; feeding that token back on the next run restores the
//! previously-approved selection with no dialog. The token is persisted by
//! [`super::restore_token`]. Both are best-effort: if the portal declines to
//! persist (it may lower the mode, in which case it says so in the `Start`
//! response), every run falls back to asking the user, which is the behaviour
//! that predates this code.
//!
//! Note that the options belong to `SelectSources`, **not** to `CreateSession`
//! and **not** to `Start`. `Start` only *returns* the token, and may return a
//! `persist_mode` that is lower than the one requested.
//!
//! # Why step 5 is not optional
//!
//! The fd returned by `OpenPipeWireRemote` is a PipeWire connection that may
//! only see the nodes this session is allowed to see. Connecting a
//! `pipewire::context::ContextRc` to the default `$PIPEWIRE_REMOTE` socket
//! instead will not see the screencast node on a sandboxed/polkit-restricted
//! daemon, and the failure mode is a perfectly healthy capture loop that
//! receives zero buffers. Hence [`ScreencastSession::fd`].
//!
//! # Threading
//!
//! This is the blocking `zbus` API and it parks the calling thread for as long
//! as the user takes to answer a dialog, so it must be called from a dedicated
//! capture thread, never from inside an async task.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use zbus::blocking::proxy::SignalIterator;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{DeserializeDict, OwnedFd as ZOwnedFd, OwnedObjectPath, Type, Value};

use super::restore_token;

/// Well-known name of the desktop portal.
pub const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
/// Object that implements `org.freedesktop.portal.ScreenCast`.
pub const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
/// The `ScreenCast` interface.
pub const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
/// The per-call `Request` interface that answers with a `Response` signal.
pub const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
/// The per-session `Session` interface, whose `Close` method ends a session.
pub const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
/// How long [`ScreencastSession::close`] waits for the portal to answer `Close`.
///
/// Short on purpose: `close` runs from `Drop`, and a wedged portal must not
/// turn "stop mirroring" into a hang. The portal normally answers in a few
/// milliseconds.
pub const SESSION_CLOSE_TIMEOUT: Duration = Duration::from_millis(800);
/// Object path prefix for portal requests.
pub const REQUEST_PATH_PREFIX: &str = "/org/freedesktop/portal/desktop/request";
/// Object path prefix for portal screencast sessions.
pub const SESSION_PATH_PREFIX: &str = "/org/freedesktop/portal/desktop/session";

/// `Response` code meaning "the portal finished and it worked".
pub const RESPONSE_SUCCESS: u32 = 0;
/// `Response` code meaning "the user dismissed the dialog".
pub const RESPONSE_CANCELLED: u32 = 1;

/// `SelectSources` source type: whole monitors.
pub const SOURCE_TYPE_MONITOR: u32 = 1;
/// `SelectSources` source type: one application window, picked in the dialog.
pub const SOURCE_TYPE_WINDOW: u32 = 2;
/// `SelectSources` source type: a new virtual monitor. The compositor creates
/// it for the session (its size follows the consumer's PipeWire format) and
/// removes it when the session closes.
pub const SOURCE_TYPE_VIRTUAL: u32 = 4;

/// What one portal screencast should capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreencastRequest {
    /// `SelectSources` `types` bitmask value ([`SOURCE_TYPE_MONITOR`], ...).
    pub source_type: u32,
    /// Connector name to preselect (monitors only; the portal cannot filter on
    /// it, so it is recorded for diagnostics).
    pub output_filter: Option<String>,
    /// Selects a separate restore-token file (see [`restore_token::token_path_for`]).
    pub token_key: Option<String>,
}

impl ScreencastRequest {
    pub fn monitor(output_filter: Option<String>, token_key: Option<String>) -> Self {
        Self { source_type: SOURCE_TYPE_MONITOR, output_filter, token_key }
    }
}

/// Human name of a source type, for logs and `describe`.
pub fn source_type_name(source_type: u32) -> &'static str {
    match source_type {
        SOURCE_TYPE_MONITOR => "monitor",
        SOURCE_TYPE_WINDOW => "window",
        SOURCE_TYPE_VIRTUAL => "virtual monitor",
        _ => "source",
    }
}

/// How long to wait for the user to answer a portal dialog.
///
/// The two dialog-triggering steps (`SelectSources` and `Start`) block on
/// human input, so this cannot be short, but it must not be infinite: a
/// portal that never emits `Response` would otherwise hang the capture
/// thread forever.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

/// `SelectSources` `persist_mode`: the grant evaporates when the app exits.
///
/// Useless for the "test repeatedly without clicking" goal, and kept only so
/// the numbering below is complete and auditable.
pub const PERSIST_MODE_NONE: u32 = 0;

/// `SelectSources` `persist_mode`: the grant lives as long as *this process*.
///
/// Also useless for repeated runs - closing DisplaySwarm and starting it again is
/// exactly the case this feature is meant to survive - but it is the least
/// privileged non-default, and it is the mode a portal picks when it wants to
/// offer persistence without committing to it.
pub const PERSIST_MODE_UNTIL_EXIT: u32 = 1;

/// `SelectSources` `persist_mode`: the grant survives until explicitly revoked.
///
/// This is the mode DisplaySwarm asks for, and the only one that removes the
/// dialog from run *N+1* when the process that got permission in run *N* has
/// already exited. `2` rather than `1` is the whole point.
///
/// The trade-off is deliberate and is a real reduction in consent: once the
/// user approves, restarting DisplaySwarm reopens the stream with no prompt. The
/// token is the portable, self-contained way to get that behaviour, and it is
/// stored `0600` by [`super::restore_token`]; the user can revoke the grant in
/// the desktop's privacy settings, or locally with
/// `DISPLAYSWARM_RESET_RESTORE_TOKEN=1`.
pub const PERSIST_MODE_UNTIL_REVOKED: u32 = 2;

/// The `a{sv}` option dictionaries sent to the portal.
pub type Options = HashMap<&'static str, Value<'static>>;

/// Which step of the handshake a failure came from, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    CreateSession,
    SelectSources,
    Start,
    OpenPipeWireRemote,
}

impl Stage {
    /// Human readable name, used as the prefix of every error message.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::CreateSession => "CreateSession",
            Stage::SelectSources => "SelectSources",
            Stage::Start => "Start",
            Stage::OpenPipeWireRemote => "OpenPipeWireRemote",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of a successful screencast request.
///
/// Only `Debug` is derived: [`ScreencastSession::fd`] is an
/// `OwnedFd`, which is neither `Clone` nor comparable, so those derives are
/// gone. Use [`ScreencastSession::dup_fd`] to hand the PipeWire connection to
/// more than one owner.
#[derive(Debug)]
pub struct ScreencastSession {
    /// The portal session object path,
    /// e.g. `/org/freedesktop/portal/desktop/session/1_42/a1b2c3d4`.
    pub session_path: String,
    /// PipeWire node id of the stream; pass this to
    /// `pipewire::stream::Stream::connect`.
    pub node_id: u32,
    /// Same value as [`ScreencastSession::node_id`].
    ///
    /// `multiple = false` means the portal hands back exactly one stream, so
    /// there is only one id to speak of. Both names are kept because existing
    /// call sites use both.
    pub stream_id: u32,
    /// The restricted PipeWire connection from `OpenPipeWireRemote`.
    ///
    /// Must be handed to `pipewire::context::ContextRc::connect_fd_rc`; `None`
    /// means the portal did not give us one and capture cannot work.
    pub fd: Option<OwnedFd>,
    /// The D-Bus connection this session was created on, kept alive on purpose.
    ///
    /// `xdg-desktop-portal` destroys a session as soon as the client that
    /// created it drops off the bus, and that takes the PipeWire node with it.
    /// Holding the connection for as long as this struct lives is what keeps
    /// the stream alive between the handshake and the capture loop; dropping it
    /// early is another way to end up with "session granted, zero frames".
    pub connection: Option<Connection>,
    /// The output name the caller asked for, echoed back so the caller can
    /// check what it actually got. See
    /// [`ScreencastSession::output_filter_applied`].
    pub requested_output: Option<String>,
    /// Always `false`. `SelectSources` has no portable "capture this
    /// connector" filter, so the requested name is *not* forwarded to the
    /// portal; see [`ScreencastSession::requested_output`].
    pub output_filter_applied: bool,
    /// Size of the captured region in logical pixels, if the portal reported it.
    pub size: Option<(i32, i32)>,
    /// Top-left corner of the captured region in logical pixels, if reported.
    pub position: Option<(i32, i32)>,
    /// Set once [`ScreencastSession::close`] has run, making it idempotent.
    pub closed: bool,
    /// The source type that was requested in `SelectSources`.
    pub source_type: u32,
    /// The source type the portal says it granted, if it said.
    pub granted_source_type: Option<u32>,
}

impl ScreencastSession {
    /// The stream's rectangle in the global desktop layout, when the portal
    /// reported both `position` and `size` (see [`region_from`]).
    pub fn region(&self) -> Option<crate::display::OutputRegion> {
        region_from(self.position, self.size)
    }

    /// Explicitly ends the portal session (`Session.Close`).
    ///
    /// Why this is needed: relying on the D-Bus connection dropping leaves the
    /// compositor's "screen is being recorded" indicator on for as long as the
    /// portal takes to notice (or forever, if the connection is kept alive by
    /// a detached thread). `Close` makes the portal and the compositor stop the
    /// cast immediately, which turns the recording icon off.
    ///
    /// Idempotent, never panics, and gives up after [`SESSION_CLOSE_TIMEOUT`]:
    /// failures are logged, not returned, because every caller is a teardown
    /// path with nothing better to do than carry on. The blocking D-Bus call
    /// runs on a helper thread precisely so that the timeout is enforceable.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let Some(conn) = self.connection.clone() else {
            return;
        };
        let path = self.session_path.clone();
        let (tx, rx) = mpsc::channel();
        let path_for_thread = path.clone();
        let spawned = std::thread::Builder::new()
            .name("displayswarm-portal-close".into())
            .spawn(move || {
                let res = conn
                    .call_method(
                        Some(PORTAL_BUS_NAME),
                        path_for_thread.as_str(),
                        Some(SESSION_INTERFACE),
                        "Close",
                        &(),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                let _ = tx.send(res);
            });
        if let Err(e) = spawned {
            log::warn!("portal: could not spawn the session-close thread: {e}");
            return;
        }
        match rx.recv_timeout(SESSION_CLOSE_TIMEOUT) {
            Ok(Ok(())) => log::info!("portal: session {path} closed"),
            Ok(Err(e)) => log::warn!("portal: closing session {path} failed: {e}"),
            Err(_) => log::warn!(
                "portal: closing session {path} timed out after {SESSION_CLOSE_TIMEOUT:?}"
            ),
        }
    }

    /// Takes ownership of the PipeWire connection fd.
    ///
    /// [`pipewire::context::ContextRc::connect_fd_rc`] consumes the fd, so
    /// exactly one caller may do this. After it returns `None` the session can
    /// no longer be used to reach PipeWire.
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.fd.take()
    }

    /// Duplicates the PipeWire connection fd, leaving the session usable.
    pub fn dup_fd(&self) -> Option<OwnedFd> {
        self.fd.as_ref().and_then(|fd| fd.try_clone().ok())
    }

    /// One-line, user-facing description of what was actually captured.
    pub fn describe(&self) -> String {
        let kind = source_type_name(self.source_type);
        let source = match (&self.requested_output, self.output_filter_applied) {
            (Some(name), true) => format!("output {name}"),
            (Some(name), false) => format!(
                "a {kind} chosen by the user (asked for output {name}, which the portal cannot filter on)"
            ),
            (None, _) if self.source_type == SOURCE_TYPE_VIRTUAL => {
                "a new virtual monitor".to_string()
            }
            (None, _) => format!("a {kind} chosen by the user"),
        };
        let size = match self.size {
            Some((w, h)) => format!("{w}x{h}"),
            None => "unknown size".to_string(),
        };
        format!(
            "portal screencast of {source}, {size}, pipewire node {}",
            self.node_id
        )
    }
}

/// One entry of the `streams` array from the `Start` response, flattened into
/// plain data so it can be reasoned about (and unit tested) without D-Bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamInfo {
    /// PipeWire node id.
    pub node_id: u32,
    /// `1` = monitor, `2` = window, `4` = virtual output.
    pub source_type: Option<u32>,
    /// Top-left corner of the captured region, in logical pixels.
    pub position: Option<(i32, i32)>,
    /// Size of the captured region, in logical pixels.
    pub size: Option<(i32, i32)>,
}

/// Per-stream properties nested inside the `Start` `Response`.
///
/// Unknown keys are ignored by the derive, so a portal that adds a property
/// does not break decoding.
#[derive(Debug, Default, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct StreamProperties {
    #[allow(dead_code)]
    id: Option<String>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    source_type: Option<u32>,
    #[allow(dead_code)]
    mapping_id: Option<String>,
}

/// The `results` dictionary of a `Response` signal.
///
/// One type for every step: the portal only ever sends
/// `session_handle` after `CreateSession` and `streams` after `Start`, and
/// unknown keys are ignored rather than fatal, so a single lenient shape is
/// both simpler and more future-proof than three strict ones.
#[derive(Debug, Default, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct ResponseResults {
    session_handle: Option<String>,
    streams: Option<Vec<(u32, StreamProperties)>>,
    #[allow(dead_code)]
    restore_token: Option<String>,
}

/// Runs the full portal handshake and returns the PipeWire node to capture.
///
/// `output_filter` is the DRM connector name the caller wants. It is
/// **not** sent to the portal: `SelectSources` has no option for it, so the
/// user always gets to pick a monitor in the portal's own dialog. The
/// requested name is recorded in
/// [`ScreencastSession::requested_output`] with
/// [`ScreencastSession::output_filter_applied`] left `false`, so a caller that
/// cares can verify the choice afterwards (the region size/position is the only
/// portable hint about which monitor was picked).
///
/// Blocks for as long as the user takes to answer the portal dialogs, up to
/// [`RESPONSE_TIMEOUT`] per step, so call it from a dedicated thread.
pub fn request_screencast_node(output_filter: Option<String>) -> Result<ScreencastSession, String> {
    request_screencast(&ScreencastRequest::monitor(output_filter, None))
}

/// [`request_screencast_node`] for any source type and restore-token key.
///
/// The grant must be of the requested type. KDE's portal happily restores a
/// token (or accepts a dialog choice) for another kind of source: an existing
/// screen for Extend (the laptop panel letterboxed into the phone's
/// "extended" display) or a virtual output for Mirror. Such a grant is
/// refused: with a restore token the token is dropped and the dialog asked
/// once more.
pub fn request_screencast(req: &ScreencastRequest) -> Result<ScreencastSession, String> {
    request_screencast_cancellable(req, &Arc::new(AtomicBool::new(false)))
}

/// Message of the error a cancelled request returns.
pub const CANCELLED: &str = "cancelled";

/// [`request_screencast`] that gives up as soon as `cancel` is set: the pending
/// portal request and session are closed, which also dismisses the dialog it
/// opened. Without this a capturer dropped while its dialog was open (a
/// reconnect, a role change) left the dialog on screen for the whole
/// [`RESPONSE_TIMEOUT`], and every new attempt stacked another one on top.
pub fn request_screencast_cancellable(req: &ScreencastRequest, cancel: &Arc<AtomicBool>) -> Result<ScreencastSession, String> {
    let token_key = req.token_key.as_deref();
    let session = match request_screencast_once(req, cancel) {
        Ok(s) => s,
        // A saved permission the portal cannot honour (issued to another app
        // id, or for a screen that is gone) fails `Start` outright instead of
        // falling back to the dialog. Drop it and ask once more.
        Err(e) if !e.contains("dismissed") && restore_token::load_keyed(token_key).is_some() => {
            log::warn!("portal: the request with the saved permission failed ({e}); forgetting it and asking again");
            let _ = restore_token::forget_keyed(token_key);
            request_screencast_once(req, cancel)?
        }
        Err(e) => return Err(e),
    };
    if !wrong_source(req.source_type, session.granted_source_type) {
        return Ok(session);
    }
    let had_token = restore_token::load_keyed(token_key).is_some();
    log::warn!(
        "portal: asked for a {} but got a {} (pipewire node {}); discarding that grant",
        source_type_name(req.source_type),
        session.granted_source_type.map_or("unknown source", source_type_name),
        session.node_id
    );
    let mut granted = session.granted_source_type;
    drop(session);
    let _ = restore_token::forget_keyed(token_key);
    if had_token {
        let session = request_screencast_once(req, cancel)?;
        if !wrong_source(req.source_type, session.granted_source_type) {
            return Ok(session);
        }
        granted = session.granted_source_type;
        let _ = restore_token::forget_keyed(token_key);
    }
    Err(format!(
        "the screen-share dialog shared a {} instead of a {}; choose that in the dialog",
        granted.map_or("different source", source_type_name),
        source_type_name(req.source_type)
    ))
}

/// Base of the app id this host registers with the portal.
pub const APP_ID: &str = "io.github.displayswarm.Host";

/// The app id a screencast for `token_key` (`<device id>/<kind>`) registers.
///
/// Without one the portal derives it from our cgroup, i.e. from whatever
/// launched the daemon (a terminal gives `org.kde.konsole`). KDE names the
/// virtual output after it, and KWin keys the layouts it saves on that name,
/// so one id for every phone makes two phones share a saved layout, and a bad
/// saved layout (the output disabled, or a mirror of the panel) sticks to
/// every run. One id per device keeps each phone's monitor separate.
pub fn app_id_for(token_key: Option<&str>) -> String {
    let device: String = token_key
        .and_then(|k| k.split('/').next())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if device.is_empty() {
        APP_ID.to_string()
    } else {
        format!("{APP_ID}.d{device}")
    }
}

/// Writes a hidden `<app_id>.desktop` into `$XDG_DATA_HOME/applications`
/// (the portal refuses to register an id it cannot find as an application).
fn ensure_desktop_file(app_id: &str) -> std::io::Result<()> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| std::io::Error::other("no XDG_DATA_HOME or HOME"))?;
    let dir = base.join("applications");
    let path = dir.join(format!("{app_id}.desktop"));
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "displayswarm-hostd".into());
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=DisplaySwarm\nComment=Phone display for this PC\n\
         Exec={exe}\nIcon=video-display\nNoDisplay=true\nTerminal=false\n"
    );
    // Rewritten when the binary moved (AppImage mount, rebuilt target dir), so the entry never points at a dead path.
    if std::fs::read_to_string(&path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, contents)
}

/// Whether a grant of `granted` fails a request for `requested` (a type
/// bitmask). KDE restores a saved permission as whatever it was issued for,
/// so a Mirror (monitor) request can come back as a virtual output and the
/// other way round. A portal that does not report the type passes.
fn wrong_source(requested: u32, granted: Option<u32>) -> bool {
    granted.is_some_and(|t| t & requested == 0)
}

fn request_screencast_once(req: &ScreencastRequest, cancel: &Arc<AtomicBool>) -> Result<ScreencastSession, String> {
    let output_filter = req.output_filter.clone();
    if let Some(name) = &output_filter {
        log::warn!(
            "requested screencast output {name:?} cannot be pushed to the portal: \
             SelectSources has no connector-name filter, so the user will pick a monitor"
        );
    }

    let token_key = req.token_key.as_deref();
    // `load_keyed` honours (and performs) the reset request itself.
    let token = restore_token::load_keyed(token_key);
    let token_ref = token.as_deref();

    let mut portal = Portal::connect()?;
    portal.cancel = cancel.clone();
    portal.register_app_id(&app_id_for(token_key));
    let session = portal.create_session(token_ref)?;
    let started = portal
        .select_sources(&session, req.source_type, PERSIST_MODE_UNTIL_REVOKED, token_ref)
        .and_then(|()| portal.start(&session, PERSIST_MODE_UNTIL_REVOKED));
    let (stream, new_token) = match started {
        Ok(v) => v,
        Err(e) => {
            // Ends the session, and with it any dialog still open for it.
            portal.close_session(&session);
            return Err(e);
        }
    };

    if let Some(tok) = new_token {
        if let Err(e) = restore_token::save_keyed(token_key, &tok) {
            log::warn!("Could not save new restore token: {e}");
        }
    }

    let fd = portal.open_pipewire_remote(&session)?;
    // Hand the bus connection to the caller: the portal tears the session (and
    // the PipeWire node) down when this process stops talking to it.
    let connection = portal.connection.clone();

    log::info!(
        "portal screencast granted: session {session}, pipewire node {}, source type {:?}, position {:?}, size {:?}, restricted fd {}",
        stream.node_id,
        stream.source_type,
        stream.position,
        stream.size,
        fd.as_raw_fd()
    );

    Ok(ScreencastSession {
        session_path: session.as_str().to_string(),
        node_id: stream.node_id,
        stream_id: stream.node_id,
        fd: Some(fd),
        connection: Some(connection),
        requested_output: output_filter.clone(),
        output_filter_applied: false,
        size: stream.size,
        position: stream.position,
        closed: false,
        source_type: req.source_type,
        granted_source_type: stream.source_type,
    })
}

impl Drop for ScreencastSession {
    /// Safety net: a session that is dropped without an explicit `close` (an
    /// error path, a panic unwinding) must still not leave the recording
    /// indicator on.
    fn drop(&mut self) {
        self.close();
    }
}

/// Checks if an error message indicates the xdg-desktop-portal crashed.
///
/// The KDE Plasma 6 portal has a known bug where it crashes with
/// `assertion failed: (session->token != NULL)` during CreateSession,
/// which manifests as a D-Bus "Remote peer disconnected" error.
pub fn is_portal_crash_error(err: &str) -> bool {
    err.contains("Remote peer disconnected")
        || err.contains("org.freedesktop.DBus.Error.NoReply")
        || err.contains("session->token")
        || err.contains("xdp_session_initable_init")
}

/// A connection to `org.freedesktop.portal.Desktop` plus the bits of state
/// needed to build request object paths.
struct Portal {
    desktop: Proxy<'static>,
    /// The connection, so it can outlive the handshake.
    connection: Connection,
    /// This process' unique bus name, e.g. `:1.42`, kept in its RAW form.
    ///
    /// It must stay raw: `sanitize_unique_name` strips the leading `:` and
    /// rewrites `.` as `_`, so feeding an already-sanitised value back into it
    /// fails the "must start with `:`" check. Sanitisation happens exactly once,
    /// in `request_object_path`.
    unique: String,
    /// Set by the caller to abandon the dialogs this connection is waiting on.
    cancel: Arc<AtomicBool>,
}

impl Portal {
    fn connect() -> Result<Self, String> {
        let conn = Connection::session().map_err(|e| {
            format!(
                "cannot open a session bus connection: {e} \
                 (DBUS_SESSION_BUS_ADDRESS is unset, so this is not a graphical session)"
            )
        })?;

        let unique = conn
            .unique_name()
            .ok_or_else(|| "the D-Bus connection has no unique name".to_string())?
            .to_string();
        // Sanitised later, in `request_object_path`. Doing it here too would
        // strip the `:` that that function requires to be present.
        sanitize_unique_name(&unique)?;

        let desktop = Proxy::new_owned(
            conn.clone(),
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            SCREENCAST_INTERFACE,
        )
        .map_err(|e| {
            format!(
                "cannot reach the desktop portal at {PORTAL_BUS_NAME} {PORTAL_OBJECT_PATH}: {e} \
                 (is xdg-desktop-portal installed and running?)"
            )
        })?;

        Ok(Self {
            desktop,
            connection: conn,
            unique,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Tells the portal who we are (`org.freedesktop.host.portal.Registry`,
    /// xdg-desktop-portal 1.19+). It must come before any other portal call
    /// on this connection; an older portal lacks it, which is harmless.
    fn register_app_id(&self, app_id: &str) {
        let registry = match Proxy::new_owned(
            self.connection.clone(),
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            "org.freedesktop.host.portal.Registry",
        ) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("portal: no Registry proxy ({e})");
                return;
            }
        };
        // The portal looks the id up as an installed application.
        if let Err(e) = ensure_desktop_file(app_id) {
            log::debug!("portal: no desktop file for {app_id} ({e})");
        }
        match registry.call_method("Register", &(app_id, Options::new())) {
            Ok(_) => log::info!("portal: registered as {app_id}"),
            Err(e) => log::info!("portal: could not register app id {app_id} ({e}); the portal picks one"),
        }
    }

    /// Builds a proxy for the `Request` object a `handle_token` maps to.
    ///
    /// Portal request paths are fully predictable, which is why the
    /// `handle_token` option exists at all: the caller must know the path
    /// before it makes the call.
    fn request_proxy(&self, handle_token: &str) -> Result<Proxy<'static>, String> {
        let path = request_object_path(&self.unique, handle_token)?;
        let object_path = OwnedObjectPath::try_from(path.clone())
            .map_err(|e| format!("built an invalid request object path {path}: {e}"))?;
        Proxy::new_owned(
            self.connection.clone(),
            PORTAL_BUS_NAME,
            object_path,
            REQUEST_INTERFACE,
        )
        .map_err(|e| format!("cannot build a proxy for the request object: {e}"))
    }

    /// Subscribes to `Response` and hands the blocking wait to a helper thread.
    ///
    /// The match rule has to be registered *before* the method call, otherwise
    /// a fast portal can answer before anyone is listening and the signal is
    /// lost forever. Registering happens here, on the caller's thread; only
    /// the blocking `next()` is moved to the helper.
    fn response_waiter(&self, stage: Stage) -> Result<(String, ResponseWaiter), String> {
        let handle_token = new_handle_token();
        let request = self.request_proxy(&handle_token)?;
        let signals: SignalIterator<'static> = request
            .receive_signal("Response")
            .map_err(|e| format!("{stage}: cannot subscribe to the portal Response signal: {e}"))?;

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("portal-{stage}"))
            .spawn(move || {
                // A dropped receiver (timeout) makes this a no-op.
                let _ = tx.send(next_response(signals, stage));
            })
            .map_err(|e| format!("{stage}: cannot spawn the portal response thread: {e}"))?;

        Ok((handle_token, ResponseWaiter { rx, stage, cancel: self.cancel.clone() }))
    }

    /// Best-effort `Session.Close`, so a failed or cancelled attempt leaves no dialog behind.
    fn close_session(&self, session: &OwnedObjectPath) {
        let _ = self.connection.call_method(Some(PORTAL_BUS_NAME), session.as_str(), Some(SESSION_INTERFACE), "Close", &());
    }

    fn create_session(&self, restore_token: Option<&str>) -> Result<OwnedObjectPath, String> {
        let session_token = new_handle_token();
        let (handle_token, waiter) = self.response_waiter(Stage::CreateSession)?;

        self.desktop
            .call_method(
                "CreateSession",
                &create_session_options(&handle_token, &session_token, restore_token),
            )
            .map_err(|e| format!("{}: the D-Bus call failed: {e}", Stage::CreateSession))?;

        let results = waiter.wait()?;
        let session = extract_session_handle(results.session_handle.as_deref())?;

        let predicted = format!("{SESSION_PATH_PREFIX}/{}/{session_token}", self.unique);
        if session.as_str() != predicted {
            // Not fatal: we trust the portal's answer, not our prediction.
            log::debug!(
                "portal session path {} differs from the predicted {predicted}",
                session.as_str()
            );
        }

        Ok(session)
    }

    fn select_sources(&self, session: &OwnedObjectPath, source_type: u32, persist_mode: u32, restore_token: Option<&str>) -> Result<(), String> {
        let (handle_token, waiter) = self.response_waiter(Stage::SelectSources)?;

        self.desktop
            .call_method(
                "SelectSources",
                &(&*session, select_sources_options_for(source_type, &handle_token, persist_mode, restore_token)),
            )
            .map_err(|e| format!("{}: the D-Bus call failed: {e}", Stage::SelectSources))?;

        // This is the dialog the user answers; the results are empty on success.
        waiter.wait()?;
        Ok(())
    }

    fn start(&self, session: &OwnedObjectPath, persist_mode: u32) -> Result<(StreamInfo, Option<String>), String> {
        let (handle_token, waiter) = self.response_waiter(Stage::Start)?;

        self.desktop
            .call_method("Start", &(&*session, "", start_options(&handle_token, persist_mode)))
            .map_err(|e| format!("{}: the D-Bus call failed: {e}", Stage::Start))?;

        let results = waiter.wait()?;
        let stream = pick_stream(&stream_infos(results.streams))?;
        Ok((stream, results.restore_token))
    }

    fn open_pipewire_remote(&self, session: &OwnedObjectPath) -> Result<OwnedFd, String> {
        let options = Options::new();
        let fd: ZOwnedFd = self
            .desktop
            .call("OpenPipeWireRemote", &(&*session, options))
            .map_err(|e| {
                format!(
                    "{}: the D-Bus call failed: {e} \
                     (without this fd the PipeWire stream would be connected to the default \
                     socket and would never see the screencast node)",
                    Stage::OpenPipeWireRemote
                )
            })?;

        Ok(fd.into())
    }
}

/// Awaits one `Response` signal, with a timeout.
struct ResponseWaiter {
    rx: Receiver<Result<ResponseResults, String>>,
    stage: Stage,
    cancel: Arc<AtomicBool>,
}

impl ResponseWaiter {
    fn wait(self) -> Result<ResponseResults, String> {
        let deadline = std::time::Instant::now() + RESPONSE_TIMEOUT;
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancel.load(Ordering::Acquire) {
                        return Err(CANCELLED.to_string());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "{}: the portal did not answer within {RESPONSE_TIMEOUT:?}; \
                             the screen-sharing dialog is probably still open - answer it, or check \
                             `journalctl --user -u xdg-desktop-portal`",
                            self.stage
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "{}: lost the connection to the desktop portal before it answered",
                        self.stage
                    ))
                }
            }
        }
    }
}

/// Blocking `Response` signal read, run on the helper thread.
fn next_response(
    mut signals: SignalIterator<'static>,
    stage: Stage,
) -> Result<ResponseResults, String> {
    let message = signals.next().ok_or_else(|| {
        format!("{stage}: the portal Response signal stream ended without a response")
    })?;
    let body = message.body();
    let (code, results): (u32, ResponseResults) = body
        .deserialize()
        .map_err(|e| format!("{stage}: cannot decode the portal response: {e}"))?;
    interpret_response_code(code, stage)?;
    Ok(results)
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested; no D-Bus, no compositor)
// ---------------------------------------------------------------------------

/// Turns a portal `Response` code into `Ok(())` or an actionable message.
///
/// These messages end up in the user-visible log, so they say what happened
/// and what to do about it instead of just "code 1".
pub fn interpret_response_code(code: u32, stage: Stage) -> Result<(), String> {
    match code {
        RESPONSE_SUCCESS => Ok(()),
        RESPONSE_CANCELLED => Err(format!(
            "{stage}: the user dismissed the screen-sharing dialog, so nothing was captured. \
             Re-run and accept the prompt to continue."
        )),
        other => Err(format!(
            "{stage}: the desktop portal failed with response code {other} \
             (only {RESPONSE_SUCCESS} = success and {RESPONSE_CANCELLED} = user-cancelled are \
             defined). Check `journalctl --user -u xdg-desktop-portal` for details.{}",
            if matches!(stage, Stage::Start) {
                " On KDE, if `journalctl --user -t xdg-desktop-portal-kde` shows \"Could not find output\", \
                 KWin failed to create the virtual monitor; a log-out and log-in clears it."
            } else {
                ""
            }
        )),
    }
}

/// `handle_token`s become path elements, so they must be restricted to
/// characters that are legal in a D-Bus object path element.
pub fn is_valid_handle_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A fresh, unpredictable `handle_token`.
///
/// The portal derives object paths from this, so two concurrent sessions in
/// one process must never collide. `RandomState` is seeded by the OS, and the
/// counter makes uniqueness within the process unconditional.
pub fn new_handle_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    format!("{:x}{:x}{:x}{:x}", seq, a, b, std::process::id())
}

/// Rewrites `:1.42` into `1_42`, the form portals use in object paths.
pub fn sanitize_unique_name(unique_name: &str) -> Result<String, String> {
    let trimmed = unique_name.strip_prefix(':').ok_or_else(|| {
        format!("{unique_name:?} is not a D-Bus unique name (it should start with ':')")
    })?;

    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Err(format!(
            "{unique_name:?} contains characters that are not allowed in an object path"
        ));
    }

    Ok(trimmed.replace('.', "_"))
}

/// The predictable object path of the request created for `handle_token`.
pub fn request_object_path(unique_name: &str, handle_token: &str) -> Result<String, String> {
    if !is_valid_handle_token(handle_token) {
        return Err(format!(
            "{handle_token:?} is not a usable handle_token \
             (must be one or more of [A-Za-z0-9_])"
        ));
    }
    Ok(format!(
        "{REQUEST_PATH_PREFIX}/{}/{handle_token}",
        sanitize_unique_name(unique_name)?
    ))
}

/// Validates the `session_handle` the portal handed back.
pub fn extract_session_handle(session_handle: Option<&str>) -> Result<OwnedObjectPath, String> {
    let handle = session_handle.ok_or_else(|| {
        "the portal approved the session but returned no session_handle; \
         this portal implementation is not speaking the xdg ScreenCast protocol"
            .to_string()
    })?;

    let path = OwnedObjectPath::try_from(handle.to_string()).map_err(|e| {
        format!("the portal returned {handle:?} as a session handle, which is not a valid object path: {e}")
    })?;

    if !path.as_str().starts_with(SESSION_PATH_PREFIX) {
        return Err(format!(
            "the portal returned {handle} as a session handle, expected a path under {SESSION_PATH_PREFIX}"
        ));
    }

    Ok(path)
}

/// `CreateSession` options: the two tokens the portal needs, plus an optional
/// `restore_token` from a previous approved session.
pub fn create_session_options(
    handle_token: &str,
    session_token: &str,
    restore_token: Option<&str>,
) -> Options {
    let mut options = Options::new();
    options.insert("handle_token", Value::from(handle_token.to_string()));
    options.insert(
        "session_handle_token",
        Value::from(session_token.to_string()),
    );
    if let Some(token) = restore_token {
        options.insert("restore_token", Value::from(token.to_string()));
    }
    options
}

/// `SelectSources` options: exactly one monitor, with an optional
/// `restore_token` and a `persist_mode` requesting a new token.
///
/// There is deliberately no key for a connector name: the portal spec has no
/// such option, and passing an unknown key is at best ignored and at worst
/// rejected by a stricter portal.
pub fn select_sources_options(
    handle_token: &str,
    persist_mode: u32,
    restore_token: Option<&str>,
) -> Options {
    select_sources_options_for(SOURCE_TYPE_MONITOR, handle_token, persist_mode, restore_token)
}

/// [`select_sources_options`] for an explicit source type.
pub fn select_sources_options_for(
    source_type: u32,
    handle_token: &str,
    persist_mode: u32,
    restore_token: Option<&str>,
) -> Options {
    let mut options = Options::new();
    options.insert("handle_token", Value::from(handle_token.to_string()));
    options.insert("types", Value::from(source_type));
    options.insert("multiple", Value::from(false));
    options.insert("persist_mode", Value::from(persist_mode));
    // 2 = embedded. Composites the cursor into the video frame so moving the mouse
    // triggers a new frame and the Android client can see it.
    options.insert("cursor_mode", Value::from(2u32));
    if let Some(token) = restore_token {
        options.insert("restore_token", Value::from(token.to_string()));
    }
    options
}

/// `Start` options: just the token and the persist_mode.
pub fn start_options(handle_token: &str, persist_mode: u32) -> Options {
    let mut options = Options::new();
    options.insert("handle_token", Value::from(handle_token.to_string()));
    options.insert("persist_mode", Value::from(persist_mode));
    options
}

/// PipeWire reserves node id 0 for "any node", so a portal reporting 0 has not
/// given us something connectable.
pub fn validate_node_id(node_id: u32) -> Result<u32, String> {
    if node_id == 0 {
        return Err(
            "the portal reported pipewire node id 0, which pipewire means \"any node\": \
             there is nothing concrete to connect the stream to"
                .to_string(),
        );
    }
    Ok(node_id)
}

/// Picks the stream to capture out of the portal's `streams` array.
///
/// `multiple = false`, so this should only ever see one entry; if a portal
/// ignores that, the first entry is the best guess and the extra ones are
/// reported rather than silently dropped.
pub fn pick_stream(streams: &[StreamInfo]) -> Result<StreamInfo, String> {
    let Some(first) = streams.first().copied() else {
        return Err(
            "the portal returned a session with no streams: the screencast was approved but no \
             pipewire node was created, so no frames can ever arrive (the source was probably \
             disconnected between SelectSources and Start)"
                .to_string(),
        );
    };

    if streams.len() > 1 {
        log::warn!(
            "the portal returned {} streams even though multiple=false; using the first \
             (node id {})",
            streams.len(),
            first.node_id
        );
    }

    Ok(StreamInfo {
        node_id: validate_node_id(first.node_id)?,
        ..first
    })
}

/// Builds an [`crate::display::OutputRegion`] from the portal's stream
/// `position` and `size` (logical pixels).
///
/// Both must be present and the size positive: a size without a position would
/// have to guess `(0, 0)`, which silently mis-maps touch on any monitor that is
/// not at the origin, so it is reported as unknown instead.
pub fn region_from(
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
) -> Option<crate::display::OutputRegion> {
    let (x, y) = position?;
    let (w, h) = size?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(crate::display::OutputRegion { x, y, width: w as u32, height: h as u32 })
}

/// Flattens the D-Bus `streams` array into plain data.
fn stream_infos(streams: Option<Vec<(u32, StreamProperties)>>) -> Vec<StreamInfo> {
    streams
        .unwrap_or_default()
        .into_iter()
        .map(|(node_id, props)| StreamInfo {
            node_id,
            source_type: props.source_type,
            position: props.position,
            size: props.size,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::os::fd::AsRawFd;

    #[test]
    fn code_zero_is_success() {
        assert!(interpret_response_code(RESPONSE_SUCCESS, Stage::Start).is_ok());
    }

    #[test]
    fn code_one_blames_the_user_plainly() {
        let err = interpret_response_code(RESPONSE_CANCELLED, Stage::SelectSources).unwrap_err();
        assert!(err.contains("dismissed"), "{err}");
        assert!(err.contains("SelectSources"), "{err}");
    }

    #[test]
    fn unknown_codes_are_quoted_verbatim() {
        let err = interpret_response_code(7, Stage::CreateSession).unwrap_err();
        assert!(err.contains('7'), "{err}");
        assert!(err.contains("CreateSession"), "{err}");
    }

    #[test]
    fn handle_tokens_are_legal_path_elements() {
        let token = new_handle_token();
        assert!(is_valid_handle_token(&token), "{token}");
        assert!(!is_valid_handle_token(""));
        assert!(!is_valid_handle_token("../etc"));
        assert!(!is_valid_handle_token("a-b"));
    }

    #[test]
    fn handle_tokens_do_not_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(new_handle_token()), "handle token collision");
        }
    }

    #[test]
    fn request_paths_follow_the_portal_convention() {
        assert_eq!(
            request_object_path(":1.42", "abcd1234").unwrap(),
            "/org/freedesktop/portal/desktop/request/1_42/abcd1234"
        );
    }

    /// Regression: `Portal::connect` used to store the SANITISED unique name and
    /// `request_object_path` then sanitised it again, so the second pass found no
    /// leading `:` and every real handshake died with
    /// `"1_793" is not a D-Bus unique name`. This pins the invariant that made
    /// that possible: sanitisation is not idempotent-safe to apply twice.
    #[test]
    fn sanitising_twice_is_rejected_so_the_value_must_be_stored_raw() {
        let raw = ":1.793";
        let once = sanitize_unique_name(raw).unwrap();
        assert_eq!(once, "1_793");
        // The second application fails - which is exactly why the stored value
        // must be the raw name.
        assert!(
            request_object_path(&once, "abcd").is_err(),
            "double-sanitising must fail; if this ever succeeds, storing the \
             sanitised form is safe and the Portal::connect fix can be revisited"
        );
        // The raw form works end to end.
        assert!(request_object_path(raw, "abcd").is_ok());
    }

    #[test]
    fn request_paths_reject_junk() {
        // Not a unique name: no leading ':'.
        assert!(request_object_path("1.42", "abcd").is_err());
        // Not legal in an object path.
        assert!(request_object_path(":1.4 2", "abcd").is_err());
        // Path traversal in the token.
        assert!(request_object_path(":1.42", "a/b").is_err());
    }

    #[test]
    fn unique_names_are_rewritten_for_object_paths() {
        assert_eq!(sanitize_unique_name(":1.42").unwrap(), "1_42");
        assert_eq!(sanitize_unique_name(":1.2.3").unwrap(), "1_2_3");
    }

    #[test]
    fn select_sources_asks_for_exactly_one_monitor() {
        let options = select_sources_options("tok", PERSIST_MODE_NONE, None);
        assert_eq!(
            options.get("types"),
            Some(&Value::from(SOURCE_TYPE_MONITOR))
        );
        assert_eq!(options.get("multiple"), Some(&Value::from(false)));
        assert_eq!(
            options.get("handle_token"),
            Some(&Value::from("tok".to_string()))
        );
        assert_eq!(SOURCE_TYPE_MONITOR, 1, "1 must be the MONITOR source type");
    }

    #[test]
    fn select_sources_source_types() {
        assert_eq!(SOURCE_TYPE_WINDOW, 2);
        assert_eq!(SOURCE_TYPE_VIRTUAL, 4);
        for t in [SOURCE_TYPE_MONITOR, SOURCE_TYPE_WINDOW, SOURCE_TYPE_VIRTUAL] {
            let o = select_sources_options_for(t, "tok", PERSIST_MODE_UNTIL_REVOKED, Some("rt"));
            assert_eq!(o.get("types"), Some(&Value::from(t)));
            assert_eq!(o.get("multiple"), Some(&Value::from(false)));
            assert_eq!(o.get("restore_token"), Some(&Value::from("rt".to_string())));
            assert_eq!(o.get("persist_mode"), Some(&Value::from(PERSIST_MODE_UNTIL_REVOKED)));
        }
        assert_eq!(source_type_name(SOURCE_TYPE_VIRTUAL), "virtual monitor");
    }

    #[test]
    fn region_needs_position_and_a_positive_size() {
        use crate::display::OutputRegion;
        assert_eq!(
            region_from(Some((1920, -200)), Some((2560, 1440))),
            Some(OutputRegion { x: 1920, y: -200, width: 2560, height: 1440 })
        );
        assert_eq!(region_from(None, Some((10, 10))), None);
        assert_eq!(region_from(Some((0, 0)), None), None);
        assert_eq!(region_from(Some((0, 0)), Some((0, 10))), None);
        assert_eq!(region_from(Some((0, 0)), Some((10, -1))), None);
    }

    #[test]
    fn stream_properties_flow_into_stream_info_and_region() {
        let infos = stream_infos(Some(vec![(
            42,
            StreamProperties {
                position: Some((1080, 0)),
                size: Some((1920, 1080)),
                source_type: Some(SOURCE_TYPE_VIRTUAL),
                ..Default::default()
            },
        )]));
        let s = pick_stream(&infos).unwrap();
        assert_eq!(s.node_id, 42);
        assert_eq!(s.source_type, Some(4));
        assert_eq!(
            region_from(s.position, s.size),
            Some(crate::display::OutputRegion { x: 1080, y: 0, width: 1920, height: 1080 })
        );
    }

    #[test]
    fn select_sources_sends_no_connector_filter() {
        // Silently claiming the filter was applied would be a lie: the portal
        // has no such option.
        let options = select_sources_options("tok", PERSIST_MODE_NONE, None);
        assert!(!options.contains_key("output"));
        assert!(!options.contains_key("connector"));
    }

    #[test]
    fn start_carries_a_token_and_persist_mode() {
        let options = start_options("tok", PERSIST_MODE_UNTIL_REVOKED);
        assert_eq!(options.len(), 2);
        assert_eq!(
            options.get("handle_token"),
            Some(&Value::from("tok".to_string()))
        );
        assert_eq!(
            options.get("persist_mode"),
            Some(&Value::from(PERSIST_MODE_UNTIL_REVOKED))
        );
    }

    #[test]
    fn node_id_zero_is_not_connectable() {
        assert!(validate_node_id(0).is_err());
        assert_eq!(validate_node_id(42).unwrap(), 42);
    }

    #[test]
    fn an_empty_stream_list_says_what_it_means() {
        let err = pick_stream(&[]).unwrap_err();
        assert!(err.contains("no streams"), "{err}");
    }

    #[test]
    fn the_first_stream_wins_and_is_validated() {
        let a = StreamInfo {
            node_id: 11,
            size: Some((1920, 1080)),
            ..Default::default()
        };
        let b = StreamInfo {
            node_id: 22,
            ..Default::default()
        };
        assert_eq!(pick_stream(&[a, b]).unwrap(), a);
        // A portal that hands back only node id 0 is broken.
        assert!(pick_stream(&[StreamInfo::default()]).is_err());
    }

    #[test]
    fn session_handles_must_be_session_paths() {
        assert!(
            extract_session_handle(Some("/org/freedesktop/portal/desktop/session/1_2/tok")).is_ok(),
            "a real session path must be accepted"
        );
        assert!(
            extract_session_handle(Some("/org/freedesktop/portal/desktop/request/1_2/tok"))
                .is_err()
        );
        assert!(extract_session_handle(Some("nonsense")).is_err());
        assert!(extract_session_handle(None).is_err());
    }

    #[test]
    fn sessions_report_whether_the_output_filter_was_applied() {
        let session = ScreencastSession {
            session_path: "/org/freedesktop/portal/desktop/session/1_2/tok".to_string(),
            node_id: 7,
            stream_id: 7,
            fd: None,
            connection: None,
            requested_output: Some("DP-1".to_string()),
            output_filter_applied: false,
            size: Some((2560, 1440)),
            position: None,
            closed: false,
            source_type: SOURCE_TYPE_MONITOR,
            granted_source_type: None,
        };

        let described = session.describe();
        assert!(described.contains("DP-1"), "{described}");
        assert!(described.contains("node 7"), "{described}");
        assert!(described.contains("2560x1440"), "{described}");
    }

    #[test]
    fn taking_the_fd_leaves_the_session_without_one() {
        let mut session = ScreencastSession {
            session_path: "/org/freedesktop/portal/desktop/session/1_2/tok".to_string(),
            node_id: 7,
            stream_id: 7,
            fd: None,
            connection: None,
            requested_output: None,
            output_filter_applied: false,
            size: None,
            position: None,
            closed: false,
            source_type: SOURCE_TYPE_MONITOR,
            granted_source_type: None,
        };

        let (mine, _theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        session.fd = Some(mine.into());

        let dup = session.dup_fd().expect("dup");
        assert_ne!(dup.as_raw_fd(), i32::MAX);
        assert!(
            session.fd.is_some(),
            "dup_fd must not consume the session's fd"
        );

        assert!(session.take_fd().is_some());
        assert!(session.take_fd().is_none(), "the fd can only be taken once");
    }

    #[test]
    fn test_is_portal_crash_error() {
        assert!(is_portal_crash_error("Remote peer disconnected"));
        assert!(is_portal_crash_error("org.freedesktop.DBus.Error.NoReply: Remote peer disconnected"));
        assert!(is_portal_crash_error("session->token != NULL assertion failed"));
        assert!(is_portal_crash_error("xdp_session_initable_init: assertion failed"));
        assert!(is_portal_crash_error("Assertion failed: (session->token != NULL)"));
        assert!(!is_portal_crash_error("User cancelled the dialog"));
        assert!(!is_portal_crash_error("Permission denied"));
        assert!(!is_portal_crash_error("Some other error"));
    }
}

#[cfg(test)]
mod source_check_tests {
    use super::*;

    #[test]
    fn a_grant_of_another_source_type_is_wrong() {
        assert!(wrong_source(SOURCE_TYPE_VIRTUAL, Some(SOURCE_TYPE_MONITOR)));
        assert!(wrong_source(SOURCE_TYPE_MONITOR, Some(SOURCE_TYPE_VIRTUAL)));
        assert!(!wrong_source(SOURCE_TYPE_VIRTUAL, Some(SOURCE_TYPE_VIRTUAL)));
        assert!(!wrong_source(SOURCE_TYPE_MONITOR, Some(SOURCE_TYPE_MONITOR)));
        assert!(!wrong_source(SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW, Some(SOURCE_TYPE_WINDOW)));
        assert!(!wrong_source(SOURCE_TYPE_VIRTUAL, None));
    }

    #[test]
    fn app_id_is_per_device_and_valid() {
        assert_eq!(app_id_for(Some("6fabb09e-f1d3-4acc/virtual")), "io.github.displayswarm.Host.d6fabb09e");
        assert_eq!(app_id_for(None), APP_ID);
        assert_eq!(app_id_for(Some("../x/monitor")), APP_ID);
    }
}
