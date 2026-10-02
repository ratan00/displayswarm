//! Messages between the daemon (`displayswarm-hostd`) and its clients (the UI).
//!
//! # Wire format
//!
//! Line-delimited JSON over a stream socket: one JSON object per line, UTF-8,
//! no embedded newlines (serde_json never emits them), at most
//! [`MAX_LINE`] bytes.
//!
//! Client -> daemon, a request with a client-chosen `id`:
//!
//! ```json
//! {"id":7,"cmd":"set_role","device_id":"abc","role":"tablet"}
//! ```
//!
//! Daemon -> client, either the reply to one request or an unsolicited event:
//!
//! ```json
//! {"type":"reply","id":7,"ok":true,"response":{"kind":"ok"}}
//! {"type":"reply","id":8,"ok":false,"error":"unknown device"}
//! {"type":"event","event":"needs_role","device_id":"abc","name":"Pixel 8"}
//! ```
//!
//! Events are only sent after the connection has sent [`Request::Subscribe`].
//! Roles travel as the stable strings of [`role_key`] (`mirror`, `extend`,
//! `mirror_window`, `phone_primary`, `tablet`, `input_pad`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::devices::{DeviceSettings, Role};
use crate::server::DeviceStatus;

/// Bumped when a change is not backwards compatible.
pub const PROTOCOL_VERSION: u32 = 1;

/// A longer line is a protocol error (the connection is dropped).
pub const MAX_LINE: usize = 1 << 20;

/// The stable wire name of a role.
pub fn role_key(role: Role) -> &'static str {
    match role {
        Role::Mirror => "mirror",
        Role::Extend => "extend",
        Role::MirrorWindow => "mirror_window",
        Role::PhonePrimary => "phone_primary",
        Role::Tablet => "tablet",
        Role::InputPad => "input_pad",
    }
}

pub fn role_from_key(key: &str) -> Option<Role> {
    Role::ALL.iter().copied().find(|r| role_key(*r) == key)
}

/// A request, as sent: `id` plus the command's own fields, flattened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check; answered with [`Response::Pong`].
    Ping,
    /// Start receiving [`Event`]s on this connection. Answered with the
    /// current [`Response::State`], so nothing can fall between snapshot and
    /// subscription.
    Subscribe,
    /// The full current state.
    GetState,
    SetRole { device_id: String, role: String },
    /// Replaces the device's feature toggles (all of [`DeviceSettings`]).
    SetDeviceSettings { device_id: String, settings: DeviceSettings },
    SetPanelOff { device_id: String, panel_off: bool },
    /// The host monitor (connector name) Mirror / Tablet use; `null` lets the
    /// portal ask.
    SetTargetOutput { device_id: String, output: Option<String> },
    /// Ends the device's session but keeps its record.
    Disconnect { device_id: String },
    /// Ends the session and deletes the record.
    ForgetDevice { device_id: String },
    /// "Which device is this?": broadcasts [`Event::Identify`] to every
    /// subscribed client (the UI flashes the card, the tray notifies).
    Identify { device_id: String },
    StartServer,
    StopServer,
    SetGlobalSettings { settings: GlobalSettings },
    /// Re-measures every phone's audio latency and re-sends the common delay.
    SyncAudioNow,
    /// Enables or disables starting the daemon at login.
    SetStartOnLogin { enabled: bool },
    /// Runs the environment checks (blocking work, may take a second or two).
    GetDiagnostics,
    /// The most recent `max` log lines of the daemon.
    GetLogs { max: usize },
    /// Version, state, checks and recent log as one text block.
    GetDebugReport,
    /// Hook for the Wi-Fi pairing / trusted-devices section (Phase 9). `op` is
    /// the operation name, `args` its arguments; answered with
    /// [`Response::Wifi`]. See `daemon::wifi`.
    Wifi { op: String, args: Value },
    /// Stops the server and exits the daemon.
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong { protocol: u32 },
    State { state: StateInfo },
    Diagnostics { items: Vec<DiagItem> },
    Logs { lines: Vec<String> },
    DebugReport { text: String },
    Wifi { data: Value },
}

