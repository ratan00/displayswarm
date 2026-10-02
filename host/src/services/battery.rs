//! Battery- and thermal-aware quality (Phase 8).
//!
//! The phone reports [`BatteryStatus`] every ~30 s and on change. The latest
//! one per device is readable through [`status`] (for the UI). With the
//! device's `battery_saver` setting on, the encoder bitrate is lowered while
//! the battery is low and not charging, or the phone runs hot, and restored
//! when it recovers. Only the bitrate is adjusted: the encoders cannot change
//! fps live.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::{QualityHandle, ServiceCtx, SessionService};
use crate::protocol::wire::{BatteryStatus, Message, THERMAL_CRITICAL, THERMAL_MODERATE, THERMAL_SEVERE};

/// How far quality is reduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Normal,
    /// Half the nominal bitrate.
    Reduced,
    /// A quarter of the nominal bitrate.
    Minimal,
}

/// The next quality level given the previous one (for hysteresis, so the
/// bitrate does not flap around a threshold) and the newest report.
/// A percentage above 100 means "unknown" and never counts as low.
pub fn decide(prev: Quality, s: &BatteryStatus) -> Quality {
    let discharging = !s.charging && s.percent <= 100;
    let very_low = discharging && (s.percent < 10 || (prev == Quality::Minimal && s.percent < 12));
    if s.thermal >= THERMAL_CRITICAL || very_low {
        return Quality::Minimal;
    }
    let low = discharging && (s.percent < 20 || (prev != Quality::Normal && s.percent < 25));
    let hot = s.thermal >= THERMAL_SEVERE || (prev != Quality::Normal && s.thermal >= THERMAL_MODERATE);
    if low || hot {
        Quality::Reduced
    } else {
        Quality::Normal
    }
}

/// The bitrate for a quality level, never above `nominal`.
pub fn bitrate_for(q: Quality, nominal: u32) -> u32 {
    match q {
        Quality::Normal => nominal,
        Quality::Reduced => (nominal / 2).max(1500).min(nominal),
        Quality::Minimal => (nominal / 4).max(1000).min(nominal),
    }
}

/// What the UI can show about a connected phone.
#[derive(Debug, Clone, Copy)]
pub struct LiveBattery {
    pub status: BatteryStatus,
    /// Whether the saver has lowered the quality, and how far.
    pub quality: Quality,
    pub updated: Instant,
}

fn registry() -> &'static Mutex<HashMap<String, LiveBattery>> {
    static R: OnceLock<Mutex<HashMap<String, LiveBattery>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The phone's latest battery/thermal report, while it is connected.
pub fn status(device_id: &str) -> Option<LiveBattery> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(device_id).copied()
}

pub struct BatteryService {
    device_id: String,
    saver: bool,
    quality: QualityHandle,
    current: Quality,
}

impl BatteryService {
    pub fn new(ctx: &ServiceCtx) -> Self {
        Self {
            device_id: ctx.device_id.clone(),
            saver: ctx.settings.battery_saver,
            quality: ctx.quality.clone(),
            current: Quality::Normal,
        }
    }
}

impl SessionService for BatteryService {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::BatteryStatus(_))
    }

    fn on_message(&mut self, msg: Message) {
        let Message::BatteryStatus(s) = msg else { return };
        if self.saver {
            let next = decide(self.current, &s);
            if next != self.current {
                let kbps = bitrate_for(next, self.quality.nominal_kbps);
                log::info!(
                    "Battery {}%{} thermal {}: {:?} -> {next:?} ({kbps} kbps)",
                    s.percent,
                    if s.charging { " charging" } else { "" },
                    s.thermal,
                    self.current
                );
                self.quality.request_bitrate(kbps);
                self.current = next;
            }
        }
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.device_id.clone(), LiveBattery { status: s, quality: self.current, updated: Instant::now() });
    }
}

