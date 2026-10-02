//! The daemon's state and request handling, independent of the socket and the
//! tray: [`DaemonCore`] owns the [`SessionManager`], the server's run state,
//! the global settings and the event stream, and answers every IPC
//! [`Request`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch, Notify};

use super::logging::LogBuffer;
use crate::devices::{DeviceStore, Role};
use crate::ipc::proto::*;
use crate::server::{self, ServerConfig, SessionManager};

/// `(status, client)` lines the server reports.
pub type StatusFn = Arc<dyn Fn(String, String) + Send + Sync>;
pub type ServerFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
/// Runs the server until the stop signal; replaceable so tests need no ports.
pub type ServerRunner =
    dyn Fn(ServerConfig, Arc<SessionManager>, watch::Receiver<bool>, StatusFn) -> ServerFuture + Send + Sync;

/// The real server.
pub fn default_runner() -> Box<ServerRunner> {
    Box::new(|config, manager, stop_rx, status| {
        Box::pin(async move {
            server::run_server_with(config, move |s, c, _input| status(s, c), stop_rx, manager)
                .await
                .map_err(|e| e.to_string())
        })
    })
}

struct ServerRun {
    running: bool,
    stop_tx: Option<watch::Sender<bool>>,
    status: String,
    client_info: String,
}

/// What the device diff remembers between two snapshots.
#[derive(Default)]
struct DeviceWatch {
    /// device id -> (name, connected)
    prev: HashMap<String, (String, bool)>,
    /// Connected devices for which a role decision was already requested.
    asked: HashSet<String>,
}

pub struct DaemonCore {
    pub manager: Arc<SessionManager>,
    pub logs: LogBuffer,
    runner: Box<ServerRunner>,
    settings: Mutex<GlobalSettings>,
    settings_path: Option<PathBuf>,
    events: broadcast::Sender<Event>,
    server: Mutex<ServerRun>,
    stopped: Notify,
    encoder: Mutex<Option<String>>,
    tray_mode: Mutex<String>,
    start_on_login: Mutex<bool>,
    watch: Mutex<DeviceWatch>,
    quit_tx: watch::Sender<bool>,
    daemon_exe: String,
}

/// `settings.json` next to `devices.json`.
pub fn default_settings_path() -> Option<PathBuf> {
    DeviceStore::default_path().map(|p| p.with_file_name("settings.json"))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn load_settings(path: Option<&PathBuf>) -> GlobalSettings {
    let Some(path) = path else { return GlobalSettings::default() };
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<GlobalSettings>(&text) {
            Ok(s) if s.validate().is_ok() => s,
            Ok(_) | Err(_) => {
                log::warn!("{} is invalid; using default settings", path.display());
                GlobalSettings::default()
            }
        },
        Err(_) => GlobalSettings::default(),
    }
}

