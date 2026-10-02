//! The session services built on the virtual devices in [`super::sink`].

use lamco_pipewire::audio::{spawn_virtual_microphone, AudioFormat, AudioSamples, PlaybackConfig, VirtualMicrophoneHandle};

use super::codec::{FrameChunker, OpusReceiver, OpusSender};
use super::delay::{host_delay_us, median_us, HostDelay};
use super::sink::{node_name, DefaultSinkGuard, SinkMode, VirtualSink};
use super::sync::hub;
use crate::protocol::wire::{Message, FEATURE_AUDIO_OUT, FEATURE_MIC, SERVICE_AUDIO_OUT, SERVICE_MIC};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::services::{ServiceCtx, ServiceOut, SessionService};

/// The audio services this session should get. A failure (no PipeWire) is
/// logged and leaves the session without that service.
pub fn audio_services(ctx: &ServiceCtx) -> Vec<Box<dyn SessionService>> {
    if ctx.features & (FEATURE_AUDIO_OUT | FEATURE_MIC) == 0 {
        return Vec::new();
    }
    vec![Box::new(AudioManager::new(ctx))]
}

/// Latency reports (one per ~2 s) whose median sets the laptop's speaker delay.
const RECENT_REPORTS: usize = 9;
/// Reports to collect before delaying the speakers at all; the first ones are skewed by the start-up.
const START_REPORTS: usize = 5;

/// Owns the virtual sink and microphone of one session. The phone always
/// offers both; its `ServiceState` says which the user has switched on. With
/// the device's `audio_always_ready` setting (default) the virtual devices
/// exist for the whole session and only the audio flow follows the phone;
/// without it they are created when the phone switches the service on and
/// removed when it switches it off, so nothing shows in the sound settings
/// otherwise.
pub struct AudioManager {
    ctx: ServiceCtx,
    /// The features the session started with; the settings can only take some away.
    base_features: u32,
    /// Bits of `SERVICE_*` the phone has switched on; 0 until its first message.
    enabled: u8,
    /// Audio is sent only while this is true; shared with the sink's callback.
    flowing: Arc<AtomicBool>,
    out: Option<AudioOutService>,
    mic: Option<MicService>,
}

impl AudioManager {
    pub fn new(ctx: &ServiceCtx) -> Self {
        let mut m = Self { ctx: ctx.clone(), base_features: ctx.features, enabled: 0, flowing: Arc::new(AtomicBool::new(false)), out: None, mic: None };
        m.apply();
        m
    }

    /// Creates or removes the virtual devices to match the settings and the phone's state.
    fn apply(&mut self) {
        let keep = self.ctx.settings.audio_always_ready;
        self.ctx.features = self.base_features & allowed_features(&self.ctx.settings);
        let want_out = self.ctx.features & FEATURE_AUDIO_OUT != 0 && (keep || self.enabled & SERVICE_AUDIO_OUT != 0);
        let want_mic = self.ctx.features & FEATURE_MIC != 0 && (keep || self.enabled & SERVICE_MIC != 0);
        self.flowing.store(self.enabled & SERVICE_AUDIO_OUT != 0, Ordering::Relaxed);
        if want_out && self.out.is_none() {
            let c = &self.ctx;
            match AudioOutService::start_with(&c.device_id, &c.device_name, c.settings.audio_default_sink, c.settings.audio_mirror, c.settings.delay_host_audio, c.out.clone(), self.flowing.clone()) {
                Ok(s) => self.out = Some(s),
                Err(e) => log::warn!("audio out unavailable: {e}"),
            }
        } else if !want_out {
            self.out = None;
        }
        if let Some(o) = self.out.as_mut() {
            o.flow_changed();
        }
        if want_mic && self.mic.is_none() {
            match MicService::start(&self.ctx) {
                Ok(s) => self.mic = Some(s),
                Err(e) => log::warn!("microphone unavailable: {e}"),
            }
        } else if !want_mic {
            self.mic = None;
        }
    }
}

/// The audio features the settings allow.
fn allowed_features(s: &crate::devices::DeviceSettings) -> u32 {
    (if s.audio_out { FEATURE_AUDIO_OUT } else { 0 }) | (if s.mic { FEATURE_MIC } else { 0 })
}

