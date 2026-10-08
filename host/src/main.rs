//! `displayswarm`: the DisplaySwarm window.
//!
//! A thin client of the daemon (`displayswarm-hostd`): it shows what the daemon
//! reports over IPC and sends the user's changes back. Closing the window
//! leaves the daemon (and the tray) running. If no daemon is running it starts
//! one. The UI thread never waits on the network: requests run on a background
//! tokio runtime and post their results back with `invoke_from_event_loop`.
//!
//! Options: `--standalone` runs the daemon inside this process (development;
//! no tray, stops when the window closes), `--cli`/`--headless` runs just that
//! in-process daemon without a window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;

use displayswarm_host::daemon::{self, logging, DaemonOptions};
use displayswarm_host::devices::{Role, VideoQuality};
use displayswarm_host::ipc::*;
use displayswarm_host::ui_model::{live_text, sparkline_path, transport_label, History};

slint::include_modules!();

static RT: OnceLock<Handle> = OnceLock::new();
static CLIENT: Mutex<Option<Arc<IpcClient>>> = Mutex::new(None);

/// Everything the network side can tell the window.
enum UiMsg {
    /// `None`: connected; otherwise the banner to show.
    Link(Option<String>),
    State(Box<StateInfo>, bool),
    Devices(Vec<DeviceInfo>),
    Server(ServerInfo),
    Settings(GlobalSettings, bool),
    Identify(String),
    Diagnostics(Vec<DiagItem>),
    Logs(Vec<String>),
    Report(String),
    Wifi(serde_json::Value),
    Note(String),
}

#[derive(Default)]
struct UiState {
    devices: Vec<DeviceInfo>,
    history: HashMap<String, History>,
    flashing: Option<String>,
    model: Option<Rc<VecModel<DeviceRow>>>,
    settings: GlobalSettings,
    report: String,
    /// Which roles the desktop can do, from the daemon's state.
    roles: Vec<RoleInfo>,
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::new(UiState::default());
}

fn post(weak: &Weak<MainWindow>, msg: UiMsg) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            apply(&ui, msg);
        }
    });
}

/// Sends `req` from the background runtime; `on_ok` turns the reply into a
/// message for the window, errors become a banner note.
fn call(
    weak: &Weak<MainWindow>,
    req: Request,
    on_ok: impl FnOnce(Response) -> Option<UiMsg> + Send + 'static,
) {
    let weak = weak.clone();
    let Some(rt) = RT.get() else { return };
    rt.spawn(async move {
        let client = CLIENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let result = match client {
            Some(c) => c.request(req).await,
            None => Err("not connected to the daemon".into()),
        };
        match result {
            Ok(r) => {
                if let Some(m) = on_ok(r) {
                    post(&weak, m);
                }
            }
            Err(e) => post(&weak, UiMsg::Note(e)),
        }
    });
}

/// The role names for the pickers: a role this desktop cannot do says so, since
/// a drop-down cannot grey one entry.
fn role_labels(roles: &[RoleInfo]) -> Vec<SharedString> {
    Role::ALL
        .iter()
        .map(|r| {
            let unavailable = roles.iter().any(|i| i.role == role_key(*r) && !i.available);
            SharedString::from(if unavailable { format!("{} (not available here)", r.label()) } else { r.label().to_string() })
        })
        .collect()
}

/// Why `role` cannot be used on this desktop, when the daemon said so.
fn role_refusal(role: Role) -> Option<String> {
    STATE.with(|s| {
        s.borrow().roles.iter().find(|i| i.role == role_key(role) && !i.available).map(|i| {
            i.reason.clone().unwrap_or_else(|| "not available on this desktop".to_string())
        })
    })
}

fn send(weak: &Weak<MainWindow>, req: Request) {
    call(weak, req, |_| None);
}

// ---------------------------------------------------------------------------
// Model -> window
// ---------------------------------------------------------------------------

