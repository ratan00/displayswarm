use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::capture::{create_capturer, CaptureState, CaptureTarget, Frame, ScreenCapturer};
use crate::devices::{unix_now, DeviceRecord, DeviceSettings, DeviceStore, Role};
#[cfg(target_os = "linux")]
use crate::display::primary::{make_primary, recover_leftovers};
use crate::display::OutputRegion;
use crate::encoder::{create_encoder, EncodedPacket, VideoEncoder};
use crate::input::router::InputRouter;
use crate::input::{create_default_injector, InputInjector};
use crate::protocol::host_monotonic_us;
use crate::protocol::wire::{
    self, Header, Hello, HelloAck, Message, MessageReader, Stats, BYE_ERROR, BYE_NORMAL,
    BYE_PROTOCOL_ERROR, BYE_SERVER_STOPPING, BYE_VERSION_MISMATCH, CH_CONTROL, CH_INPUT, CH_VIDEO, CODEC_H264,
    HEADER_SIZE, HELLO_BUSY, HELLO_OK,
    HELLO_REJECTED, HELLO_VERSION_MISMATCH, HOST_STATE_AWAITING_PERMISSION,
    HOST_STATE_CAPTURE_FAILED, HOST_STATE_STREAMING, MAGIC, MSG_HELLO, MSG_VIDEO_FRAME,
    ROLE_UNSET, VERSION, VIDEO_FRAGMENT,
};
use crate::transport::{tls::TlsIdentity, NetGate};
use crate::transport::{connect_aoa_device, AoaConfig, AoaResetHandle, TransportStream};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub target_fps: u32,
    pub bitrate_kbps: u32,
    pub aoa_config: AoaConfig,
    /// TLS, pairing, discovery and adaptive bitrate for the TCP transport.
    pub network: crate::transport::NetworkConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 9999,
            target_fps: 60,
            bitrate_kbps: 20000,
            aoa_config: AoaConfig::default(),
            network: Default::default(),
        }
    }
}

/// Maximum time a freshly accepted client is given to complete the Hello
/// handshake before the session is abandoned.
///
/// Without this bound a device that is present on the bus but not running the
/// client app (the common case for USB AOA, where the phone enumerates as an
/// accessory long before DisplaySwarm is launched on it) pins a session forever.
///
/// It must also comfortably exceed how long a human needs to answer a portal
/// screen-share dialog. At 10s it did not, and that produced a livelock: the AOA
/// session timed out, the capturer was torn down and re-requested the portal, a
/// *new* dialog appeared, and the next AOA attempt expired before either could be
/// answered - so the dialog could never be approved.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes of unrecognised data the handshake skips before giving up on a peer.
const HELLO_SCAN_LIMIT: usize = 1024 * 1024;

/// Smallest and largest panel dimension (pixels) a Hello may report.
const MIN_PANEL: u16 = 16;
const MAX_PANEL: u16 = 8192;

/// Most devices that may be connected at once. Every session owns a capturer
/// (a portal screencast), an x264 encoder thread and a `/dev/uinput` device, so
/// the count is bounded; a device beyond it is refused with `HELLO_BUSY`.
///
/// Known limits of the multi-session design: sessions are independent (no shared
/// encoder budget, so N software encoders compete for CPU), and two sessions may
/// capture the same monitor or window (each has its own portal grant). Only a
/// *reconnect of the same device* is treated as a conflict: it replaces its old
/// session.
pub const MAX_SESSIONS: usize = 8;

/// How long a reconnecting device waits for its previous session to finish
/// tearing down before it proceeds anyway.
const REPLACE_WAIT: Duration = Duration::from_secs(5);

/// Builds the host-side devices of a session. Injectable so tests need neither
/// a portal, an x264 encoder nor `/dev/uinput`.
pub struct Backends {
    pub capture: Box<dyn Fn(&CaptureTarget, Option<&str>, u32, u32) -> Box<dyn ScreenCapturer> + Send + Sync>,
    /// `(width, height, fps, kbps, qp)`; `qp` selects constant quality (wired links).
    pub encoder: Box<dyn Fn(u32, u32, u32, u32, Option<u32>) -> Box<dyn VideoEncoder> + Send + Sync>,
    pub injector: Box<dyn Fn(u32, u32) -> Box<dyn InputInjector> + Send + Sync>,
    /// Makes the region primary; the returned guard restores the layout when
    /// dropped. `Err` carries a message for the UI.
    /// Also returns the region the monitor really has afterwards.
    pub primary: Box<
        dyn Fn(OutputRegion, bool) -> Result<(Box<dyn std::any::Any + Send>, OutputRegion), String> + Send + Sync,
    >,
    /// The monitor the Tablet role maps pen and touch onto (the primary one).
    /// `None` leaves the compositor's default mapping.
    pub tablet_region: Box<dyn Fn() -> Option<OutputRegion> + Send + Sync>,
    /// The per-session services (audio, clipboard, files, ...); see
    /// [`crate::services`].
    pub services: Box<crate::services::ServiceFactory>,
    /// What this session supports (desktop, capture, layout, input); asked when
    /// a role is chosen so unsupported ones can be refused with a reason.
    pub capabilities: Box<dyn Fn() -> crate::display::model::Capabilities + Send + Sync>,
    /// The monitor-layout backend ([`crate::display::backend::NoLayout`] where
    /// the desktop's layout is out of reach).
    pub layout: Box<dyn Fn() -> Arc<dyn crate::display::backend::LayoutBackend> + Send + Sync>,
}

impl Default for Backends {
    fn default() -> Self {
        Self {
            capture: Box::new(|target, key, w, h| create_capturer(target, key, w, h)),
            encoder: Box::new(|w, h, fps, kbps, qp| create_encoder(w, h, fps, kbps, qp)),
            injector: Box::new(|w, h| create_default_injector(w, h)),
            primary: Box::new(|region, panel_off| {
                #[cfg(target_os = "linux")]
                {
                    // The guard's region can differ from `region`: with the panel
                    // off the layout shifts to keep its origin at (0,0).
                    make_primary(region, panel_off)
                        .map(|g| {
                            let moved = g.region();
                            (Box::new(g) as Box<dyn std::any::Any + Send>, moved)
                        })
                        .map_err(|e| e.to_string())
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (region, panel_off);
                    Err("making a monitor the primary screen is not supported on this platform".to_string())
                }
            }),
            tablet_region: Box::new(|| {
                // None (no layout backend, or no primary monitor) leaves the
                // compositor's default mapping.
                crate::display::layout::current().primary_region()
            }),
            services: Box::new(crate::services::default_services),
            capabilities: Box::new(crate::display::env::detect),
            layout: Box::new(crate::display::layout::current),
        }
    }
}

/// What can be asked of a running session besides what the phone sends.
#[derive(Debug, Clone)]
enum SessionCmd {
    /// The phone's `SetRole` (raw wire value, possibly invalid).
    PhoneSetRole(u8),
    /// The phone's `SetQuality` (raw wire value, possibly invalid).
    PhoneSetQuality(u8),
    /// The host UI's role change.
    SetRole(Role),
    /// The phone's panel changed (rotation, split screen): rebuild the virtual
    /// monitor at the new size. Raw wire values, validated when applied.
    Resize { width: u16, height: u16 },
    /// The host UI changed the device's settings.
    Settings(DeviceSettings),
    /// End the session.
    End(EndKind, String),
}

/// One row of the device list: a remembered device and, if it is connected, its
/// live session.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceStatus {
    pub device_id: String,
    pub name: String,
    /// The remembered role (`None` until chosen).
    pub role: Option<Role>,
    pub connected: bool,
    /// The role actually in effect right now (differs from `role` while a role
    /// is unchosen, or when PhonePrimary fell back to Extend).
    pub effective_role: Option<Role>,
    /// Human-readable session state ("Streaming 60 fps", "Waiting for ...").
    pub state: String,
    /// Set when something went wrong that the user should see.
    pub notice: Option<String>,
    pub target_output: Option<String>,
    pub panel_off: bool,
    /// The remembered feature toggles.
    pub settings: DeviceSettings,
    /// USB (AOA) rather than network, while connected.
    pub via_aoa: bool,
    /// The phone's native size and refresh rate, while connected.
    pub resolution: Option<Resolution>,
    /// The last one-second window of the video stream, while streaming.
    pub live: Option<LiveStats>,
}

/// The phone's native panel, from its Hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// Live numbers of a streaming session (the last one-second window).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveStats {
    pub fps: u32,
    pub kbps: u32,
    /// Phone-reported glass-to-glass latency, once it has reported.
    pub latency_ms: Option<u32>,
    pub encoder: String,
}

struct SessionEntry {
    gen: u64,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    /// Closed (sender dropped) when the session has fully finished.
    done_rx: watch::Receiver<()>,
    via_aoa: bool,
    effective_role: Role,
    state: String,
    notice: Option<String>,
    resolution: Option<Resolution>,
    live: Option<LiveStats>,
}

/// Holds every live session (keyed by device id) and the device store.
///
/// Replaces the old global `SESSION_ACTIVE`: any number of devices (up to
/// [`MAX_SESSIONS`]) can be connected at once. The UI reads [`Self::snapshot`]
/// and drives sessions through [`Self::set_role`] / [`Self::forget`].
pub struct SessionManager {
    store: Mutex<DeviceStore>,
    backends: Backends,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    next_gen: AtomicU64,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Device id of the one session that is the primary display, if any.
    primary_owner: Mutex<Option<String>>,
    /// Pairing offers, PIN events and trusted phones for network connections.
    pairing: Arc<crate::transport::pairing::PairingManager>,
    /// What the session supports, as of the last look (see [`Self::capabilities`]).
    caps_cache: Mutex<Option<(Instant, crate::display::model::Capabilities)>>,
}

/// How long a capability lookup is reused. Detection asks the desktop's
/// screen-share portal, which can be slow, and the answer hardly ever changes.
const CAPS_TTL: Duration = Duration::from_secs(30);

/// Held by a running session; removes it from the manager on drop.
struct Registration {
    manager: Arc<SessionManager>,
    device_id: String,
    gen: u64,
    cmd_rx: Option<mpsc::UnboundedReceiver<SessionCmd>>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    _done_tx: watch::Sender<()>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        {
            let mut map = self.manager.lock_sessions();
            if map.get(&self.device_id).map(|e| e.gen) == Some(self.gen) {
                map.remove(&self.device_id);
            }
        }
        self.manager.notify();
    }
}

impl SessionManager {
    pub fn new(store: DeviceStore, backends: Backends) -> Arc<Self> {
        Self::new_with_pairing(store, backends, crate::transport::pairing::PairingManager::in_memory())
    }

    pub fn new_with_pairing(
        store: DeviceStore,
        backends: Backends,
        pairing: Arc<crate::transport::pairing::PairingManager>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pairing,
            store: Mutex::new(store),
            backends,
            sessions: Mutex::new(HashMap::new()),
            next_gen: AtomicU64::new(1),
            on_change: Mutex::new(None),
            primary_owner: Mutex::new(None),
            caps_cache: Mutex::new(None),
        })
    }

    /// What this session supports. May block on the desktop's portal, so async
    /// callers use [`Self::role_support_async`].
    pub fn capabilities(&self) -> crate::display::model::Capabilities {
        let cached = {
            let guard = self.caps_cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().filter(|(at, _)| at.elapsed() < CAPS_TTL).map(|(_, c)| c.clone())
        };
        if let Some(caps) = cached {
            return caps;
        }
        let caps = (self.backends.capabilities)();
        log::info!(
            "Session capabilities: {:?} on {:?}, capture {:?}, layout {:?}, input {}, {}",
            caps.desktop,
            caps.session,
            caps.capture,
            caps.layout,
            caps.input_ok,
            caps.rung.label()
        );
        *self.caps_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), caps.clone()));
        caps
    }

    /// The last capabilities lookup, however old, without looking again; `None`
    /// before the first one. For callers that must not block (the UI state).
    pub fn capabilities_peek(&self) -> Option<crate::display::model::Capabilities> {
        self.caps_cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(_, c)| c.clone())
    }

    /// Whether `role` can work here, and if not, why.
    pub fn role_support(&self, role: Role) -> crate::display::model::Availability {
        self.capabilities().role_support(role)
    }

    /// [`Self::role_support`] off the async runtime.
    async fn role_support_async(self: &Arc<Self>, role: Role) -> crate::display::model::Availability {
        let this = self.clone();
        // A lookup that cannot run must not lock the user out of every role.
        tokio::task::spawn_blocking(move || this.role_support(role))
            .await
            .unwrap_or(crate::display::model::Availability::Yes)
    }

    /// Takes the single primary-display slot, or `None` if another device has it.
    fn claim_primary(self: &Arc<Self>, device_id: &str) -> Option<PrimaryClaim> {
        let mut owner = self.primary_owner.lock().unwrap_or_else(|e| e.into_inner());
        if owner.is_some() {
            return None;
        }
        *owner = Some(device_id.to_string());
        Some(PrimaryClaim { manager: self.clone() })
    }

    /// Real devices and the store in the user's config directory; if that cannot
    /// be opened, remembered roles are kept for this run only.
    pub fn with_defaults() -> Arc<Self> {
        let store = DeviceStore::open_default().unwrap_or_else(|e| {
            log::warn!("Device store unavailable ({e}); roles will not be remembered");
            DeviceStore::in_memory()
        });
        Self::new_with_pairing(store, Backends::default(), crate::transport::pairing::PairingManager::with_defaults())
    }

    /// Pairing state: open a PIN/QR offer, listen for PIN events, list or revoke
    /// trusted phones.
    pub fn pairing(&self) -> &Arc<crate::transport::pairing::PairingManager> {
        &self.pairing
    }

    /// Registers a callback run (on an arbitrary thread) whenever the device
    /// list or a session's state changes. It must be cheap and non-blocking.
    pub fn set_change_listener(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    fn notify(&self) {
        let f = self.on_change.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(f) = f {
            f();
        }
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_store(&self) -> std::sync::MutexGuard<'_, DeviceStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of connected sessions.
    pub fn session_count(&self) -> usize {
        self.lock_sessions().len()
    }

    /// Whether a USB AOA session is live (it owns the USB interface).
    fn aoa_active(&self) -> bool {
        self.lock_sessions().values().any(|e| e.via_aoa)
    }

    /// The remembered record for a device.
    pub fn record(&self, device_id: &str) -> Option<DeviceRecord> {
        self.lock_store().get(device_id)
    }

    /// Remembered devices merged with live session state, newest connect first.
    pub fn snapshot(&self) -> Vec<DeviceStatus> {
        let mut records = self.lock_store().all();
        records.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.device_id.cmp(&b.device_id)));
        let map = self.lock_sessions();
        records
            .into_iter()
            .map(|r| {
                let live = map.get(&r.device_id);
                DeviceStatus {
                    connected: live.is_some(),
                    effective_role: live.map(|e| e.effective_role),
                    state: live.map(|e| e.state.clone()).unwrap_or_else(|| "Not connected".into()),
                    notice: live.and_then(|e| e.notice.clone()),
                    role: r.role,
                    name: r.name,
                    target_output: r.target_output,
                    panel_off: r.panel_off,
                    settings: r.settings,
                    via_aoa: live.map(|e| e.via_aoa).unwrap_or(false),
                    resolution: live.and_then(|e| e.resolution),
                    live: live.and_then(|e| e.live.clone()),
                    device_id: r.device_id,
                }
            })
            .collect()
    }

    /// Changes a device's role: remembered always, and applied at once (without
    /// dropping the transport) when the device is connected.
    pub fn set_role(&self, device_id: &str, role: Role) {
        let live_tx = self.lock_sessions().get(device_id).map(|e| e.cmd_tx.clone());
        match live_tx {
            Some(tx) => {
                let _ = tx.send(SessionCmd::SetRole(role));
            }
            None => {
                if let Some(mut rec) = self.record(device_id) {
                    rec.role = Some(role);
                    if let Err(e) = self.lock_store().put(rec) {
                        log::warn!("Could not save the role for {device_id}: {e}");
                    }
                    self.notify();
                }
            }
        }
    }

    /// Replaces a device's feature toggles. Sessions read them when they start;
    /// a running session applies the audio and microphone ones at once.
    pub fn set_settings(&self, device_id: &str, settings: DeviceSettings) {
        let live_tx = self.lock_sessions().get(device_id).map(|e| e.cmd_tx.clone());
        if let Some(tx) = live_tx {
            // Audio and mic follow at once (switching off always works; switching on needs
            // the phone to have been offered the service when the session began).
            let _ = tx.send(SessionCmd::Settings(settings.clone()));
        }
        self.edit_record(device_id, |r| r.settings = settings);
    }

    /// Sets the picture-quality mode (the phone's choice).
    pub fn set_video_quality(&self, device_id: &str, quality: crate::devices::VideoQuality) {
        self.edit_record(device_id, |r| r.settings.video_quality = quality);
    }

    /// Sets the "turn the panel off while the phone is primary" option.
    pub fn set_panel_off(&self, device_id: &str, panel_off: bool) {
        self.edit_record(device_id, |r| r.panel_off = panel_off);
    }

    /// Sets the host monitor (connector name) Mirror / Tablet use; `None` lets
    /// the portal ask.
    pub fn set_target_output(&self, device_id: &str, output: Option<String>) {
        self.edit_record(device_id, |r| r.target_output = output);
    }

    fn edit_record(&self, device_id: &str, f: impl FnOnce(&mut DeviceRecord)) {
        {
            let mut store = self.lock_store();
            let Some(mut rec) = store.get(device_id) else { return };
            f(&mut rec);
            if let Err(e) = store.put(rec) {
                log::warn!("Could not save the record of {device_id}: {e}");
            }
        }
        self.notify();
    }

    /// Ends the device's session, keeping the record. False if not connected.
    pub fn disconnect(&self, device_id: &str) -> bool {
        match self.lock_sessions().get(device_id) {
            Some(e) => {
                let _ = e.cmd_tx.send(SessionCmd::End(EndKind::ClientGone, "disconnected from the host".into()));
                true
            }
            None => false,
        }
    }

    /// Ends the device's session (if any) and forgets it.
    pub fn forget(&self, device_id: &str) {
        if let Some(e) = self.lock_sessions().get(device_id) {
            let _ = e.cmd_tx.send(SessionCmd::End(EndKind::ClientGone, "device forgotten on the host".into()));
        }
        if let Err(e) = self.lock_store().forget(device_id) {
            log::warn!("Could not forget {device_id}: {e}");
        }
        self.notify();
    }

    /// Records a Hello: creates or refreshes the device's record (name,
    /// last-seen) and returns it. The role is untouched, so a new device has
    /// `role == None`.
    fn touch_record(&self, device_id: &str, name: &str) -> DeviceRecord {
        let mut store = self.lock_store();
        let mut rec = store.get(device_id).unwrap_or_else(|| DeviceRecord::new(device_id, name));
        if !name.is_empty() {
            rec.name = name.to_string();
        }
        rec.last_seen = unix_now();
        if let Err(e) = store.put(rec.clone()) {
            log::warn!("Could not save the device record: {e}");
        }
        rec
    }

    /// Persists `role` for a device (keeping its other options).
    fn save_role(&self, device_id: &str, role: Role) {
        let mut store = self.lock_store();
        if let Some(mut rec) = store.get(device_id) {
            rec.role = Some(role);
            if let Err(e) = store.put(rec) {
                log::warn!("Could not save the role for {device_id}: {e}");
            }
        }
    }

    /// Adds a session for `device_id`, replacing (and waiting, bounded, for) an
    /// older session of the same device. `Err(())` when the manager is full.
    async fn register(
        self: &Arc<Self>,
        device_id: &str,
        via_aoa: bool,
        effective_role: Role,
    ) -> Result<Registration, ()> {
        let old_done = {
            let map = self.lock_sessions();
            match map.get(device_id) {
                Some(old) => {
                    let _ = old.cmd_tx.send(SessionCmd::End(
                        EndKind::Replaced,
                        "replaced by a newer connection from the same device".into(),
                    ));
                    Some(old.done_rx.clone())
                }
                None if map.len() >= MAX_SESSIONS => return Err(()),
                None => None,
            }
        };
        if let Some(mut done) = old_done {
            log::info!("{device_id} reconnected; waiting for its old session to finish");
            let _ = tokio::time::timeout(REPLACE_WAIT, async { while done.changed().await.is_ok() {} }).await;
        }

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = watch::channel(());
        let gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        {
            let mut map = self.lock_sessions();
            if let Some(raced) = map.insert(
                device_id.to_string(),
                SessionEntry {
                    gen,
                    cmd_tx: cmd_tx.clone(),
                    done_rx,
                    via_aoa,
                    effective_role,
                    state: "Connecting".into(),
                    notice: None,
                    resolution: None,
                    live: None,
                },
            ) {
                // Another connection of the same device slipped in meanwhile.
                let _ = raced.cmd_tx.send(SessionCmd::End(EndKind::Replaced, "replaced".into()));
            }
        }
        self.notify();
        Ok(Registration {
            manager: self.clone(),
            device_id: device_id.to_string(),
            gen,
            cmd_rx: Some(cmd_rx),
            cmd_tx,
            _done_tx: done_tx,
        })
    }

    fn update_entry(&self, device_id: &str, gen: u64, f: impl FnOnce(&mut SessionEntry)) {
        {
            let mut map = self.lock_sessions();
            match map.get_mut(device_id) {
                Some(e) if e.gen == gen => f(e),
                _ => return,
            }
        }
        self.notify();
    }
}