impl DaemonCore {
    pub fn new(
        manager: Arc<SessionManager>,
        runner: Box<ServerRunner>,
        settings_path: Option<PathBuf>,
        logs: LogBuffer,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let (quit_tx, _) = watch::channel(false);
        let initial_settings = load_settings(settings_path.as_ref());
        crate::audio::sync::hub().set_enabled(initial_settings.audio_sync);
        Arc::new(Self {
            manager,
            logs,
            runner,
            settings: Mutex::new(initial_settings),
            settings_path,
            events,
            server: Mutex::new(ServerRun {
                running: false,
                stop_tx: None,
                status: "Server stopped".into(),
                client_info: "No client connected".into(),
            }),
            stopped: Notify::new(),
            encoder: Mutex::new(None),
            tray_mode: Mutex::new("none".into()),
            start_on_login: Mutex::new(false),
            watch: Mutex::new(DeviceWatch::default()),
            quit_tx,
            daemon_exe: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "displayswarm-hostd".into()),
        })
    }

    /// Hooks the device-change listener and runs the one-off probes. Needs a
    /// running tokio runtime.
    pub fn start_background(self: &Arc<Self>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        self.manager.set_change_listener(move || {
            let _ = tx.send(());
        });
        let core = self.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // Coalesce a burst into one diff.
                while rx.try_recv().is_ok() {}
                core.on_devices_changed();
            }
        });
        // Wi-Fi pairing PINs reach the tray, notifications and the UI as
        // events. Weak: the manager outlives nothing it would keep alive.
        let weak = Arc::downgrade(self);
        self.manager.pairing().set_event_listener(move |ev| {
            let Some(core) = weak.upgrade() else { return };
            use crate::transport::pairing::PairEvent;
            core.emit(match ev {
                PairEvent::PinRequested { device_id, device_name, pin, valid_for } => {
                    Event::PairingPin { device_id, name: device_name, pin, valid_secs: valid_for.as_secs() }
                }
                PairEvent::Paired { device_id, device_name } => Event::Paired { device_id, name: device_name },
                PairEvent::Failed { device_id, reason } => Event::PairingFailed { device_id, reason },
            });
        });
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            *lock(&core.start_on_login) = super::autostart::is_enabled();
            let enc = super::diagnostics::probe_encoder();
            log::info!("Encoder probe: {enc}");
            *lock(&core.encoder) = Some(enc);
            // What this desktop can do; the UI greys out the roles it cannot.
            core.manager.capabilities();
            core.emit_settings();
        });
    }

    /// The session manager (pairing, trusted phones) for the other daemon parts.
    pub(crate) fn session_manager(&self) -> &Arc<SessionManager> {
        &self.manager
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn emit(&self, event: Event) {
        // No receivers is fine (nobody is watching).
        let _ = self.events.send(event);
    }

    fn emit_settings(&self) {
        self.emit(Event::SettingsChanged { settings: lock(&self.settings).clone(), start_on_login: *lock(&self.start_on_login) });
    }

    pub fn set_tray_mode(&self, mode: &str) {
        *lock(&self.tray_mode) = mode.to_string();
    }

    /// Resolves when a `Quit` request (or `request_quit`) arrives.
    pub fn quit_signal(&self) -> watch::Receiver<bool> {
        self.quit_tx.subscribe()
    }

    pub fn request_quit(&self) {
        self.emit(Event::Shutdown);
        self.quit_tx.send_replace(true);
    }

    pub fn settings(&self) -> GlobalSettings {
        lock(&self.settings).clone()
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.manager.snapshot().iter().map(DeviceInfo::from).collect()
    }

    pub fn server_info(&self) -> ServerInfo {
        let s = lock(&self.server);
        ServerInfo { running: s.running, status: s.status.clone(), client_info: s.client_info.clone() }
    }

    pub fn state(&self) -> StateInfo {
        let settings = self.settings();
        let listen = match server::resolve_bind_addr(settings.port) {
            Ok(a) => a.to_string(),
            Err(e) => format!("invalid bind address: {e}"),
        };
        // Never blocks: the probe runs once at startup (see `start_background`).
        let caps = self.manager.capabilities_peek();
        StateInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL_VERSION,
            devices: self.devices(),
            server: self.server_info(),
            settings,
            start_on_login: *lock(&self.start_on_login),
            encoder: lock(&self.encoder).clone(),
            desktop_backend: super::diagnostics::backend_name().into(),
            roles: caps.as_ref().map(crate::ipc::proto::role_availability).unwrap_or_default(),
            rung: caps.map(|c| c.rung.label().to_string()).unwrap_or_default(),
            tray: lock(&self.tray_mode).clone(),
            listen,
        }
    }

    // -- server run state ---------------------------------------------------

    /// Starts the server unless it runs already.
    pub fn start_server(self: &Arc<Self>) {
        let settings = self.settings();
        let rx = {
            let mut s = lock(&self.server);
            if s.running {
                return;
            }
            let (tx, rx) = watch::channel(false);
            s.running = true;
            s.stop_tx = Some(tx);
            s.status = "Starting\u{2026}".into();
            s.client_info = "No client connected".into();
            rx
        };
        self.emit(Event::ServerChanged { server: self.server_info() });
        let config = ServerConfig {
            port: settings.port,
            target_fps: settings.target_fps,
            bitrate_kbps: settings.bitrate_mbps * 1000,
            aoa_config: Default::default(),
            network: Default::default(),
        };
        let core = self.clone();
        let status_core = Arc::downgrade(self);
        let status: StatusFn = Arc::new(move |status, client| {
            if let Some(core) = status_core.upgrade() {
                {
                    let mut s = lock(&core.server);
                    if !s.running {
                        return;
                    }
                    s.status = status;
                    s.client_info = client;
                }
                core.emit(Event::ServerChanged { server: core.server_info() });
            }
        });
        tokio::spawn(async move {
            let result = (core.runner)(config, core.manager.clone(), rx, status).await;
            if let Err(e) = &result {
                log::error!("Server error: {e}");
            }
            {
                let mut s = lock(&core.server);
                s.running = false;
                s.stop_tx = None;
                s.status = match result {
                    Ok(()) => "Server stopped".into(),
                    Err(e) => format!("Server stopped: {e}"),
                };
                s.client_info = "No client connected".into();
            }
            core.emit(Event::ServerChanged { server: core.server_info() });
            core.stopped.notify_waiters();
        });
    }

    /// Asks the server to stop; it is stopped when `running` turns false.
    pub fn stop_server(&self) {
        let tx = {
            let mut s = lock(&self.server);
            let tx = s.stop_tx.take();
            if tx.is_some() {
                s.status = "Stopping\u{2026}".into();
                s.client_info = "Releasing screen capture".into();
            }
            tx
        };
        if let Some(tx) = tx {
            log::info!("Stopping the server");
            let _ = tx.send(true);
            self.emit(Event::ServerChanged { server: self.server_info() });
        }
    }

    /// Waits (bounded) until the server has finished.
    pub async fn wait_stopped(&self, timeout: Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.stopped.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !lock(&self.server).running {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    // -- devices ------------------------------------------------------------

    fn on_devices_changed(&self) {
        let devices = self.devices();
        let default_role = self.settings().default_role.as_deref().and_then(role_from_key);
        let (events, assign) = {
            let mut w = lock(&self.watch);
            diff_devices(&mut w, &devices, default_role)
        };
        for e in events {
            self.emit(e);
        }
        self.emit(Event::DevicesChanged { devices });
        for (id, role) in assign {
            log::info!("Applying the default role {} to {id}", role_key(role));
            self.manager.set_role(&id, role);
        }
    }

    fn require_device(&self, device_id: &str) -> Result<(), String> {
        self.manager.record(device_id).map(|_| ()).ok_or_else(|| format!("unknown device {device_id:?}"))
    }

    // -- requests -----------------------------------------------------------

    pub async fn handle(self: &Arc<Self>, request: Request) -> Result<Response, String> {
        match request {
            Request::Ping => Ok(Response::Pong { protocol: PROTOCOL_VERSION }),
            // The connection layer starts the event forwarding; the state is
            // the reply either way.
            Request::Subscribe | Request::GetState => Ok(Response::State { state: self.state() }),
            Request::SetRole { device_id, role } => {
                let role = role_from_key(&role).ok_or_else(|| format!("unknown role {role:?}"))?;
                self.require_device(&device_id)?;
                self.manager.set_role(&device_id, role);
                Ok(Response::Ok)
            }
            Request::SetDeviceSettings { device_id, settings } => {
                self.require_device(&device_id)?;
                self.manager.set_settings(&device_id, settings);
                Ok(Response::Ok)
            }
            Request::SetPanelOff { device_id, panel_off } => {
                self.require_device(&device_id)?;
                self.manager.set_panel_off(&device_id, panel_off);
                Ok(Response::Ok)
            }
            Request::SetTargetOutput { device_id, output } => {
                self.require_device(&device_id)?;
                let output = output.filter(|o| !o.trim().is_empty());
                self.manager.set_target_output(&device_id, output);
                Ok(Response::Ok)
            }
            Request::Disconnect { device_id } => {
                if self.manager.disconnect(&device_id) {
                    Ok(Response::Ok)
                } else {
                    Err("that device is not connected".into())
                }
            }
            Request::ForgetDevice { device_id } => {
                self.require_device(&device_id)?;
                self.manager.forget(&device_id);
                Ok(Response::Ok)
            }
            Request::Identify { device_id } => {
                let rec = self.manager.record(&device_id).ok_or_else(|| format!("unknown device {device_id:?}"))?;
                log::info!("Identify requested for {} ({device_id})", rec.name);
                self.emit(Event::Identify { device_id, name: rec.name });
                Ok(Response::Ok)
            }
            Request::StartServer => {
                self.start_server();
                Ok(Response::Ok)
            }
            Request::StopServer => {
                self.stop_server();
                Ok(Response::Ok)
            }
            Request::SetGlobalSettings { settings } => {
                settings.validate()?;
                self.apply_settings(settings)?;
                Ok(Response::Ok)
            }
            Request::SyncAudioNow => {
                crate::audio::sync::hub().resync()?;
                Ok(Response::Ok)
            }
            Request::SetStartOnLogin { enabled } => {
                let exe = self.daemon_exe.clone();
                tokio::task::spawn_blocking(move || super::autostart::set_enabled(enabled, &exe))
                    .await
                    .map_err(|e| e.to_string())??;
                *lock(&self.start_on_login) = enabled;
                self.emit_settings();
                Ok(Response::Ok)
            }
            Request::GetDiagnostics => {
                let encoder = lock(&self.encoder).clone();
                let tray = lock(&self.tray_mode).clone();
                let items = tokio::task::spawn_blocking(move || super::diagnostics::run(encoder.as_deref(), &tray))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Response::Diagnostics { items })
            }
            Request::GetLogs { max } => Ok(Response::Logs { lines: self.logs.tail(max.min(super::logging::LOG_CAPACITY)) }),
            Request::GetDebugReport => Ok(Response::DebugReport { text: self.debug_report().await }),
            Request::Wifi { op, args } => {
                let data = super::wifi::handle(self, &op, &args).await?;
                Ok(Response::Wifi { data })
            }
            Request::Quit => {
                self.request_quit();
                Ok(Response::Ok)
            }
        }
    }

    /// Stores new settings and, when the server's own parameters changed while
    /// it runs, restarts it in the background.
    fn apply_settings(self: &Arc<Self>, new: GlobalSettings) -> Result<(), String> {
        let old = std::mem::replace(&mut *lock(&self.settings), new.clone());
        if let Some(path) = &self.settings_path {
            let saved = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|_| std::fs::write(path, serde_json::to_string_pretty(&new).unwrap_or_default()));
            if let Err(e) = saved {
                log::warn!("Could not save {}: {e}", path.display());
            }
        }
        crate::audio::sync::hub().set_enabled(new.audio_sync);
        self.emit_settings();
        let server_changed = (old.port, old.target_fps, old.bitrate_mbps) != (new.port, new.target_fps, new.bitrate_mbps);
        if server_changed && lock(&self.server).running {
            log::info!("Server settings changed; restarting the server");
            self.stop_server();
            let core = self.clone();
            tokio::spawn(async move {
                if core.wait_stopped(Duration::from_secs(15)).await {
                    core.start_server();
                } else {
                    log::warn!("The server did not stop in time; not restarting it");
                }
            });
        }
        // A newly chosen default role also applies to devices waiting for one.
        self.on_devices_changed();
        Ok(())
    }

    async fn debug_report(&self) -> String {
        let state = self.state();
        let encoder = state.encoder.clone();
        let tray = state.tray.clone();
        let checks = tokio::task::spawn_blocking(move || super::diagnostics::run(encoder.as_deref(), &tray))
            .await
            .unwrap_or_default();
        let mut out = String::new();
        out.push_str(&format!("DisplaySwarm {} (IPC protocol {})\n", state.version, state.protocol));
        out.push_str(&format!("Server: {} | {}\n", state.server.status, state.server.client_info));
        out.push_str(&format!("Listen: {} | Desktop backend: {} | Tray: {}\n", state.listen, state.desktop_backend, state.tray));
        out.push_str(&format!("Settings: {}\n", serde_json::to_string(&state.settings).unwrap_or_default()));
        out.push_str("\n== Checks ==\n");
        for c in checks {
            out.push_str(&format!("[{:?}] {}: {}\n", c.status, c.name, c.detail));
        }
        out.push_str("\n== Devices ==\n");
        for d in &state.devices {
            out.push_str(&format!(
                "{} ({}) role={:?} connected={} state={:?} transport={:?}\n",
                d.display_name(),
                d.device_id,
                d.role,
                d.connected,
                d.state,
                d.transport
            ));
        }
        out.push_str("\n== Recent log ==\n");
        for l in self.logs.tail(300) {
            out.push_str(&l);
            out.push('\n');
        }
        out
    }
}