fn to_row(d: &DeviceInfo, hist: Option<&History>, flashing: bool) -> DeviceRow {
    let role = d.role.as_deref().and_then(role_from_key);
    let shown = d.effective_role.as_deref().and_then(role_from_key).or(role);
    let mut state = d.state.clone();
    if d.connected && role.is_some() && shown != role {
        if let Some(r) = shown {
            state.push_str(&format!(" (running as {})", r.label()));
        }
    }
    if let Some(n) = &d.notice {
        state.push_str(&format!(" \u{2013} {n}"));
    }
    let t = live_text(d);
    let empty = std::collections::VecDeque::new();
    DeviceRow {
        id: d.device_id.as_str().into(),
        name: d.display_name().into(),
        role_index: role.and_then(|r| Role::ALL.iter().position(|x| *x == r)).map(|i| i as i32).unwrap_or(-1),
        state: state.into(),
        connected: d.connected,
        needs_role: d.role.is_none(),
        transport: transport_label(d).into(),
        resolution: d.resolution.map(|r| r.label()).unwrap_or_default().into(),
        fps_text: t.fps.into(),
        latency_text: t.latency.into(),
        bitrate_text: t.bitrate.into(),
        latency_path: sparkline_path(hist.map(|h| &h.latency_ms).unwrap_or(&empty)).into(),
        bitrate_path: sparkline_path(hist.map(|h| &h.mbps).unwrap_or(&empty)).into(),
        audio_out: d.settings.audio_out,
        quality_index: match d.settings.video_quality {
            VideoQuality::Auto => 0,
            VideoQuality::Max => 1,
            VideoQuality::Balanced => 2,
            VideoQuality::DataSaver => 3,
        },
        audio_mode: if d.settings.audio_mirror { 1 } else if d.settings.audio_default_sink { 2 } else { 0 },
        delay_host: d.settings.delay_host_audio,
        mic: d.settings.mic,
        clipboard: d.settings.clipboard,
        battery_saver: d.settings.battery_saver,
        panel_off: d.panel_off,
        show_panel_off: shown == Some(Role::PhonePrimary),
        target_output: d.target_output.clone().unwrap_or_default().into(),
        show_target: matches!(shown, Some(Role::Mirror) | Some(Role::Tablet)),
        flashing,
    }
}

/// Pushes the device list into the window. Rows are updated in place when the
/// set of devices and their roles is unchanged (keeps the cards, and any
/// half-typed text, alive); otherwise the model is rebuilt.
fn render_devices(ui: &MainWindow) {
    STATE.with(|s| {
        let s = s.borrow();
        let rows: Vec<DeviceRow> = s
            .devices
            .iter()
            .map(|d| to_row(d, s.history.get(&d.device_id), s.flashing.as_deref() == Some(d.device_id.as_str())))
            .collect();
        let pending = s.devices.iter().position(|d| d.connected && d.role.is_none());
        let pending_name: SharedString = pending.map(|i| s.devices[i].display_name()).unwrap_or("").into();
        // A device that just arrived without a role is shown, so its role picker is in view.
        if let Some(i) = pending {
            if ui.get_pending_device_name() != pending_name {
                ui.set_selected_device(i as i32);
            }
        }
        ui.set_pending_device_name(pending_name);

        let model = s.model.clone().expect("model is created at startup");
        let same_shape = model.row_count() == rows.len()
            && rows.iter().enumerate().all(|(i, r)| {
                let old = model.row_data(i).expect("row in range");
                old.id == r.id && old.role_index == r.role_index && old.show_target == r.show_target
            });
        if same_shape {
            for (i, r) in rows.into_iter().enumerate() {
                model.set_row_data(i, r);
            }
        } else {
            model.set_vec(rows);
        }
    });
}

fn apply_server(ui: &MainWindow, s: &ServerInfo) {
    ui.set_server_running(s.running);
    ui.set_server_status(s.status.as_str().into());
    ui.set_client_info(s.client_info.as_str().into());
}