/// Maps a role to what it captures. `None` for the input-only roles.
///
/// `Extend` and `PhonePrimary` ask for a virtual monitor of the phone's native
/// size and refresh; `Mirror` uses the remembered monitor (or lets the portal
/// ask); `MirrorWindow` lets the portal ask for a window.
pub fn target_for(role: Role, record: &DeviceRecord, hello: &Hello) -> Option<CaptureTarget> {
    match role {
        Role::Mirror => Some(CaptureTarget::Monitor { output: record.target_output.clone() }),
        Role::Extend | Role::PhonePrimary => Some(CaptureTarget::Virtual {
            width: hello.width as u32,
            height: hello.height as u32,
            refresh_mhz: hello.refresh_mhz,
        }),
        Role::MirrorWindow => Some(CaptureTarget::Window),
        Role::Tablet | Role::InputPad => None,
    }
}

/// The portal restore-token key for a device and role: each device keeps its own
/// grant per kind of source.
pub fn token_key_for(device_id: &str, role: Role) -> String {
    let kind = match role {
        Role::Mirror => "monitor",
        Role::Extend | Role::PhonePrimary => "virtual",
        Role::MirrorWindow => "window",
        Role::Tablet | Role::InputPad => "input",
    };
    format!("{device_id}/{kind}")
}

pub struct ServerController {
    stop_tx: watch::Sender<bool>,
}

impl ServerController {
    pub fn new(stop_tx: watch::Sender<bool>) -> Self {
        Self { stop_tx }
    }

    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

/// Name of the environment variable that selects which local interface the TCP
/// server listens on. See [`parse_bind_spec`].
pub const BIND_ENV_VAR: &str = "DISPLAYSWARM_BIND";

/// Resolves the TCP listen address from a `DISPLAYSWARM_BIND` spec string.
///
/// # Threat model / why loopback is the default
///
/// The TCP transport has **no authentication at all**. A `Hello` message
/// is the only thing a peer has to send to be admitted to a session, and from
/// that moment every input message arriving on the socket is handed straight
/// to the uinput (or Windows synthetic pointer) injector. There is no token, no TLS, no client identity check and
/// no per-event authorisation.
///
/// That means *anyone who can open a TCP connection to this port owns the
/// logged-in user's input devices and can see the screen.* Binding to
/// `0.0.0.0` therefore hands that capability to every device on the same
/// Wi-Fi - coffee-shop networks, hotel and conference networks, dorm LANs - and
/// to anything that can route to the host at all. A port scan is all it takes
/// to find. That is materially worse than what a commercial product ships.
///
/// So the default is loopback-only. Opening the socket to a network is a
/// deliberate act the operator has to opt into, and [`run_server`] logs a
/// prominent warning when they do.
///
/// # Accepted specs
///
/// | `DISPLAYSWARM_BIND`        | listen address                |
/// |------------------------|------------------------------|
/// | unset / `""`           | `127.0.0.1:<port>` (default) |
/// | `loopback`/`localhost`  | `127.0.0.1:<port>`           |
/// | `all` / `*` / `0.0.0.0` | `0.0.0.0:<port>`             |
/// | `1.2.3.4`               | `1.2.3.4:<port>`             |
/// | `1.2.3.4:9000`          | exactly `1.2.3.4:9000`       |
/// | `[::1]` / `[::1]:9000`   | IPv6 loopback literal        |
///
/// Bare addresses take the port from [`ServerConfig`]; an explicit `host:port`
/// overrides it. Hostnames are deliberately *not* accepted - only keywords and
/// IP literals - so a name lookup can never quietly redirect the listener.
/// Anything else is an error rather than a silent fallback: a typo in a
/// security-relevant setting must not quietly reopen the port.
pub fn parse_bind_spec(spec: &str, port: u16) -> Result<SocketAddr, String> {
    let trimmed = spec.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "" | "loopback" | "localhost" | "lo" => return Ok(SocketAddr::from(([127, 0, 0, 1], port))),
        "all" | "any" | "*" | "0.0.0.0" => return Ok(SocketAddr::from(([0, 0, 0, 0], port))),
        _ => {}
    }

    // Explicit "addr:port" (also covers the bracketed IPv6 forms).
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(addr);
    }

    // Bare IP literal. `[::1]` is not itself a valid `IpAddr`, so unwrap the
    // brackets first; `[::1]:9000` was already handled above.
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    match literal.parse::<IpAddr>() {
        Ok(ip) => Ok(SocketAddr::new(ip, port)),
        Err(_) => Err(format!(
            "invalid {BIND_ENV_VAR} value {trimmed:?}: expected 'loopback' (the secure \
             default), 'all' to expose the port on every interface, or an explicit address \
             such as '192.168.1.10' or '192.168.1.10:{port}'"
        )),
    }
}

/// [`parse_bind_spec`] applied to the process environment. A missing or
/// unparseable value is *not* silently downgraded to loopback: an operator who
/// explicitly asked to expose the port should be told their value was wrong,
/// not handed a server that silently only listens locally (or, worse, one that
/// silently listens on everything).
pub fn resolve_bind_addr(port: u16) -> Result<SocketAddr, String> {
    let spec = std::env::var(BIND_ENV_VAR).unwrap_or_default();
    parse_bind_spec(&spec, port)
}

pub async fn run_server<F>(
    config: ServerConfig,
    status_callback: F,
    stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String, String, String) + Send + Sync + 'static,
{
    run_server_with(config, status_callback, stop_rx, SessionManager::with_defaults()).await
}

/// [`run_server`] with an explicit [`SessionManager`], which the UI keeps a
/// handle to for the device list and role changes.
pub async fn run_server_with<F>(
    config: ServerConfig,
    status_callback: F,
    mut stop_rx: watch::Receiver<bool>,
    manager: Arc<SessionManager>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String, String, String) + Send + Sync + 'static,
{
    // Sessions are spawned, so the callback has to be shareable across tasks.
    let status_callback = Arc::new(status_callback);

    // A layout (primary display) left behind by a crashed run is put back
    // before anything new can change it. Blocking D-Bus work, so off the runtime.
    #[cfg(target_os = "linux")]
    let _ = tokio::task::spawn_blocking(|| {
        recover_leftovers();
        // An X11 virtual monitor (a RandR mode on a spare output) left lit.
        if crate::display::env::current_session() == crate::display::model::SessionType::X11 {
            crate::display::x11_virtual::recover_leftovers();
        }
    })
    .await;
    // The same for the virtual display driver's settings on Windows.
    #[cfg(windows)]
    let _ = tokio::task::spawn_blocking(crate::display::windows_caps::recover_leftovers).await;

    // Loopback by default. See the `parse_bind_spec` docs for the threat model:
    // the TCP transport is completely unauthenticated and every input event it
    // carries is injected into the real input stack, so listening on all
    // interfaces hands remote control of the desktop to the whole LAN.
    //
    // The TCP transport is TLS + pairing unless the dev flag says otherwise. Only
    // then is listening on the LAN by default sound: without DISPLAYSWARM_BIND set,
    // the secured listener takes every interface so phones can find it.
    let secure = !config.network.insecure_tcp;
    let bind_unset = std::env::var(BIND_ENV_VAR).map(|v| v.trim().is_empty()).unwrap_or(true);
    let addr = if secure && bind_unset {
        SocketAddr::from(([0, 0, 0, 0], config.port))
    } else {
        resolve_bind_addr(config.port)?
    };
    let is_loopback = addr.ip().is_loopback();
    let gate: Option<Arc<NetGate>> = if secure {
        let identity = match config.network.config_dir.clone().or_else(crate::transport::tls::default_config_dir) {
            Some(dir) => TlsIdentity::load_or_create(&dir, &host_name())?,
            None => {
                log::warn!("No config directory; using a TLS identity that lasts only for this run");
                TlsIdentity::generate(&host_name())?
            }
        };
        log::info!("TLS host fingerprint (SHA-256): {}", identity.fingerprint_hex());
        Some(Arc::new(NetGate::new(identity, manager.pairing().clone())?))
    } else {
        None
    };
    if secure {
        log::info!("TCP transport: TLS with pairing required");
    } else if is_loopback {
        log::info!(
            "{} is not set, so the TCP server is loopback-only. A phone on the LAN \
             cannot connect; use {}='all' (or an explicit address) to expose it.",
            BIND_ENV_VAR,
            BIND_ENV_VAR
        );
    }
    if !secure && is_loopback {
        log::warn!("{} is set: plain TCP without TLS or pairing (development only)", crate::transport::INSECURE_TCP_ENV);
    }
    if !secure && !is_loopback {
        // Deliberately duplicated on stderr: this is a security warning, and
        // `log::error!` disappears silently under an RUST_LOG that filters
        // errors out (or when logging is redirected to a file the operator has
        // not opened yet).
        let warning = format!(
            "SECURITY WARNING: DisplaySwarm is listening on {addr}, i.e. it is reachable from \
             the network. The TCP transport has NO AUTHENTICATION: anyone who can open a \
             connection to this port can see your screen and inject keystrokes, clicks and \
             stylus input with your privileges. Only do this on a network you fully trust."
        );
        log::error!("{warning}");
        eprintln!("{warning}");
    }

    // Match the socket family to the requested address; an explicit `::1` or
    // `[::]` bind needs new_v6(), and new_v4() would refuse the address.
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    let _ = socket.set_reuseaddr(true);
    // NOTE: deliberately NOT setting SO_REUSEPORT. With it, a second DisplaySwarm
    // instance can bind the same port and the kernel load-balances incoming
    // connections between the two, so half of them silently vanish into a
    // process that is not the one you think you are talking to. Refuse to start
    // instead, so a second instance fails loudly.
    socket.bind(addr)?;
    let listener = socket.listen(128)?;
    log::info!(
        "DisplaySwarm Server listening on TCP {} ({}) and polling for Native USB AOA",
        addr,
        if secure {
            "TLS + pairing"
        } else if is_loopback {
            "loopback only"
        } else {
            "NETWORK EXPOSED, unauthenticated"
        }
    );
    status_callback(
        format!("Listening on {} / USB AOA", addr),
        if secure {
            "Waiting for connection (Wi-Fi with pairing, or USB)...".into()
        } else if is_loopback {
            "Waiting for connection (loopback or USB)...".into()
        } else {
            "LAN EXPOSED - no authentication! (loopback or USB)".into()
        },
        "Idle".into(),
    );

    // Discovery: phones find the host through `_displayswarm._tcp`. Kept alive for
    // the whole run; dropping it withdraws the advertisement.
    let _advertisement = match (&gate, config.network.advertise_mdns) {
        (Some(g), true) => {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(config.port);
            match crate::transport::mdns::Advertisement::start(&host_name(), port, &g.identity.fingerprint(), true) {
                Ok(a) => Some(a),
                Err(e) => {
                    log::warn!("mDNS advertisement failed ({e}); phones must be given the address");
                    None
                }
            }
        }
        _ => None,
    };

    let mut usb_scan_interval = tokio::time::interval(Duration::from_millis(1500));
    usb_scan_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Every session is tracked here so that stopping the server can wait for
    // them. Without this, `run_server` returned while sessions (and the capturer
    // that owns the portal screencast session) were still alive, so the desktop's
    // "screen is being recorded" indicator stayed on after "Stop".
    let mut sessions: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                // `Err` means the controller was dropped; that is also a stop, and
                // must break rather than spin on an arm that is always ready.
                if changed.is_err() || *stop_rx.borrow() {
                    log::info!("Server stopped by controller.");
                    break;
                }
            }
            // Reap finished sessions so the set does not grow without bound.
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, peer_addr)) => {
                        let _ = stream.set_nodelay(true);
                        log::info!("Client connected via TCP from {}", peer_addr);

                        // No early rejection here: a client beyond MAX_SESSIONS is
                        // turned away gracefully in `handle_client_stream`, which
                        // closes the connection with a real error instead of an
                        // abrupt TCP reset.
                        let gate = gate.clone();
                        let connect = async move {
                            match gate {
                                Some(g) => g.accept(stream).await.map(|(r, w, _device)| (r, w)),
                                None => Ok(TransportStream::Tcp(stream).into_split()),
                            }
                        };
                        spawn_client_session(
                            &mut sessions,
                            manager.clone(),
                            connect,
                            config.clone(),
                            status_callback.clone(),
                            stop_rx.clone(),
                            "TCP client",
                            None,
                        );
                    }
                    Err(e) => {
                        log::error!("TCP accept failed: {:?}", e);
                    }
                }
            }
            _ = usb_scan_interval.tick() => {
                // Check if an Android accessory is available or can be switched via AOA
                match connect_aoa_device(&config.aoa_config).await {
                    Ok(Some(aoa_stream)) => {
                        if manager.aoa_active() {
                            // A live AOA session already owns the USB interface;
                            // claiming it again would tear down the stream.
                            log::debug!("Skipping AOA attach: a USB session is already streaming");
                            drop(aoa_stream);
                            continue;
                        }
                        log::info!("Client connected via Native USB AOA 2.0");
                        let reset = aoa_stream.reset_handle();
                        let (reader, writer) = TransportStream::Aoa(aoa_stream).into_split();

                        spawn_client_session(
                            &mut sessions,
                            manager.clone(),
                            async move { Ok((reader, writer)) },
                            config.clone(),
                            status_callback.clone(),
                            stop_rx.clone(),
                            "USB AOA client",
                            Some(reset),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::debug!("USB AOA probe notice: {:?}", e);
                    }
                }
            }
        }
    }

    // Stop the listener first so nothing new can connect while we tear down.
    drop(listener);

    // Every session watches the same stop signal, so they are already winding
    // down (Bye to the phone, capturer dropped). Wait for that, but bounded: a
    // wedged session must not be able to hang the Stop button forever.
    let drained = tokio::time::timeout(SESSION_STOP_TIMEOUT, async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        log::warn!(
            "{} session(s) did not stop within {:?}; aborting them",
            sessions.len(),
            SESSION_STOP_TIMEOUT
        );
        sessions.abort_all();
        while sessions.join_next().await.is_some() {}
    }
    log::info!("Server stopped.");

    Ok(())
}

/// How long `run_server` waits for sessions to finish after a stop before
/// aborting them.
/// How long a freshly claimed accessory may stay silent before the host logs a hint.
pub const AOA_HELLO_WARN_AFTER: Duration = Duration::from_secs(20);

pub const SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a client session is in its life, as the UI should present it.
///
/// Exists because the old code reported "Client Connected" the instant a
/// transport attached, *before* any `Hello` had arrived. On USB AOA that
/// fires for every probe of a phone that is merely plugged in (the app not even
/// running), so the UI flapped between "connected" and "waiting" forever. A
/// state machine makes "connected" mean one thing: the phone's app said hello.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    /// Hello received; HelloAck sent; capture/encode being set up.
    Handshaking,
    /// The user has not yet answered the screen-share permission dialog.
    AwaitingPermission,
    /// Frames are flowing. Numbers are the last one-second window.
    Streaming {
        fps: u32,
        kbps: u32,
        /// Phone-reported glass-to-glass latency, once `ClientStats` arrives.
        latency_ms: Option<u32>,
        encoder: String,
    },
    /// A role without video (drawing tablet, input pad) is running.
    InputOnly,
    /// The session ended (or capture failed); the string says why.
    Disconnected(String),
}

impl SessionState {
    /// A short phrase for the device list row.
    pub fn short(&self) -> String {
        match self {
            SessionState::Handshaking => "Connecting".into(),
            SessionState::AwaitingPermission => "Waiting for screen-share permission".into(),
            SessionState::Streaming { fps, kbps, .. } => {
                format!("Streaming, {fps} fps, {:.1} Mbps", *kbps as f32 / 1000.0)
            }
            SessionState::InputOnly => "Input only".into(),
            SessionState::Disconnected(why) => format!("Disconnected: {why}"),
        }
    }

    /// Renders the state as the `(status, client)` pair of the UI callback.
    pub fn to_status(&self, label: &str) -> (String, String) {
        match self {
            SessionState::Handshaking => ("Client Connected".into(), format!("{label} (negotiating)")),
            SessionState::AwaitingPermission => (
                "Waiting for screen-share permission".into(),
                format!("{label}: approve the screen-share dialog on this PC"),
            ),
            SessionState::Streaming { fps, kbps, latency_ms, encoder } => {
                let mut client = format!("{label}: {fps} fps, {:.1} Mbps, {encoder}", *kbps as f32 / 1000.0);
                if let Some(ms) = latency_ms {
                    client.push_str(&format!(", {ms} ms"));
                }
                ("Streaming".into(), client)
            }
            SessionState::InputOnly => ("Input only".into(), format!("{label}: input only, no video")),
            SessionState::Disconnected(reason) => {
                ("Waiting for reconnect...".into(), format!("Disconnected: {reason}"))
            }
        }
    }
}

/// Session-state sink: `(state, input_summary)`.
type ReportFn = dyn Fn(SessionState, String) + Send + Sync;

