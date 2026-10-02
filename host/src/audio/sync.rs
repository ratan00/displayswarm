//! Keeping several phones' audio in step.
//!
//! Each phone reports how long its audio takes from the host's timestamp to
//! its speaker (`AudioLatency`: transit + decode + buffers + output). The
//! slowest ("farthest") phone sets the pace: every phone is told the same
//! playout delay `D = max(latency) + margin` (`AudioSync`) and makes the sample
//! with host pts `T` audible at host time `T + D`, so the fast ones wait for
//! the slow one. Only active while enabled and with at least two phones.
//!
//! The laptop's own speakers cannot be delayed and play ahead of the phones.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::protocol::wire::Message;
use crate::services::ServiceOut;

/// Headroom over the slowest phone's latency, so it is not right at its limit.
pub const MARGIN_US: u32 = 20_000;
/// A phone is re-told the delay only when it moved by at least this much.
pub const MIN_CHANGE_US: u32 = 5_000;
/// Ceiling for the common delay.
pub const MAX_DELAY_US: u32 = 1_000_000;
/// A re-measure waits at most this long for every phone's fresh report (they report every 2 s).
pub const RESYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// The common delay for the given (reported) latencies; 0 = sync off.
pub fn target_delay(enabled: bool, phones: usize, latencies_us: &[u32]) -> u32 {
    if !enabled || phones < 2 {
        return 0;
    }
    match latencies_us.iter().max() {
        Some(max) => max.saturating_add(MARGIN_US).min(MAX_DELAY_US),
        None => 0,
    }
}

/// Smooths one phone's reports: a higher latency counts at once, a lower one
/// is followed slowly, so the shared delay does not chatter.
pub fn smooth(prev: Option<u32>, new: u32) -> u32 {
    match prev {
        Some(p) if new < p => p - (p - new) / 8,
        _ => new,
    }
}

/// Whether a phone that was last told `sent` should be told `target`.
pub fn should_send(sent: Option<u32>, target: u32) -> bool {
    match sent {
        None => true,
        Some(s) => (s == 0) != (target == 0) || s.abs_diff(target) >= MIN_CHANGE_US,
    }
}

struct Entry {
    latency_us: Option<u32>,
    out: ServiceOut,
    sent: Option<u32>,
    /// Reported again since the current re-measure began.
    fresh: bool,
}

#[derive(Default)]
struct Inner {
    enabled: bool,
    phones: HashMap<String, Entry>,
    /// A re-measure is waiting for fresh reports; the delay is held until it ends.
    resync_until: Option<Instant>,
}

#[derive(Default)]
pub struct AudioSyncHub {
    inner: Mutex<Inner>,
}

impl AudioSyncHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut g = self.lock();
        if g.enabled != enabled {
            g.enabled = enabled;
            g.resync_until = None;
            Self::recompute(&mut g, false);
        }
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// A phone starts playing host audio. It is told the current delay at once.
    pub fn register(&self, device_id: &str, out: ServiceOut) {
        let mut g = self.lock();
        g.phones.insert(device_id.to_string(), Entry { latency_us: None, out, sent: None, fresh: false });
        Self::recompute(&mut g, false);
    }

    pub fn unregister(&self, device_id: &str) {
        let mut g = self.lock();
        if g.phones.remove(device_id).is_some() {
            Self::finish_resync_if_ready(&mut g);
            Self::recompute(&mut g, false);
        }
    }

    /// "Sync now": forgets the smoothed latencies, waits for every phone's next report (up to
    /// [`RESYNC_TIMEOUT`]), then tells every phone the fresh common delay whether or not it moved.
    /// The delay in force is kept meanwhile, so the audio does not jump.
    pub fn resync(&self) -> Result<(), String> {
        let mut g = self.lock();
        if !g.enabled {
            return Err("Turn on \"Keep all phones' audio in sync\" and press Apply first".into());
        }
        if g.phones.len() < 2 {
            return Err("Sync needs at least two phones playing host audio".into());
        }
        for e in g.phones.values_mut() {
            e.fresh = false;
        }
        g.resync_until = Some(Instant::now() + RESYNC_TIMEOUT);
        Ok(())
    }

    fn finish_resync_if_ready(g: &mut Inner) {
        let Some(until) = g.resync_until else { return };
        if g.phones.values().all(|e| e.fresh) || Instant::now() >= until {
            g.resync_until = None;
            Self::recompute(g, true);
        }
    }

    /// A phone's `AudioLatency`.
    pub fn report(&self, device_id: &str, latency_us: u32) {
        let mut g = self.lock();
        let resyncing = g.resync_until.is_some();
        let Some(e) = g.phones.get_mut(device_id) else { return };
        if resyncing {
            // A re-measure takes the new report as it is, without the slow fall-off.
            e.latency_us = Some(latency_us);
            e.fresh = true;
            Self::finish_resync_if_ready(&mut g);
            return;
        }
        e.latency_us = Some(smooth(e.latency_us, latency_us));
        Self::recompute(&mut g, false);
    }

    /// The delay the phones are currently told (0 when off).
    pub fn current_delay_us(&self) -> u32 {
        let g = self.lock();
        Self::target(&g)
    }

    fn target(g: &Inner) -> u32 {
        let lat: Vec<u32> = g.phones.values().filter_map(|e| e.latency_us).collect();
        target_delay(g.enabled, g.phones.len(), &lat)
    }

    fn recompute(g: &mut Inner, force: bool) {
        if g.resync_until.is_some() && !force {
            return;
        }
        let target = Self::target(g);
        for e in g.phones.values_mut() {
            if (force || should_send(e.sent, target)) && e.out.send(Message::AudioSync { delay_us: target }) {
                e.sent = Some(target);
            }
        }
    }
}