fn apply_settings_fields(ui: &MainWindow, s: &GlobalSettings) {
    ui.set_port_text(s.port.to_string().into());
    ui.set_fps(s.target_fps as i32);
    ui.set_bitrate_mbps(s.bitrate_mbps as i32);
    let idx = s
        .default_role
        .as_deref()
        .and_then(role_from_key)
        .and_then(|r| Role::ALL.iter().position(|x| *x == r))
        .map(|i| i as i32 + 1)
        .unwrap_or(0);
    ui.set_default_role_index(idx);
    ui.set_audio_sync(s.audio_sync);
    STATE.with(|st| st.borrow_mut().settings = s.clone());
}

fn apply_state(ui: &MainWindow, st: &StateInfo, full: bool) {
    ui.set_encoder_text(st.encoder.clone().unwrap_or_else(|| "probing\u{2026}".into()).into());
    ui.set_transports_text(format!("USB (Android Open Accessory) and TCP on {}", st.listen).into());
    ui.set_backend_text(
        if st.rung.is_empty() { st.desktop_backend.clone() } else { format!("{} ({})", st.desktop_backend, st.rung) }
            .as_str()
            .into(),
    );
    // Only when the availability changed, so an open drop-down is not reset by
    // every state push.
    if STATE.with(|s| s.borrow().roles != st.roles) {
        ui.set_role_names(ModelRc::new(VecModel::from(role_labels(&st.roles))));
        let info = |r: Role| st.roles.iter().find(|i| i.role == role_key(r));
        let available: Vec<bool> = Role::ALL.iter().map(|r| info(*r).map_or(true, |i| i.available)).collect();
        let reasons: Vec<SharedString> = Role::ALL
            .iter()
            .map(|r| match info(*r) {
                Some(i) if !i.available => {
                    i.reason.clone().unwrap_or_else(|| "Not available on this desktop".into()).into()
                }
                _ => SharedString::new(),
            })
            .collect();
        ui.set_role_available(ModelRc::new(VecModel::from(available)));
        ui.set_role_reasons(ModelRc::new(VecModel::from(reasons)));
    }
    ui.set_tray_text(
        match st.tray.as_str() {
            "tray" => "Tray icon shown",
            "notifications" => "No tray host found (on GNOME install the AppIndicator extension); using notifications",
            _ => "No tray",
        }
        .into(),
    );
    ui.set_start_on_login(st.start_on_login);
    apply_server(ui, &st.server);
    if full {
        apply_settings_fields(ui, &st.settings);
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.devices = st.devices.clone();
        s.roles = st.roles.clone();
    });
    record_history();
    render_devices(ui);
}

fn record_history() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let now = Instant::now();
        let devices = s.devices.clone();
        for d in &devices {
            s.history.entry(d.device_id.clone()).or_default().record(d, now);
        }
        s.history.retain(|id, _| devices.iter().any(|d| &d.device_id == id));
    });
}