/// Resolves once a stop has been requested (or the controller has gone away,
/// which is also a stop).
async fn wait_for_stop(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Runs a client session on its own task so that a stalled or long-lived
/// connection can never stop `run_server` from accepting new clients, re-probing
/// USB, or honouring the stop signal.
///
/// The task is spawned into `sessions` so `run_server` can wait for it on stop.
///
/// Nothing is reported to the UI until the Hello arrives (see
/// [`SessionState`]); a session that ends without ever having been reported is
/// invisible, so a probe of a device that never ran the app cannot flap the UI.
fn spawn_client_session<R, W, F, C>(
    sessions: &mut JoinSet<()>,
    manager: Arc<SessionManager>,
    connect: C,
    config: ServerConfig,
    status_callback: Arc<F>,
    mut stop_rx: watch::Receiver<bool>,
    label: &'static str,
    aoa_reset: Option<AoaResetHandle>,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
    F: Fn(String, String, String) + Send + Sync + 'static,
    // Produces the byte stream. For the network this is where TLS and pairing
    // run, on the session's own task, so a slow phone never blocks the accept loop.
    C: std::future::Future<Output = std::io::Result<(R, W)>> + Send + 'static,
{
    // NOTE: the session slot is *not* claimed here. It is claimed in
    // `handle_client_stream` once the Hello actually arrives, so that a
    // device sitting on the bus with no client attached can never lock out a
    // real client that is trying to connect.
    let aoa_reset_present = aoa_reset.is_some();
    sessions.spawn(async move {
        let announced = Arc::new(AtomicBool::new(false));
        let report = {
            let announced = announced.clone();
            let status_callback = status_callback.clone();
            move |state: SessionState, input: String| {
                announced.store(true, Ordering::SeqCst);
                let (status, client) = state.to_status(label);
                status_callback(status, client, input);
            }
        };

        // A phone in accessory mode whose app is not running never says hello.
        // We do not reset its USB connection (that re-opens the permission
        // dialog and re-enumerates the device); the user opens the app or replugs.
        let watchdog = {
            let announced = announced.clone();
            async move {
                if aoa_reset.is_some() {
                    tokio::time::sleep(AOA_HELLO_WARN_AFTER).await;
                    if !announced.load(Ordering::SeqCst) {
                        log::warn!(
                            "{label}: no Hello within {AOA_HELLO_WARN_AFTER:?}; the DisplaySwarm app is probably \
                             not running or the USB permission dialog is unanswered. Open the app or replug the phone."
                        );
                    }
                }
                std::future::pending::<()>().await
            }
        };
        let outcome = tokio::select! {
            outcome = async {
                let mut stop_probe = stop_rx.clone();
                let (reader, writer) = tokio::select! {
                    c = connect => c?,
                    _ = wait_for_stop(&mut stop_probe) => return Ok("server stopping".to_string()),
                };
                handle_client_stream(reader, writer, &config, &manager, aoa_reset_present, &mut stop_rx, &report).await
            } => outcome,
            _ = watchdog => unreachable!("the watchdog never completes"),
        };
        let stopping = *stop_rx.borrow();
        let was_announced = announced.load(Ordering::SeqCst);

        let reason = match outcome {
            Ok(reason) => reason,
            Err(e) => {
                if was_announced {
                    log::warn!("{} disconnected: {:?}", label, e);
                } else {
                    // Never said hello: a bus probe or a stray connection.
                    log::debug!("{} ended before Hello: {:?}", label, e);
                }
                e.to_string()
            }
        };

        // On a server stop the UI is driven by `run_server` returning; a
        // per-session "Disconnected" here would only race with it.
        if was_announced && !stopping {
            let others = manager.session_count();
            if others == 0 {
                report(SessionState::Disconnected(reason), "Idle".into());
            } else {
                // Another device is still connected: do not replace its status
                // with this one's disconnect.
                status_callback(
                    "Streaming".into(),
                    format!("{others} device(s) connected"),
                    "Idle".into(),
                );
            }
        }

        // Settle delay before re-scanning USB so the device/USB stack can reset
        // cleanly. Interruptible, so it never delays a stop.
        if !stopping {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
                _ = wait_for_stop(&mut stop_rx) => {}
            }
        }
    });
}

/// What the first recognisable message from a new peer turned out to be.
#[derive(Debug)]
enum HelloOutcome {
    /// A protocol v2 Hello.
    Hello(Hello),
    /// A protocol v1 `VMCT` ClientHello: an app older than this host.
    LegacyV1,
    /// A Hello framed with another numeric protocol version.
    WrongVersion(u8),
}

/// Grows `buf` to at least `n` bytes from `reader`.
async fn fill<R: AsyncReadExt + Unpin>(reader: &mut R, buf: &mut Vec<u8>, n: usize) -> std::io::Result<()> {
    if buf.len() < n {
        let old = buf.len();
        buf.resize(n, 0);
        reader.read_exact(&mut buf[old..]).await?;
    }
    Ok(())
}

/// Finds the peer's Hello, bounded by [`HANDSHAKE_TIMEOUT`].
///
/// The endpoints can hold unconsumed bytes from an earlier session (input the
/// app sent while no host was reading), so the Hello is not necessarily the
/// first thing on the wire. Whole v2 frames that are not a Hello are skipped by
/// their length; anything else is skipped a byte at a time until a frame
/// header lines up. A stale Hello from an earlier app run is adopted, which is
/// harmless: the app re-sends its Hello until it gets an answer, and any extra
/// copy is ignored once the session runs.
///
/// A v1 `VMCT` ClientHello and a Hello framed with another version are
/// reported rather than skipped, so the caller can tell the phone clearly.
async fn read_hello<R>(reader: &mut R) -> Result<HelloOutcome, Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin,
{
    let scan = async {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut discarded = 0usize;
        loop {
            if discarded > HELLO_SCAN_LIMIT {
                return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                    "no Hello: the peer sent too much unrecognised data".into(),
                );
            }
            fill(reader, &mut buf, MAGIC.len()).await?;
            if buf[..2] != MAGIC {
                buf.remove(0);
                discarded += 1;
                continue;
            }
            fill(reader, &mut buf, HEADER_SIZE).await?;
            if &buf[2..5] == b"CT\x01" {
                // v1 ClientHello: 12 bytes, "VMCT" + type 1 + version...
                fill(reader, &mut buf, 12).await?;
                return Ok(HelloOutcome::LegacyV1);
            }
            let header = Header::parse(buf[..HEADER_SIZE].try_into().unwrap())?;
            if header.version == VERSION && header.check().is_ok() {
                let end = HEADER_SIZE + header.len as usize;
                fill(reader, &mut buf, end).await?;
                if header.channel == CH_CONTROL && header.msg_type == MSG_HELLO && header.flags == 0 {
                    if let Ok(Message::Hello(hello)) =
                        Message::decode(header.channel, header.msg_type, &buf[HEADER_SIZE..end])
                    {
                        if discarded > 0 {
                            log::info!("Discarded {discarded} stale byte(s) before the Hello");
                        }
                        return Ok(HelloOutcome::Hello(hello));
                    }
                }
                // A whole frame from an earlier session: drop it by its length.
                buf.drain(..end);
                discarded += end;
                continue;
            }
            // Versions are small numbers; v1's letters were handled above.
            if header.version != VERSION
                && header.version < b'A'
                && header.channel == CH_CONTROL
                && header.msg_type == MSG_HELLO
            {
                return Ok(HelloOutcome::WrongVersion(header.version));
            }
            buf.remove(0);
            discarded += 1;
        }
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, scan).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(e)) => {
            log::debug!("Handshake failed: {e:?}");
            Err(e)
        }
        Err(_) => {
            log::warn!("No Hello within {:?}; abandoning session", HANDSHAKE_TIMEOUT);
            Err(format!("Handshake timed out after {:?}", HANDSHAKE_TIMEOUT).into())
        }
    }
}

/// Reads messages from the phone and acts on them until the stream ends, the
/// session ends, or the phone breaks the protocol.
///
/// Input goes to the injector through the [`InputRouter`]; everything else is a
/// control message. Framing is exact in v2, so there is no resynchronisation:
/// a malformed frame ends the session with a protocol error.
async fn pump_messages<R>(
    reader: R,
    injector: &mut dyn InputInjector,
    shared: &SessionShared,
    ctrl_tx: &mpsc::UnboundedSender<Message>,
    cmd_tx: Option<&mpsc::UnboundedSender<SessionCmd>>,
    mut region_rx: Option<watch::Receiver<Option<OutputRegion>>>,
    svc_tx: Option<&mpsc::UnboundedSender<ServiceEvent>>,
) where
    R: AsyncReadExt + Unpin,
{
    let mut messages = MessageReader::new(reader);
    let mut router = InputRouter::default();
    while shared.is_active() {
        let msg = match messages.next().await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                log::warn!("Protocol error from the phone: {e}");
                shared.end(EndKind::Protocol, format!("protocol error: {e}"));
                return;
            }
            Err(e) => {
                log::debug!("Input stream ended: {e:?}");
                return;
            }
        };
        let t_recv_us = host_monotonic_us();
        shared.touch();
        if msg.channel() == CH_INPUT {
            // The capture's region can change while the session runs (a role
            // switch, a monitor move). Applying it here, just before the next
            // event, keeps the injector on this task, which owns it.
            if let Some(rx) = region_rx.as_mut() {
                if rx.has_changed().unwrap_or(false) {
                    let region = *rx.borrow_and_update();
                    injector.set_output_region(region);
                }
            }
            let n = router.route(&msg, injector);
            shared.injected.fetch_add(n, Ordering::Relaxed);
        } else if is_service_message(&msg) {
            match svc_tx {
                Some(tx) => {
                    let _ = tx.send(ServiceEvent::Message(msg));
                }
                None => log::debug!("Ignoring message type {:#04x}: no services", msg.msg_type()),
            }
        } else {
            handle_control_message(msg, shared, ctrl_tx, cmd_tx, t_recv_us);
        }
    }
}

/// Audio, clipboard and file messages, and `BatteryStatus`, go to the
/// session's services rather than the control handler.
fn is_service_message(msg: &Message) -> bool {
    matches!(msg.channel(), wire::CH_AUDIO | wire::CH_CLIPBOARD | wire::CH_FILE)
        || matches!(msg, Message::BatteryStatus(_) | Message::AudioLatency { .. } | Message::ServiceState { .. })
}

#[cfg(test)]
mod service_routing_tests {
    use super::*;

    #[test]
    fn service_state_reaches_the_services() {
        assert!(is_service_message(&Message::ServiceState { enabled: 3 }));
        assert!(is_service_message(&Message::AudioLatency { latency_us: 1 }));
        assert!(!is_service_message(&Message::Heartbeat));
    }
}

/// What the services task receives.
enum ServiceEvent {
    Message(Message),
    Role(Role),
    Settings(DeviceSettings),
}

/// Runs the session's services until `rx` closes, then drops them (their
/// teardown) off the runtime.
async fn services_task(
    manager: Arc<SessionManager>,
    ctx: crate::services::ServiceCtx,
    mut rx: mpsc::UnboundedReceiver<ServiceEvent>,
) {
    let mut services = (manager.backends.services)(&ctx);
    if !services.is_empty() {
        log::info!("Session services: {}", services.iter().map(|s| s.name()).collect::<Vec<_>>().join(", "));
    }
    while let Some(ev) = rx.recv().await {
        match ev {
            ServiceEvent::Message(m) => {
                let t = m.msg_type();
                if !crate::services::dispatch(&mut services, m) {
                    log::debug!("Ignoring message type {t:#04x}: no service wants it");
                }
            }
            ServiceEvent::Role(r) => services.iter_mut().for_each(|s| s.on_role(r)),
            ServiceEvent::Settings(st) => services.iter_mut().for_each(|s| s.on_settings(&st)),
        }
    }
    let _ = tokio::task::spawn_blocking(move || drop(services)).await;
}

// ===========================================================================
// Session runtime: shared state, the three-stage video pipeline, teardown
// ===========================================================================

/// Time without any inbound message after which the phone is declared gone.
/// The app sends a heartbeat every 500 ms.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(3);

/// How often the sender emits a `Heartbeat` when nothing else is going out, so
/// the phone can tell "static screen" from "dead link".
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Encoded frames buffered between the encoder thread and the sender. Small on
/// purpose: a deep queue is latency the phone would just watch go by.
const FRAME_QUEUE_DEPTH: usize = 2;

/// Bulk (clipboard, file) messages queued for the sender before a service's
/// `send_bulk` waits.
const BULK_QUEUE_DEPTH: usize = 4;

/// How long a session waits for the sender to flush its final `Bye` before
/// giving up on a peer that has stopped reading.
const BYE_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// How long teardown waits for the capture/encoder threads (and therefore the
/// capturer's `Drop`, which ends the portal screencast) to finish.
const THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Why a session ended; picks the `Bye` reason code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndKind {
    ServerStopping,
    ClientGone,
    /// The same device connected again; this older session is superseded.
    Replaced,
    Error,
    /// The phone broke the framing or message rules.
    Protocol,
}

/// State shared by every part of one session (input task, sender task, capture
/// and encoder threads, monitor loop). All fields are atomics or tiny mutexes so
/// none of the hot paths can block on another.
struct SessionShared {
    /// Cleared when the session should wind down, for any reason. Every thread
    /// and task polls it, which is what makes them all exit promptly.
    active: AtomicBool,
    /// Set by the phone's KeyframeRequest, or by the host after dropping an
    /// encoded frame; consumed by the encoder thread.
    keyframe: AtomicBool,
    epoch: Instant,
    /// Milliseconds since `epoch` at which the last inbound message completed.
    last_rx_ms: AtomicU64,
    /// Bitrate the phone asked for with `SetBitrate`, in kbps; 0 = none
    /// pending. Consumed by the encoder thread.
    bitrate_request: AtomicU32,
    injected: AtomicU64,
    /// Latest `Stats` message from the phone.
    client_stats: Mutex<Option<Stats>>,
    end: Mutex<Option<(EndKind, String)>>,
    // One-second window counters (swapped to zero by the monitor).
    sent_frames: AtomicU64,
    sent_bytes: AtomicU64,
    enc_us_sum: AtomicU64,
    enc_count: AtomicU64,
    // Cumulative.
    dropped_frames: AtomicU64,
}

impl SessionShared {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
            keyframe: AtomicBool::new(true), // a new client needs an IDR first
            epoch: Instant::now(),
            last_rx_ms: AtomicU64::new(0),
            bitrate_request: AtomicU32::new(0),
            injected: AtomicU64::new(0),
            client_stats: Mutex::new(None),
            end: Mutex::new(None),
            sent_frames: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            enc_us_sum: AtomicU64::new(0),
            enc_count: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Ends the session. The first reason wins, so a later consequence of the
    /// teardown (a write error on the closing socket) cannot overwrite the cause.
    fn end(&self, kind: EndKind, reason: impl Into<String>) {
        let mut slot = self.end.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some((kind, reason.into()));
        }
        self.active.store(false, Ordering::SeqCst);
    }

    fn end_reason(&self) -> (EndKind, String) {
        self.end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or((EndKind::ClientGone, "session ended".into()))
    }

    /// Records that bytes just arrived from the client.
    fn touch(&self) {
        self.last_rx_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// True when nothing has arrived from the phone for `timeout`.
    fn link_dead(&self, timeout: Duration) -> bool {
        let now = self.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_rx_ms.load(Ordering::Relaxed)) > timeout.as_millis() as u64
    }
}

/// Clears `SessionShared::active` when dropped, so cancelling (aborting) a
/// session future still stops its threads.
struct ActiveGuard(Arc<SessionShared>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::SeqCst);
    }
}

/// Latest-frame slot between the capture thread and the encoder thread.
///
/// A slot rather than a queue: if the encoder is behind, the older unconsumed
/// frame is simply replaced, so latency never accumulates and stale pixels are
/// never encoded.
struct FrameSlot {
    frame: Mutex<Option<Frame>>,
    ready: Condvar,
}

impl FrameSlot {
    fn new() -> Self {
        Self { frame: Mutex::new(None), ready: Condvar::new() }
    }

    fn put(&self, frame: Frame) {
        *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        self.ready.notify_one();
    }

    /// Takes the newest frame, waiting up to `timeout` for one.
    fn take(&self, timeout: Duration) -> Option<Frame> {
        let guard = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |f| f.is_none())
            .unwrap_or_else(|e| e.into_inner());
        guard.take()
    }

    fn wake(&self) {
        self.ready.notify_all();
    }
}

/// All the packets of one encoded frame, stamped with the frame's capture time.
/// Frames cross the encoder->sender channel whole so a partial frame can never
/// reach the wire.
struct EncodedFrame {
    timestamp_us: u64,
    packets: Vec<EncodedPacket>,
}

/// Capture stage: pulls new frames from the capturer into the slot.
///
/// Runs on its own std thread because `next_frame` blocks. Pacing sleeps
/// *before* asking for the next frame (rather than discarding early ones), so
/// the frame obtained afterwards is always the latest and a screen that goes
/// still right after an update still delivers its final state. The pacing
/// interval is 90% of the nominal one, so capture-clock jitter cannot make it
/// skip every other frame at 60 fps.
///
/// The capturer is dropped here, on this thread, never on a runtime worker: its
/// `Drop` joins the PipeWire thread and closes the portal session, which can
/// block.
fn capture_loop(
    mut capturer: Box<dyn ScreenCapturer>,
    slot: Arc<FrameSlot>,
    shared: Arc<SessionShared>,
    state_cell: Arc<Mutex<CaptureState>>,
    target_fps: u32,
    run: Arc<AtomicBool>,
    region_tx: Arc<watch::Sender<Option<OutputRegion>>>,
) {
    let interval = Duration::from_micros(900_000 / target_fps.max(1) as u64);
    let mut next_allowed = Instant::now();
    let mut errors = 0u64;
    let mut last_region: Option<OutputRegion> = None;

    while shared.is_active() && run.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < next_allowed {
            std::thread::sleep((next_allowed - now).min(Duration::from_millis(10)));
            continue;
        }

        let result = capturer.next_frame(Duration::from_millis(100));

        // Publish lifecycle changes for the monitor (permission dialog etc.).
        let state = capturer.state();
        {
            let mut cell = state_cell.lock().unwrap_or_else(|e| e.into_inner());
            if *cell != state {
                *cell = state;
            }
        }

        // Where this capture sits on the desktop, for the input mapping.
        // Only the capturer's own changes are published, so a corrected region
        // set after make_primary is not overwritten by the reported one.
        let region = capturer.output_region();
        if region != last_region {
            last_region = region;
            region_tx.send_replace(region);
        }

        match result {
            Ok(Some(frame)) => {
                slot.put(frame);
                next_allowed = Instant::now() + interval;
            }
            Ok(None) => {} // nothing new: nothing to encode
            Err(e) => {
                errors += 1;
                if errors == 1 || errors % 100 == 0 {
                    log::warn!("Frame capture failed ({} so far): {:?}", errors, e);
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        }
    }

    drop(capturer);
    log::info!("Capturer released");
}

/// Encode stage: newest frame in, whole encoded frame out.
///
/// If the sender is behind and the bounded channel is full, the *entire*
/// encoded frame is dropped and a keyframe forced, because later P-frames would
/// reference the one the phone never got. Partial frames are never sent.
fn encoder_loop(
    mut encoder: Box<dyn VideoEncoder>,
    slot: Arc<FrameSlot>,
    tx: mpsc::Sender<EncodedFrame>,
    shared: Arc<SessionShared>,
    run: Arc<AtomicBool>,
) {
    while shared.is_active() && run.load(Ordering::Relaxed) {
        let Some(frame) = slot.take(Duration::from_millis(100)) else {
            continue;
        };

        if shared.keyframe.swap(false, Ordering::AcqRel) {
            encoder.request_keyframe();
        }
        let kbps = shared.bitrate_request.swap(0, Ordering::AcqRel);
        if kbps > 0 {
            log::info!("Phone requested {kbps} kbps");
            encoder.set_bitrate(kbps);
        }

        let started = Instant::now();
        let packets = match encoder.encode(&frame) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Frame encode failed: {:?}", e);
                std::thread::sleep(Duration::from_millis(16));
                continue;
            }
        };
        let elapsed = started.elapsed();
        shared.enc_us_sum.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        shared.enc_count.fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "encode {:.2} ms ({} packets, {} bytes)",
            elapsed.as_secs_f64() * 1000.0,
            packets.len(),
            packets.iter().map(|p| p.data.len()).sum::<usize>()
        );

        if packets.is_empty() {
            continue;
        }

        match tx.try_send(EncodedFrame { timestamp_us: frame.timestamp_us, packets }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                shared.keyframe.store(true, Ordering::Release);
                log::debug!("Sender behind: dropped one encoded frame, forcing a keyframe");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
    log::debug!("Encoder thread exiting");
}