impl SessionService for AudioManager {
    fn name(&self) -> &'static str {
        "audio"
    }
    fn on_settings(&mut self, s: &crate::devices::DeviceSettings) {
        let c = &self.ctx.settings;
        // These shape the virtual device itself, so it is made again.
        if (c.audio_mirror, c.audio_default_sink, c.delay_host_audio) != (s.audio_mirror, s.audio_default_sink, s.delay_host_audio) {
            self.out = None;
        }
        self.ctx.settings = s.clone();
        self.apply();
    }
    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::ServiceState { .. } | Message::AudioLatency { .. } | Message::MicFrame(_))
    }
    fn on_message(&mut self, msg: Message) {
        match msg {
            Message::ServiceState { enabled } => {
                log::info!("phone services: audio {}, mic {}", enabled & SERVICE_AUDIO_OUT != 0, enabled & SERVICE_MIC != 0);
                self.enabled = enabled;
                self.apply();
            }
            other => {
                if let Some(o) = self.out.as_mut().filter(|o| o.wants(&other)) {
                    o.on_message(other);
                } else if let Some(m) = self.mic.as_mut().filter(|m| m.wants(&other)) {
                    m.on_message(other);
                }
            }
        }
    }
}

/// Host output -> phone.
pub struct AudioOutService {
    // Field order is drop order: give the laptop's speakers and default output
    // back first, then remove the sink.
    host_delay: Option<HostDelay>,
    _default: Option<DefaultSinkGuard>,
    _sink: VirtualSink,
    /// Registered with the multi-phone sync hub for as long as it lives.
    device_id: String,
    /// The user wants the laptop's speakers delayed to match the phone (mirror mode only).
    delay_host: bool,
    flowing: Arc<AtomicBool>,
    /// Latency reports seen since the audio started flowing; the first is skewed by start-up.
    /// The last few latency reports, newest last.
    recent: Vec<u32>,
}

impl Drop for AudioOutService {
    fn drop(&mut self) {
        hub().unregister(&self.device_id);
    }
}

impl AudioOutService {
    pub fn start(ctx: &ServiceCtx) -> Result<Self, String> {
        Self::start_with(&ctx.device_id, &ctx.device_name, ctx.settings.audio_default_sink, ctx.settings.audio_mirror, ctx.settings.delay_host_audio, ctx.out.clone(), Arc::new(AtomicBool::new(true)))
    }

    pub fn start_with(device_id: &str, device_name: &str, make_default: bool, mirror: bool, delay_host: bool, out: ServiceOut, flowing: Arc<AtomicBool>) -> Result<Self, String> {
        let mut chunker = FrameChunker::new(2);
        let mut sender = OpusSender::new(2)?;
        let hub_out = out.clone();
        let flowing_cb = flowing.clone();
        // Mirroring taps the laptop's current output instead of adding one, so
        // it cannot also be made the default (that would feed itself).
        let mode = if mirror { SinkMode::MirrorDefault } else { SinkMode::Output };
        let make_default = make_default && !mirror;
        let sink = VirtualSink::create_mode(
            &node_name(if mirror { "displayswarm_tap" } else { "displayswarm_sink" }, device_id),
            &format!("DisplaySwarm \u{2013} {device_name}"),
            mode,
            Box::new(move |pcm, end_us| {
                // Nothing to send while the phone has audio switched off; keep the chunker empty.
                if !flowing_cb.load(Ordering::Relaxed) {
                    return;
                }
                for (pts, frame) in chunker.push(pcm, end_us) {
                    match sender.encode(&frame, pts) {
                        Ok(p) => {
                            // The session is gone: drop the audio, the service is about to be dropped.
                            let _ = out.send(Message::AudioFrame(p));
                        }
                        Err(e) => log::warn!("opus encode failed: {e}"),
                    }
                }
            }),
        )?;
        let default = if make_default { DefaultSinkGuard::set(sink.node_name()) } else { None };
        log::info!("audio out: {} {} created", if mirror { "laptop-audio tap" } else { "virtual sink" }, sink.node_name());
        hub().register(device_id, hub_out);
        Ok(Self { host_delay: None, _default: default, _sink: sink, device_id: device_id.to_string(), delay_host: delay_host && mirror, flowing, recent: Vec::new() })
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
            hub().report(&self.device_id, latency_us);
            self.update_host_delay(latency_us);
        }
    }
}

impl AudioOutService {
    /// The phone switched audio on or off: without audio the laptop's speakers go back to normal.
    fn flow_changed(&mut self) {
        if !self.flowing.load(Ordering::Relaxed) {
            self.host_delay = None;
            self.recent.clear();
        }
    }