fn apply(ui: &MainWindow, msg: UiMsg) {
    match msg {
        UiMsg::Link(None) => {
            ui.set_daemon_connected(true);
            ui.set_banner("".into());
        }
        UiMsg::Link(Some(b)) => {
            ui.set_daemon_connected(false);
            ui.set_banner(b.into());
        }
        UiMsg::State(st, full) => apply_state(ui, &st, full),
        UiMsg::Devices(devices) => {
            STATE.with(|s| s.borrow_mut().devices = devices);
            record_history();
            render_devices(ui);
        }
        UiMsg::Server(s) => apply_server(ui, &s),
        UiMsg::Settings(s, login) => {
            apply_settings_fields(ui, &s);
            ui.set_start_on_login(login);
        }
        UiMsg::Identify(id) => {
            STATE.with(|s| s.borrow_mut().flashing = Some(id));
            render_devices(ui);
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(2500), move || {
                STATE.with(|s| s.borrow_mut().flashing = None);
                if let Some(ui) = weak.upgrade() {
                    render_devices(&ui);
                }
            });
        }
        UiMsg::Diagnostics(items) => {
            let rows: Vec<DiagRow> = items
                .into_iter()
                .map(|i| DiagRow {
                    name: i.name.into(),
                    status: match i.status {
                        DiagStatus::Ok => 0,
                        DiagStatus::Warn => 1,
                        DiagStatus::Fail => 2,
                    },
                    detail: i.detail.into(),
                })
                .collect();
            ui.set_diagnostics(ModelRc::new(VecModel::from(rows)));
        }
        UiMsg::Logs(lines) => ui.set_log_text(lines.join("\n").into()),
        UiMsg::Report(text) => {
            let status = copy_to_clipboard(&text);
            STATE.with(|s| s.borrow_mut().report = text);
            ui.set_report_status(status.into());
        }
        UiMsg::Wifi(v) => {
            let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            ui.set_wifi_status(field("status").into());
            ui.set_wifi_detail(field("detail").into());
            ui.set_wifi_firewall(field("firewall").into());
        }
        UiMsg::Note(n) => ui.set_banner(n.into()),
    }
}

/// Copies through whichever clipboard tool exists; as a last resort writes the
/// text to a file. Returns what to tell the user. (Slint has no clipboard API
/// for plain strings.)
fn copy_to_clipboard(text: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (bin, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..]), ("xsel", &["-ib"][..])] {
        if let Ok(mut child) = Command::new(bin).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            // wl-copy / xclip stay alive to serve the selection: do not wait.
            return format!("Debug report copied ({bin}).");
        }
    }
    let path = default_socket_path().with_file_name("debug-report.txt");
    match std::fs::write(&path, text) {
        Ok(()) => format!("No clipboard tool found; saved the report to {}", path.display()),
        Err(e) => format!("Could not copy or save the report: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Daemon connection
// ---------------------------------------------------------------------------

/// Starts `displayswarm-hostd`: through systemd when its unit is installed,
/// otherwise as a detached child.
async fn start_daemon() {
    let started = tokio::task::spawn_blocking(|| {
        if daemon::autostart::systemd_start() {
            log::info!("Started the daemon through systemd");
            return true;
        }
        let name = if cfg!(windows) { "displayswarm-hostd.exe" } else { "displayswarm-hostd" };
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(name)))
            .filter(|p| p.exists())
            .unwrap_or_else(|| name.into());
        let mut cmd = std::process::Command::new(&exe);
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        // Detached: its own process group (Unix) or no console and its own group (Windows).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        match cmd.spawn() {
            Ok(_) => {
                log::info!("Started {}", exe.display());
                true
            }
            Err(e) => {
                log::warn!("Could not start {}: {e}", exe.display());
                false
            }
        }
    })
    .await;
    let _ = started;
}