/// Send stage: the only owner of the writer.
///
/// Control messages always go first. Video is framed into [`VIDEO_FRAGMENT`]
/// pieces and sent one fragment per loop turn, with every control message
/// queued in the meantime written in between, so a `Pong`, `HostState` or `Bye`
/// waits for at most one fragment rather than a whole keyframe. A `Pong`'s
/// reply timestamp is refreshed at the last moment, to keep the phone's
/// clock-offset estimate tight.
///
/// Writes are not flushed per fragment: the USB transport keeps several
/// transfers in flight, and a flush waits for all of them. The sender flushes
/// once it has nothing left to send.
async fn sender_loop<W>(
    mut writer: W,
    mut frames: mpsc::Receiver<EncodedFrame>,
    mut ctrl: mpsc::UnboundedReceiver<Message>,
    mut bulk: mpsc::Receiver<Message>,
    shared: Arc<SessionShared>,
) where
    W: AsyncWriteExt + Unpin,
{
    // Framed bytes of the bulk message (clipboard, file) being sent.
    let mut bulk_bytes: Vec<u8> = Vec::new();
    let mut bulk_pos = 0usize;
    let mut bulk_open = true;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut frame_index: u32 = 0;
    // Framed bytes of the video frame being sent, and how far into it we are.
    let mut video: Vec<u8> = Vec::new();
    let mut video_pos = 0usize;
    let mut video_payload = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    // The encoder thread exits (dropping its sender) as soon as the session is
    // marked ended, which can be before the Bye is queued. Keep serving control
    // messages until then; leaving here would drop the Bye.
    let mut frames_open = true;

    let push_ctrl = |mut m: Message, buf: &mut Vec<u8>| -> bool {
        if let Message::Pong { t_reply_us, .. } = &mut m {
            *t_reply_us = host_monotonic_us();
        }
        buf.extend_from_slice(&m.encode());
        matches!(m, Message::Bye { .. })
    };

    'send: loop {
        buf.clear();
        let mut is_bye = false;

        if video_pos >= video.len() && bulk_pos >= bulk_bytes.len() {
            // Idle: wait for anything to send.
            tokio::select! {
                biased;
                msg = ctrl.recv() => match msg {
                    Some(m) => is_bye = push_ctrl(m, &mut buf),
                    None => break 'send,
                },
                m = bulk.recv(), if bulk_open => match m {
                    Some(m) => {
                        bulk_bytes = m.encode_fragmented(VIDEO_FRAGMENT);
                        bulk_pos = 0;
                    }
                    None => bulk_open = false,
                },
                _ = heartbeat.tick() => buf.extend_from_slice(&Message::Heartbeat.encode()),
                frame = frames.recv(), if frames_open => match frame {
                    Some(f) => {
                        video.clear();
                        video_pos = 0;
                        video_payload = 0;
                        for p in &f.packets {
                            let prefix = wire::video_frame_prefix(CODEC_H264, p.frame_type, frame_index, f.timestamp_us);
                            wire::write_frames(CH_VIDEO, MSG_VIDEO_FRAME, &[&prefix, &p.data], VIDEO_FRAGMENT, &mut video);
                            video_payload += p.data.len() as u64;
                        }
                        frame_index = frame_index.wrapping_add(1);
                    }
                    None => frames_open = false,
                },
            }
        }

        // Everything else already queued on the control channel goes first.
        while !is_bye {
            match ctrl.try_recv() {
                Ok(m) => is_bye = push_ctrl(m, &mut buf),
                Err(_) => break,
            }
        }

        // Then at most one video fragment.
        let mut frame_done = false;
        if !is_bye && video_pos < video.len() {
            let len = u32::from_be_bytes(video[video_pos + 6..video_pos + HEADER_SIZE].try_into().unwrap()) as usize;
            let end = video_pos + HEADER_SIZE + len;
            buf.extend_from_slice(&video[video_pos..end]);
            video_pos = end;
            frame_done = video_pos == video.len();
        }

        // Then at most one bulk fragment (clipboard, files): after video, so a
        // large transfer slows down instead of stalling the picture.
        if !is_bye {
            if bulk_pos >= bulk_bytes.len() && bulk_open {
                if let Ok(m) = bulk.try_recv() {
                    bulk_bytes = m.encode_fragmented(VIDEO_FRAGMENT);
                    bulk_pos = 0;
                }
            }
            if bulk_pos < bulk_bytes.len() {
                let len = u32::from_be_bytes(bulk_bytes[bulk_pos + 6..bulk_pos + HEADER_SIZE].try_into().unwrap()) as usize;
                let end = bulk_pos + HEADER_SIZE + len;
                buf.extend_from_slice(&bulk_bytes[bulk_pos..end]);
                bulk_pos = end;
            }
        }

        if buf.is_empty() {
            continue;
        }
        if let Err(e) = writer.write_all(&buf).await {
            log::info!("Client disconnected (write error: {:?})", e);
            shared.end(EndKind::ClientGone, format!("write failed: {e}"));
            break;
        }
        if (video_pos >= video.len() && bulk_pos >= bulk_bytes.len()) || is_bye {
            if let Err(e) = writer.flush().await {
                log::info!("Client disconnected (flush error: {:?})", e);
                shared.end(EndKind::ClientGone, format!("flush failed: {e}"));
                break;
            }
        }
        if frame_done {
            shared.sent_frames.fetch_add(1, Ordering::Relaxed);
            shared.sent_bytes.fetch_add(video_payload, Ordering::Relaxed);
        }
        if is_bye {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Acts on one decoded non-input message from the phone.
fn handle_control_message(
    msg: Message,
    shared: &SessionShared,
    ctrl_tx: &mpsc::UnboundedSender<Message>,
    cmd_tx: Option<&mpsc::UnboundedSender<SessionCmd>>,
    t_recv_us: u64,
) {
    match msg {
        Message::SetRole { role } => match cmd_tx {
            Some(tx) => {
                let _ = tx.send(SessionCmd::PhoneSetRole(role));
            }
            None => log::debug!("Ignoring SetRole {role}: no session to apply it to"),
        },
        Message::SetQuality { quality } => match cmd_tx {
            Some(tx) => {
                let _ = tx.send(SessionCmd::PhoneSetQuality(quality));
            }
            None => log::debug!("Ignoring SetQuality {quality}: no session to apply it to"),
        },
        Message::Resize { width, height, .. } => match cmd_tx {
            Some(tx) => {
                let _ = tx.send(SessionCmd::Resize { width, height });
            }
            None => log::debug!("Ignoring Resize {width}x{height}: no session to apply it to"),
        },
        Message::Ping { id, t_send_us } => {
            let _ = ctrl_tx.send(Message::Pong {
                id,
                t_ping_us: t_send_us,
                t_recv_us,
                t_reply_us: host_monotonic_us(),
            });
        }
        Message::KeyframeRequest => shared.keyframe.store(true, Ordering::Release),
        Message::Heartbeat => {} // liveness already noted
        Message::Bye { reason, text } => {
            log::info!("Client said goodbye (reason {reason}): {text}");
            shared.end(EndKind::ClientGone, "client closed the session");
        }
        Message::Stats(stats) => {
            *shared.client_stats.lock().unwrap_or_else(|e| e.into_inner()) = Some(stats);
        }
        Message::SetBitrate { kbps } => {
            shared.bitrate_request.store(kbps.clamp(500, 200_000), Ordering::Release)
        }
        // A Hello the app re-sent before our HelloAck reached it.
        Message::Hello(_) => {}
        Message::Unknown { channel, msg_type, .. } => {
            log::debug!("Skipping unknown message type {msg_type:#04x} on channel {channel}")
        }
        // Host->phone messages arriving from the phone are meaningless; ignore.
        _ => {}
    }
}

/// Best-effort name of this machine, for the phone's UI.
fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "DisplaySwarm host".into())
}

/// Writes a refusal the phone can show (HelloAck with `status`, then Bye), and
/// reports it to the UI when `show_in_ui`. A refusal issued while another
/// session is streaming must not be shown: it would replace that session's
/// status with "Disconnected". Returns the error that ends the session.
async fn refuse<W>(
    writer: &mut W,
    report: &ReportFn,
    show_in_ui: bool,
    status: u8,
    bye_reason: u8,
    message: String,
) -> Box<dyn std::error::Error + Send + Sync>
where
    W: AsyncWriteExt + Unpin,
{
    let mut out = Message::HelloAck(HelloAck {
        status,
        host_name: host_name(),
        message: message.clone(),
        ..Default::default()
    })
    .encode();
    out.extend(Message::Bye { reason: bye_reason, text: message.clone() }.encode());
    let _ = writer.write_all(&out).await;
    let _ = writer.flush().await;
    log::warn!("Refused the client: {message}");
    if show_in_ui {
        report(SessionState::Disconnected(message.clone()), "Idle".into());
    }
    message.into()
}

/// Handles one client from Hello to teardown.
async fn handle_client_stream<R, W>(
    mut reader: R,
    mut writer: W,
    config: &ServerConfig,
    manager: &Arc<SessionManager>,
    via_aoa: bool,
    stop_rx: &mut watch::Receiver<bool>,
    report: &ReportFn,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    // 1. Read the Hello, bounded by HANDSHAKE_TIMEOUT so a silent peer cannot
    //    hold the slot forever, and abandoned immediately on a server stop.
    let outcome = tokio::select! {
        h = read_hello(&mut reader) => h?,
        _ = wait_for_stop(stop_rx) => return Ok("server stopping".into()),
    };
    let hello = match outcome {
        HelloOutcome::Hello(hello) => hello,
        HelloOutcome::LegacyV1 => {
            // A v1 app only understands v1: end its handshake and show it why.
            let _ = writer
                .write_all(&wire::legacy_v1_rejection(
                    "This PC runs a newer DisplaySwarm host. Update the DisplaySwarm app on this phone.",
                ))
                .await;
            let _ = writer.flush().await;
            let why = "The DisplaySwarm app on the phone is outdated (protocol v1); update it";
            log::warn!("{why}");
            if manager.session_count() == 0 {
                report(SessionState::Disconnected(why.into()), "Idle".into());
            }
            return Err(why.into());
        }
        HelloOutcome::WrongVersion(v) => {
            let why = format!(
                "The phone app speaks DisplaySwarm protocol v{v}, this host speaks v{VERSION}. \
                 Install matching versions."
            );
            let show = manager.session_count() == 0;
            return Err(refuse(&mut writer, report, show, HELLO_VERSION_MISMATCH, BYE_VERSION_MISMATCH, why).await);
        }
    };

    // The capturer and encoder are sized from these; a zero or absurd size
    // from an unauthenticated peer must not reach them (and 65535 would wrap to
    // 0 when aligned up to 16).
    let show = manager.session_count() == 0;
    if !(MIN_PANEL..=MAX_PANEL).contains(&hello.width) || !(MIN_PANEL..=MAX_PANEL).contains(&hello.height) {
        let why = format!("The phone reported an unusable screen size {}x{}", hello.width, hello.height);
        return Err(refuse(&mut writer, report, show, HELLO_REJECTED, BYE_ERROR, why).await);
    }
    log::info!(
        "Hello from {:?} ({}, app {}): {}x{} @ {:.2} Hz, {} dpi, codecs {:#04x}, features {:#x}",
        hello.device_name,
        hello.device_id,
        hello.app_version,
        hello.width,
        hello.height,
        hello.refresh_mhz as f64 / 1000.0,
        hello.density_dpi,
        hello.codecs,
        hello.features
    );
    // The role this device is used as. Unknown devices stream as Mirror until
    // a role is chosen; the phone is told with ROLE_UNSET so it shows its chooser.
    let anonymous = hello.device_id.is_empty();
    let device_id = if anonymous {
        format!("anonymous-{}", manager.next_gen.fetch_add(1, Ordering::Relaxed))
    } else {
        hello.device_id.clone()
    };
    let record = if anonymous {
        DeviceRecord::new(device_id.clone(), hello.device_name.clone())
    } else {
        manager.touch_record(&device_id, &hello.device_name)
    };
    let remembered = record.role;
    let mut role = remembered.unwrap_or(Role::Mirror);
    // A remembered role this desktop cannot do runs as Mirror for now; the saved
    // role stays, so it works again on a desktop that can.
    let mut degraded_notice = None;
    if role != Role::Mirror {
        if let Some(why) = manager.role_support_async(role).await.reason().map(str::to_string) {
            if manager.role_support_async(Role::Mirror).await.is_yes() {
                log::warn!("Remembered role {} is not available here ({why}); running as Mirror", role.label());
                degraded_notice = Some(format!("{} is not available here: {why} Running as Mirror.", role.label()));
                role = Role::Mirror;
            }
        }
    }
    if role.streams_video() && hello.codecs & CODEC_H264 == 0 {
        let why = "The phone cannot decode H.264, the only codec this host sends".to_string();
        return Err(refuse(&mut writer, report, show, HELLO_REJECTED, BYE_ERROR, why).await);
    }

    // 2. Register the session. A reconnect of the same device replaces its old
    //    session; only a full manager turns a device away.
    let Ok(mut registration) = manager.register(&device_id, via_aoa, role).await else {
        let why = format!("This PC already serves the maximum of {MAX_SESSIONS} devices");
        return Err(refuse(&mut writer, report, false, HELLO_BUSY, BYE_ERROR, why).await);
    };
    let resolution = Resolution {
        width: hello.width as u32,
        height: hello.height as u32,
        refresh_mhz: hello.refresh_mhz,
    };
    manager.update_entry(&device_id, registration.gen, |e| {
        e.resolution = Some(resolution);
        e.notice = degraded_notice.take();
    });

    // Only now is there a real client to show.
    report(SessionState::Handshaking, "Negotiating...".into());

    // 3. Accept.
    let aligned_width = (hello.width as u32 + 15) & !15;
    let aligned_height = (hello.height as u32 + 15) & !15;
    let fps = record.settings.fps(config.target_fps);
    let plan = record.settings.video_quality.plan(config.bitrate_kbps, crate::encoder::wired_qp());
    let qp = plan.qp;
    log::info!(
        "Video ({:?}): {} over {}, {fps} fps",
        record.settings.video_quality,
        qp.map_or(format!("{} kbps {}", plan.kbps, if plan.adaptive { "ceiling, adaptive" } else { "target" }), |q| format!("constant quality QP {q}")),
        if via_aoa { "USB" } else { "the network" }
    );
    let ack = Message::HelloAck(HelloAck {
        status: HELLO_OK,
        width: aligned_width as u16,
        height: aligned_height as u16,
        fps: fps as u16,
        codec: CODEC_H264,
        features: crate::services::negotiated_features(hello.features, &record.settings),
        // The role running, which is Mirror when the remembered one was not possible.
        role: remembered.map(|_| role.to_wire()).unwrap_or(ROLE_UNSET),
        host_name: host_name(),
        message: String::new(),
    });
    writer.write_all(&ack.encode()).await?;
    writer.flush().await?;
    log::info!(
        "HelloAck sent: {}x{} @ {} fps, role {}",
        aligned_width,
        aligned_height,
        fps,
        remembered.map(Role::label).unwrap_or("not chosen yet (streaming as Mirror)")
    );

    // 4. Initialize the input injector; the capture pipeline is built by the
    //    session itself, since a role change rebuilds it.
    let injector = (manager.backends.injector)(aligned_width, aligned_height);
    let cmd_rx = registration.cmd_rx.take().expect("fresh registration has its command receiver");

    run_streaming_session(
        reader,
        writer,
        SessionSetup {
            env: SessionEnv {
                manager: manager.clone(),
                device_id,
                hello,
                fps,
                bitrate_kbps: plan.kbps,
                adaptive: config.network.adaptive_bitrate && plan.adaptive,
                qp,
                target_kbps: config.bitrate_kbps,
                allow_adaptive: config.network.adaptive_bitrate,
                gen: registration.gen,
                restart_tx: registration.cmd_tx.clone(),
            },
            record,
            role,
            injector,
            cmd_rx,
            cmd_tx: registration.cmd_tx.clone(),
        },
        stop_rx,
        report,
    )
    .await
}