/// Compares the new device list with the last one and returns the events to
/// emit plus the `(device, default role)` assignments to perform.
fn diff_devices(w: &mut DeviceWatch, devices: &[DeviceInfo], default_role: Option<Role>) -> (Vec<Event>, Vec<(String, Role)>) {
    let mut events = Vec::new();
    let mut assign = Vec::new();
    let mut seen = HashSet::new();
    for d in devices {
        seen.insert(d.device_id.clone());
        let was = w.prev.get(&d.device_id).map(|p| p.1).unwrap_or(false);
        if d.connected && !was {
            events.push(Event::DeviceConnected { device_id: d.device_id.clone(), name: d.name.clone() });
        }
        if !d.connected && was {
            events.push(Event::DeviceDisconnected { device_id: d.device_id.clone(), name: d.name.clone() });
        }
        if !d.connected || d.role.is_some() {
            w.asked.remove(&d.device_id);
        } else if w.asked.insert(d.device_id.clone()) {
            match default_role {
                Some(r) => assign.push((d.device_id.clone(), r)),
                None => events.push(Event::NeedsRole { device_id: d.device_id.clone(), name: d.name.clone() }),
            }
        }
    }
    // A forgotten device disappears from the list; if it was connected it is gone.
    for (id, (name, connected)) in &w.prev {
        if !seen.contains(id) && *connected {
            events.push(Event::DeviceDisconnected { device_id: id.clone(), name: name.clone() });
        }
    }
    w.asked.retain(|id| seen.contains(id));
    w.prev = devices.iter().map(|d| (d.device_id.clone(), (d.name.clone(), d.connected))).collect();
    (events, assign)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{DeviceRecord, DeviceSettings};

    /// A runner that "serves" until told to stop, without touching any port.
    fn fake_runner() -> Box<ServerRunner> {
        Box::new(|_config, _manager, mut stop_rx, status| {
            Box::pin(async move {
                status("Listening (fake)".into(), "Waiting".into());
                while !*stop_rx.borrow() {
                    if stop_rx.changed().await.is_err() {
                        break;
                    }
                }
                Ok(())
            })
        })
    }

    /// A core over an in-memory store holding a record for each of `ids`.
    pub(crate) fn core_with(ids: &[&str]) -> Arc<DaemonCore> {
        let mut store = DeviceStore::in_memory();
        for id in ids {
            let mut rec = DeviceRecord::new(*id, "Test phone");
            rec.last_seen = 1;
            store.put(rec).unwrap();
        }
        let manager = SessionManager::new(store, server::Backends::default());
        DaemonCore::new(manager, fake_runner(), None, LogBuffer::new())
    }

    fn device(id: &str, connected: bool, role: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            device_id: id.into(),
            name: format!("Phone {id}"),
            role: role.map(String::from),
            effective_role: None,
            connected,
            state: String::new(),
            notice: None,
            target_output: None,
            panel_off: false,
            settings: DeviceSettings::default(),
            transport: None,
            resolution: None,
            live: None,
        }
    }

    #[test]
    fn diff_reports_connect_disconnect_and_asks_once_for_a_role() {
        let mut w = DeviceWatch::default();
        let (ev, assign) = diff_devices(&mut w, &[device("a", true, None)], None);
        assert_eq!(
            ev,
            vec![
                Event::DeviceConnected { device_id: "a".into(), name: "Phone a".into() },
                Event::NeedsRole { device_id: "a".into(), name: "Phone a".into() },
            ]
        );
        assert!(assign.is_empty());
        // Still connected without a role: asked only once.
        let (ev, _) = diff_devices(&mut w, &[device("a", true, None)], None);
        assert!(ev.is_empty());
        // Role chosen, then it drops.
        let (ev, _) = diff_devices(&mut w, &[device("a", true, Some("tablet"))], None);
        assert!(ev.is_empty());
        let (ev, _) = diff_devices(&mut w, &[device("a", false, Some("tablet"))], None);
        assert_eq!(ev, vec![Event::DeviceDisconnected { device_id: "a".into(), name: "Phone a".into() }]);
        // Reconnect without role again asks again.
        let (ev, _) = diff_devices(&mut w, &[device("a", true, None)], None);
        assert!(ev.iter().any(|e| matches!(e, Event::NeedsRole { .. })));
    }

    #[test]
    fn diff_applies_the_default_role_instead_of_asking() {
        let mut w = DeviceWatch::default();
        let (ev, assign) = diff_devices(&mut w, &[device("a", true, None)], Some(Role::Extend));
        assert_eq!(assign, vec![("a".to_string(), Role::Extend)]);
        assert!(!ev.iter().any(|e| matches!(e, Event::NeedsRole { .. })));
    }

    #[test]
    fn diff_treats_a_forgotten_connected_device_as_disconnected() {
        let mut w = DeviceWatch::default();
        diff_devices(&mut w, &[device("a", true, Some("mirror"))], None);
        let (ev, _) = diff_devices(&mut w, &[], None);
        assert_eq!(ev, vec![Event::DeviceDisconnected { device_id: "a".into(), name: "Phone a".into() }]);
    }

    #[tokio::test]
    async fn requests_change_device_records() {
        let core = core_with(&["dev1"]);

        core.handle(Request::SetRole { device_id: "dev1".into(), role: "tablet".into() }).await.unwrap();
        let mut settings = DeviceSettings::default();
        settings.mic = true;
        settings.audio_out = false;
        core.handle(Request::SetDeviceSettings { device_id: "dev1".into(), settings: settings.clone() }).await.unwrap();
        core.handle(Request::SetPanelOff { device_id: "dev1".into(), panel_off: true }).await.unwrap();
        core.handle(Request::SetTargetOutput { device_id: "dev1".into(), output: Some("eDP-1".into()) }).await.unwrap();

        let rec = core.manager.record("dev1").unwrap();
        assert_eq!(rec.role, Some(Role::Tablet));
        assert_eq!(rec.settings, settings);
        assert!(rec.panel_off);
        assert_eq!(rec.target_output.as_deref(), Some("eDP-1"));

        // An empty output clears it.
        core.handle(Request::SetTargetOutput { device_id: "dev1".into(), output: Some("  ".into()) }).await.unwrap();
        assert_eq!(core.manager.record("dev1").unwrap().target_output, None);

        let Response::State { state } = core.handle(Request::GetState).await.unwrap() else { panic!("state") };
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].role.as_deref(), Some("tablet"));
        assert!(!state.devices[0].connected);

        core.handle(Request::ForgetDevice { device_id: "dev1".into() }).await.unwrap();
        assert!(core.manager.record("dev1").is_none());
    }

    #[tokio::test]
    async fn bad_requests_are_errors() {
        let core = core_with(&["dev1"]);
        assert!(core.handle(Request::SetRole { device_id: "dev1".into(), role: "wizard".into() }).await.is_err());
        assert!(core.handle(Request::SetRole { device_id: "nope".into(), role: "tablet".into() }).await.is_err());
        assert!(core.handle(Request::ForgetDevice { device_id: "nope".into() }).await.is_err());
        assert!(core.handle(Request::Identify { device_id: "nope".into() }).await.is_err());
        // Known but not connected.
        assert!(core.handle(Request::Disconnect { device_id: "dev1".into() }).await.is_err());
        let bad = GlobalSettings { port: 0, ..Default::default() };
        assert!(core.handle(Request::SetGlobalSettings { settings: bad }).await.is_err());
        assert!(core.handle(Request::Wifi { op: "nonsense".into(), args: serde_json::Value::Null }).await.is_err());
    }

    #[tokio::test]
    async fn identify_is_broadcast() {
        let core = core_with(&["dev1"]);
        let mut rx = core.subscribe();
        core.handle(Request::Identify { device_id: "dev1".into() }).await.unwrap();
        assert_eq!(rx.try_recv().unwrap(), Event::Identify { device_id: "dev1".into(), name: "Test phone".into() });
    }

    #[tokio::test]
    async fn server_start_stop_cycle() {
        let core = core_with(&[]);
        assert!(!core.server_info().running);
        core.handle(Request::StartServer).await.unwrap();
        assert!(core.server_info().running);
        // A second start is a no-op.
        core.handle(Request::StartServer).await.unwrap();
        core.handle(Request::StopServer).await.unwrap();
        assert!(core.wait_stopped(Duration::from_secs(5)).await);
        let info = core.server_info();
        assert!(!info.running);
        assert_eq!(info.status, "Server stopped");
        // And it can start again.
        core.handle(Request::StartServer).await.unwrap();
        assert!(core.server_info().running);
        core.stop_server();
        assert!(core.wait_stopped(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn settings_are_saved_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("displayswarm-core-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let core = DaemonCore::new(
            SessionManager::new(DeviceStore::in_memory(), server::Backends::default()),
            fake_runner(),
            Some(path.clone()),
            LogBuffer::new(),
        );
        let s = GlobalSettings { port: 4321, default_role: Some("extend".into()), ..Default::default() };
        core.handle(Request::SetGlobalSettings { settings: s.clone() }).await.unwrap();
        assert_eq!(load_settings(Some(&path)), s);
        assert_eq!(core.settings(), s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn logs_and_quit() {
        let core = core_with(&[]);
        core.logs.push("hello".into());
        let Response::Logs { lines } = core.handle(Request::GetLogs { max: 10 }).await.unwrap() else { panic!("logs") };
        assert_eq!(lines, vec!["hello".to_string()]);
        let quit = core.quit_signal();
        core.handle(Request::Quit).await.unwrap();
        assert!(*quit.borrow());
    }
}
