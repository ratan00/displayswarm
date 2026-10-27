//! The Windows audio session service: the default output, captured by
//! [`super::loopback`], Opus-encoded and sent to the phone.
//!
//! What the Linux backend does beyond that is not possible here without a
//! driver, so it is left out cleanly: there is no virtual microphone
//! (`FEATURE_MIC` is not offered, see `services::HOST_FEATURES`) and the
//! laptop's speakers are not delayed to match the phone (a request for it is
//! logged and ignored). The phone's latency reports still feed the multi-phone
//! sync hub, which tells the phones how long to buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::codec::{FrameChunker, OpusSender, SilenceGate};
use super::loopback::Loopback;
use super::sync::hub;
use crate::protocol::wire::{Message, FEATURE_AUDIO_OUT, SERVICE_AUDIO_OUT};
use crate::services::{ServiceCtx, ServiceOut, SessionService};

/// The audio services this session should get.
pub fn audio_services(ctx: &ServiceCtx) -> Vec<Box<dyn SessionService>> {
    if ctx.features & FEATURE_AUDIO_OUT == 0 {
        return Vec::new();
    }
    vec![Box::new(AudioManager::new(ctx))]
}

/// Owns the capture of one session. It runs only while the phone has audio
/// switched on; there is no virtual device that would have to stay around.
pub struct AudioManager {
    ctx: ServiceCtx,
    /// Bits of `SERVICE_*` the phone has switched on; 0 until its first message.
    enabled: u8,
    out: Option<AudioOutService>,
}

impl AudioManager {
    pub fn new(ctx: &ServiceCtx) -> Self {
        Self { ctx: ctx.clone(), enabled: 0, out: None }
    }

    fn apply(&mut self) {
        let want = self.ctx.settings.audio_out && self.enabled & SERVICE_AUDIO_OUT != 0;
        if want && self.out.is_none() {
            match AudioOutService::start(&self.ctx) {
                Ok(s) => self.out = Some(s),
                Err(e) => log::warn!("audio out unavailable: {e}"),
            }
        } else if !want {
            self.out = None;
        }
    }
}

impl SessionService for AudioManager {
    fn name(&self) -> &'static str {
        "audio"
    }
    fn on_settings(&mut self, s: &crate::devices::DeviceSettings) {
        self.ctx.settings = s.clone();
        self.apply();
    }
    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::ServiceState { .. } | Message::AudioLatency { .. })
    }
    fn on_message(&mut self, msg: Message) {
        match msg {
            Message::ServiceState { enabled } => {
                log::info!("phone services: audio {}", enabled & SERVICE_AUDIO_OUT != 0);
                self.enabled = enabled;
                self.apply();
            }
            Message::AudioLatency { latency_us } => {
                if let Some(o) = self.out.as_mut() {
                    o.on_latency(latency_us);
                }
            }
            _ => {}
        }
    }
}

/// Host output -> phone.
pub struct AudioOutService {
    // Stops capturing first (field order is drop order), then the hub forgets the phone.
    _loopback: Loopback,
    device_id: String,
}

impl Drop for AudioOutService {
    fn drop(&mut self) {
        hub().unregister(&self.device_id);
    }
}

impl AudioOutService {
    pub fn start(ctx: &ServiceCtx) -> Result<Self, String> {
        if ctx.settings.delay_host_audio {
            log::warn!("audio out: delaying the laptop's speakers is not available on Windows");
        }
        Self::start_with(&ctx.device_id, ctx.out.clone(), Arc::new(AtomicBool::new(true)))
    }

    pub fn start_with(device_id: &str, out: ServiceOut, flowing: Arc<AtomicBool>) -> Result<Self, String> {
        let mut chunker = FrameChunker::new(2);
        let mut sender = OpusSender::new(2)?;
        let mut gate = SilenceGate::default();
        let hub_out = out.clone();
        let loopback = Loopback::start(Box::new(move |pcm, end_us| {
            if !flowing.load(Ordering::Relaxed) {
                return;
            }
            for (pts, frame) in chunker.push(pcm, end_us) {
                let (send, resumed) = gate.admit(&frame);
                if !send {
                    continue;
                }
                if resumed {
                    sender.reset();
                }
                match sender.encode(&frame, pts) {
                    // The session is gone: drop the audio, the service is about to be dropped.
                    Ok(p) => {
                        let _ = out.send(Message::AudioFrame(p));
                    }
                    Err(e) => log::warn!("opus encode failed: {e}"),
                }
            }
        }))?;
        log::info!("audio out: capturing the default output");
        hub().register(device_id, hub_out);
        Ok(Self { _loopback: loopback, device_id: device_id.to_string() })
    }

    fn on_latency(&mut self, latency_us: u32) {
        hub().report(&self.device_id, latency_us);
    }
}

impl SessionService for AudioOutService {
    fn name(&self) -> &'static str {
        "audio-out"
    }
    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::AudioLatency { .. })
    }
    fn on_message(&mut self, msg: Message) {
        if let Message::AudioLatency { latency_us } = msg {
            self.on_latency(latency_us);
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::audio::codec::OpusReceiver;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    /// Needs an output device and something playing during the 2 s it listens
    /// (e.g. `[console]::beep(440,1500)`): the loopback must arrive as
    /// decodable, in-order Opus frames. Prints frame count and peak level.
    #[test]
    #[ignore]
    fn live_loopback_captures_what_plays() {
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
        let (bulk_tx, _bulk_rx) = mpsc::channel(4);
        let svc = AudioOutService::start_with("live-test", ServiceOut::new(ctrl_tx, bulk_tx), Arc::new(AtomicBool::new(true))).expect("loopback");
        let end = Instant::now() + Duration::from_secs(2);
        let mut rx = OpusReceiver::new();
        let (mut frames, mut peak, mut last_seq) = (0u32, 0f32, None);
        while Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
            while let Ok(m) = ctrl_rx.try_recv() {
                let Message::AudioFrame(p) = m else { continue };
                if let Some(s) = last_seq {
                    assert_eq!(p.seq, u32::wrapping_add(s, 1), "sequence gap");
                }
                last_seq = Some(p.seq);
                let (ch, pcm) = rx.decode(&p);
                assert_eq!(ch, 2);
                peak = pcm.iter().fold(peak, |m, v| m.max(v.abs()));
                frames += 1;
            }
        }
        println!("live loopback: {frames} Opus frames in 2 s, peak {peak:.4}");
        assert!(frames >= 20, "expected frames, got {frames}");
        drop(svc);
    }
}