/// Runs the streaming phase of a session and tears it down completely.
///
/// Pipeline (all stages overlap): capture thread -> [latest-frame slot] ->
/// encoder thread -> [bounded channel] -> async sender task -> writer. The input
/// task feeds the injector and the control handlers. This function is the
/// monitor: it watches for stop, liveness, capture-state changes, and reports
/// statistics once a second.
///
/// # Teardown (why Stop used to leave the screen-recording icon on)
///
/// The old code awaited the input task on exit. That task sat in a read from a
/// still-connected phone, so the function never returned and the capturer (which
/// owns the portal screencast session) was never dropped. Now the input task is
/// *aborted*, a `Bye` is sent, and the capturer is dropped on its own thread
/// which is joined (with a timeout) before this function returns.
async fn run_streaming_session<R, W>(
    mut reader: R,
    writer: W,
    setup: SessionSetup,
    stop_rx: &mut watch::Receiver<bool>,
    outer_report: &ReportFn,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let SessionSetup { mut env, record, role, mut injector, mut cmd_rx, cmd_tx } = setup;
    let shared = Arc::new(SessionShared::new());
    // Aborting/cancelling this future must still stop the threads.
    let _guard = ActiveGuard(shared.clone());

    // Every state report also lands in the device list.
    let report = |state: SessionState, input: String| {
        env.manager.update_entry(&env.device_id, env.gen, |e| {
            e.state = state.short();
            e.live = match &state {
                SessionState::Streaming { fps, kbps, latency_ms, encoder } => {
                    Some(LiveStats { fps: *fps, kbps: *kbps, latency_ms: *latency_ms, encoder: encoder.clone() })
                }
                _ => None,
            };
        });
        outer_report(state, input);
    };

    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(FRAME_QUEUE_DEPTH);
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<Message>();
    let (region_tx, region_rx) = watch::channel::<Option<OutputRegion>>(None);
    let region_tx = Arc::new(region_tx);

    let mut live = Live { effective: role, pipeline: None, primary_attempted: false, panel: (env.hello.width, env.hello.height) };
    live.pipeline = start_pipeline(&env, live.panel, role, &record, &shared, &frame_tx, &region_tx)?;
    let (bulk_tx, bulk_rx) = mpsc::channel::<Message>(BULK_QUEUE_DEPTH);
    let mut sender_task = tokio::spawn(sender_loop(writer, frame_rx, ctrl_rx, bulk_rx, shared.clone()));

    let (svc_tx, svc_rx) = mpsc::unbounded_channel::<ServiceEvent>();
    let settings = env.manager.record(&env.device_id).unwrap_or_else(|| record.clone()).settings;
    let svc_ctx = crate::services::ServiceCtx {
        device_id: env.device_id.clone(),
        device_name: env.hello.device_name.clone(),
        features: crate::services::negotiated_features(env.hello.features, &settings),
        settings,
        role,
        out: crate::services::ServiceOut::new(ctrl_tx.clone(), bulk_tx),
        quality: {
            let shared = shared.clone();
            crate::services::QualityHandle::new(env.bitrate_kbps, move |kbps| {
                shared.bitrate_request.store(kbps.clamp(500, 200_000), Ordering::Release)
            })
        },
    };
    let services = tokio::spawn(services_task(env.manager.clone(), svc_ctx, svc_rx));
    let mut svc_role = role;

    let input_task = {
        let shared = shared.clone();
        let ctrl_tx = ctrl_tx.clone();
        let cmd_tx = cmd_tx.clone();
        let region_rx = region_rx.clone();
        let svc_tx = svc_tx.clone();
        tokio::spawn(async move {
            pump_messages(&mut reader, injector.as_mut(), &shared, &ctrl_tx, Some(&cmd_tx), Some(region_rx), Some(&svc_tx)).await;
            shared.end(EndKind::ClientGone, "client disconnected");
        })
    };
    let mut region_watch = region_rx;

    // ---- Monitor loop ------------------------------------------------------
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut window_start = Instant::now();
    let mut last_injected: u64 = 0;
    let mut abr = env.adaptive.then(|| crate::transport::bitrate::BitrateController::new(env.bitrate_kbps));
    let mut last_reported_state: Option<CaptureState> = None;
    let mut input_only_reported = false;

    loop {
        tokio::select! {
            _ = wait_for_stop(stop_rx) => {
                shared.end(EndKind::ServerStopping, "server stopping");
                break;
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SessionCmd::End(kind, why) => {
                        shared.end(kind, why);
                        break;
                    }
                    SessionCmd::SetRole(role) => {
                        if let Err(e) = apply_role(&env, &mut live, role, &record, &shared, &frame_tx, &region_tx, &ctrl_tx).await {
                            shared.end(EndKind::Error, e);
                            break;
                        }
                    }
                    SessionCmd::Resize { width, height } => {
                        if let Err(e) = apply_resize(&env, &mut live, (width, height), &record, &shared, &frame_tx, &region_tx).await {
                            shared.end(EndKind::Error, e);
                            break;
                        }
                    }
                    SessionCmd::PhoneSetQuality(v) => match crate::devices::VideoQuality::from_wire(v) {
                        Some(q) => {
                            let plan = q.plan(env.target_kbps, crate::encoder::wired_qp());
                            if plan.qp != env.qp || plan.kbps != env.bitrate_kbps || (env.allow_adaptive && plan.adaptive) != env.adaptive {
                                log::info!("Picture quality {q:?}: {}", plan.qp.map_or(format!("{} kbps", plan.kbps), |x| format!("constant quality QP {x}")));
                                env.manager.set_video_quality(&env.device_id, q);
                                env.qp = plan.qp;
                                env.bitrate_kbps = plan.kbps;
                                env.adaptive = env.allow_adaptive && plan.adaptive;
                                abr = env.adaptive.then(|| crate::transport::bitrate::BitrateController::new(env.bitrate_kbps));
                                if let Err(e) = restart_pipeline(&env, &mut live, &record, &shared, &frame_tx, &region_tx).await {
                                    shared.end(EndKind::Error, e);
                                    break;
                                }
                            } else {
                                env.manager.set_video_quality(&env.device_id, q);
                            }
                        }
                        None => log::warn!("The phone asked for unknown picture quality {v}"),
                    },
                    SessionCmd::Settings(s) => {
                        let _ = svc_tx.send(ServiceEvent::Settings(s));
                    }
                    SessionCmd::PhoneSetRole(v) => match Role::from_wire(v) {
                        Some(role) => {
                            if let Err(e) = apply_role(&env, &mut live, role, &record, &shared, &frame_tx, &region_tx, &ctrl_tx).await {
                                shared.end(EndKind::Error, e);
                                break;
                            }
                        }
                        None => {
                            log::warn!("The phone asked for unknown role {v}; keeping {}", live.effective.label());
                            let _ = ctrl_tx.send(Message::SetRole { role: live.effective.to_wire() });
                        }
                    },
                }
                if live.effective != svc_role {
                    svc_role = live.effective;
                    let _ = svc_tx.send(ServiceEvent::Role(svc_role));
                }
                // A new pipeline reports its own state from scratch.
                last_reported_state = None;
                input_only_reported = false;
                continue;
            }
            _ = tick.tick() => {}
        }
        if !shared.is_active() {
            break;
        }
        if shared.link_dead(LIVENESS_TIMEOUT) {
            log::warn!("No data from the client for {:?}; declaring the link dead", LIVENESS_TIMEOUT);
            shared.end(EndKind::Error, "client stopped responding");
            break;
        }

        // Phone as main screen: once the virtual monitor's place is known,
        // make it primary. The guard lives as long as the pipeline.
        if live.effective == Role::PhonePrimary && !live.primary_attempted {
            let region = *region_watch.borrow_and_update();
            if let (Some(region), true) = (region, live.pipeline.is_some()) {
                live.primary_attempted = true;
                let panel_off = env.manager.record(&env.device_id).map(|r| r.panel_off).unwrap_or(record.panel_off);
                let manager = env.manager.clone();
                // Only one phone may be the primary display: make_primary starts
                // by recovering any journal, which would clobber another's layout.
                let outcome = match env.manager.claim_primary(&env.device_id) {
                    None => Err("another device is already the primary display".to_string()),
                    Some(claim) => {
                        let attempt = tokio::task::spawn_blocking(move || (manager.backends.primary)(region, panel_off));
                        match tokio::time::timeout(PRIMARY_TIMEOUT, attempt).await {
                            Ok(Ok(Ok((guard, actual)))) => Ok((PrimaryHold { guard, _claim: claim }, actual)),
                            Ok(Ok(Err(e))) => Err(e),
                            Ok(Err(e)) => Err(format!("internal error: {e}")),
                            Err(_) => Err("timed out".to_string()),
                        }
                    }
                };
                match outcome {
                    Ok((hold, actual)) => {
                        if let Some(p) = live.pipeline.as_mut() {
                            p.primary = Some(hold);
                        }
                        // The layout may have shifted (panel off puts the phone at
                        // the origin); the injector must map onto where it is now.
                        region_tx.send_replace(Some(actual));
                        log::info!("The phone is now the primary display at {actual:?}");
                    }
                    Err(why) => {
                        // Keep streaming, as a plain extended monitor.
                        log::warn!("Could not make the phone the primary display: {why}; continuing as Extend");
                        live.effective = Role::Extend;
                        let notice = format!("Phone as main screen failed ({why}); running as Extend");
                        env.manager.update_entry(&env.device_id, env.gen, |e| {
                            e.effective_role = Role::Extend;
                            e.notice = Some(notice);
                        });
                        let _ = ctrl_tx.send(Message::SetRole { role: Role::Extend.to_wire() });
                    }
                }
            }
        }

        // Input-only roles have no capture to watch.
        let Some(pipeline) = live.pipeline.as_ref() else {
            if !input_only_reported || window_start.elapsed() >= Duration::from_secs(1) {
                input_only_reported = true;
                window_start = Instant::now();
                report(SessionState::InputOnly, format!("{} input events", shared.injected.load(Ordering::Relaxed)));
            }
            continue;
        };
        let encoder_name = pipeline.encoder_name.clone();
        let capture_state = pipeline.capture_state.clone();

        // Capture lifecycle -> phone (HostState) and UI (SessionState).
        let state = capture_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if last_reported_state.as_ref() != Some(&state) {
            let (code, detail) = match &state {
                CaptureState::AwaitingPermission => (HOST_STATE_AWAITING_PERMISSION, String::new()),
                CaptureState::Live => (HOST_STATE_STREAMING, String::new()),
                CaptureState::Failed(why) => (HOST_STATE_CAPTURE_FAILED, why.clone()),
            };
            let _ = ctrl_tx.send(Message::HostState { state: code, detail });
            match &state {
                CaptureState::AwaitingPermission => {
                    report(SessionState::AwaitingPermission, "Idle".into())
                }
                CaptureState::Live => report(
                    SessionState::Streaming {
                        fps: 0,
                        kbps: 0,
                        latency_ms: None,
                        encoder: encoder_name.clone(),
                    },
                    format!("{} input events", shared.injected.load(Ordering::Relaxed)),
                ),
                CaptureState::Failed(why) => {
                    log::error!("Capture failed: {why}");
                    report(SessionState::Disconnected(format!("capture failed: {why}")), "Idle".into())
                }
            }
            last_reported_state = Some(state.clone());
        }

        // Once-per-second stats.
        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            window_start = Instant::now();
            let frames = shared.sent_frames.swap(0, Ordering::Relaxed);
            let bytes = shared.sent_bytes.swap(0, Ordering::Relaxed);
            let enc_us = shared.enc_us_sum.swap(0, Ordering::Relaxed);
            let enc_n = shared.enc_count.swap(0, Ordering::Relaxed);
            let dropped = shared.dropped_frames.load(Ordering::Relaxed);
            let total_injected = shared.injected.load(Ordering::Relaxed);
            let input_delta = total_injected - last_injected;
            last_injected = total_injected;
            let fps = frames as f64 / secs;
            let kbps = bytes as f64 * 8.0 / 1000.0 / secs;
            let avg_enc_ms = if enc_n > 0 { enc_us as f64 / enc_n as f64 / 1000.0 } else { 0.0 };
            let phone = *shared.client_stats.lock().unwrap_or_else(|e| e.into_inner());
            let latency_ms = phone.map(|s| s.present_latency_us / 1000);
            if let Some(ctl) = abr.as_mut() {
                if state == CaptureState::Live {
                    let next = ctl.update(crate::transport::bitrate::Sample {
                        latency_ms,
                        phone_dropped: phone.map(|s| s.frames_dropped).unwrap_or(0),
                        host_dropped: dropped,
                        rx_kbps: phone.map(|s| s.rx_kbps),
                        sent_kbps: Some(kbps as u32),
                    });
                    if let Some(kbps) = next {
                        log::info!("Adaptive bitrate: {kbps} kbps");
                        shared.bitrate_request.store(kbps, Ordering::Release);
                        let _ = ctrl_tx.send(Message::SetBitrate { kbps });
                    }
                }
            }

            log::info!(
                "Stream stats: {:.1} fps, {:.0} kbps, encoder {} avg {:.2} ms, dropped {}, \
                 input {:.0} ev/s ({} total){}",
                fps,
                kbps,
                encoder_name,
                avg_enc_ms,
                dropped,
                input_delta as f64 / secs,
                total_injected,
                match phone {
                    Some(s) => format!(", phone present {} ms, phone dropped {}", s.present_latency_us / 1000, s.frames_dropped),
                    None => String::new(),
                }
            );
            if state == CaptureState::Live {
                report(
                    SessionState::Streaming {
                        fps: fps.round() as u32,
                        kbps: kbps.round() as u32,
                        latency_ms,
                        encoder: encoder_name.clone(),
                    },
                    format!("{} input events", total_injected),
                );
            }
        }
    }

    // ---- Teardown ----------------------------------------------------------
    let (kind, reason) = shared.end_reason();
    log::info!("Session ending: {reason}");
    let bye_code = match kind {
        EndKind::ServerStopping => BYE_SERVER_STOPPING,
        EndKind::ClientGone | EndKind::Replaced => BYE_NORMAL,
        EndKind::Error => BYE_ERROR,
        EndKind::Protocol => BYE_PROTOCOL_ERROR,
    };
    // Tell the phone why. If the sender is already gone this just fails.
    let _ = ctrl_tx.send(Message::Bye { reason: bye_code, text: reason.clone() });
    if tokio::time::timeout(BYE_FLUSH_TIMEOUT, &mut sender_task).await.is_err() {
        log::warn!("Sender did not flush Bye within {:?}; aborting it", BYE_FLUSH_TIMEOUT);
        sender_task.abort();
    }

    // Never await the input task: it may be blocked in a read from a phone that
    // will never send again. Aborting drops the reader and injector.
    input_task.abort();
    let _ = input_task.await;

    // Closing the channel ends the services task, which drops the services
    // (removing their virtual devices).
    drop(svc_tx);
    if tokio::time::timeout(THREAD_JOIN_TIMEOUT, services).await.is_err() {
        log::warn!("Session services did not stop within {:?}", THREAD_JOIN_TIMEOUT);
    }

    // Wake the encoder, then join both threads off the runtime. Joining is what
    // guarantees the capturer (and its portal session) is really gone before we
    // report "stopped".
    if let Some(pipeline) = live.pipeline.take() {
        stop_pipeline(pipeline).await;
    }
    drop(frame_tx);

    Ok(reason)
}

/// Everything about a session that does not change while it runs.
struct SessionEnv {
    manager: Arc<SessionManager>,
    device_id: String,
    hello: Hello,
    fps: u32,
    bitrate_kbps: u32,
    /// Adapt the bitrate to the link (network transports only).
    adaptive: bool,
    /// Constant-quality QP, or `None` to encode at `bitrate_kbps`.
    qp: Option<u32>,
    /// The configured bitrate, which the quality modes derive their ceiling from.
    target_kbps: u32,
    /// Adaptive bitrate is enabled in the config.
    allow_adaptive: bool,
    /// The registration generation, so state updates from a superseded session
    /// cannot touch its replacement's row.
    gen: u64,
    /// Lets a capture helper ask the session loop to rebuild the pipeline.
    restart_tx: mpsc::UnboundedSender<SessionCmd>,
}

struct SessionSetup {
    env: SessionEnv,
    /// The device's record as of the Hello (fallback when the store has none).
    record: DeviceRecord,
    /// The role to start in.
    role: Role,
    injector: Box<dyn InputInjector>,
    cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
}

/// How long making the phone primary may take before it counts as failed.
const PRIMARY_TIMEOUT: Duration = Duration::from_secs(10);

/// The running role of a session.
struct Live {
    /// The role in effect (what the phone was last told).
    effective: Role,
    /// `None` for the input-only roles.
    pipeline: Option<Pipeline>,
    /// Whether `make_primary` has been tried for this pipeline.
    primary_attempted: bool,
    /// The phone's panel as last reported (Hello, then `Resize`), unaligned.
    /// The virtual monitor is sized from it and the encoder from it aligned up.
    panel: (u16, u16),
}

/// The encoder size for a panel: aligned up to 16.
fn aligned_size(panel: (u16, u16)) -> (u32, u32) {
    ((panel.0 as u32 + 15) & !15, (panel.1 as u32 + 15) & !15)
}

/// One capture -> encode pipeline. Rebuilt on a role change without touching
/// the transport: it has its own `run` flag, separate from the session's.
struct Pipeline {
    run: Arc<AtomicBool>,
    slot: Arc<FrameSlot>,
    capture_thread: std::thread::JoinHandle<()>,
    encoder_thread: std::thread::JoinHandle<()>,
    capture_state: Arc<Mutex<CaptureState>>,
    encoder_name: String,
    /// Restores the previous monitor layout when dropped (PhonePrimary).
    primary: Option<PrimaryHold>,
}

/// The primary-display guard plus this device's claim on the single primary
/// slot. Field order matters: the layout is restored, then the slot freed.
struct PrimaryHold {
    guard: Box<dyn std::any::Any + Send>,
    _claim: PrimaryClaim,
}

/// Held by the one session that is the primary display.
struct PrimaryClaim {
    manager: Arc<SessionManager>,
}