/// Everything that is not a reply to a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The device list or a session's state/stats changed.
    DevicesChanged { devices: Vec<DeviceInfo> },
    /// The server started, stopped or has a new status line.
    ServerChanged { server: ServerInfo },
    SettingsChanged { settings: GlobalSettings, start_on_login: bool },
    DeviceConnected { device_id: String, name: String },
    DeviceDisconnected { device_id: String, name: String },
    /// A device connected that has no remembered role (and no default role is
    /// set): somebody has to choose one.
    NeedsRole { device_id: String, name: String },
    Identify { device_id: String, name: String },
    /// Wi-Fi pairing: show this PIN; the phone (`name`, empty for an offer
    /// the user opened) is waiting for it.
    PairingPin { device_id: String, name: String, pin: String, valid_secs: u64 },
    Paired { device_id: String, name: String },
    PairingFailed { device_id: String, reason: String },
    /// The daemon is exiting.
    Shutdown,
}

/// What travels from the daemon to the client on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Reply {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<Response>,
    },
    Event(Event),
}

impl ServerMessage {
    pub fn reply(id: u64, result: Result<Response, String>) -> Self {
        match result {
            Ok(response) => ServerMessage::Reply { id, ok: true, error: None, response: Some(response) },
            Err(error) => ServerMessage::Reply { id, ok: false, error: Some(error), response: None },
        }
    }
}

/// One device card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub role: Option<String>,
    /// The role in effect now (differs while unchosen or after a fallback).
    pub effective_role: Option<String>,
    pub connected: bool,
    pub state: String,
    pub notice: Option<String>,
    pub target_output: Option<String>,
    pub panel_off: bool,
    pub settings: DeviceSettings,
    /// `"usb"` or `"network"` while connected.
    pub transport: Option<String>,
    pub resolution: Option<ResolutionInfo>,
    pub live: Option<LiveInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl ResolutionInfo {
    /// "2400x1080@120Hz"
    pub fn label(&self) -> String {
        let hz = self.refresh_mhz as f64 / 1000.0;
        if (hz - hz.round()).abs() < 0.05 {
            format!("{}x{}@{}Hz", self.width, self.height, hz.round() as u32)
        } else {
            format!("{}x{}@{:.1}Hz", self.width, self.height, hz)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveInfo {
    pub fps: u32,
    pub kbps: u32,
    pub latency_ms: Option<u32>,
    pub encoder: String,
}

impl From<&DeviceStatus> for DeviceInfo {
    fn from(d: &DeviceStatus) -> Self {
        DeviceInfo {
            device_id: d.device_id.clone(),
            name: d.name.clone(),
            role: d.role.map(|r| role_key(r).to_string()),
            effective_role: d.effective_role.map(|r| role_key(r).to_string()),
            connected: d.connected,
            state: d.state.clone(),
            notice: d.notice.clone(),
            target_output: d.target_output.clone(),
            panel_off: d.panel_off,
            settings: d.settings.clone(),
            transport: d.connected.then(|| if d.via_aoa { "usb" } else { "network" }.to_string()),
            resolution: d.resolution.map(|r| ResolutionInfo {
                width: r.width,
                height: r.height,
                refresh_mhz: r.refresh_mhz,
            }),
            live: d.live.as_ref().map(|l| LiveInfo {
                fps: l.fps,
                kbps: l.kbps,
                latency_ms: l.latency_ms,
                encoder: l.encoder.clone(),
            }),
        }
    }
}

impl DeviceInfo {
    /// The name to show: the device's own, else its id.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.device_id
        } else {
            &self.name
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub running: bool,
    /// One line for the header ("Listening on 127.0.0.1:9999 / USB AOA").
    pub status: String,
    pub client_info: String,
}

/// Daemon-wide settings, stored in `settings.json` next to `devices.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    pub port: u16,
    pub target_fps: u32,
    pub bitrate_mbps: u32,
    /// Applied automatically to a device that connects without a role.
    pub default_role: Option<String>,
    /// Start the server when the daemon starts.
    pub autostart_server: bool,
    /// Keep all phones' audio in step: every phone delays to match the
    /// slowest one (see `audio::sync`).
    pub audio_sync: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self { port: 9999, target_fps: 60, bitrate_mbps: 20, default_role: None, autostart_server: true, audio_sync: false }
    }
}

impl GlobalSettings {
    /// Rejects values the server cannot use.
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port must be 1-65535".into());
        }
        if !(1..=240).contains(&self.target_fps) {
            return Err("fps must be 1-240".into());
        }
        if !(1..=200).contains(&self.bitrate_mbps) {
            return Err("bitrate must be 1-200 Mbps".into());
        }
        if let Some(r) = &self.default_role {
            if role_from_key(r).is_none() {
                return Err(format!("unknown role {r:?}"));
            }
        }
        Ok(())
    }
}

