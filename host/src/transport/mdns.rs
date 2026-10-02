//! mDNS advertisement of the host as `_displayswarm._tcp`, so phones on the same
//! network find it without typing an address (Android: `NsdManager`).
//!
//! TXT records (all ASCII):
//!
//! | key    | value                                                        |
//! |--------|--------------------------------------------------------------|
//! | `v`    | protocol version (`2`)                                       |
//! | `name` | the host's display name                                      |
//! | `id`   | first 16 hex digits of the certificate's SHA-256 fingerprint |
//! | `pair` | `1` when a phone must pair before it can connect             |
//!
//! The fingerprint prefix is only a hint to recognise an already-paired host;
//! the phone still pins the full fingerprint during TLS.

use super::tls::{fingerprint_hex, Fingerprint};
use crate::protocol::wire::VERSION;

pub const SERVICE_TYPE: &str = "_displayswarm._tcp.local.";

/// The TXT key/value pairs for a host.
pub fn txt_records(host_name: &str, fingerprint: &Fingerprint, pairing_required: bool) -> Vec<(String, String)> {
    // A TXT entry (key=value) is limited to 255 bytes; keep the name well below.
    let name: String = host_name.chars().take(60).collect();
    // The IPv4 addresses, because Android's resolver may hand back only a link-local
    // IPv6 address, which cannot be connected to without its interface scope.
    let mut ips: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_link_local() => Some(v4.to_string()),
            _ => None,
        })
        .collect();
    ips.dedup();
    ips.truncate(6);
    vec![
        ("ip".into(), ips.join(",")),
        ("v".into(), VERSION.to_string()),
        ("name".into(), name),
        ("id".into(), fingerprint_hex(fingerprint)[..16].to_string()),
        ("pair".into(), if pairing_required { "1" } else { "0" }.into()),
    ]
}

/// mDNS instance names may not contain dots and should be short.
pub fn instance_name(host_name: &str) -> String {
    let cleaned: String = host_name.chars().filter(|c| !c.is_control() && *c != '.').take(60).collect();
    if cleaned.trim().is_empty() {
        "DisplaySwarm".into()
    } else {
        cleaned
    }
}

/// A running advertisement; dropping it withdraws the service.
pub struct Advertisement {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Advertises `port` on every interface. Errors (no multicast, no
    /// network) are for the caller to log; the server works without discovery.
    pub fn start(host_name: &str, port: u16, fingerprint: &Fingerprint, pairing_required: bool) -> Result<Self, String> {
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?;
        let instance = instance_name(host_name);
        let dns_host = format!("{}.local.", instance.replace(' ', "-"));
        let props = txt_records(host_name, fingerprint, pairing_required);
        let info = mdns_sd::ServiceInfo::new(SERVICE_TYPE, &instance, &dns_host, "", port, &props[..])
            .map_err(|e| e.to_string())?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| e.to_string())?;
        log::info!("Advertising {SERVICE_TYPE} as {instance:?} on port {port}");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_carries_version_name_id_prefix_and_pairing_flag() {
        let fp = [0xabu8; 32];
        let txt = txt_records("desk", &fp, true);
        let get = |k: &str| txt.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("v"), Some("2"));
        assert_eq!(get("name"), Some("desk"));
        assert_eq!(get("id"), Some("abababababababab"));
        assert_eq!(get("pair"), Some("1"));
        assert_eq!(get_pair(&txt_records("x", &fp, false)), "0");
    }

    fn get_pair(txt: &[(String, String)]) -> &str {
        txt.iter().find(|(k, _)| k == "pair").map(|(_, v)| v.as_str()).unwrap()
    }

    #[test]
    fn every_txt_entry_fits_the_255_byte_limit() {
        let long = "n".repeat(1000);
        for (k, v) in txt_records(&long, &[1; 32], true) {
            assert!(k.len() + 1 + v.len() <= 255);
        }
    }

    #[test]
    fn instance_names_are_dot_free_and_never_empty() {
        assert_eq!(instance_name("my.pc"), "mypc");
        assert_eq!(instance_name("  "), "DisplaySwarm");
    }
}