impl Drop for PrimaryClaim {
    fn drop(&mut self) {
        *self.manager.primary_owner.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Builds the pipeline for `role`, or `None` for a role without video.
fn start_pipeline(
    env: &SessionEnv,
    panel: (u16, u16),
    role: Role,
    record: &DeviceRecord,
    shared: &Arc<SessionShared>,
    frame_tx: &mpsc::Sender<EncodedFrame>,
    region_tx: &Arc<watch::Sender<Option<OutputRegion>>>,
) -> std::io::Result<Option<Pipeline>> {
    let hello = Hello { width: panel.0, height: panel.1, ..env.hello.clone() };
    let Some(target) = target_for(role, record, &hello) else {
        log::info!("Role {}: input only, no capture or video", role.label());
        if role == Role::Tablet {
            // Blocking layout query (a few ms); the capture factory above is too.
            let region = (env.manager.backends.tablet_region)();
            log::info!("Tablet maps onto {region:?}");
            region_tx.send_replace(region);
        }
        return Ok(None);
    };
    let key = token_key_for(&env.device_id, role);
    let (w, h) = aligned_size(panel);
    let backends = &env.manager.backends;
    // Where the phone's monitor cannot be told from a name (GNOME), it is the one
    // that appears after this snapshot.
    let baseline = if role == Role::Extend {
        let layout = (backends.layout)();
        crate::display::watch::needs_baseline(layout.kind()).then(|| layout.snapshot().ok()).flatten()
    } else {
        None
    };
    let capturer = (backends.capture)(&target, Some(&key), w, h);
    log::info!("Role {}: capture source {} ({:?})", role.label(), capturer.describe_source(), target);
    let encoder = (backends.encoder)(w, h, env.fps, env.bitrate_kbps, env.qp);

    let encoder_name = encoder.backend_name();
    let capture_state = Arc::new(Mutex::new(capturer.state()));
    let slot = Arc::new(FrameSlot::new());
    let run = Arc::new(AtomicBool::new(true));

    let capture_thread = {
        let (slot, shared, state) = (slot.clone(), shared.clone(), capture_state.clone());
        let (run, region_tx, fps) = (run.clone(), region_tx.clone(), env.fps);
        std::thread::Builder::new()
            .name("displayswarm-capture".into())
            .spawn(move || capture_loop(capturer, slot, shared, state, fps, run, region_tx))?
    };
    let encoder_thread = {
        let (slot, shared, run_enc, tx) = (slot.clone(), shared.clone(), run.clone(), frame_tx.clone());
        match std::thread::Builder::new()
            .name("displayswarm-encode".into())
            .spawn(move || encoder_loop(encoder, slot, tx, shared, run_enc))
        {
            Ok(t) => t,
            Err(e) => {
                run.store(false, Ordering::SeqCst);
                let _ = capture_thread.join();
                return Err(e);
            }
        }
    };
    // The desktop's own display switcher (KDE's F4) can move, mirror or disable the
    // phone's monitor.
    if role == Role::Extend {
        let layout = (backends.layout)();
        if let Some(ownership) =
            crate::display::watch::ownership_for(layout.kind(), &env.device_id, baseline.as_ref())
        {
            let tx = env.restart_tx.clone();
            crate::display::watch::spawn(
                layout,
                ownership,
                crate::display::reconcile::Intent::Extend,
                run.clone(),
                region_tx.clone(),
                // KWin drops the phone's monitor when the user picks "Unify outputs" (mirror) in
                // its display switcher. That is a request to see the main screen on the
                // phone, so the session becomes a Mirror.
                Box::new(move || {
                    let _ = tx.send(SessionCmd::SetRole(Role::Mirror));
                }),
            );
        }
    }
    // A new pipeline (and a new phone-side decoder state) needs an IDR first.
    shared.keyframe.store(true, Ordering::Release);
    Ok(Some(Pipeline { run, slot, capture_thread, encoder_thread, capture_state, encoder_name, primary: None }))
}

/// Stops a pipeline and waits (bounded) until its threads are gone, which is
/// what guarantees the capturer and its portal session are really released.
/// The primary-display layout is restored first.
async fn stop_pipeline(pipeline: Pipeline) {
    let Pipeline { run, slot, capture_thread, encoder_thread, primary, .. } = pipeline;
    // Restore the layout while the virtual monitor still exists: the guard goes
    // before the capture threads are asked to stop (blocking D-Bus, so off the
    // runtime).
    if primary.is_some() {
        let _ = tokio::time::timeout(PRIMARY_TIMEOUT, tokio::task::spawn_blocking(move || drop(primary))).await;
    }
    // Wake the encoder, then join both threads off the runtime.
    run.store(false, Ordering::SeqCst);
    slot.wake();
    let joined = tokio::task::spawn_blocking(move || {
        let _ = capture_thread.join();
        let _ = encoder_thread.join();
    });
    if tokio::time::timeout(THREAD_JOIN_TIMEOUT + PRIMARY_TIMEOUT, joined).await.is_err() {
        log::warn!("Capture/encoder threads did not exit within {:?}", THREAD_JOIN_TIMEOUT);
    }
}

/// Switches a running session to `role`: remembers it, tears the capture
/// pipeline down (and the primary-display guard with it), builds the new one
/// and tells the phone which role is now in effect. The transport is untouched.
///
/// A refusal (the phone cannot decode video, or the device is unknown to the
/// store) leaves the current role running and states it back. `Err` only when
/// the new pipeline cannot be built at all; the session then ends.
#[allow(clippy::too_many_arguments)]
async fn apply_role(
    env: &SessionEnv,
    live: &mut Live,
    role: Role,
    fallback_record: &DeviceRecord,
    shared: &Arc<SessionShared>,
    frame_tx: &mpsc::Sender<EncodedFrame>,
    region_tx: &Arc<watch::Sender<Option<OutputRegion>>>,
    ctrl_tx: &mpsc::UnboundedSender<Message>,
) -> Result<(), String> {
    if role.streams_video() && env.hello.codecs & CODEC_H264 == 0 {
        log::warn!("Refusing role {}: the phone cannot decode H.264", role.label());
        let _ = ctrl_tx.send(Message::SetRole { role: live.effective.to_wire() });
        return Ok(());
    }
    // A role this desktop cannot do is refused with the reason, and nothing changes
    // (the phone is told the role still in effect; the user sees the reason).
    if let Some(why) = env.manager.role_support_async(role).await.reason().map(str::to_string) {
        log::warn!("Refusing role {}: {why}", role.label());
        let notice = format!("{} is not available here: {why}", role.label());
        env.manager.update_entry(&env.device_id, env.gen, |e| e.notice = Some(notice));
        let _ = ctrl_tx.send(Message::SetRole { role: live.effective.to_wire() });
        return Ok(());
    }
    env.manager.save_role(&env.device_id, role);

    let unchanged = role == live.effective && live.pipeline.is_some() == role.streams_video();
    if !unchanged {
        log::info!("Switching {} from {} to {}", env.device_id, live.effective.label(), role.label());
        if let Some(old) = live.pipeline.take() {
            stop_pipeline(old).await;
        }
        region_tx.send_if_modified(|cur| cur.take().is_some());
        let record = env.manager.record(&env.device_id).unwrap_or_else(|| fallback_record.clone());
        live.pipeline = start_pipeline(env, live.panel, role, &record, shared, frame_tx, region_tx)
            .map_err(|e| format!("could not start capture for {}: {e}", role.label()))?;
        live.effective = role;
        live.primary_attempted = false;
    }
    env.manager.update_entry(&env.device_id, env.gen, |e| {
        e.effective_role = live.effective;
        e.notice = None;
    });
    // Every change is stated back, whichever side asked for it.
    let _ = ctrl_tx.send(Message::SetRole { role: live.effective.to_wire() });
    Ok(())
}

/// The phone's panel changed. When the role captures a virtual monitor, that
/// monitor (and the encoder) is rebuilt at the new size; other roles ignore it,
/// since they show an existing screen scaled to the stream. Sizes outside the
/// Hello's accepted range and repeats are ignored.
/// Rebuilds the capture/encode pipeline as it is (new encoder settings).
async fn restart_pipeline(
    env: &SessionEnv,
    live: &mut Live,
    fallback_record: &DeviceRecord,
    shared: &Arc<SessionShared>,
    frame_tx: &mpsc::Sender<EncodedFrame>,
    region_tx: &Arc<watch::Sender<Option<OutputRegion>>>,
) -> Result<(), String> {
    if live.pipeline.is_none() {
        return Ok(());
    }
    if let Some(old) = live.pipeline.take() {
        stop_pipeline(old).await;
    }
    region_tx.send_if_modified(|cur| cur.take().is_some());
    let record = env.manager.record(&env.device_id).unwrap_or_else(|| fallback_record.clone());
    live.pipeline = start_pipeline(env, live.panel, live.effective, &record, shared, frame_tx, region_tx)
        .map_err(|e| format!("could not rebuild the encoder: {e}"))?;
    live.primary_attempted = false;
    Ok(())
}

async fn apply_resize(
    env: &SessionEnv,
    live: &mut Live,
    panel: (u16, u16),
    fallback_record: &DeviceRecord,
    shared: &Arc<SessionShared>,
    frame_tx: &mpsc::Sender<EncodedFrame>,
    region_tx: &Arc<watch::Sender<Option<OutputRegion>>>,
) -> Result<(), String> {
    if !(MIN_PANEL..=MAX_PANEL).contains(&panel.0) || !(MIN_PANEL..=MAX_PANEL).contains(&panel.1) {
        log::warn!("Ignoring Resize to an unusable size {}x{}", panel.0, panel.1);
        return Ok(());
    }
    if panel == live.panel {
        return Ok(());
    }
    log::info!("Phone panel {}x{} -> {}x{}", live.panel.0, live.panel.1, panel.0, panel.1);
    live.panel = panel;
    // Every role that captures is rebuilt: a mirror is scaled into the panel's size, so
    // frames left at the old size would be stretched by the rotated phone.
    if live.pipeline.is_none() {
        return Ok(());
    }
    if let Some(old) = live.pipeline.take() {
        stop_pipeline(old).await;
    }
    region_tx.send_if_modified(|cur| cur.take().is_some());
    let record = env.manager.record(&env.device_id).unwrap_or_else(|| fallback_record.clone());
    live.pipeline = start_pipeline(env, live.panel, live.effective, &record, shared, frame_tx, region_tx)
        .map_err(|e| format!("could not rebuild capture at {}x{}: {e}", panel.0, panel.1))?;
    live.primary_attempted = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{Key, Mouse, Pen, PenSample, Touch, TouchPoint, ACTION_DOWN, ACTION_MOVE};
    use crate::protocol::{InputEvent, KeyEvent, INPUT_TYPE_STYLUS};

    /// Records what the dispatcher handed to the injector, so the whole input
    /// path can be asserted on without a `/dev/uinput` fd.
    #[derive(Default)]
    struct RecordingInjector {
        pointers: Vec<InputEvent>,
        keys: Vec<KeyEvent>,
    }

    impl InputInjector for RecordingInjector {
        fn inject(&mut self, event: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.pointers.push(event.clone());
            Ok(())
        }
        fn inject_key(&mut self, event: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.keys.push(event.clone());
            Ok(())
        }
    }

    fn pen(x: f32) -> Message {
        Message::Pen(Pen {
            action: ACTION_MOVE,
            tool: wire::PEN_TOOL_PEN,
            buttons: 0,
            samples: vec![PenSample { age_us: 0, x, y: 0.5, pressure: 0.5, tilt_x: 0.0, tilt_y: 0.0 }],
        })
    }

    fn key(code: u16, text: &str) -> Message {
        Message::Key(Key { action: ACTION_DOWN, key_code: code, scan_code: 0, meta: 0, text: text.into() })
    }

    fn stream(messages: &[Message]) -> Vec<u8> {
        messages.iter().flat_map(|m| m.encode()).collect()
    }

    /// Runs the dispatcher over `bytes` as a complete stream (the reader is then
    /// at EOF, which is how the pump terminates). Returns what was injected, the
    /// messages queued for the sender, and the shared state.
    async fn dispatch(bytes: Vec<u8>) -> (RecordingInjector, Vec<Message>, SessionShared) {
        let (mut client, host) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            client.write_all(&bytes).await.expect("write to duplex");
            client.shutdown().await.expect("shutdown duplex");
        });
        let shared = SessionShared::new();
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
        let mut injector = RecordingInjector::default();
        pump_messages(host, &mut injector, &shared, &ctrl_tx, None, None, None).await;
        writer.await.expect("writer task");
        drop(ctrl_tx);
        let mut out = Vec::new();
        while let Some(m) = ctrl_rx.recv().await {
            out.push(m);
        }
        (injector, out, shared)
    }

    // -----------------------------------------------------------------------
    // Message dispatch
    // -----------------------------------------------------------------------

    /// Input and control interleave freely: input reaches the injector, a Ping
    /// yields a Pong echoing the phone's send time, stats and keyframe requests
    /// land in the shared state.
    #[tokio::test]
    async fn test_input_and_control_interleave() {
        let (injector, ctrl, shared) = dispatch(stream(&[
            pen(0.1),
            Message::Ping { id: 7, t_send_us: 1234 },
            key(29, "a"),
            Message::Stats(Stats {
                frames_decoded: 10,
                frames_dropped: 1,
                decode_latency_us: 2000,
                present_latency_us: 35_000,
                rx_kbps: 9000,
            }),
            Message::KeyframeRequest,
            Message::SetBitrate { kbps: 6000 },
            Message::Heartbeat,
            Message::Touch(Touch { action: ACTION_DOWN, action_id: 0, points: vec![TouchPoint { id: 0, x: 0.5, y: 0.5, pressure: 1.0 }] }),
            Message::Mouse(Mouse { action: ACTION_DOWN, buttons: wire::MOUSE_BUTTON_PRIMARY, relative: false, x: 0.2, y: 0.2 }),
        ]))
        .await;

        assert_eq!(injector.pointers.len(), 3);
        assert_eq!(injector.pointers[0].event_type, INPUT_TYPE_STYLUS);
        assert_eq!(injector.keys.len(), 1);
        assert_eq!(shared.injected.load(Ordering::SeqCst), 4);
        assert_eq!(ctrl.len(), 1, "only the Ping needs a reply");
        match &ctrl[0] {
            Message::Pong { id, t_ping_us, t_recv_us, t_reply_us } => {
                assert_eq!((*id, *t_ping_us), (7, 1234));
                assert!(t_reply_us >= t_recv_us);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
        assert!(shared.keyframe.load(Ordering::SeqCst));
        assert_eq!(shared.bitrate_request.load(Ordering::SeqCst), 6000);
        let stats = shared.client_stats.lock().unwrap().unwrap();
        assert_eq!(stats.present_latency_us, 35_000);
        assert_eq!(stats.frames_dropped, 1);
    }

    /// Unknown types and not-yet-supported messages are skipped by length, so
    /// what follows is still read.
    #[tokio::test]
    async fn test_unknown_and_unsupported_messages_are_skipped() {
        let mut bytes = Vec::new();
        wire::write_frames(CH_CONTROL, 0x1F, &[b"VM\x02 fake header inside"], 64, &mut bytes);
        bytes.extend(Message::Clipboard { mime: "text/plain".into(), data: vec![b'x'; 70_000] }.encode());
        bytes.extend(stream(&[Message::SetRole { role: wire::ROLE_EXTEND }, pen(0.3)]));
        let (injector, ctrl, shared) = dispatch(bytes).await;
        assert_eq!(injector.pointers.len(), 1);
        assert!(ctrl.is_empty());
        assert!(shared.is_active(), "skipping is not an error");
    }

    /// A Bye from the phone ends the session; nothing after it is processed.
    #[tokio::test]
    async fn test_bye_ends_session() {
        let (injector, _, shared) =
            dispatch(stream(&[Message::Bye { reason: BYE_NORMAL, text: "bye".into() }, pen(0.1)])).await;
        assert!(!shared.is_active());
        assert!(injector.pointers.is_empty());
    }

    /// v2 framing is exact: garbage is a protocol error that ends the session,
    /// not something to resynchronise past.
    #[tokio::test]
    async fn test_garbage_is_a_protocol_error() {
        let mut bytes = stream(&[pen(0.1)]);
        bytes.extend_from_slice(b"garbage that is not a frame at all");
        bytes.extend(stream(&[pen(0.2)]));
        let (injector, _, shared) = dispatch(bytes).await;
        assert_eq!(injector.pointers.len(), 1);
        assert!(!shared.is_active());
        let (kind, reason) = shared.end_reason();
        assert_eq!(kind, EndKind::Protocol);
        assert!(reason.contains("protocol error"), "{reason}");
    }

    /// An empty stream is the normal way a session ends, and must not panic.
    #[tokio::test]
    async fn test_empty_stream() {
        let (injector, ctrl, shared) = dispatch(Vec::new()).await;
        assert!(injector.pointers.is_empty() && injector.keys.is_empty() && ctrl.is_empty());
        assert!(shared.is_active(), "a closed stream ends the session through the caller");
    }

    #[test]
    fn test_link_dead_after_silence() {
        let shared = SessionShared::new();
        shared.touch();
        std::thread::sleep(Duration::from_millis(30));
        assert!(shared.link_dead(Duration::from_millis(10)));
        shared.touch();
        assert!(!shared.link_dead(Duration::from_secs(1)));
    }

    // -----------------------------------------------------------------------
    // Handshake
    // -----------------------------------------------------------------------

    async fn hello_from(bytes: Vec<u8>) -> Result<HelloOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let (mut client, mut host) = tokio::io::duplex(64 * 1024);
        client.write_all(&bytes).await.unwrap();
        drop(client);
        read_hello(&mut host).await
    }

    fn sample_hello() -> Hello {
        Hello { width: 1600, height: 720, refresh_mhz: 60_000, codecs: CODEC_H264, device_name: "phone".into(), ..Default::default() }
    }

    /// Leftovers from an earlier session - whole frames and loose bytes - are
    /// skipped, and the Hello behind them is found.
    #[tokio::test]
    async fn test_hello_found_behind_stale_frames_and_junk() {
        let mut bytes = b"V junk VM".to_vec();
        bytes.extend(stream(&[pen(0.1), key(29, "VM\x02")]));
        bytes.extend(Message::Hello(sample_hello()).encode());
        match hello_from(bytes).await.unwrap() {
            HelloOutcome::Hello(h) => assert_eq!(h, sample_hello()),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn test_v1_client_hello_is_recognised() {
        let v1 = b"VMCT\x01\x01\x06\x40\x02\xD0\x3C\x01".to_vec();
        assert!(matches!(hello_from(v1).await.unwrap(), HelloOutcome::LegacyV1));
    }

    #[tokio::test]
    async fn test_other_version_hello_is_recognised() {
        let mut bytes = Message::Hello(sample_hello()).encode();
        bytes[2] = 3;
        assert!(matches!(hello_from(bytes).await.unwrap(), HelloOutcome::WrongVersion(3)));
    }

    #[tokio::test]
    async fn test_stream_ending_before_hello_is_an_error() {
        assert!(hello_from(stream(&[pen(0.1)])).await.is_err());
    }

    /// A full manager refuses one more device with HelloAck(BUSY) and a Bye the
    /// phone can show, without touching the live sessions' UI state.
    #[tokio::test]
    async fn test_full_manager_refuses_with_reason() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut phones = Vec::new();
        for i in 0..MAX_SESSIONS {
            let mut p = Phone::connect(&manager, hello_for(&format!("dev{i}"))).await;
            p.expect(|m| matches!(m, Message::HelloAck(_))).await;
            phones.push(p);
        }
        assert_eq!(manager.session_count(), MAX_SESSIONS);
        let mut extra = Phone::connect(&manager, hello_for("one-too-many")).await;
        match extra.expect(|m| matches!(m, Message::HelloAck(_))).await {
            Message::HelloAck(a) => {
                assert_eq!(a.status, HELLO_BUSY);
                assert!(a.message.contains("maximum"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(extra.expect(|m| matches!(m, Message::Bye { .. })).await, Message::Bye { reason: BYE_ERROR, .. }));
        assert!(extra.task.await.unwrap().is_err());
        assert_eq!(manager.session_count(), MAX_SESSIONS);
        for p in phones {
            p.finish().await;
        }
        assert_eq!(manager.session_count(), 0);
    }

    /// A Hello with a zero or absurd panel size is refused before anything is
    /// sized from it (65535 would wrap to 0 when aligned up to 16).
    #[tokio::test]
    async fn test_unusable_panel_size_is_refused() {
        for (w, h) in [(0u16, 720u16), (1600, 0), (65535, 720), (1600, 9000)] {
            let (mut phone, host) = tokio::io::duplex(64 * 1024);
            let (host_read, host_write) = tokio::io::split(host);
            let hello = Hello { width: w, height: h, ..sample_hello() };
            phone.write_all(&Message::Hello(hello).encode()).await.unwrap();
            let (_stop_tx, mut stop_rx) = watch::channel(false);
            let report: Box<ReportFn> = Box::new(|_, _| {});
            let manager = test_manager(&Arc::new(Probe::default()), None, true);
            let result =
                handle_client_stream(host_read, host_write, &ServerConfig::default(), &manager, false, &mut stop_rx, &*report).await;
            assert!(result.is_err(), "{w}x{h}");
            let mut r = MessageReader::new(phone);
            match r.next().await.unwrap() {
                Message::HelloAck(a) => assert_eq!(a.status, HELLO_REJECTED, "{w}x{h}"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// The whole point of DEFECT 1: with no `DISPLAYSWARM_BIND` set, the server must
    /// be reachable only from this machine. `0.0.0.0` here means anyone on the
    /// LAN can inject input into the host, because the transport has no auth.
    #[test]
    fn test_bind_defaults_to_loopback() {
        for spec in ["", "  ", "loopback", "LOOPBACK", "localhost", "LocalHost", "lo"] {
            let addr = parse_bind_spec(spec, 9999).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
            assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 9999)), "spec {spec:?}");
            assert!(addr.ip().is_loopback(), "spec {spec:?} must stay on loopback");
        }
    }

    /// Opting in to a network-exposed listener. `all` is the only shorthand that
    /// may produce a non-loopback address; everything else has to name an
    /// address explicitly.
    #[test]
    fn test_bind_opt_in_specs() {
        for spec in ["all", "ALL", " * ", "any", "0.0.0.0"] {
            let addr = parse_bind_spec(spec, 9999).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
            assert_eq!(addr, SocketAddr::from(([0, 0, 0, 0], 9999)), "spec {spec:?}");
            assert!(!addr.ip().is_loopback(), "spec {spec:?} is expected to expose");
        }

        // Bare address: inherits the configured port.
        assert_eq!(
            parse_bind_spec("192.168.1.10", 9999).unwrap(),
            "192.168.1.10:9999".parse::<SocketAddr>().unwrap()
        );
        // Address + port: overrides the configured port.
        assert_eq!(
            parse_bind_spec("192.168.1.10:1234", 9999).unwrap(),
            "192.168.1.10:1234".parse::<SocketAddr>().unwrap()
        );
        // Loopback spelled as a literal is still loopback.
        assert_eq!(
            parse_bind_spec("127.0.0.1", 9999).unwrap(),
            "127.0.0.1:9999".parse::<SocketAddr>().unwrap()
        );
    }

    /// IPv6 has to work too, including the bracketed literal without a port
    /// (which is not a valid `IpAddr` and so needs the brackets stripped).
    #[test]
    fn test_bind_ipv6_literals() {
        assert_eq!(
            parse_bind_spec("[::1]", 9999).unwrap(),
            "[::1]:9999".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_bind_spec("[::1]:8080", 9999).unwrap(),
            "[::1]:8080".parse::<SocketAddr>().unwrap()
        );
        // Wildcard IPv6.
        assert_eq!(
            parse_bind_spec("[::]", 9999).unwrap(),
            "[::]:9999".parse::<SocketAddr>().unwrap()
        );
        assert!(parse_bind_spec("[::]", 9999).unwrap().ip().is_unspecified());

        // `::1:8080` is genuinely ambiguous: it is a legal IPv6 *address*
        // (`::1:8080`) and could also be read as "::1 on port 8080". It is
        // parsed as the address, inheriting the configured port, which is the
        // documented rule for bare literals. Brackets remove the ambiguity,
        // which is why the two cases above are the ones to actually use.
        assert_eq!(
            parse_bind_spec("::1:8080", 9999).unwrap(),
            SocketAddr::new("::1:8080".parse::<IpAddr>().unwrap(), 9999)
        );
    }

    /// A typo in a security-relevant setting must fail loudly rather than
    /// silently falling back - in either direction. Quietly downgrading to
    /// loopback would leave the operator wondering why the phone will not
    /// connect; quietly upgrading to `0.0.0.0` would defeat the whole change.
    #[test]
    fn test_bind_rejects_garbage() {
        for spec in [
            "0.0.0.0.0",
            "alll",
            "999.999.999.999",
            "http://0.0.0.0",
            "-1",
            "0.0.0.0:99999", // port out of u16 range
            // Hostnames are not resolved on purpose: only keywords and IP
            // literals are accepted, so there is no way for a name lookup to
            // quietly point somewhere unexpected.
            "localhost:9999",
            "example.com",
        ] {
            let err = parse_bind_spec(spec, 9999)
                .expect_err(&format!("{spec:?} must be rejected rather than guessed at"));
            assert!(
                err.contains(BIND_ENV_VAR),
                "error for {spec:?} should name the env var, got: {err}"
            );
        }

        // Whitespace is trimmed rather than rejected.
        assert_eq!(
            parse_bind_spec("  all  ", 9999).unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 9999))
        );
    }

    /// The env-var indirection must not change the secure default. This test
    /// runs in a shared test binary, so it only runs the branch that does not
    /// mutate global state, and it restores the previous value if it does.
    #[test]
    fn test_resolve_bind_addr_honours_env() {
        let previous = std::env::var(BIND_ENV_VAR).ok();

        std::env::set_var(BIND_ENV_VAR, "all");
        assert_eq!(
            resolve_bind_addr(9999).unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 9999))
        );

        std::env::set_var(BIND_ENV_VAR, "");
        assert_eq!(
            resolve_bind_addr(4242).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 4242)),
            "an empty DISPLAYSWARM_BIND must be the secure loopback default"
        );

        match previous {
            Some(v) => std::env::set_var(BIND_ENV_VAR, v),
            None => std::env::remove_var(BIND_ENV_VAR),
        }
    }