    /// Keeps the laptop's speakers as late as the phone's sound, when the user asked for it.
    fn update_host_delay(&mut self, phone_latency_us: u32) {
        if !self.delay_host || !self.flowing.load(Ordering::Relaxed) {
            return;
        }
        self.recent.push(phone_latency_us);
        if self.recent.len() > RECENT_REPORTS {
            self.recent.remove(0);
        }
        let d = host_delay_us(median_us(&self.recent), hub().current_delay_us());
        match self.host_delay.as_mut() {
            Some(h) => h.set_delay(d),
            // Wait for a few reports: the first ones are inflated by the start-up.
            None if self.recent.len() >= START_REPORTS && d > 0 => match HostDelay::start(d) {
                Ok(h) => self.host_delay = Some(h),
                Err(e) => {
                    log::warn!("speaker delay unavailable: {e}");
                    self.delay_host = false;
                }
            },
            None => {}
        }
    }
}

/// Phone microphone -> host.
pub struct MicService {
    mic: VirtualMicrophoneHandle,
    rx: OpusReceiver,
}

impl MicService {
    pub fn start(ctx: &ServiceCtx) -> Result<Self, String> {
        let cfg = PlaybackConfig { sample_rate: 48_000, channels: 1, format: AudioFormat::F32, buffer_frames: 480 };
        let mic = spawn_virtual_microphone(cfg, Some(format!("DisplaySwarm Mic \u{2013} {}", ctx.device_name)), 64)
            .map_err(|e| format!("{e:#}"))?;
        log::info!("microphone: virtual source for {} created", ctx.device_name);
        Ok(Self { mic, rx: OpusReceiver::new() })
    }
}

impl Drop for MicService {
    fn drop(&mut self) {
        self.mic.stop();
    }
}