impl Drop for BatteryService {
    fn drop(&mut self) {
        registry().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.device_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{THERMAL_LIGHT, THERMAL_NONE};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn st(percent: u8, charging: bool, thermal: u8) -> BatteryStatus {
        BatteryStatus { percent, charging, thermal, temp_decidegrees: 300 }
    }

    #[test]
    fn policy_thresholds() {
        use Quality::*;
        assert_eq!(decide(Normal, &st(80, false, THERMAL_NONE)), Normal);
        assert_eq!(decide(Normal, &st(19, false, THERMAL_NONE)), Reduced);
        assert_eq!(decide(Normal, &st(19, true, THERMAL_NONE)), Normal);
        assert_eq!(decide(Normal, &st(9, false, THERMAL_NONE)), Minimal);
        assert_eq!(decide(Normal, &st(80, false, THERMAL_SEVERE)), Reduced);
        assert_eq!(decide(Normal, &st(80, true, THERMAL_CRITICAL)), Minimal);
        assert_eq!(decide(Normal, &st(255, false, THERMAL_LIGHT)), Normal, "unknown percent");
    }

    #[test]
    fn policy_has_hysteresis() {
        use Quality::*;
        assert_eq!(decide(Reduced, &st(22, false, THERMAL_NONE)), Reduced);
        assert_eq!(decide(Reduced, &st(25, false, THERMAL_NONE)), Normal);
        assert_eq!(decide(Normal, &st(22, false, THERMAL_NONE)), Normal);
        assert_eq!(decide(Reduced, &st(80, false, THERMAL_MODERATE)), Reduced);
        assert_eq!(decide(Normal, &st(80, false, THERMAL_MODERATE)), Normal);
        assert_eq!(decide(Minimal, &st(11, false, THERMAL_NONE)), Minimal);
        assert_eq!(decide(Minimal, &st(12, false, THERMAL_NONE)), Reduced);
        assert_eq!(decide(Reduced, &st(50, true, THERMAL_NONE)), Normal, "plugging in recovers");
    }

    #[test]
    fn bitrates() {
        assert_eq!(bitrate_for(Quality::Normal, 20_000), 20_000);
        assert_eq!(bitrate_for(Quality::Reduced, 20_000), 10_000);
        assert_eq!(bitrate_for(Quality::Minimal, 20_000), 5_000);
        assert_eq!(bitrate_for(Quality::Minimal, 2_000), 1_000);
        assert_eq!(bitrate_for(Quality::Reduced, 1_000), 1_000, "never above nominal");
    }

    fn service(saver: bool, id: &str) -> (BatteryService, Arc<AtomicU32>) {
        let asked = Arc::new(AtomicU32::new(0));
        let a = asked.clone();
        let svc = BatteryService {
            device_id: id.into(),
            saver,
            quality: QualityHandle::new(20_000, move |k| a.store(k, Ordering::SeqCst)),
            current: Quality::Normal,
        };
        (svc, asked)
    }

    #[test]
    fn service_requests_bitrate_and_publishes_status() {
        let (mut svc, asked) = service(true, "bat-test-1");
        svc.on_message(Message::BatteryStatus(st(15, false, THERMAL_NONE)));
        assert_eq!(asked.load(Ordering::SeqCst), 10_000);
        let live = status("bat-test-1").unwrap();
        assert_eq!(live.status.percent, 15);
        assert_eq!(live.quality, Quality::Reduced);
        svc.on_message(Message::BatteryStatus(st(60, true, THERMAL_NONE)));
        assert_eq!(asked.load(Ordering::SeqCst), 20_000);
        drop(svc);
        assert!(status("bat-test-1").is_none());
    }

    #[test]
    fn saver_off_only_reports() {
        let (mut svc, asked) = service(false, "bat-test-2");
        svc.on_message(Message::BatteryStatus(st(5, false, THERMAL_CRITICAL)));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        assert_eq!(status("bat-test-2").unwrap().quality, Quality::Normal);
    }
}
