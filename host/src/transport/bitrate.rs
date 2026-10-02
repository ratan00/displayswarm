//! Adaptive bitrate for network transports (Wi-Fi), AIMD style: back off
//! multiplicatively when the link shows congestion, creep back up additively
//! while it is clean. USB has bandwidth to spare and does not use this.
//!
//! Congestion is: the phone dropped frames since the last sample, the host's
//! sender dropped frames, or the phone's present latency grew well past its
//! recent floor for two samples in a row.

use std::time::Duration;

/// One second of observations.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Phone-reported present latency; `None` before the first `Stats`.
    pub latency_ms: Option<u32>,
    /// Phone's cumulative dropped-frame counter.
    pub phone_dropped: u32,
    /// Host sender's cumulative dropped-frame counter.
    pub host_dropped: u64,
    /// What the phone measured receiving, if known (kbps).
    pub rx_kbps: Option<u32>,
    /// What the host actually sent over the same second (kbps), if known. On a
    /// quiet screen this is far below the target, and the phone receiving just
    /// that much says nothing about the link's capacity.
    pub sent_kbps: Option<u32>,
}

pub const MIN_KBPS: u32 = 2_500;
/// Seconds to wait after a decrease before a further one.
const HOLD_AFTER_DECREASE: u32 = 3;
/// Clean seconds before an increase.
const CLEAN_BEFORE_INCREASE: u32 = 3;

#[derive(Debug)]
pub struct BitrateController {
    max_kbps: u32,
    current: u32,
    latency_floor: Option<u32>,
    last_phone_dropped: Option<u32>,
    last_host_dropped: Option<u64>,
    high_latency_streak: u32,
    hold: u32,
    clean: u32,
}

impl BitrateController {
    /// `max_kbps` is the configured bitrate, which is also the starting point
    /// and the ceiling.
    pub fn new(max_kbps: u32) -> Self {
        let max_kbps = max_kbps.max(MIN_KBPS);
        Self {
            max_kbps,
            current: max_kbps,
            latency_floor: None,
            last_phone_dropped: None,
            last_host_dropped: None,
            high_latency_streak: 0,
            hold: 0,
            clean: 0,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// Feeds one second of observations. `Some(kbps)` when the encoder should
    /// switch to a new bitrate.
    pub fn update(&mut self, s: Sample) -> Option<u32> {
        let phone_drops = match (self.last_phone_dropped, s.phone_dropped) {
            (Some(prev), now) => now.saturating_sub(prev) > 0,
            _ => false,
        };
        self.last_phone_dropped = Some(s.phone_dropped);
        let host_drops = self.last_host_dropped.is_some_and(|prev| s.host_dropped > prev);
        self.last_host_dropped = Some(s.host_dropped);

        let mut latency_bad = false;
        if let Some(ms) = s.latency_ms {
            // The floor follows the minimum, creeping up slowly so a permanent
            // change of path does not leave the controller pessimistic forever.
            let floor = match self.latency_floor {
                Some(f) if ms >= f => (f + 1).min(ms),
                _ => ms,
            };
            self.latency_floor = Some(floor);
            let limit = (floor * 2).max(floor + 60);
            if ms > limit {
                self.high_latency_streak += 1;
            } else {
                self.high_latency_streak = 0;
            }
            latency_bad = self.high_latency_streak >= 2;
        }

        self.hold = self.hold.saturating_sub(1);
        if phone_drops || host_drops || latency_bad {
            self.clean = 0;
            if self.hold > 0 {
                return None;
            }
            let mut target = self.current / 10 * 7;
            // If the phone measured clearly less than the host sent, we are
            // past capacity. A receive rate that merely matches a low send
            // rate (a still screen) is the content, not the link.
            if let Some(rx) = s.rx_kbps.filter(|r| *r > 0) {
                let short = s.sent_kbps.map_or(true, |sent| rx < sent / 10 * 9);
                if short {
                    target = target.min(rx / 100 * 85);
                }
            }
            let target = target.clamp(MIN_KBPS, self.max_kbps);
            self.hold = HOLD_AFTER_DECREASE;
            self.high_latency_streak = 0;
            if target < self.current {
                self.current = target;
                return Some(target);
            }
            return None;
        }

        self.clean += 1;
        if self.clean >= CLEAN_BEFORE_INCREASE && self.current < self.max_kbps {
            self.clean = 0;
            let step = (self.max_kbps / 10).max(500);
            self.current = (self.current + step).min(self.max_kbps);
            return Some(self.current);
        }
        None
    }
}

/// How often the session feeds the controller.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(latency: u32) -> Sample {
        Sample { latency_ms: Some(latency), ..Default::default() }
    }