impl SessionService for MicService {
    fn name(&self) -> &'static str {
        "microphone"
    }
    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::MicFrame(_))
    }
    fn on_message(&mut self, msg: Message) {
        let Message::MicFrame(p) = msg else { return };
        let (channels, pcm) = self.rx.decode(&p);
        if pcm.is_empty() {
            return;
        }
        // The source is mono; fold a stereo phone stream down.
        let mono = if channels == 2 { pcm.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect() } else { pcm };
        // Full queue means the virtual source is not being read: drop, never block.
        let _ = self.mic.sender.try_send(AudioSamples::F32(mono));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::audio::codec::{OpusReceiver, OpusSender, FRAME_SAMPLES};
    use crate::devices::{DeviceSettings, Role};
    use std::process::Command;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn nodes() -> String {
        String::from_utf8_lossy(&Command::new("pw-cli").args(["ls", "Node"]).output().unwrap().stdout).to_string()
    }

    /// 16-bit PCM WAV of a 440 Hz tone.
    fn write_wav(path: &std::path::Path, channels: u16, secs: u32) {
        let n = 48_000 * secs;
        let mut d = Vec::new();
        for i in 0..n {
            let v = ((2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5 * 32767.0) as i16;
            for _ in 0..channels {
                d.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + d.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&48_000u32.to_le_bytes());
        w.extend_from_slice(&(48_000 * 2 * channels as u32).to_le_bytes());
        w.extend_from_slice(&(2 * channels).to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(d.len() as u32).to_le_bytes());
        w.extend_from_slice(&d);
        std::fs::write(path, w).unwrap();
    }

    /// Needs a running PipeWire: a tone played into the virtual sink must come
    /// out as decodable Opus frames, and the sink must be gone afterwards.
    #[test]
    #[ignore]
    fn live_sink_captures_a_tone_as_opus() {
        let wav = std::env::temp_dir().join("displayswarm-live-tone.wav");
        write_wav(&wav, 2, 2);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
        let (bulk_tx, _bulk_rx) = mpsc::channel(4);
        let out = ServiceOut::new(ctrl_tx, bulk_tx);
        let svc = AudioOutService::start_with("live-test", "Live Test", false, false, false, out, Arc::new(AtomicBool::new(true))).expect("virtual sink");
        assert!(nodes().contains("DisplaySwarm \u{2013} Live Test"), "sink not listed:\n{}", nodes());

        let status = Command::new("pw-play")
            .args(["--target", "displayswarm_sink_live_test"])
            .arg(&wav)
            .status()
            .expect("pw-play");
        assert!(status.success());
        std::thread::sleep(Duration::from_millis(200));

        let mut rx = OpusReceiver::new();
        let mut frames = 0;
        let mut energy = 0f64;
        let mut last_seq = None;
        let mut last_pts = 0;
        while let Ok(m) = ctrl_rx.try_recv() {
            let Message::AudioFrame(p) = m else { continue };
            if let Some(s) = last_seq {
                assert_eq!(p.seq, u32::wrapping_add(s, 1), "sequence gap");
            }
            assert!(p.pts_us >= last_pts, "pts went backwards");
            last_seq = Some(p.seq);
            last_pts = p.pts_us;
            let (ch, pcm) = rx.decode(&p);
            assert_eq!(ch, 2);
            energy += pcm.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
            frames += 1;
        }
        println!("live sink: {frames} Opus frames, energy {energy:.1}");
        assert!(frames >= 150, "expected ~200 frames for 2 s, got {frames}");
        assert!(energy > 100.0, "frames are silent");
        drop(svc);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!nodes().to_lowercase().contains("displayswarm"), "sink left behind:\n{}", nodes());
        let _ = std::fs::remove_file(wav);
    }

    fn default_sink() -> String {
        let out = Command::new("pw-metadata").args(["-n", "default", "0", "default.audio.sink"]).output().unwrap();
        crate::audio::sink::parse_metadata_value(&String::from_utf8_lossy(&out.stdout), "default.audio.sink")
            .unwrap_or_default()
    }

    /// Needs a running PipeWire: `audio_default_sink` makes the virtual sink
    /// the default output and gives the old one back on drop.
    #[test]
    #[ignore]
    fn live_default_sink_is_taken_and_restored() {
        let before = default_sink();
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel();
        let (bulk_tx, _bulk_rx) = mpsc::channel(4);
        let svc = AudioOutService::start_with("live-test", "Live Test", true, false, false, ServiceOut::new(ctrl_tx, bulk_tx), Arc::new(AtomicBool::new(true))).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let during = default_sink();
        println!("default sink: {before} -> {during}");
        assert!(during.contains("displayswarm_sink_live_test"), "default is {during}");
        drop(svc);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(default_sink(), before);
    }

    /// Needs a running PipeWire: MicFrames must appear as sound on the virtual
    /// source, which must disappear with the service.
    #[test]
    #[ignore]
    fn live_mic_plays_phone_audio_into_a_virtual_source() {
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel();
        let (bulk_tx, _bulk_rx) = mpsc::channel(4);
        let ctx = ServiceCtx {
            device_id: "live-test".into(),
            device_name: "Live Test".into(),
            features: FEATURE_MIC,
            settings: DeviceSettings { mic: true, ..DeviceSettings::default() },
            role: Role::Mirror,
            out: ServiceOut::new(ctrl_tx, bulk_tx),
            quality: crate::services::QualityHandle::detached(),
        };
        let mut svc = MicService::start(&ctx).expect("virtual source");
        std::thread::sleep(Duration::from_millis(500));
        assert!(nodes().contains("DisplaySwarm Mic \u{2013} Live Test"), "source not listed:\n{}", nodes());

        let rec = std::env::temp_dir().join("displayswarm-live-mic.wav");
        let mut child = Command::new("pw-record")
            .args(["--target", "lamco-rdp-microphone", "--rate", "48000", "--channels", "1"])
            .arg(&rec)
            .spawn()
            .expect("pw-record");
        std::thread::sleep(Duration::from_millis(300));
        // Feed 1.5 s of tone at real-time pace.
        let mut tx = OpusSender::new(1).unwrap();
        let start = Instant::now();
        for k in 0..150usize {
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| (2.0 * std::f32::consts::PI * 440.0 * (k * FRAME_SAMPLES + i) as f32 / 48_000.0).sin() * 0.5)
                .collect();
            svc.on_message(Message::MicFrame(tx.encode(&pcm, 0).unwrap()));
            let due = start + Duration::from_millis(10 * (k as u64 + 1));
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        let _ = child.wait();
        let data = std::fs::read(&rec).unwrap();
        let samples: Vec<i16> =
            data[44.min(data.len())..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        println!("live mic: recorded {} samples, peak {peak}", samples.len());
        assert!(peak > 5000, "the virtual source recorded silence (peak {peak})");
        drop(svc);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!nodes().to_lowercase().contains("displayswarm mic"), "source left behind:\n{}", nodes());
        let _ = std::fs::remove_file(rec);
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn settings_allow_audio_and_mic_independently() {
        let mut st = crate::devices::DeviceSettings::default();
        st.audio_out = true;
        st.mic = false;
        assert_eq!(allowed_features(&st), FEATURE_AUDIO_OUT);
        st.audio_out = false;
        st.mic = true;
        assert_eq!(allowed_features(&st), FEATURE_MIC);
        st.mic = false;
        assert_eq!(allowed_features(&st), 0);
    }
}