    // -----------------------------------------------------------------------
    // Whole-session behaviour with fake devices
    // -----------------------------------------------------------------------

    struct FakeCapturer {
        probe: Arc<Probe>,
        region: Option<OutputRegion>,
    }
    impl ScreenCapturer for FakeCapturer {
        fn output_region(&self) -> Option<OutputRegion> {
            self.region
        }
        fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Frame::new_rgba(16, 16, vec![0; 16 * 16 * 4], host_monotonic_us()))
        }
        fn width(&self) -> u32 { 16 }
        fn height(&self) -> u32 { 16 }
        fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, Box<dyn std::error::Error + Send + Sync>> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            self.capture_frame().map(Some)
        }
    }
    impl Drop for FakeCapturer {
        fn drop(&mut self) {
            self.probe.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// What the fake backends saw, shared with the test body.
    #[derive(Default)]
    struct Probe {
        /// Every capturer built: target and token key.
        built: Mutex<Vec<(CaptureTarget, Option<String>)>>,
        /// Capturers dropped so far.
        dropped: AtomicU32,
        /// Every `make_primary` call.
        primary_calls: Mutex<Vec<(OutputRegion, bool)>>,
        primary_dropped: AtomicU32,
        /// Every `set_output_region` the injector received.
        regions: Mutex<Vec<Option<OutputRegion>>>,
    }

    struct ProbeInjector(Arc<Probe>);
    impl InputInjector for ProbeInjector {
        fn inject(&mut self, _: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn inject_key(&mut self, _: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn set_output_region(&mut self, region: Option<OutputRegion>) {
            self.0.regions.lock().unwrap().push(region);
        }
    }

    struct FakeGuard(Arc<Probe>);
    impl Drop for FakeGuard {
        fn drop(&mut self) {
            self.0.primary_dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    const TABLET_REGION: OutputRegion = OutputRegion { x: 0, y: 0, width: 1920, height: 1080 };

    /// A manager with an in-memory store and fake devices. Every capturer
    /// reports `region`; `make_primary` succeeds or fails per `primary_ok`.
    /// A session that supports every role, so existing tests see no gating.
    fn test_capabilities() -> crate::display::model::Capabilities {
        use crate::display::model::*;
        Capabilities {
            desktop: Desktop::Kde,
            session: SessionType::Wayland,
            capture: CaptureCaps { portal: true, monitor: true, window: true, virtual_: true },
            layout: LayoutCaps::ALL,
            input_ok: true,
            rung: Rung::PortalVirtual,
            virtual_reason: None,
        }
    }

    fn test_manager(probe: &Arc<Probe>, region: Option<OutputRegion>, primary_ok: bool) -> Arc<SessionManager> {
        test_manager_with(probe, region, primary_ok, Box::new(|_| Vec::new()))
    }

    fn test_manager_with(
        probe: &Arc<Probe>,
        region: Option<OutputRegion>,
        primary_ok: bool,
        services: Box<crate::services::ServiceFactory>,
    ) -> Arc<SessionManager> {
        test_manager_full(probe, region, primary_ok, services, test_capabilities())
    }

    /// A manager on a desktop with these capabilities.
    fn test_manager_caps(probe: &Arc<Probe>, caps: crate::display::model::Capabilities) -> Arc<SessionManager> {
        test_manager_full(probe, None, true, Box::new(|_| Vec::new()), caps)
    }

    fn test_manager_full(
        probe: &Arc<Probe>,
        region: Option<OutputRegion>,
        primary_ok: bool,
        services: Box<crate::services::ServiceFactory>,
        caps: crate::display::model::Capabilities,
    ) -> Arc<SessionManager> {
        let (p1, p2, p3) = (probe.clone(), probe.clone(), probe.clone());
        let backends = Backends {
            capture: Box::new(move |target, key, _, _| {
                p1.built.lock().unwrap().push((target.clone(), key.map(str::to_string)));
                Box::new(FakeCapturer { probe: p1.clone(), region })
            }),
            encoder: Box::new(|_, _, _, _, _| Box::new(FakeEncoder)),
            injector: Box::new(move |_, _| Box::new(ProbeInjector(p2.clone()))),
            primary: Box::new(move |region, panel_off| {
                p3.primary_calls.lock().unwrap().push((region, panel_off));
                if primary_ok {
                    Ok((Box::new(FakeGuard(p3.clone())) as Box<dyn std::any::Any + Send>, region))
                } else {
                    Err("compositor said no".to_string())
                }
            }),
            tablet_region: Box::new(|| Some(TABLET_REGION)),
            services,
            capabilities: Box::new(move || caps.clone()),
            layout: Box::new(|| Arc::new(crate::display::backend::NoLayout)),
        };
        SessionManager::new(DeviceStore::in_memory(), backends)
    }

    fn hello_for(id: &str) -> Hello {
        Hello { device_id: id.into(), device_name: format!("phone {id}"), ..sample_hello() }
    }

    /// A phone on the far end of a duplex pipe, running a whole session.
    struct Phone {
        rx: MessageReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        task: tokio::task::JoinHandle<Result<String, Box<dyn std::error::Error + Send + Sync>>>,
        stop: watch::Sender<bool>,
    }

    impl Phone {
        async fn connect(manager: &Arc<SessionManager>, hello: Hello) -> Phone {
            let (phone_side, host_side) = tokio::io::duplex(1 << 20);
            let (host_read, host_write) = tokio::io::split(host_side);
            let (phone_read, mut tx) = tokio::io::split(phone_side);
            tx.write_all(&Message::Hello(hello).encode()).await.unwrap();
            let (stop, mut stop_rx) = watch::channel(false);
            let manager = manager.clone();
            let task = tokio::spawn(async move {
                let report: Box<ReportFn> = Box::new(|_, _| {});
                handle_client_stream(host_read, host_write, &ServerConfig::default(), &manager, false, &mut stop_rx, &*report)
                    .await
            });
            Phone { rx: MessageReader::new(phone_read), tx, task, stop }
        }

        /// The next message satisfying `pred` (others are skipped).
        async fn expect(&mut self, pred: impl Fn(&Message) -> bool) -> Message {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let m = self.rx.next().await.expect("host closed the stream early");
                    if pred(&m) {
                        return m;
                    }
                }
            })
            .await
            .expect("timed out waiting for a message")
        }

        async fn send(&mut self, m: Message) {
            self.tx.write_all(&m.encode()).await.unwrap();
        }

        async fn set_role(&mut self, role: u8) -> u8 {
            self.send(Message::SetRole { role }).await;
            match self.expect(|m| matches!(m, Message::SetRole { .. })).await {
                Message::SetRole { role } => role,
                _ => unreachable!(),
            }
        }

        /// Stops the server side and waits for the session to end.
        async fn finish(self) {
            let _ = self.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(6), self.task).await.expect("session must stop");
        }
    }

    async fn ack_role(phone: &mut Phone) -> u8 {
        match phone.expect(|m| matches!(m, Message::HelloAck(_))).await {
            Message::HelloAck(a) => {
                assert_eq!(a.status, HELLO_OK);
                a.role
            }
            _ => unreachable!(),
        }
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    const REGION: OutputRegion = OutputRegion { x: 1920, y: 0, width: 1600, height: 720 };

    /// Role -> capture target, per the Phase 3 table.
    #[test]
    fn test_role_to_target_mapping() {
        let mut rec = DeviceRecord::new("d", "n");
        rec.target_output = Some("eDP-1".into());
        let hello = Hello { width: 2400, height: 1080, refresh_mhz: 120_000, ..sample_hello() };
        assert_eq!(target_for(Role::Mirror, &rec, &hello), Some(CaptureTarget::Monitor { output: Some("eDP-1".into()) }));
        rec.target_output = None;
        assert_eq!(target_for(Role::Mirror, &rec, &hello), Some(CaptureTarget::Monitor { output: None }));
        let virt = CaptureTarget::Virtual { width: 2400, height: 1080, refresh_mhz: 120_000 };
        assert_eq!(target_for(Role::Extend, &rec, &hello), Some(virt.clone()));
        assert_eq!(target_for(Role::PhonePrimary, &rec, &hello), Some(virt));
        assert_eq!(target_for(Role::MirrorWindow, &rec, &hello), Some(CaptureTarget::Window));
        assert_eq!(target_for(Role::Tablet, &rec, &hello), None);
        assert_eq!(target_for(Role::InputPad, &rec, &hello), None);
        assert_eq!(token_key_for("abc", Role::Extend), "abc/virtual");
        assert_eq!(token_key_for("abc", Role::Mirror), "abc/monitor");
        assert_eq!(token_key_for("abc", Role::MirrorWindow), "abc/window");
    }

    /// An unknown device is told ROLE_UNSET and streams as Mirror meanwhile; once
    /// a role is remembered, the next connect gets it and captures accordingly.
    #[tokio::test]
    async fn test_helloack_role_unknown_then_known() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);

        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, ROLE_UNSET);
        wait_until("the mirror capturer", || probe.built.lock().unwrap().len() == 1).await;
        assert_eq!(probe.built.lock().unwrap()[0].0, CaptureTarget::Monitor { output: None });
        // The device is now known (by name) but has no role.
        let rec = manager.record("dev1").unwrap();
        assert_eq!((rec.name.as_str(), rec.role), ("phone dev1", None));
        assert!(rec.last_seen > 0);
        phone.finish().await;

        manager.set_role("dev1", Role::Extend);
        assert_eq!(manager.record("dev1").unwrap().role, Some(Role::Extend));
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, wire::ROLE_EXTEND);
        wait_until("the virtual capturer", || probe.built.lock().unwrap().len() == 2).await;
        {
            let built = probe.built.lock().unwrap();
            assert_eq!(built[1].0, CaptureTarget::Virtual { width: 1600, height: 720, refresh_mhz: 60_000 });
            assert_eq!(built[1].1.as_deref(), Some("dev1/virtual"));
        }
        phone.finish().await;
    }

    /// A phone SetRole switches the running session without dropping the
    /// transport, saves the role, and is answered with the role in effect.
    #[tokio::test]
    async fn test_set_role_switches_pipeline_live() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, ROLE_UNSET);
        wait_until("mirror", || probe.built.lock().unwrap().len() == 1).await;
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 0);

        // Tablet: input only. The capturer is torn down, nothing new is built.
        assert_eq!(phone.set_role(wire::ROLE_TABLET).await, wire::ROLE_TABLET);
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 1, "the old capturer is dropped before the reply");
        assert_eq!(probe.built.lock().unwrap().len(), 1);
        assert_eq!(manager.record("dev1").unwrap().role, Some(Role::Tablet));
        // The link is still up and the host keeps sending heartbeats.
        phone.expect(|m| *m == Message::Heartbeat).await;

        // Back to video: a fresh capturer for the new role.
        assert_eq!(phone.set_role(wire::ROLE_EXTEND).await, wire::ROLE_EXTEND);
        assert_eq!(probe.built.lock().unwrap().len(), 2);
        assert!(matches!(probe.built.lock().unwrap()[1].0, CaptureTarget::Virtual { .. }));
        phone.expect(|m| matches!(m, Message::VideoFrame(_))).await;

        // An invalid role is answered with the role in effect and changes nothing.
        assert_eq!(phone.set_role(99).await, wire::ROLE_EXTEND);
        assert_eq!(manager.record("dev1").unwrap().role, Some(Role::Extend));
        assert_eq!(probe.built.lock().unwrap().len(), 2);

        // Choosing the role already running rebuilds nothing.
        assert_eq!(phone.set_role(wire::ROLE_EXTEND).await, wire::ROLE_EXTEND);
        assert_eq!(probe.built.lock().unwrap().len(), 2);

        // Window mirroring uses the window target.
        assert_eq!(phone.set_role(wire::ROLE_MIRROR_WINDOW).await, wire::ROLE_MIRROR_WINDOW);
        assert_eq!(probe.built.lock().unwrap()[2].0, CaptureTarget::Window);
        phone.finish().await;
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 3, "every capturer is gone after the session");
    }

    /// What a desktop that can capture but not create a virtual monitor (COSMIC,
    /// Cinnamon on Wayland) looks like: no layout backend, no VIRTUAL source.
    fn mirror_only_caps() -> crate::display::model::Capabilities {
        use crate::display::model::*;
        Capabilities {
            desktop: Desktop::Cosmic,
            session: SessionType::Wayland,
            capture: CaptureCaps { portal: true, monitor: true, window: true, virtual_: false },
            layout: LayoutCaps::NONE,
            input_ok: true,
            rung: Rung::MirrorOnly,
            virtual_reason: None,
        }
    }

    /// Mirror needs capture only. With no layout backend at all (COSMIC,
    /// Cinnamon on Wayland) it still streams, the tablet still maps, and nothing
    /// tries to read or change the monitor layout.
    #[tokio::test]
    async fn test_mirror_streams_with_no_layout_backend() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager_caps(&probe, mirror_only_caps());
        assert_eq!(manager.capabilities().layout, crate::display::model::LayoutCaps::NONE);
        assert_eq!((manager.backends.layout)().kind(), "none");
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, ROLE_UNSET);
        phone.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        assert!(matches!(probe.built.lock().unwrap()[0].0, CaptureTarget::Monitor { .. }));
        assert!(probe.primary_calls.lock().unwrap().is_empty());
        // Tablet is input only and needs no layout either.
        assert_eq!(phone.set_role(wire::ROLE_TABLET).await, wire::ROLE_TABLET);
        // Back to Mirror.
        assert_eq!(phone.set_role(wire::ROLE_MIRROR).await, wire::ROLE_MIRROR);
        phone.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        // The layout-dependent roles say why not.
        assert_eq!(phone.set_role(wire::ROLE_PHONE_PRIMARY).await, wire::ROLE_MIRROR);
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert!(row.notice.unwrap().contains("virtual monitor"));
        phone.finish().await;
    }

    /// A role the desktop cannot do is refused with the reason: the phone is told
    /// the role still running, the device list carries the reason, and the saved
    /// role does not change.
    #[tokio::test]
    async fn test_unsupported_role_is_refused_with_its_reason() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager_caps(&probe, mirror_only_caps());
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, ROLE_UNSET);
        wait_until("mirror", || probe.built.lock().unwrap().len() == 1).await;

        assert_eq!(phone.set_role(wire::ROLE_EXTEND).await, wire::ROLE_MIRROR, "the role in effect is sent back");
        assert_eq!(probe.built.lock().unwrap().len(), 1, "nothing was rebuilt");
        assert_eq!(manager.record("dev1").unwrap().role, None, "the refused role is not remembered");
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert!(row.notice.unwrap().contains("virtual monitor"));
        assert_eq!(row.effective_role, Some(Role::Mirror));

        // Roles that do work are still accepted, and a good change clears the notice.
        assert_eq!(phone.set_role(wire::ROLE_TABLET).await, wire::ROLE_TABLET);
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert_eq!(row.notice, None);
        phone.finish().await;
    }

    /// A remembered role the desktop cannot do starts as Mirror, tells the phone
    /// so in the HelloAck, and keeps the saved role for a desktop that can.
    #[tokio::test]
    async fn test_remembered_unsupported_role_degrades_to_mirror() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager_caps(&probe, mirror_only_caps());
        let mut rec = DeviceRecord::new("dev1", "phone dev1");
        rec.role = Some(Role::PhonePrimary);
        manager.lock_store().put(rec).unwrap();
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, wire::ROLE_MIRROR);
        wait_until("a capturer", || probe.built.lock().unwrap().len() == 1).await;
        assert!(matches!(probe.built.lock().unwrap()[0].0, CaptureTarget::Monitor { .. }), "Mirror, not a virtual monitor");
        assert_eq!(manager.record("dev1").unwrap().role, Some(Role::PhonePrimary), "the saved role is untouched");
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert_eq!(row.effective_role, Some(Role::Mirror));
        assert!(row.notice.unwrap().contains("Running as Mirror"));
        assert!(probe.primary_calls.lock().unwrap().is_empty());
        phone.finish().await;
    }

    /// A phone Resize rebuilds the virtual monitor at the new size (Extend),
    /// ignores repeats and unusable sizes, and rebuilds Mirror at the new size too.
    #[tokio::test]
    async fn test_resize_rebuilds_virtual_monitor() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        ack_role(&mut phone).await;
        assert_eq!(phone.set_role(wire::ROLE_EXTEND).await, wire::ROLE_EXTEND);
        wait_until("the virtual capturer", || probe.built.lock().unwrap().len() == 2).await;

        // Same size again: nothing happens. Then an unusable size: ignored.
        phone.send(Message::Resize { width: 1600, height: 720, density_dpi: 320, rotation: 1 }).await;
        phone.send(Message::Resize { width: 4, height: 720, density_dpi: 320, rotation: 1 }).await;
        // A real change (portrait): the monitor is rebuilt at the new size.
        phone.send(Message::Resize { width: 720, height: 1600, density_dpi: 320, rotation: 0 }).await;
        wait_until("the rebuilt capturer", || probe.built.lock().unwrap().len() == 3).await;
        {
            let built = probe.built.lock().unwrap();
            assert_eq!(built[2].0, CaptureTarget::Virtual { width: 720, height: 1600, refresh_mhz: 60_000 });
            assert_eq!(built.len(), 3, "repeats and bad sizes rebuild nothing");
        }
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 2, "the old capturers are dropped");
        phone.expect(|m| matches!(m, Message::VideoFrame(_))).await;

        // Mirror is rebuilt too, so its frames match the rotated phone instead of being stretched.
        assert_eq!(phone.set_role(wire::ROLE_MIRROR).await, wire::ROLE_MIRROR);
        assert_eq!(probe.built.lock().unwrap().len(), 4);
        phone.send(Message::Resize { width: 1600, height: 720, density_dpi: 320, rotation: 1 }).await;
        wait_until("the rebuilt mirror", || probe.built.lock().unwrap().len() == 5).await;
        // Switching back to Extend uses the latest size.
        assert_eq!(phone.set_role(wire::ROLE_EXTEND).await, wire::ROLE_EXTEND);
        assert_eq!(
            probe.built.lock().unwrap()[5].0,
            CaptureTarget::Virtual { width: 1600, height: 720, refresh_mhz: 60_000 }
        );
        phone.finish().await;
    }

    /// The host UI changes a connected device's role through the same path, and
    /// the phone is told.
    #[tokio::test]
    async fn test_ui_set_role_reaches_the_phone() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        ack_role(&mut phone).await;
        manager.set_role("dev1", Role::InputPad);
        match phone.expect(|m| matches!(m, Message::SetRole { .. })).await {
            Message::SetRole { role } => assert_eq!(role, wire::ROLE_INPUT_PAD),
            _ => unreachable!(),
        }
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert_eq!((row.connected, row.role, row.effective_role), (true, Some(Role::InputPad), Some(Role::InputPad)));
        phone.finish().await;
        let row = manager.snapshot().into_iter().find(|d| d.device_id == "dev1").unwrap();
        assert!(!row.connected);
    }

    /// A clipboard message from the phone reaches a service, and what the
    /// service sends on the bulk queue reaches the phone.
    #[tokio::test]
    async fn test_services_get_their_messages_and_reply_in_bulk() {
        struct Echo(crate::services::ServiceOut);
        impl crate::services::SessionService for Echo {
            fn name(&self) -> &'static str {
                "echo"
            }
            fn wants(&self, m: &Message) -> bool {
                matches!(m, Message::Clipboard { .. })
            }
            fn on_message(&mut self, m: Message) {
                let out = self.0.clone();
                if let Message::Clipboard { data, .. } = m {
                    let big = data.repeat(40_000); // several fragments
                    tokio::spawn(async move { out.send_bulk(Message::Clipboard { mime: "text/plain".into(), data: big }).await });
                }
            }
        }
        let probe = Arc::new(Probe::default());
        let manager = test_manager_with(&probe, None, true, Box::new(|ctx| vec![Box::new(Echo(ctx.out.clone())) as Box<dyn crate::services::SessionService>]));
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        ack_role(&mut phone).await;
        phone.send(Message::Clipboard { mime: "text/plain".into(), data: b"hi".to_vec() }).await;
        phone.expect(|m| matches!(m, Message::Clipboard { data, .. } if data.len() == 80_000)).await;
        phone.finish().await;
    }

    /// A low-battery report from the phone reaches the encoder as a lower bitrate.
    #[tokio::test]
    async fn test_battery_saver_lowers_the_encoder_bitrate() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager_with(
            &probe,
            None,
            true,
            Box::new(|ctx| vec![Box::new(crate::services::battery::BatteryService::new(ctx)) as Box<dyn crate::services::SessionService>]),
        );
        let mut phone = Phone::connect(&manager, hello_for("dev-battery")).await;
        ack_role(&mut phone).await;
        let status = |percent, charging| Message::BatteryStatus(wire::BatteryStatus { percent, charging, thermal: 0, temp_decidegrees: 300 });
        phone.send(status(12, false)).await;
        wait_until("the reduced bitrate", || FAKE_ENCODER_KBPS.load(Ordering::SeqCst) == 10_000).await;
        assert_eq!(crate::services::battery::status("dev-battery").unwrap().status.percent, 12);
        phone.send(status(80, true)).await;
        wait_until("the restored bitrate", || FAKE_ENCODER_KBPS.load(Ordering::SeqCst) == 20_000).await;
        phone.finish().await;
    }

    /// Tablet: no capture, but pen and touch are confined to the primary
    /// monitor; leaving the role clears the mapping.
    #[tokio::test]
    async fn test_tablet_maps_onto_primary_monitor() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, ROLE_UNSET);
        assert_eq!(phone.set_role(wire::ROLE_TABLET).await, wire::ROLE_TABLET);
        let pen_down = || Message::Touch(Touch { action: ACTION_DOWN, action_id: 0, points: vec![TouchPoint { id: 0, x: 0.5, y: 0.5, pressure: 1.0 }] });
        phone.send(pen_down()).await;
        wait_until("the tablet region", || probe.regions.lock().unwrap().contains(&Some(TABLET_REGION))).await;

        assert_eq!(phone.set_role(wire::ROLE_INPUT_PAD).await, wire::ROLE_INPUT_PAD);
        phone.send(pen_down()).await;
        wait_until("the mapping cleared", || probe.regions.lock().unwrap().last() == Some(&None)).await;
        phone.finish().await;
    }

    /// PhonePrimary: once the virtual monitor's region is known it is made
    /// primary, the injector is given the region, and the guard lives exactly as
    /// long as the role.
    #[tokio::test]
    async fn test_phone_primary_lifecycle_and_region() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, Some(REGION), true);
        let mut rec = DeviceRecord::new("dev1", "phone");
        rec.role = Some(Role::PhonePrimary);
        rec.panel_off = true;
        manager.lock_store().put(rec).unwrap();

        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, wire::ROLE_PHONE_PRIMARY);
        wait_until("make_primary", || !probe.primary_calls.lock().unwrap().is_empty()).await;
        assert_eq!(probe.primary_calls.lock().unwrap()[0], (REGION, true));
        assert_eq!(probe.primary_dropped.load(Ordering::SeqCst), 0);

        // The region reaches the injector before the next input event.
        phone.send(Message::Heartbeat).await;
        phone.send(Message::Touch(Touch { action: ACTION_DOWN, action_id: 0, points: vec![TouchPoint { id: 0, x: 0.5, y: 0.5, pressure: 1.0 }] })).await;
        wait_until("the injector region", || probe.regions.lock().unwrap().contains(&Some(REGION))).await;

        // Leaving the role restores the layout.
        assert_eq!(phone.set_role(wire::ROLE_MIRROR).await, wire::ROLE_MIRROR);
        assert_eq!(probe.primary_dropped.load(Ordering::SeqCst), 1);
        phone.finish().await;
        assert_eq!(probe.primary_calls.lock().unwrap().len(), 1);
    }

    /// Only one phone may be the primary display; a second is downgraded to
    /// Extend and told so.
    #[tokio::test]
    async fn test_second_phone_primary_is_downgraded() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, Some(REGION), true);
        for id in ["A", "B"] {
            let mut rec = DeviceRecord::new(id, id);
            rec.role = Some(Role::PhonePrimary);
            manager.lock_store().put(rec).unwrap();
        }
        let mut a = Phone::connect(&manager, hello_for("A")).await;
        ack_role(&mut a).await;
        wait_until("A primary", || probe.primary_calls.lock().unwrap().len() == 1).await;
        let mut b = Phone::connect(&manager, hello_for("B")).await;
        ack_role(&mut b).await;
        match b.expect(|m| matches!(m, Message::SetRole { .. })).await {
            Message::SetRole { role } => assert_eq!(role, wire::ROLE_EXTEND),
            _ => unreachable!(),
        }
        assert_eq!(probe.primary_calls.lock().unwrap().len(), 1, "B never reached make_primary");
        a.finish().await;
        b.finish().await;
    }

    /// If making the phone primary fails, streaming continues as Extend and the
    /// phone is told so; the remembered role is untouched.
    #[tokio::test]
    async fn test_phone_primary_failure_falls_back_to_extend() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, Some(REGION), false);
        let mut rec = DeviceRecord::new("dev1", "phone");
        rec.role = Some(Role::PhonePrimary);
        manager.lock_store().put(rec).unwrap();

        let mut phone = Phone::connect(&manager, hello_for("dev1")).await;
        assert_eq!(ack_role(&mut phone).await, wire::ROLE_PHONE_PRIMARY);
        match phone.expect(|m| matches!(m, Message::SetRole { .. })).await {
            Message::SetRole { role } => assert_eq!(role, wire::ROLE_EXTEND),
            _ => unreachable!(),
        }
        phone.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        let row = manager.snapshot().pop().unwrap();
        assert_eq!((row.role, row.effective_role), (Some(Role::PhonePrimary), Some(Role::Extend)));
        assert!(row.notice.unwrap().contains("compositor said no"));
        phone.finish().await;
    }

    /// Two devices stream at once; a reconnect of one replaces only its own
    /// old session.
    #[tokio::test]
    async fn test_two_sessions_coexist_and_reconnect_replaces() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let mut a = Phone::connect(&manager, hello_for("A")).await;
        let mut b = Phone::connect(&manager, hello_for("B")).await;
        ack_role(&mut a).await;
        ack_role(&mut b).await;
        a.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        b.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        assert_eq!(manager.session_count(), 2);
        let snap = manager.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|d| d.connected));

        // A connects again: its first session ends with a Bye, B is untouched.
        let mut a2 = Phone::connect(&manager, hello_for("A")).await;
        ack_role(&mut a2).await;
        match a.expect(|m| matches!(m, Message::Bye { .. })).await {
            Message::Bye { reason, .. } => assert_eq!(reason, BYE_NORMAL),
            _ => unreachable!(),
        }
        assert!(a.task.await.unwrap().is_ok());
        assert_eq!(manager.session_count(), 2, "the replacement took A's place");
        a2.expect(|m| matches!(m, Message::VideoFrame(_))).await;
        b.expect(|m| matches!(m, Message::VideoFrame(_))).await;

        // Forgetting a connected device ends its session and its record.
        manager.forget("B");
        b.expect(|m| matches!(m, Message::Bye { .. })).await;
        assert!(manager.record("B").is_none());
        wait_until("B to leave", || manager.session_count() == 1).await;
        a2.finish().await;
    }

    struct FakeEncoder;
    impl VideoEncoder for FakeEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![EncodedPacket { frame_type: 1, data: vec![0xAB; 64], timestamp_us: frame.timestamp_us }])
        }
        fn request_keyframe(&mut self) {}
        fn set_bitrate(&mut self, kbps: u32) {
            FAKE_ENCODER_KBPS.store(kbps, Ordering::SeqCst);
        }
        fn backend_name(&self) -> String { "fake".into() }
    }

    /// The last bitrate any `FakeEncoder` was switched to.
    static FAKE_ENCODER_KBPS: AtomicU32 = AtomicU32::new(0);

    /// Every complete message in `bytes`.
    fn decode_stream(bytes: &[u8]) -> Vec<Message> {
        let mut out = Vec::new();
        let mut re = wire::Reassembler::default();
        let mut s = bytes;
        while s.len() >= HEADER_SIZE {
            let h = Header::parse(s[..HEADER_SIZE].try_into().unwrap()).unwrap();
            h.check().unwrap();
            let end = HEADER_SIZE + h.len as usize;
            if let Some(p) = re.push(&h, s[HEADER_SIZE..end].to_vec()).unwrap() {
                out.push(Message::decode(h.channel, h.msg_type, &p).unwrap());
            }
            s = &s[end..];
        }
        assert!(s.is_empty(), "stream ends on a frame boundary");
        out
    }

    /// Stopping a session whose reader never yields a single byte (a phone that
    /// stays connected but silent) must still complete promptly, drop the
    /// capturer, and tell the phone the server is stopping. This is the
    /// regression behind "GNOME's recording icon stays on after Stop".
    #[tokio::test]
    async fn test_stop_completes_with_silent_reader_and_drops_capturer() {
        let probe = Arc::new(Probe::default());
        let manager = test_manager(&probe, None, true);
        let Phone { mut rx, tx: _phone_write, task, stop } = Phone::connect(&manager, hello_for("dev1")).await;

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 0, "capturer alive while streaming");
        stop.send(true).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .expect("session must stop within the timeout")
            .expect("session task");
        assert!(result.is_ok());
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 1, "capturer must be dropped before the session returns");
        assert_eq!(manager.session_count(), 0);

        // The phone saw video and, last, a Bye(SERVER_STOPPING).
        let mut messages = Vec::new();
        while let Ok(m) = rx.next().await {
            messages.push(m);
        }
        assert!(messages.iter().any(|m| matches!(m, Message::VideoFrame(_))), "video was sent");
        assert_eq!(
            messages.last(),
            Some(&Message::Bye { reason: BYE_SERVER_STOPPING, text: "server stopping".into() })
        );
    }

    /// A control message queued while a large frame is being written goes out
    /// between its fragments, not after the whole frame.
    #[tokio::test]
    async fn test_sender_interleaves_control_between_video_fragments() {
        // A small pipe, so the sender blocks inside the frame.
        let (host_side, mut phone_side) = tokio::io::duplex(4096);
        let shared = Arc::new(SessionShared::new());
        let (frame_tx, frame_rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(sender_loop(host_side, frame_rx, ctrl_rx, mpsc::channel(1).1, shared.clone()));

        let data: Vec<u8> = (0..64 * 1024).map(|i| i as u8).collect();
        frame_tx
            .send(EncodedFrame {
                timestamp_us: 42,
                packets: vec![EncodedPacket { frame_type: 2, data: data.clone(), timestamp_us: 0 }],
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let state = Message::HostState { state: HOST_STATE_STREAMING, detail: String::new() };
        ctrl_tx.send(state.clone()).unwrap();

        let reader = tokio::spawn(async move {
            let mut out = Vec::new();
            phone_side.read_to_end(&mut out).await.unwrap();
            out
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        ctrl_tx.send(Message::Bye { reason: BYE_NORMAL, text: String::new() }).unwrap();
        task.await.unwrap();
        let out = reader.await.unwrap();

        // Frame-level order: where did the HostState land among the fragments?
        let mut s = &out[..];
        let (mut fragments_before, mut fragments_after, mut seen_state) = (0, 0, false);
        while !s.is_empty() {
            let h = Header::parse(s[..HEADER_SIZE].try_into().unwrap()).unwrap();
            if h.channel == CH_VIDEO {
                if seen_state { fragments_after += 1 } else { fragments_before += 1 }
            } else if h.msg_type == wire::MSG_HOST_STATE {
                seen_state = true;
            }
            s = &s[HEADER_SIZE + h.len as usize..];
        }
        assert!(seen_state);
        assert!(fragments_before >= 1 && fragments_after >= 1, "{fragments_before} before, {fragments_after} after");

        let messages = decode_stream(&out);
        let video = messages.iter().find_map(|m| match m { Message::VideoFrame(v) => Some(v), _ => None }).unwrap();
        assert_eq!(video.data, data, "fragments reassemble to the encoded frame");
        assert_eq!((video.capture_us, video.frame_index, video.frame_type), (42, 0, 2));
        assert_eq!(messages.last(), Some(&Message::Bye { reason: BYE_NORMAL, text: String::new() }));
        assert_eq!(shared.sent_frames.load(Ordering::SeqCst), 1);
    }

    /// The encoder thread drops its end of the frame channel as soon as the
    /// session is marked ended, which can be before the Bye is queued. The
    /// sender must still deliver that Bye.
    #[tokio::test]
    async fn test_sender_still_sends_bye_after_frame_source_closes() {
        let (host_side, mut phone_side) = tokio::io::duplex(1 << 16);
        let shared = Arc::new(SessionShared::new());
        let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(FRAME_QUEUE_DEPTH);
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(sender_loop(host_side, frame_rx, ctrl_rx, mpsc::channel(1).1, shared));
        drop(frame_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished(), "sender must wait for the Bye");
        ctrl_tx.send(Message::Bye { reason: BYE_PROTOCOL_ERROR, text: "x".into() }).unwrap();
        task.await.unwrap();
        let mut out = Vec::new();
        phone_side.read_to_end(&mut out).await.unwrap();
        assert_eq!(
            decode_stream(&out).last(),
            Some(&Message::Bye { reason: BYE_PROTOCOL_ERROR, text: "x".into() })
        );
    }

    /// The sender emits heartbeats while there is nothing else to send.
    #[tokio::test]
    async fn test_sender_heartbeats_when_idle() {
        let (host_side, mut phone_side) = tokio::io::duplex(1 << 16);
        let shared = Arc::new(SessionShared::new());
        let (_frame_tx, frame_rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(sender_loop(host_side, frame_rx, ctrl_rx, mpsc::channel(1).1, shared));
        tokio::time::sleep(HEARTBEAT_INTERVAL + Duration::from_millis(100)).await;
        ctrl_tx.send(Message::Bye { reason: BYE_NORMAL, text: String::new() }).unwrap();
        task.await.unwrap();
        let mut out = Vec::new();
        phone_side.read_to_end(&mut out).await.unwrap();
        let beats = decode_stream(&out).iter().filter(|m| **m == Message::Heartbeat).count();
        assert!(beats >= 2, "{beats} heartbeats");
    }

    /// State strings never claim a connection before the hello.
    #[test]
    fn test_session_state_status_text() {
        let (status, _) = SessionState::Handshaking.to_status("USB");
        assert_eq!(status, "Client Connected");
        let (status, client) = SessionState::Streaming {
            fps: 60, kbps: 12000, latency_ms: Some(40), encoder: "x264".into(),
        }.to_status("USB");
        assert_eq!(status, "Streaming");
        assert!(client.contains("60 fps") && client.contains("40 ms") && client.contains("x264"));
    }

    /// A fake phone over real loopback TCP: TLS, pairing with a PIN, Hello, HelloAck;
    /// then a reconnect with the pinned certificate and the token, no PIN.
    #[tokio::test]
    async fn test_loopback_tls_pairing_then_trusted_reconnect() {
        use crate::protocol::wire::{PAIR_MODE_PIN, PAIR_MODE_TOKEN, PAIR_OK, PAIR_PIN_REQUIRED, PAIR_UNTRUSTED};
        use crate::transport::pairing::proof;
        use crate::transport::tls::{pinned_connector, Pin, TlsIdentity};

        let manager = test_manager(&Arc::new(Probe::default()), None, true);
        let identity = TlsIdentity::generate("testhost").unwrap();
        let fp = identity.fingerprint();
        let gate = Arc::new(NetGate::new(identity, manager.pairing().clone()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = watch::channel(false);

        let server = {
            let manager = manager.clone();
            tokio::spawn(async move {
                let mut sessions: JoinSet<()> = JoinSet::new();
                let cb = Arc::new(|_: String, _: String, _: String| {});
                loop {
                    let Ok((stream, _)) = listener.accept().await else { break };
                    let gate = gate.clone();
                    let connect = async move { gate.accept(stream).await.map(|(r, w, _)| (r, w)) };
                    spawn_client_session(
                        &mut sessions,
                        manager.clone(),
                        connect,
                        ServerConfig::default(),
                        cb.clone(),
                        stop_rx.clone(),
                        "TCP client",
                        None,
                    );
                }
            })
        };
        let name = || rustls::pki_types::ServerName::try_from("displayswarm.local").unwrap();

        // First contact: unpinned, ask for a PIN, prove it.
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = pinned_connector(Pin::Unpinned).unwrap().connect(name(), tcp).await.unwrap();
        let (r, mut w) = tokio::io::split(tls);
        let mut r = MessageReader::new(r);
        let req = |mode, cred: Vec<u8>| {
            Message::PairRequest { mode, device_id: "dev1".into(), device_name: "Phone".into(), credential: cred }.encode()
        };
        w.write_all(&req(PAIR_MODE_PIN, vec![])).await.unwrap();
        match r.next().await.unwrap() {
            Message::PairResponse { status, .. } => assert_eq!(status, PAIR_PIN_REQUIRED),
            other => panic!("{other:?}"),
        }
        let pin = manager.pairing().pending_offer().unwrap().pin.unwrap();
        w.write_all(&req(PAIR_MODE_PIN, proof(pin.as_bytes(), &fp, "dev1"))).await.unwrap();
        let token = match r.next().await.unwrap() {
            Message::PairResponse { status, token, .. } => {
                assert_eq!(status, PAIR_OK);
                token
            }
            other => panic!("{other:?}"),
        };
        w.write_all(&Message::Hello(sample_hello()).encode()).await.unwrap();
        loop {
            match r.next().await.unwrap() {
                Message::HelloAck(a) => {
                    assert_eq!(a.status, HELLO_OK);
                    break;
                }
                _ => continue,
            }
        }
        drop((r, w));

        // Later: pinned, token only.
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = pinned_connector(Pin::Fingerprint(fp)).unwrap().connect(name(), tcp).await.unwrap();
        let (r, mut w) = tokio::io::split(tls);
        let mut r = MessageReader::new(r);
        w.write_all(&req(PAIR_MODE_TOKEN, vec![0; 32])).await.unwrap();
        match r.next().await.unwrap() {
            Message::PairResponse { status, .. } => assert_eq!(status, PAIR_UNTRUSTED),
            other => panic!("{other:?}"),
        }
        w.write_all(&req(PAIR_MODE_TOKEN, token)).await.unwrap();
        match r.next().await.unwrap() {
            Message::PairResponse { status, .. } => assert_eq!(status, PAIR_OK),
            other => panic!("{other:?}"),
        }
        w.write_all(&Message::Hello(sample_hello()).encode()).await.unwrap();
        loop {
            if let Message::HelloAck(a) = r.next().await.unwrap() {
                assert_eq!(a.status, HELLO_OK);
                break;
            }
        }
        let _ = stop_tx.send(true);
        server.abort();
    }
}