    #[test]
    fn stable_link_keeps_the_configured_bitrate() {
        let mut c = BitrateController::new(20_000);
        for _ in 0..60 {
            assert_eq!(c.update(clean(30)), None);
        }
        assert_eq!(c.current(), 20_000);
    }

    #[test]
    fn phone_drops_cut_the_rate_multiplicatively() {
        let mut c = BitrateController::new(20_000);
        c.update(Sample { phone_dropped: 0, ..clean(30) });
        assert_eq!(c.update(Sample { phone_dropped: 5, ..clean(30) }), Some(14_000));
    }

    #[test]
    fn measured_receive_rate_bounds_the_cut() {
        let mut c = BitrateController::new(20_000);
        c.update(Sample::default());
        let got = c.update(Sample { phone_dropped: 1, rx_kbps: Some(8_000), ..clean(30) });
        assert_eq!(got, Some(6_800));
    }

    #[test]
    fn a_receive_rate_that_matches_a_quiet_send_rate_is_not_congestion() {
        let mut c = BitrateController::new(20_000);
        c.update(Sample::default());
        // The screen is nearly still: 3 Mbit/s sent, 2.9 received. A stray
        // dropped frame cuts multiplicatively, not down to the content's rate.
        let got = c.update(Sample { phone_dropped: 1, rx_kbps: Some(2_900), sent_kbps: Some(3_000), ..clean(30) });
        assert_eq!(got, Some(14_000));
    }

    #[test]
    fn a_real_shortfall_still_bounds_the_cut() {
        let mut c = BitrateController::new(20_000);
        c.update(Sample::default());
        let got = c.update(Sample { phone_dropped: 1, rx_kbps: Some(8_000), sent_kbps: Some(15_000), ..clean(30) });
        assert_eq!(got, Some(6_800));
    }

    #[test]
    fn latency_growth_needs_two_samples_and_then_backs_off() {
        let mut c = BitrateController::new(10_000);
        for _ in 0..3 {
            c.update(clean(30));
        }
        assert_eq!(c.update(clean(400)), None, "one spike is ignored");
        assert_eq!(c.update(clean(420)), Some(7_000));
    }

    #[test]
    fn decreases_are_rate_limited_then_recovery_is_additive_up_to_the_cap() {
        let mut c = BitrateController::new(10_000);
        c.update(Sample::default());
        assert_eq!(c.update(Sample { phone_dropped: 1, ..clean(30) }), Some(7_000));
        // Still dropping right after: held, no second cut yet.
        assert_eq!(c.update(Sample { phone_dropped: 2, ..clean(30) }), None);
        assert_eq!(c.current(), 7_000);
        let mut last = c.current();
        let mut ups = 0;
        for _ in 0..200 {
            if let Some(k) = c.update(Sample { phone_dropped: 2, ..clean(30) }) {
                assert!(k > last && k - last <= 1_000, "additive steps");
                last = k;
                ups += 1;
            }
        }
        assert!(ups >= 3);
        assert_eq!(c.current(), 10_000, "never above the configured bitrate");
    }

    #[test]
    fn never_drops_below_the_floor() {
        let mut c = BitrateController::new(2_000);
        let mut dropped = 0;
        c.update(Sample::default());
        for _ in 0..100 {
            dropped += 1;
            c.update(Sample { phone_dropped: dropped, ..clean(30) });
        }
        assert_eq!(c.current(), MIN_KBPS);
    }

    #[test]
    fn host_side_drops_count_as_congestion() {
        let mut c = BitrateController::new(10_000);
        c.update(Sample::default());
        assert_eq!(c.update(Sample { host_dropped: 3, ..Default::default() }), Some(7_000));
    }
}
