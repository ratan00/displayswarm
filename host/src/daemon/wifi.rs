//! Wi-Fi pairing and trusted phones over IPC (Phase 9).
//!
//! The UI sends `{"id":N,"cmd":"wifi","op":"<name>","args":{...}}` and gets
//! `{"kind":"wifi","data":<JSON>}` back. Ops:
//!
//! * `status`: `{status, detail, listen, pin, pin_secs, trusted}`.
//! * `start_pairing`: opens a PIN any phone may use for two minutes; returns
//!   `{pin, valid_secs}`. The PIN also goes out as `Event::PairingPin`, so the
//!   tray's notification shows it. A phone that asks to pair on its own gets a
//!   PIN the same way, without this op.
//! * `cancel_pairing`, `list_trusted`, `revoke {device_id}`.
//! * `open_firewall`: adds the listen port and mDNS to ufw/firewalld via `pkexec`
//!   (shows polkit's dialog, so it can take as long as the user does).

use serde_json::{json, Value};

use super::core::DaemonCore;
use crate::transport::firewall;
use crate::transport::pairing::PIN_TTL;

/// Handles one `wifi` request. Runs on the daemon's runtime; may be slow.
pub async fn handle(core: &DaemonCore, op: &str, args: &Value) -> Result<Value, String> {
    let pairing = core.session_manager().pairing().clone();
    let trusted = || -> Value {
        pairing
            .trusted_devices()
            .into_iter()
            .map(|d| json!({ "device_id": d.device_id, "name": d.name, "paired_at": d.paired_at }))
            .collect()
    };
    match op {
        "status" => {
            let state = core.state();
            let offer = pairing.pending_offer();
            let n = pairing.trusted_devices().len();
            Ok(json!({
                "status": if state.server.running { format!("Listening on {}", state.listen) } else { "Server stopped".into() },
                "detail": match &offer {
                    Some(o) if o.pin.is_some() => format!("Pairing PIN {} ({} s left)", o.pin.as_deref().unwrap_or(""), o.remaining.as_secs()),
                    _ => format!("{n} paired phone(s). A new phone asks for a PIN when it connects."),
                },
                "listen": state.listen,
                "firewall": firewall::detect().map(|f| f.name()),
                "pin": offer.as_ref().and_then(|o| o.pin.clone()),
                "pin_secs": offer.as_ref().map(|o| o.remaining.as_secs()),
                "trusted": trusted(),
            }))
        }
        "start_pairing" => {
            let pin = pairing
                .open_pin_offer(PIN_TTL)
                .ok_or("Pairing is locked after too many wrong PINs; try again in a little while")?;
            // `open_pin_offer` has already sent `Event::PairingPin` through
            // the pairing listener.
            Ok(json!({ "pin": pin, "valid_secs": PIN_TTL.as_secs() }))
        }
        "open_firewall" => {
            let port = core.state().settings.port;
            let msg = tokio::task::spawn_blocking(move || firewall::open_port(port))
                .await
                .map_err(|e| e.to_string())??;
            Ok(json!({ "message": msg }))
        }
        "cancel_pairing" => {
            pairing.close_offer();
            Ok(json!({}))
        }
        "list_trusted" => Ok(json!({ "trusted": trusted() })),
        "revoke" => {
            let id = args.get("device_id").and_then(Value::as_str).ok_or("revoke needs device_id")?;
            Ok(json!({ "revoked": pairing.revoke(id) }))
        }
        other => Err(format!("wifi: unsupported operation {other:?}")),
    }
}