async fn connection_loop(weak: Weak<MainWindow>, transport: Arc<dyn IpcTransport>, may_start: bool) {
    let mut failures = 0u32;
    loop {
        match IpcClient::connect(&*transport).await {
            Ok((client, mut events)) => {
                failures = 0;
                let client = Arc::new(client);
                match client.request(Request::Subscribe).await {
                    Ok(Response::State { state }) => {
                        *CLIENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(client.clone());
                        post(&weak, UiMsg::Link(None));
                        post(&weak, UiMsg::State(Box::new(state), true));
                    }
                    other => {
                        log::warn!("Unexpected reply to subscribe: {other:?}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
                while let Some(ev) = events.recv().await {
                    match ev {
                        Event::DevicesChanged { devices } => post(&weak, UiMsg::Devices(devices)),
                        Event::ServerChanged { server } => post(&weak, UiMsg::Server(server)),
                        Event::SettingsChanged { settings, start_on_login } => {
                            post(&weak, UiMsg::Settings(settings, start_on_login));
                            // The encoder probe and tray mode ride along with this event.
                            if let Ok(Response::State { state }) = client.request(Request::GetState).await {
                                post(&weak, UiMsg::State(Box::new(state), false));
                            }
                        }
                        Event::Identify { device_id, .. } => post(&weak, UiMsg::Identify(device_id)),
                        // The Wi-Fi section shows the live PIN / paired phones.
                        Event::PairingPin { .. } | Event::Paired { .. } | Event::PairingFailed { .. } => {
                            let req = Request::Wifi { op: "status".into(), args: serde_json::Value::Null };
                            if let Ok(Response::Wifi { data }) = client.request(req).await {
                                post(&weak, UiMsg::Wifi(data));
                            }
                        }
                        Event::Shutdown => {}
                        Event::DeviceConnected { .. } | Event::DeviceDisconnected { .. } | Event::NeedsRole { .. } => {}
                    }
                }
                *CLIENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
                post(&weak, UiMsg::Link(Some("Lost the connection to the daemon; reconnecting\u{2026}".into())));
            }
            Err(_) => {
                if may_start && failures % 20 == 0 {
                    post(&weak, UiMsg::Link(Some("Starting the DisplaySwarm daemon\u{2026}".into())));
                    start_daemon().await;
                } else if failures >= 20 {
                    post(
                        &weak,
                        UiMsg::Link(Some("The daemon is not answering. Run displayswarm-hostd in a terminal to see why.".into())),
                    );
                }
                failures += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One UI per user: a second launch exits quietly. The lock lives as long as
/// the returned file.
fn single_instance_lock() -> Option<Option<std::fs::File>> {
    let path = default_socket_path().with_file_name("ui.lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(windows)]
    {
        // Share mode 0 is an exclusive open: a second launch fails with a sharing violation.
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
        opts.open(&path).ok().map(Some)
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let file = opts.open(&path).ok()?;
        // SAFETY: plain flock(2) on a descriptor we own.
        let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if ok {
            Some(Some(file))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn current_settings(ui: &MainWindow) -> Result<GlobalSettings, String> {
    let port: u16 = ui.get_port_text().trim().parse().map_err(|_| "The port must be a number from 1 to 65535".to_string())?;
    let mut s = STATE.with(|st| st.borrow().settings.clone());
    s.port = port;
    s.target_fps = ui.get_fps() as u32;
    s.bitrate_mbps = ui.get_bitrate_mbps() as u32;
    let idx = ui.get_default_role_index();
    s.audio_sync = ui.get_audio_sync();
    s.default_role = usize::try_from(idx - 1).ok().and_then(|i| Role::ALL.get(i)).map(|r| role_key(*r).to_string());
    s.validate()?;
    Ok(s)
}

fn device_of(id: &str) -> Option<DeviceInfo> {
    STATE.with(|s| s.borrow().devices.iter().find(|d| d.device_id == id).cloned())
}

fn wire_callbacks(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_set_device_role({
        let weak = weak.clone();
        move |id, index| match usize::try_from(index).ok().and_then(|i| Role::ALL.get(i).copied()) {
            Some(role) => match role_refusal(role) {
                // Say why right here instead of asking the daemon to refuse.
                Some(why) => {
                    STATE.with(|s| {
                        if let Some(d) = s.borrow_mut().devices.iter_mut().find(|d| d.device_id == id.as_str()) {
                            d.notice = Some(format!("{} is not available here: {why}", role.label()));
                        }
                    });
                    if let Some(ui) = weak.upgrade() {
                        render_devices(&ui);
                    }
                }
                None => send(&weak, Request::SetRole { device_id: id.to_string(), role: role_key(role).into() }),
            },
            None => log::warn!("Ignoring role index {index}"),
        }
    });
    ui.on_toggle_setting({
        let weak = weak.clone();
        move |id, kind, value| {
            let Some(d) = device_of(id.as_str()) else { return };
            if kind == 4 {
                send(&weak, Request::SetPanelOff { device_id: d.device_id, panel_off: value });
                return;
            }
            let mut settings = d.settings.clone();
            match kind {
                0 => settings.audio_out = value,
                1 => settings.mic = value,
                2 => settings.clipboard = value,
                3 => settings.battery_saver = value,
                5 => settings.audio_default_sink = value,
                6 => settings.audio_mirror = value,
                7 => settings.audio_always_ready = value,
                8 => settings.delay_host_audio = value,
                _ => return,
            }
            send(&weak, Request::SetDeviceSettings { device_id: d.device_id, settings });
        }
    });
    ui.on_set_audio_mode({
        let weak = weak.clone();
        move |id, mode| {
            let Some(d) = device_of(id.as_str()) else { return };
            let mut settings = d.settings.clone();
            settings.audio_mirror = mode == 1;
            settings.audio_default_sink = mode == 2;
            send(&weak, Request::SetDeviceSettings { device_id: d.device_id, settings });
        }
    });
    ui.on_set_video_quality({
        let weak = weak.clone();
        move |id, idx| {
            let Some(d) = device_of(id.as_str()) else { return };
            let mut settings = d.settings.clone();
            settings.video_quality = match idx {
                1 => VideoQuality::Max,
                2 => VideoQuality::Balanced,
                3 => VideoQuality::DataSaver,
                _ => VideoQuality::Auto,
            };
            send(&weak, Request::SetDeviceSettings { device_id: d.device_id, settings });
        }
    });
    ui.on_set_target_output({
        let weak = weak.clone();
        move |id, out| {
            let out = out.trim();
            send(
                &weak,
                Request::SetTargetOutput {
                    device_id: id.to_string(),
                    output: (!out.is_empty()).then(|| out.to_string()),
                },
            );
        }
    });
    ui.on_disconnect_device({
        let weak = weak.clone();
        move |id| send(&weak, Request::Disconnect { device_id: id.to_string() })
    });
    ui.on_forget_device({
        let weak = weak.clone();
        move |id| send(&weak, Request::ForgetDevice { device_id: id.to_string() })
    });
    ui.on_identify_device({
        let weak = weak.clone();
        move |id| send(&weak, Request::Identify { device_id: id.to_string() })
    });
    ui.on_toggle_server({
        let weak = weak.clone();
        let ui_weak = ui.as_weak();
        move || {
            let running = ui_weak.upgrade().map(|u| u.get_server_running()).unwrap_or(false);
            send(&weak, if running { Request::StopServer } else { Request::StartServer });
        }
    });
    ui.on_apply_settings({
        let weak = weak.clone();
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match current_settings(&ui) {
                Ok(s) => {
                    ui.set_settings_status("Saved".into());
                    call(&weak, Request::SetGlobalSettings { settings: s }, |_| None);
                }
                Err(e) => ui.set_settings_status(e.into()),
            }
        }
    });
    ui.on_sync_audio_now({
        let weak = weak.clone();
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_status("Re-measuring audio latency...".into());
            }
            call(&weak, Request::SyncAudioNow, |_| Some(UiMsg::Note("Audio re-synced".into())));
        }
    });
    ui.on_set_start_on_login({
        let weak = weak.clone();
        let ui_weak = ui.as_weak();
        move |enabled| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_start_on_login(enabled);
            }
            send(&weak, Request::SetStartOnLogin { enabled });
        }
    });
    ui.on_refresh_diagnostics({
        let weak = weak.clone();
        move || {
            call(&weak, Request::GetDiagnostics, |r| match r {
                Response::Diagnostics { items } => Some(UiMsg::Diagnostics(items)),
                _ => None,
            })
        }
    });
    ui.on_refresh_logs({
        let weak = weak.clone();
        move || {
            call(&weak, Request::GetLogs { max: 400 }, |r| match r {
                Response::Logs { lines } => Some(UiMsg::Logs(lines)),
                _ => None,
            })
        }
    });
    ui.on_copy_report({
        let weak = weak.clone();
        move || {
            call(&weak, Request::GetDebugReport, |r| match r {
                Response::DebugReport { text } => Some(UiMsg::Report(text)),
                _ => None,
            })
        }
    });
    ui.on_refresh_wifi({
        let weak = weak.clone();
        move || {
            call(&weak, Request::Wifi { op: "status".into(), args: serde_json::Value::Null }, |r| match r {
                Response::Wifi { data } => Some(UiMsg::Wifi(data)),
                _ => None,
            })
        }
    });
    ui.on_start_pairing({
        let weak = weak.clone();
        move || {
            // The PIN arrives as an event, which refreshes the Wi-Fi section.
            call(&weak, Request::Wifi { op: "start_pairing".into(), args: serde_json::Value::Null }, |_| None)
        }
    });
    ui.on_open_firewall({
        let weak = weak.clone();
        move || {
            // pkexec's dialog blocks the daemon's handler, not the UI.
            if let Some(ui) = weak.upgrade() {
                ui.set_banner("Opening the firewall port\u{2026}".into());
            }
            call(&weak, Request::Wifi { op: "open_firewall".into(), args: serde_json::Value::Null }, |r| match r {
                Response::Wifi { data } => {
                    Some(UiMsg::Note(data.get("message").and_then(|m| m.as_str()).unwrap_or("Firewall updated").to_string()))
                }
                _ => None,
            })
        }
    });
    ui.on_setup_display({
        let ui_weak = ui.as_weak();
        move || {
            // Privileged and slow (pkexec): off the UI thread.
            let ui_weak = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_display_status_text("Setting up\u{2026}".into());
            }
            std::thread::spawn(move || {
                let report = displayswarm_host::display::run_setup(None);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_display_status_text(report.summary.into());
                    }
                });
            });
        }
    });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.iter().any(|a| a == "--cli" || a == "--headless");
    let standalone = headless || args.iter().any(|a| a == "--standalone");

    let logs = logging::LogBuffer::new();
    logging::init(&logs);

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _ = RT.set(runtime.handle().clone());

    // --standalone: the daemon lives in this process, on a private socket.
    let mut embedded = None;
    let transport: Arc<dyn IpcTransport> = if standalone {
        transport_at(default_socket_path().with_file_name(format!("standalone-{}.sock", std::process::id())))
    } else {
        default_transport()
    };
    if standalone {
        let opts = DaemonOptions {
            transport: transport.clone(),
            tray: false,
            start_server: None,
            manager: None,
            logs,
        };
        embedded = Some(runtime.block_on(daemon::start(opts))?);
    }

    if headless {
        let d = embedded.take().expect("headless implies standalone");
        log::info!("Running DisplaySwarm without a window; Ctrl-C stops it");
        runtime.block_on(async {
            tokio::select! {
                _ = d.wait_quit() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            d.shutdown().await;
        });
        return Ok(());
    }

    let _lock = match single_instance_lock() {
        Some(l) => l,
        None if !standalone => {
            eprintln!("DisplaySwarm is already open.");
            return Ok(());
        }
        None => None,
    };

    let ui = MainWindow::new()?;
    let model = Rc::new(VecModel::<DeviceRow>::default());
    STATE.with(|s| s.borrow_mut().model = Some(model.clone()));
    ui.set_devices(ModelRc::from(model));
    ui.set_role_names(ModelRc::new(VecModel::from(role_labels(&[]))));
    wire_callbacks(&ui);

    runtime.spawn(connection_loop(ui.as_weak(), transport, !standalone));
    // The Wi-Fi section asks once at startup; the wireless work fills the answer.
    let weak = ui.as_weak();
    ui.invoke_refresh_wifi();
    let _ = weak;

    ui.run()?;

    // Closing the window: the daemon (when separate) keeps running.
    if let Some(d) = embedded {
        runtime.block_on(d.shutdown());
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}