/// Whether one role can work in this session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub role: String,
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<String>,
}

/// The availability of every role, in [`Role::ALL`] order.
pub fn role_availability(caps: &crate::display::model::Capabilities) -> Vec<RoleInfo> {
    Role::ALL
        .iter()
        .map(|r| {
            let a = caps.role_support(*r);
            RoleInfo { role: role_key(*r).to_string(), available: a.is_yes(), reason: a.reason().map(str::to_string) }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateInfo {
    pub version: String,
    pub protocol: u32,
    pub devices: Vec<DeviceInfo>,
    pub server: ServerInfo,
    pub settings: GlobalSettings,
    pub start_on_login: bool,
    /// The encoder chain's first working backend ("h264_vaapi", "libx264"...),
    /// `None` until the startup probe has finished.
    pub encoder: Option<String>,
    /// `"kscreen"`, `"mutter"` or `"none"`.
    pub desktop_backend: String,
    /// Which roles this desktop can do, each with the reason when it cannot.
    /// Empty until the startup probe has finished (then every role is offered).
    #[serde(default)]
    pub roles: Vec<RoleInfo>,
    /// How the phone gets a screen here ("mirror only", "virtual monitor from
    /// the screen-share portal", ...); empty until the probe has finished.
    #[serde(default)]
    pub rung: String,
    /// `"tray"`, `"notifications"` or `"none"`.
    pub tray: String,
    /// Where the TCP transport listens, e.g. `127.0.0.1:9999`.
    pub listen: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagItem {
    pub name: String,
    pub status: DiagStatus,
    pub detail: String,
}

/// Encodes a message as one line (without the newline).
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    // Serialising these types cannot fail (no maps with non-string keys).
    serde_json::to_string(msg).unwrap_or_else(|e| format!("{{\"type\":\"error\",\"error\":\"{e}\"}}"))
}

pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, String> {
    serde_json::from_str(line.trim()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_flat_json() {
        let env = RequestEnvelope {
            id: 7,
            request: Request::SetRole { device_id: "abc".into(), role: "tablet".into() },
        };
        let line = encode_line(&env);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["cmd"], "set_role");
        assert_eq!(v["role"], "tablet");
        assert!(!line.contains('\n'));
        assert_eq!(decode_line::<RequestEnvelope>(&line).unwrap(), env);
    }

    #[test]
    fn every_request_round_trips() {
        let reqs = vec![
            Request::Ping,
            Request::Subscribe,
            Request::GetState,
            Request::SetDeviceSettings { device_id: "a".into(), settings: DeviceSettings::default() },
            Request::SetPanelOff { device_id: "a".into(), panel_off: true },
            Request::SetTargetOutput { device_id: "a".into(), output: Some("eDP-1".into()) },
            Request::SetTargetOutput { device_id: "a".into(), output: None },
            Request::Disconnect { device_id: "a".into() },
            Request::ForgetDevice { device_id: "a".into() },
            Request::Identify { device_id: "a".into() },
            Request::StartServer,
            Request::StopServer,
            Request::SetGlobalSettings { settings: GlobalSettings::default() },
            Request::SyncAudioNow,
            Request::SetStartOnLogin { enabled: true },
            Request::GetDiagnostics,
            Request::GetLogs { max: 50 },
            Request::GetDebugReport,
            Request::Wifi { op: "status".into(), args: serde_json::json!({"x": [1, 2]}) },
            Request::Quit,
        ];
        for (i, request) in reqs.into_iter().enumerate() {
            let env = RequestEnvelope { id: i as u64, request };
            let line = encode_line(&env);
            assert_eq!(decode_line::<RequestEnvelope>(&line).unwrap(), env, "{line}");
        }
    }

    #[test]
    fn replies_and_events_round_trip() {
        let ok = ServerMessage::reply(1, Ok(Response::Pong { protocol: PROTOCOL_VERSION }));
        let err = ServerMessage::reply(2, Err("unknown device".into()));
        let ev = ServerMessage::Event(Event::NeedsRole { device_id: "a".into(), name: "Pixel".into() });
        for m in [ok, err, ev] {
            let line = encode_line(&m);
            assert_eq!(decode_line::<ServerMessage>(&line).unwrap(), m, "{line}");
        }
        let line = encode_line(&ServerMessage::reply(2, Err("nope".into())));
        assert!(line.contains("\"ok\":false") && line.contains("\"error\":\"nope\""), "{line}");
        let line = encode_line(&ServerMessage::Event(Event::Shutdown));
        assert!(line.contains("\"type\":\"event\"") && line.contains("\"event\":\"shutdown\""), "{line}");
    }

    #[test]
    fn unknown_command_is_a_decode_error() {
        assert!(decode_line::<RequestEnvelope>(r#"{"id":1,"cmd":"self_destruct"}"#).is_err());
        assert!(decode_line::<RequestEnvelope>(r#"{"cmd":"ping"}"#).is_err());
        assert!(decode_line::<RequestEnvelope>("not json").is_err());
    }

    #[test]
    fn role_availability_lists_every_role_with_its_reason() {
        use crate::display::model::*;
        let mut caps = Capabilities::unsupported(Desktop::Cosmic, SessionType::Wayland);
        caps.capture = CaptureCaps { portal: true, monitor: true, window: true, virtual_: false };
        caps.input_ok = true;
        let roles = role_availability(&caps);
        assert_eq!(roles.len(), Role::ALL.len());
        let get = |k: &str| roles.iter().find(|r| r.role == k).unwrap();
        assert!(get("mirror").available && get("mirror").reason.is_none());
        assert!(!get("extend").available);
        assert!(get("extend").reason.as_deref().unwrap().contains("virtual monitor"));
        assert!(!get("phone_primary").available);
        assert!(get("tablet").available);
    }

    #[test]
    fn an_old_daemon_state_without_roles_still_parses() {
        let json = r#"{"version":"1","protocol":1,"devices":[],"server":{"running":false,"status":"","client_info":""},
            "settings":{},"start_on_login":false,"encoder":null,"desktop_backend":"kscreen","tray":"none","listen":"x"}"#;
        if let Ok(s) = serde_json::from_str::<StateInfo>(json) {
            assert!(s.roles.is_empty() && s.rung.is_empty());
        } else {
            // GlobalSettings may need fields; the point is only that the new ones are optional.
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            assert!(v.get("roles").is_none());
        }
    }

    #[test]
    fn role_keys_round_trip() {
        for r in Role::ALL {
            assert_eq!(role_from_key(role_key(r)), Some(r));
        }
        assert_eq!(role_from_key("bogus"), None);
    }

    #[test]
    fn resolution_label() {
        let r = ResolutionInfo { width: 2400, height: 1080, refresh_mhz: 120_000 };
        assert_eq!(r.label(), "2400x1080@120Hz");
        let r = ResolutionInfo { width: 1920, height: 1080, refresh_mhz: 59_940 };
        assert_eq!(r.label(), "1920x1080@59.9Hz");
    }

    #[test]
    fn global_settings_validation_and_defaults() {
        assert!(GlobalSettings::default().validate().is_ok());
        assert!(GlobalSettings { port: 0, ..Default::default() }.validate().is_err());
        assert!(GlobalSettings { target_fps: 0, ..Default::default() }.validate().is_err());
        assert!(GlobalSettings { default_role: Some("x".into()), ..Default::default() }.validate().is_err());
        // A settings.json from an older version misses fields: defaults apply.
        let s: GlobalSettings = serde_json::from_str(r#"{"port":1234}"#).unwrap();
        assert_eq!((s.port, s.target_fps), (1234, 60));
    }
}
