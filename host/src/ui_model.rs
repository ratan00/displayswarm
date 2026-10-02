//! Toolkit-independent pieces of the UI: the per-device history behind the
//! sparklines, and the text shown on a device card.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::ipc::proto::DeviceInfo;

/// Samples kept per series (about a minute).
pub const HISTORY_LEN: usize = 60;
/// Sparkline canvas, matching the `viewbox` in `appwindow.slint`.
pub const SPARK_W: f32 = 120.0;
pub const SPARK_H: f32 = 28.0;

/// SVG path data drawing `values` as a line scaled to the canvas; empty when
/// there is nothing to draw. The vertical range is `0..max` (with headroom), so
/// a flat series sits at a stable height instead of jumping around.
pub fn sparkline_path(values: &VecDeque<f32>) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let max = values.iter().copied().fold(0.0_f32, f32::max).max(1.0) * 1.1;
    let step = SPARK_W / (HISTORY_LEN - 1) as f32;
    // Newest sample at the right edge.
    let x0 = SPARK_W - step * (values.len() - 1) as f32;
    let mut out = String::new();
    for (i, v) in values.iter().enumerate() {
        let x = x0 + step * i as f32;
        let y = SPARK_H - 1.0 - (v / max) * (SPARK_H - 2.0);
        out.push_str(if i == 0 { "M " } else { " L " });
        out.push_str(&format!("{x:.1} {y:.1}"));
    }
    out
}

/// Recent latency and bitrate of one device.
#[derive(Default)]
pub struct History {
    pub latency_ms: VecDeque<f32>,
    pub mbps: VecDeque<f32>,
    last: Option<Instant>,
}

impl History {
    /// Adds a sample from `d` if it is streaming and the last one is at least
    /// ~900 ms old (events also arrive for other devices' changes). A device
    /// that stopped streaming starts a fresh history.
    pub fn record(&mut self, d: &DeviceInfo, now: Instant) {
        let Some(live) = &d.live else {
            self.latency_ms.clear();
            self.mbps.clear();
            self.last = None;
            return;
        };
        if self.last.map(|t| now.duration_since(t) < Duration::from_millis(900)).unwrap_or(false) {
            return;
        }
        self.last = Some(now);
        push(&mut self.latency_ms, live.latency_ms.unwrap_or(0) as f32);
        push(&mut self.mbps, live.kbps as f32 / 1000.0);
    }
}

fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() >= HISTORY_LEN {
        q.pop_front();
    }
    q.push_back(v);
}

/// "Streaming" text lines of a card.
pub struct LiveText {
    pub fps: String,
    pub latency: String,
    pub bitrate: String,
}

pub fn live_text(d: &DeviceInfo) -> LiveText {
    match &d.live {
        Some(l) => LiveText {
            fps: format!("{} fps \u{b7} {}", l.fps, l.encoder),
            latency: l.latency_ms.map(|ms| format!("{ms} ms")).unwrap_or_else(|| "\u{2013}".into()),
            bitrate: format!("{:.1} Mbps", l.kbps as f32 / 1000.0),
        },
        None => LiveText { fps: String::new(), latency: String::new(), bitrate: String::new() },
    }
}

pub fn transport_label(d: &DeviceInfo) -> &'static str {
    match d.transport.as_deref() {
        Some("usb") => "USB",
        Some(_) => "Network",
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::DeviceSettings;
    use crate::ipc::proto::LiveInfo;

    fn streaming(latency: Option<u32>, kbps: u32) -> DeviceInfo {
        DeviceInfo {
            device_id: "d".into(),
            name: "P".into(),
            role: None,
            effective_role: None,
            connected: true,
            state: String::new(),
            notice: None,
            target_output: None,
            panel_off: false,
            settings: DeviceSettings::default(),
            transport: Some("usb".into()),
            resolution: None,
            live: Some(LiveInfo { fps: 60, kbps, latency_ms: latency, encoder: "h264_vaapi".into() }),
        }
    }

    #[test]
    fn sparkline_needs_two_points_and_stays_in_the_canvas() {
        assert_eq!(sparkline_path(&VecDeque::from(vec![1.0])), "");
        let p = sparkline_path(&VecDeque::from(vec![0.0, 50.0, 100.0]));
        assert!(p.starts_with("M "));
        assert_eq!(p.matches(" L ").count(), 2);
        for pair in p.replace("M ", "").replace(" L ", ";").split(';') {
            let mut it = pair.split(' ').map(|v| v.parse::<f32>().unwrap());
            let (x, y) = (it.next().unwrap(), it.next().unwrap());
            assert!((0.0..=SPARK_W).contains(&x) && (0.0..=SPARK_H).contains(&y), "{pair}");
        }
    }

    #[test]
    fn history_throttles_and_resets() {
        let mut h = History::default();
        let t = Instant::now();
        h.record(&streaming(Some(30), 8000), t);
        h.record(&streaming(Some(31), 8000), t + Duration::from_millis(100));
        assert_eq!(h.latency_ms.len(), 1);
        h.record(&streaming(None, 9000), t + Duration::from_millis(1000));
        assert_eq!(h.latency_ms.len(), 2);
        assert_eq!(h.mbps.back().copied(), Some(9.0));
        let mut idle = streaming(None, 0);
        idle.live = None;
        h.record(&idle, t + Duration::from_secs(5));
        assert!(h.latency_ms.is_empty());
        for i in 0..(HISTORY_LEN + 20) {
            h.record(&streaming(Some(1), 1000), t + Duration::from_secs(10 + i as u64));
        }
        assert_eq!(h.latency_ms.len(), HISTORY_LEN);
    }

    #[test]
    fn card_text() {
        let t = live_text(&streaming(Some(42), 12_500));
        assert_eq!((t.latency.as_str(), t.bitrate.as_str()), ("42 ms", "12.5 Mbps"));
        assert!(t.fps.starts_with("60 fps"));
        assert_eq!(transport_label(&streaming(None, 0)), "USB");
    }
}
