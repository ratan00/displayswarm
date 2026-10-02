//! The daemon's presence on the desktop: a StatusNotifierItem tray icon with a
//! device menu, and desktop notifications (always for "choose a role", and for
//! connect/disconnect when no tray host is around, e.g. GNOME without the
//! AppIndicator extension).
//!
//! The tray never blocks the daemon: menu callbacks only post a [`TrayCmd`] to
//! a channel, [`run`] executes it.

pub mod notify;

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{MenuItem, Status, TrayMethods};
use tokio::sync::mpsc;

use crate::daemon::core::DaemonCore;
use crate::devices::Role;
use crate::ipc::proto::{role_key, DeviceInfo, Event, Request};
use notify::{notice_for, parse_action, NoticeAction};

/// What a menu click asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayCmd {
    OpenUi,
    SetRole(String, Role),
    Disconnect(String),
    ToggleServer,
    Quit,
}

/// How often to look again for a tray host after failing to find one (it may
/// simply start after us at login).
const SNI_RETRY: Duration = Duration::from_secs(20);

struct VmTray {
    devices: Vec<DeviceInfo>,
    server_running: bool,
    tx: mpsc::UnboundedSender<TrayCmd>,
}

/// "Pixel 8 - Streaming, 60 fps" for a menu entry.
pub fn device_label(d: &DeviceInfo) -> String {
    let state = if d.connected { d.state.as_str() } else { "not connected" };
    format!("{} \u{2013} {}", d.display_name(), state)
}

impl VmTray {
    fn needs_attention(&self) -> bool {
        self.devices.iter().any(|d| d.connected && d.role.is_none())
    }