/// The process-wide hub the audio services and the settings talk to.
pub fn hub() -> &'static AudioSyncHub {
    static HUB: OnceLock<AudioSyncHub> = OnceLock::new();
    HUB.get_or_init(AudioSyncHub::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn phone() -> (ServiceOut, mpsc::UnboundedReceiver<Message>) {
        let (ctrl, rx) = mpsc::unbounded_channel();
        let (bulk, _brx) = mpsc::channel(1);
        (ServiceOut::new(ctrl, bulk), rx)
    }

    fn delays(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<u32> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Message::AudioSync { delay_us } = m {
                v.push(delay_us);
            }
        }
        v
    }

    #[test]
    fn delay_is_the_slowest_phone_plus_margin() {
        assert_eq!(target_delay(true, 3, &[40_000, 90_000, 60_000]), 90_000 + MARGIN_US);
    }

    #[test]
    fn no_delay_when_off_or_alone_or_unreported() {
        assert_eq!(target_delay(false, 3, &[40_000, 90_000]), 0);
        assert_eq!(target_delay(true, 1, &[90_000]), 0);
        assert_eq!(target_delay(true, 2, &[]), 0);
    }

    #[test]
    fn delay_is_capped() {
        assert_eq!(target_delay(true, 2, &[u32::MAX, 1]), MAX_DELAY_US);
    }

    #[test]
    fn smoothing_rises_at_once_and_falls_slowly() {
        assert_eq!(smooth(None, 50_000), 50_000);
        assert_eq!(smooth(Some(50_000), 80_000), 80_000);
        assert_eq!(smooth(Some(80_000), 40_000), 75_000);
    }

    #[test]
    fn small_moves_are_not_resent() {
        assert!(should_send(None, 0));
        assert!(!should_send(Some(100_000), 103_000));
        assert!(should_send(Some(100_000), 106_000));
        assert!(should_send(Some(0), 1_000), "off -> on is always sent");
        assert!(should_send(Some(60_000), 0), "on -> off is always sent");
    }

    #[test]
    fn hub_syncs_two_phones_to_the_slower_one() {
        let hub = AudioSyncHub::new();
        let (a, mut arx) = phone();
        let (b, mut brx) = phone();
        hub.register("a", a);
        hub.register("b", b);
        hub.report("a", 40_000);
        hub.report("b", 90_000);
        assert_eq!(hub.current_delay_us(), 0, "off by default");
        assert_eq!(delays(&mut arx).last().copied(), Some(0));
        hub.set_enabled(true);
        assert_eq!(hub.current_delay_us(), 110_000);
        assert_eq!(delays(&mut arx).last().copied(), Some(110_000));
        assert_eq!(delays(&mut brx).last().copied(), Some(110_000));
        // The slow phone leaves: the remaining one is alone, so sync stops.
        hub.unregister("b");
        assert_eq!(delays(&mut arx).last().copied(), Some(0));
    }

    #[test]
    fn a_late_joiner_is_told_the_current_delay() {
        let hub = AudioSyncHub::new();
        hub.set_enabled(true);
        let (a, mut arx) = phone();
        let (b, mut brx) = phone();
        hub.register("a", a);
        hub.report("a", 70_000);
        hub.register("b", b);
        assert_eq!(delays(&mut brx).last().copied(), Some(90_000));
        assert_eq!(delays(&mut arx).last().copied(), Some(90_000));
    }

    #[test]
    fn sync_now_takes_fresh_reports_and_resends_to_everyone() {
        let hub = AudioSyncHub::new();
        let (a, mut arx) = phone();
        let (b, mut brx) = phone();
        assert!(hub.resync().is_err(), "off");
        hub.set_enabled(true);
        hub.register("a", a);
        assert!(hub.resync().is_err(), "alone");
        hub.register("b", b);
        hub.report("a", 40_000);
        hub.report("b", 90_000);
        assert_eq!(hub.current_delay_us(), 110_000);
        delays(&mut arx);
        delays(&mut brx);
        hub.resync().unwrap();
        // Held while waiting: a report from one phone changes nothing yet.
        hub.report("b", 50_000);
        assert!(delays(&mut arx).is_empty() && delays(&mut brx).is_empty());
        // The second report completes it: the fall-off is skipped and both are re-told.
        hub.report("a", 45_000);
        assert_eq!(hub.current_delay_us(), 50_000 + MARGIN_US);
        assert_eq!(delays(&mut arx), vec![70_000]);
        assert_eq!(delays(&mut brx), vec![70_000]);
    }
}
