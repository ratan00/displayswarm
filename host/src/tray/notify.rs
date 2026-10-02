//! Desktop notifications with actions (`org.freedesktop.Notifications`): the
//! fallback when there is no StatusNotifier host, and the way to ask "choose a
//! role for this device" either way.

use std::collections::HashMap;

use zbus::zvariant::Value;

use crate::ipc::proto::Event;
use crate::devices::Role;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;
}

/// One notification to show.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub device_id: String,
    pub summary: String,
    pub body: String,
    /// `(action key, label)`. The key `default` is a click on the notification.
    pub actions: Vec<(String, String)>,
}

/// What an action key asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum NoticeAction {
    OpenUi,
    SetRole(Role),
    Disconnect,
}

pub fn parse_action(key: &str) -> Option<NoticeAction> {
    match key {
        "default" | "open" => Some(NoticeAction::OpenUi),
        "disconnect" => Some(NoticeAction::Disconnect),
        _ => key
            .strip_prefix("role:")
            .and_then(crate::ipc::proto::role_from_key)
            .map(NoticeAction::SetRole),
    }
}

/// The notification for an event, or `None` if it is not worth one.
/// `tray_present`: connect/disconnect are only announced when there is no tray
/// icon to show them.
pub fn notice_for(event: &Event, tray_present: bool) -> Option<Notice> {
    let name_of = |name: &str, id: &str| if name.is_empty() { id.to_string() } else { name.to_string() };
    match event {
        Event::NeedsRole { device_id, name } => Some(Notice {
            device_id: device_id.clone(),
            summary: format!("{} connected", name_of(name, device_id)),
            body: "Choose what to use it for.".into(),
            actions: vec![
                ("role:extend".into(), "Extend".into()),
                ("role:tablet".into(), "Tablet".into()),
                ("role:mirror".into(), "Mirror".into()),
                ("open".into(), "More\u{2026}".into()),
            ],
        }),
        Event::DeviceConnected { device_id, name } if !tray_present => Some(Notice {
            device_id: device_id.clone(),
            summary: format!("{} connected", name_of(name, device_id)),
            body: String::new(),
            actions: vec![("disconnect".into(), "Disconnect".into()), ("open".into(), "Open DisplaySwarm".into())],
        }),
        Event::DeviceDisconnected { device_id, name } if !tray_present => Some(Notice {
            device_id: device_id.clone(),
            summary: format!("{} disconnected", name_of(name, device_id)),
            body: String::new(),
            actions: vec![("open".into(), "Open DisplaySwarm".into())],
        }),
        Event::Identify { device_id, name } => Some(Notice {
            device_id: device_id.clone(),
            summary: format!("This is {}", name_of(name, device_id)),
            body: String::new(),
            actions: Vec::new(),
        }),
        // Shown whatever the tray: the PIN exists nowhere else.
        Event::PairingPin { device_id, name, pin, valid_secs } => Some(Notice {
            device_id: format!("pair:{device_id}"),
            summary: format!("DisplaySwarm pairing PIN: {pin}"),
            body: if name.is_empty() {
                format!("Enter it on the phone. Valid for {valid_secs} s.")
            } else {
                format!("{name} wants to connect over Wi-Fi. Enter this PIN on it. Valid for {valid_secs} s.")
            },
            actions: Vec::new(),
        }),
        Event::Paired { device_id, name } => Some(Notice {
            device_id: format!("pair:{device_id}"),
            summary: format!("{} is paired", name_of(name, device_id)),
            body: "It can now connect over Wi-Fi.".into(),
            actions: Vec::new(),
        }),
        Event::PairingFailed { device_id, reason } => Some(Notice {
            device_id: format!("pair:{device_id}"),
            summary: "Pairing failed".into(),
            body: reason.clone(),
            actions: Vec::new(),
        }),
        _ => None,
    }
}

/// A connection to the notification service.
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    /// Last notification id per device, so a device's messages replace each
    /// other instead of piling up.
    last: HashMap<String, u32>,
}

impl Notifier {
    pub async fn connect() -> zbus::Result<Self> {
        let conn = zbus::Connection::session().await?;
        Ok(Self { proxy: NotificationsProxy::new(&conn).await?, last: HashMap::new() })
    }

    pub async fn action_stream(&self) -> zbus::Result<ActionInvokedStream> {
        self.proxy.receive_action_invoked().await
    }

    /// Shows the notice; returns the notification id.
    pub async fn show(&mut self, notice: &Notice) -> zbus::Result<u32> {
        let mut actions: Vec<&str> = Vec::new();
        for (key, label) in &notice.actions {
            actions.push(key);
            actions.push(label);
        }
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("displayswarm"));
        let replaces = self.last.get(&notice.device_id).copied().unwrap_or(0);
        let id = self
            .proxy
            .notify("DisplaySwarm", replaces, "video-display", &notice.summary, &notice.body, &actions, hints, 8000)
            .await?;
        self.last.insert(notice.device_id.clone(), id);
        Ok(id)
    }

    /// The device that owns notification `id`.
    pub fn device_of(&self, id: u32) -> Option<&str> {
        self.last.iter().find(|(_, v)| **v == id).map(|(k, _)| k.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::proto::role_key;

    #[test]
    fn needs_role_offers_role_buttons_even_with_a_tray() {
        let ev = Event::NeedsRole { device_id: "d".into(), name: "Pixel".into() };
        let n = notice_for(&ev, true).unwrap();
        assert_eq!(n.summary, "Pixel connected");
        assert!(n.actions.iter().any(|(k, _)| k == "role:tablet"));
        for (k, _) in &n.actions {
            assert!(parse_action(k).is_some(), "unparseable action {k}");
        }
    }

    #[test]
    fn connect_and_disconnect_only_without_a_tray() {
        let c = Event::DeviceConnected { device_id: "d".into(), name: "".into() };
        assert!(notice_for(&c, true).is_none());
        assert_eq!(notice_for(&c, false).unwrap().summary, "d connected");
        let d = Event::DeviceDisconnected { device_id: "d".into(), name: "Pixel".into() };
        assert!(notice_for(&d, true).is_none());
        assert_eq!(notice_for(&d, false).unwrap().summary, "Pixel disconnected");
        assert!(notice_for(&Event::Shutdown, false).is_none());
    }

    #[test]
    fn action_keys_parse() {
        assert_eq!(parse_action("default"), Some(NoticeAction::OpenUi));
        assert_eq!(parse_action("disconnect"), Some(NoticeAction::Disconnect));
        assert_eq!(parse_action(&format!("role:{}", role_key(Role::PhonePrimary))), Some(NoticeAction::SetRole(Role::PhonePrimary)));
        assert_eq!(parse_action("role:nonsense"), None);
        assert_eq!(parse_action("whatever"), None);
    }
}