    fn device_menu(&self, d: &DeviceInfo) -> MenuItem<Self> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        for role in Role::ALL {
            let id = d.device_id.clone();
            items.push(
                CheckmarkItem {
                    label: role.label().into(),
                    checked: d.role.as_deref() == Some(role_key(role)),
                    activate: Box::new(move |this: &mut Self| {
                        let _ = this.tx.send(TrayCmd::SetRole(id.clone(), role));
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if d.connected {
            items.push(MenuItem::Separator);
            let id = d.device_id.clone();
            items.push(
                StandardItem {
                    label: "Disconnect".into(),
                    activate: Box::new(move |this: &mut Self| {
                        let _ = this.tx.send(TrayCmd::Disconnect(id.clone()));
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        SubMenu { label: device_label(d), submenu: items, ..Default::default() }.into()
    }
}

impl ksni::Tray for VmTray {
    fn id(&self) -> String {
        "displayswarm".into()
    }

    fn title(&self) -> String {
        "DisplaySwarm".into()
    }

    fn icon_name(&self) -> String {
        "video-display".into()
    }

    fn attention_icon_name(&self) -> String {
        "dialog-question".into()
    }

    fn status(&self) -> Status {
        if self.needs_attention() {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayCmd::OpenUi);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m: Vec<MenuItem<Self>> = vec![StandardItem {
            label: "Open DisplaySwarm".into(),
            icon_name: "window-new".into(),
            activate: Box::new(|this: &mut Self| {
                let _ = this.tx.send(TrayCmd::OpenUi);
            }),
            ..Default::default()
        }
        .into()];
        m.push(MenuItem::Separator);
        if self.devices.is_empty() {
            m.push(StandardItem { label: "No devices yet".into(), enabled: false, ..Default::default() }.into());
        }
        for d in &self.devices {
            m.push(self.device_menu(d));
        }
        m.push(MenuItem::Separator);
        m.push(
            StandardItem {
                label: if self.server_running { "Stop server" } else { "Start server" }.into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayCmd::ToggleServer);
                }),
                ..Default::default()
            }
            .into(),
        );
        m.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayCmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        m
    }
}

/// Starts the UI process (`displayswarm`, next to this executable or on PATH).
/// Does nothing if one started by us is still running.
pub fn open_ui(child: &mut Option<std::process::Child>) {
    if let Some(c) = child {
        if matches!(c.try_wait(), Ok(None)) {
            log::info!("The UI is already open");
            return;
        }
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("displayswarm")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "displayswarm".into());
    match std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => *child = Some(c),
        Err(e) => log::warn!("Could not start the UI ({}): {e}", exe.display()),
    }
}

/// Runs the tray and the notifications until the daemon quits. Never returns
/// an error: a missing tray or notification service only degrades.
pub async fn run(core: Arc<DaemonCore>) {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<TrayCmd>();
    let mut quit = core.quit_signal();
    let mut events = core.subscribe();
    let mut notifier = match notify::Notifier::connect().await {
        Ok(n) => Some(n),
        Err(e) => {
            log::warn!("No notification service: {e}");
            None
        }
    };
    let mut actions = match &notifier {
        Some(n) => n.action_stream().await.ok(),
        None => None,
    };

    let mut handle: Option<ksni::Handle<VmTray>> = None;
    let mut next_sni_try = tokio::time::Instant::now();
    let mut ui_child: Option<std::process::Child> = None;

    loop {
        // (Re)try to get a tray icon; also notice when the host went away.
        if handle.as_ref().map(|h| h.is_closed()).unwrap_or(false) {
            handle = None;
            core.set_tray_mode("notifications");
        }
        if handle.is_none() && tokio::time::Instant::now() >= next_sni_try {
            let tray = VmTray {
                devices: core.devices(),
                server_running: core.server_info().running,
                tx: cmd_tx.clone(),
            };
            match tray.spawn().await {
                Ok(h) => {
                    log::info!("Tray icon registered");
                    core.set_tray_mode("tray");
                    handle = Some(h);
                }
                Err(e) => {
                    log::info!("No tray available ({e}); using notifications");
                    core.set_tray_mode("notifications");
                    next_sni_try = tokio::time::Instant::now() + SNI_RETRY;
                }
            }
        }

        tokio::select! {
            _ = quit.changed() => break,
            _ = tokio::time::sleep(SNI_RETRY) => {}
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                run_cmd(&core, cmd, &mut ui_child).await;
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        Event::DevicesChanged { devices: core.devices() }
                    }
                    Err(_) => break,
                };
                match &ev {
                    Event::DevicesChanged { .. } | Event::ServerChanged { .. } => {
                        if let Some(h) = &handle {
                            let devices = core.devices();
                            let running = core.server_info().running;
                            h.update(move |t| {
                                t.devices = devices;
                                t.server_running = running;
                            }).await;
                        }
                    }
                    _ => {}
                }
                let tray_present = handle.is_some();
                if let (Some(n), Some(notice)) = (notifier.as_mut(), notice_for(&ev, tray_present)) {
                    if let Err(e) = n.show(&notice).await {
                        log::debug!("Could not show a notification: {e}");
                    }
                }
            }
            act = async {
                match actions.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(sig) = act else { actions = None; continue };
                let Ok(args) = sig.args() else { continue };
                let device = notifier.as_ref().and_then(|n| n.device_of(args.id)).map(String::from);
                match (parse_action(args.action_key), device) {
                    (Some(NoticeAction::OpenUi), _) => run_cmd(&core, TrayCmd::OpenUi, &mut ui_child).await,
                    (Some(NoticeAction::SetRole(r)), Some(d)) => run_cmd(&core, TrayCmd::SetRole(d, r), &mut ui_child).await,
                    (Some(NoticeAction::Disconnect), Some(d)) => run_cmd(&core, TrayCmd::Disconnect(d), &mut ui_child).await,
                    _ => {}
                }
            }
        }
    }
    if let Some(h) = handle {
        h.shutdown().await;
    }
}

async fn run_cmd(core: &Arc<DaemonCore>, cmd: TrayCmd, ui_child: &mut Option<std::process::Child>) {
    let result = match cmd {
        TrayCmd::OpenUi => {
            open_ui(ui_child);
            return;
        }
        TrayCmd::SetRole(device_id, role) => {
            core.handle(Request::SetRole { device_id, role: role_key(role).into() }).await
        }
        TrayCmd::Disconnect(device_id) => core.handle(Request::Disconnect { device_id }).await,
        TrayCmd::ToggleServer => {
            if core.server_info().running {
                core.handle(Request::StopServer).await
            } else {
                core.handle(Request::StartServer).await
            }
        }
        TrayCmd::Quit => core.handle(Request::Quit).await,
    };
    if let Err(e) = result {
        log::warn!("Tray action failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::DeviceSettings;

    fn dev(connected: bool, role: Option<Role>, state: &str) -> DeviceInfo {
        DeviceInfo {
            device_id: "id".into(),
            name: "Pixel".into(),
            role: role.map(|r| role_key(r).to_string()),
            effective_role: None,
            connected,
            state: state.into(),
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
    fn labels_and_attention() {
        assert_eq!(device_label(&dev(true, None, "Connecting")), "Pixel \u{2013} Connecting");
        assert_eq!(device_label(&dev(false, None, "Not connected")), "Pixel \u{2013} not connected");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut t = VmTray { devices: vec![dev(true, None, "x")], server_running: false, tx };
        assert!(t.needs_attention());
        t.devices = vec![dev(true, Some(Role::Extend), "x"), dev(false, None, "x")];
        assert!(!t.needs_attention());
    }

    #[test]
    fn menu_has_a_submenu_per_device() {
        use ksni::Tray;
        let (tx, _rx) = mpsc::unbounded_channel();
        let t = VmTray { devices: vec![dev(true, Some(Role::Tablet), "Input only")], server_running: true, tx };
        let m = t.menu();
        assert!(m.iter().any(|i| matches!(i, MenuItem::SubMenu(s) if s.label.starts_with("Pixel"))));
        assert!(m.iter().any(|i| matches!(i, MenuItem::Standard(s) if s.label == "Stop server")));
    }
}
