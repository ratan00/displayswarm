//! Audio on Windows, through WASAPI.
//!
//! * **Audio out**: a loopback capture of the default playback device (what
//!   the PC is playing), Opus-encoded with the same framing, silence gate and
//!   multi-phone sync hub as on Linux. Windows has no API to add a playback
//!   device without a driver, so this is the Linux "mirror the laptop's audio"
//!   mode: the PC's own speakers keep playing too.
//! * **Microphone**: the phone's mic is played into a virtual cable's input,
//!   whose output other applications record from. Without a driver Windows
//!   cannot add a microphone, so this needs one installed: VB-Audio Virtual
//!   Cable ("CABLE Input") is found automatically, any other device by setting
//!   `DISPLAYSWARM_MIC_DEVICE` to part of its name. Without one the mic is
//!   unavailable and the log says what to install.
//!
//! Both open the stream at 48 kHz float with `AUTOCONVERTPCM`, so the Windows
//! audio engine converts from whatever the device's mix format is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::codec::{FrameChunker, OpusReceiver, OpusSender, SilenceGate, SAMPLE_RATE};
use super::sync::hub;
use crate::protocol::host_monotonic_us;
use crate::protocol::wire::{Message, FEATURE_AUDIO_OUT, FEATURE_MIC, SERVICE_AUDIO_OUT, SERVICE_MIC};
use crate::services::{ServiceCtx, ServiceOut, SessionService};

pub use super::cable::{pick_mic_device, to_mono, KNOWN_CABLES, MIC_DEVICE_ENV};
use super::cable::{native_to_stereo, NativeFormat, Resampler};

/// The audio services this session gets.
pub fn audio_services(ctx: &ServiceCtx) -> Vec<Box<dyn SessionService>> {
    if ctx.features & (FEATURE_AUDIO_OUT | FEATURE_MIC) == 0 {
        return Vec::new();
    }
    vec![Box::new(AudioManager::new(ctx))]
}

/// Starts and stops the two directions as the phone and the settings ask.
pub struct AudioManager {
    ctx: ServiceCtx,
    base_features: u32,
    enabled: u8,
    flowing: Arc<AtomicBool>,
    out: Option<LoopbackOut>,
    mic: Option<CableMic>,
    /// The mic failed to start for this reason; not retried every message.
    mic_failed: Option<String>,
}

impl AudioManager {
    pub fn new(ctx: &ServiceCtx) -> Self {
        let mut m = Self {
            ctx: ctx.clone(),
            base_features: ctx.features,
            enabled: 0,
            flowing: Arc::new(AtomicBool::new(false)),
            out: None,
            mic: None,
            mic_failed: None,
        };
        m.apply();
        m
    }

    fn apply(&mut self) {
        let s = &self.ctx.settings;
        let allowed = (if s.audio_out { FEATURE_AUDIO_OUT } else { 0 }) | (if s.mic { FEATURE_MIC } else { 0 });
        let features = self.base_features & allowed;
        let keep = s.audio_always_ready;
        let want_out = features & FEATURE_AUDIO_OUT != 0 && (keep || self.enabled & SERVICE_AUDIO_OUT != 0);
        let want_mic = features & FEATURE_MIC != 0 && self.enabled & SERVICE_MIC != 0;
        self.flowing.store(self.enabled & SERVICE_AUDIO_OUT != 0, Ordering::Relaxed);
        if want_out && self.out.is_none() {
            self.out = Some(LoopbackOut::start(&self.ctx.device_id, self.ctx.out.clone(), self.flowing.clone()));
        } else if !want_out {
            self.out = None;
        }
        if want_mic && self.mic.is_none() && self.mic_failed.is_none() {
            match CableMic::start() {
                Ok(m) => self.mic = Some(m),
                Err(e) => {
                    log::warn!("microphone unavailable: {e}");
                    self.mic_failed = Some(e);
                }
            }
        } else if !want_mic {
            self.mic = None;
            self.mic_failed = None;
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
        matches!(msg, Message::ServiceState { .. } | Message::AudioLatency { .. } | Message::MicFrame(_))
    }
    fn on_message(&mut self, msg: Message) {
        match msg {
            Message::ServiceState { enabled } => {
                log::info!("phone services: audio {}, mic {}", enabled & SERVICE_AUDIO_OUT != 0, enabled & SERVICE_MIC != 0);
                self.enabled = enabled;
                self.apply();
            }
            Message::AudioLatency { latency_us } => {
                if self.out.is_some() {
                    hub().report(&self.ctx.device_id, latency_us);
                }
            }
            Message::MicFrame(p) => {
                if let Some(m) = self.mic.as_mut() {
                    m.push(&p);
                }
            }
            _ => {}
        }
    }
}

/// The PC's playback, captured by loopback and sent to the phone.
pub struct LoopbackOut {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    device_id: String,
}

impl LoopbackOut {
    pub fn start(device_id: &str, out: ServiceOut, flowing: Arc<AtomicBool>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        hub().register(device_id, out.clone());
        let thread = std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || {
                // A device change or a driver reset ends a capture; start over
                // on whatever is the default output then.
                while !s2.load(Ordering::Relaxed) {
                    if let Err(e) = wasapi::loopback(&s2, &mut |pcm, end_us| encode(pcm, end_us, &out, &flowing)) {
                        log::warn!("audio out: loopback capture stopped ({e}); retrying");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            })
            .ok();
        log::info!("audio out: capturing the PC's playback for the phone");
        Self { stop, thread, device_id: device_id.to_string() }
    }
}

thread_local! {
    static ENCODER: std::cell::RefCell<Option<(FrameChunker, OpusSender, SilenceGate)>> = const { std::cell::RefCell::new(None) };
}

/// Chunks, gates and encodes interleaved stereo `pcm` ending at `end_us`.
fn encode(pcm: &[f32], end_us: u64, out: &ServiceOut, flowing: &AtomicBool) {
    ENCODER.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            match OpusSender::new(2) {
                Ok(s) => *cell = Some((FrameChunker::new(2), s, SilenceGate::default())),
                Err(e) => {
                    log::warn!("opus encoder unavailable: {e}");
                    return;
                }
            }
        }
        let (chunker, sender, gate) = cell.as_mut().expect("just set");
        if !flowing.load(Ordering::Relaxed) {
            chunker.clear();
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
                Ok(p) => {
                    let _ = out.send(Message::AudioFrame(p));
                }
                Err(e) => log::warn!("opus encode failed: {e}"),
            }
        }
    });
}

impl Drop for LoopbackOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        hub().unregister(&self.device_id);
    }
}

/// Mic samples queued for the cable, at most this much (older ones are dropped).
const MIC_QUEUE: usize = SAMPLE_RATE as usize / 5; // 200 ms

/// The phone's microphone, played into a virtual cable.
pub struct CableMic {
    rx: OpusReceiver,
    tx: crossbeam_channel::Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CableMic {
    pub fn start() -> Result<Self, String> {
        let wanted = std::env::var(MIC_DEVICE_ENV).ok();
        let names = wasapi::playback_devices()?;
        let index = pick_mic_device(&names.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(), wanted.as_deref())
            .ok_or_else(|| match &wanted {
                Some(w) => format!("no playback device matches {MIC_DEVICE_ENV}={w:?}"),
                None => "Windows cannot add a microphone without a driver. Install VB-Audio Virtual Cable \
                         (vb-audio.com/Cable) and record from \"CABLE Output\", or set DISPLAYSWARM_MIC_DEVICE \
                         to another virtual cable's playback device."
                    .into(),
            })?;
        let (device, name) = names[index].clone();
        let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-mic".into())
            .spawn(move || {
                if let Err(e) = wasapi::render_mono(&device, &rx, &s2) {
                    log::warn!("microphone: playing into the cable stopped: {e}");
                }
            })
            .map_err(|e| e.to_string())?;
        log::info!("microphone: playing the phone's mic into {name}");
        Ok(Self { rx: OpusReceiver::new(), tx, stop, thread: Some(thread) })
    }

    fn push(&mut self, p: &crate::protocol::wire::AudioPacket) {
        let (channels, pcm) = self.rx.decode(p);
        if !pcm.is_empty() {
            let _ = self.tx.try_send(to_mono(channels, pcm));
        }
    }
}

impl Drop for CableMic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The WASAPI calls.
mod wasapi {
    use super::*;
    use ::windows::core::{GUID, PWSTR};
    use ::windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use ::windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
    };
    use ::windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use ::windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
    use ::windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use ::windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use ::windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;

    /// 100-ns units of the shared-mode buffer.
    const BUFFER_HNS: i64 = 2_000_000; // 200 ms

    fn err(e: ::windows::core::Error) -> String {
        e.to_string()
    }

    fn com() {
        // Already initialised on this thread is fine.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    fn enumerator() -> Result<IMMDeviceEnumerator, String> {
        com();
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(err)
    }

    fn take_wide(p: PWSTR) -> String {
        let s = unsafe { p.to_string() }.unwrap_or_default();
        unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
        s
    }

    fn device_id(d: &IMMDevice) -> String {
        unsafe { d.GetId() }.map(take_wide).unwrap_or_default()
    }

    fn friendly_name(d: &IMMDevice) -> String {
        unsafe {
            let Ok(store) = d.OpenPropertyStore(STGM_READ) else { return String::new() };
            let Ok(value) = store.GetValue(&PKEY_Device_FriendlyName) else { return String::new() };
            PropVariantToStringAlloc(&value).map(take_wide).unwrap_or_default()
        }
    }

    /// Every active playback device: (endpoint id, friendly name).
    pub fn playback_devices() -> Result<Vec<(String, String)>, String> {
        let en = enumerator()?;
        let list = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.map_err(err)?;
        let n = unsafe { list.GetCount() }.map_err(err)?;
        Ok((0..n)
            .filter_map(|i| unsafe { list.Item(i) }.ok())
            .map(|d| (device_id(&d), friendly_name(&d)))
            .collect())
    }

    /// 48 kHz float with `channels` channels.
    fn format(channels: u16) -> WAVEFORMATEXTENSIBLE {
        let block = channels * 4;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * block as u32,
                nBlockAlign: block,
                wBitsPerSample: 32,
                cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
            // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT, or SPEAKER_FRONT_CENTER.
            dwChannelMask: if channels == 2 { 0x3 } else { 0x4 },
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        }
    }

    /// The device's own mix format, for when the engine will not convert.
    fn open_native(device: &IMMDevice, flags: u32) -> Result<(IAudioClient, NativeFormat), String> {
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(err)?;
        let mix = unsafe { client.GetMixFormat() }.map_err(err)?;
        let native = unsafe { native_format(mix) };
        let r = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, mix, None::<*const GUID>) };
        unsafe { CoTaskMemFree(Some(mix as *const _)) };
        r.map_err(err)?;
        let native = native.ok_or("the device's mix format is not one DisplaySwarm reads")?;
        Ok((client, native))
    }

    /// Reads a `WAVEFORMATEX` (or its EXTENSIBLE form).
    unsafe fn native_format(f: *const WAVEFORMATEX) -> Option<NativeFormat> {
        const PCM: u16 = 1;
        const IEEE_FLOAT: u16 = 3;
        // The structures are packed: read them whole, never by field reference.
        let base = std::ptr::read_unaligned(f);
        let (tag, rate, channels, bits) = (base.wFormatTag, base.nSamplesPerSec, base.nChannels, base.wBitsPerSample);
        let float = match tag {
            IEEE_FLOAT => true,
            PCM => false,
            t if t as u32 == WAVE_FORMAT_EXTENSIBLE => {
                let ext = std::ptr::read_unaligned(f as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat;
                if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    true
                } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                    false
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(NativeFormat { rate, channels, bits, float })
    }

    fn open(device: &IMMDevice, flags: u32, channels: u16) -> Result<IAudioClient, String> {
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(err)?;
        let fmt = format(channels);
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &fmt as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
                None::<*const GUID>,
            )
        }
        .map_err(err)?;
        Ok(client)
    }

    /// Captures what the default playback device plays, calling `sink` with
    /// interleaved stereo and the capture time of its last sample, until `stop`
    /// or until the default device changes (then `Ok`, to be called again).
    pub fn loopback(stop: &AtomicBool, sink: &mut dyn FnMut(&[f32], u64)) -> Result<(), String> {
        let en = enumerator()?;
        let device = unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(err)?;
        let id = device_id(&device);
        log::info!("audio out: loopback of {}", friendly_name(&device));
        // 48 kHz stereo float converted by the engine, or (older Windows, some
        // drivers) the device's own format converted here.
        let (client, native) = match open(&device, AUDCLNT_STREAMFLAGS_LOOPBACK, 2) {
            Ok(c) => (c, None),
            Err(e) => {
                log::info!("audio out: the engine will not convert ({e}); converting the mix format here");
                let (c, f) = open_native(&device, AUDCLNT_STREAMFLAGS_LOOPBACK)?;
                log::info!("audio out: device format {f:?}");
                (c, Some(f))
            }
        };
        let mut resampler = Resampler::new(native.map_or(SAMPLE_RATE, |f| f.rate), SAMPLE_RATE);
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(err)?;
        unsafe { client.Start() }.map_err(err)?;
        let mut last_check = Instant::now();
        let mut buf: Vec<f32> = Vec::new();
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            if last_check.elapsed() >= Duration::from_secs(2) {
                last_check = Instant::now();
                let now = unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }.map(|d| device_id(&d));
                if now.as_deref().is_ok_and(|n| n != id) {
                    log::info!("audio out: the default playback device changed");
                    break Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let n = match unsafe { capture.GetNextPacketSize() } {
                    Ok(n) => n,
                    Err(e) => return Err(err(e)),
                };
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                if let Err(e) = unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) } {
                    return Err(err(e));
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                buf.clear();
                match native {
                    None if silent => buf.resize(frames as usize * 2, 0.0),
                    None => buf.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize * 2) }),
                    Some(_) if silent => buf.resize(frames as usize * 2, 0.0),
                    Some(f) => {
                        let bytes = unsafe { std::slice::from_raw_parts(data, frames as usize * f.block_bytes()) };
                        buf.extend(native_to_stereo(bytes, f).unwrap_or_default());
                    }
                }
                let _ = unsafe { capture.ReleaseBuffer(frames) };
                let pcm = resampler.process(&buf);
                sink(&pcm, host_monotonic_us());
            }
        };
        let _ = unsafe { client.Stop() };
        result
    }

    /// Plays mono chunks from `rx` on the playback device `device_id` until `stop`.
    pub fn render_mono(device_id: &str, rx: &crossbeam_channel::Receiver<Vec<f32>>, stop: &AtomicBool) -> Result<(), String> {
        let en = enumerator()?;
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        let device = unsafe { en.GetDevice(::windows::core::PCWSTR(wide.as_ptr())) }.map_err(err)?;
        let client = open(&device, 0, 1)?;
        let render: IAudioRenderClient = unsafe { client.GetService() }.map_err(err)?;
        let size = unsafe { client.GetBufferSize() }.map_err(err)?;
        unsafe { client.Start() }.map_err(err)?;
        let mut pending: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
        while !stop.load(Ordering::Relaxed) {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(5)) {
                pending.extend(chunk);
            }
            while let Ok(chunk) = rx.try_recv() {
                pending.extend(chunk);
            }
            // Keep the latency bounded: a backlog is old speech nobody wants.
            while pending.len() > MIC_QUEUE {
                pending.pop_front();
            }
            let padding = unsafe { client.GetCurrentPadding() }.map_err(err)?;
            let room = size.saturating_sub(padding) as usize;
            let n = room.min(pending.len());
            if n == 0 {
                continue;
            }
            let p = unsafe { render.GetBuffer(n as u32) }.map_err(err)?;
            let out = unsafe { std::slice::from_raw_parts_mut(p as *mut f32, n) };
            for (dst, src) in out.iter_mut().zip(pending.drain(..n)) {
                *dst = src;
            }
            unsafe { render.ReleaseBuffer(n as u32, 0) }.map_err(err)?;
        }
        let _ = unsafe { client.Stop() };
        Ok(())
    }
}

/// Needs an audio stack (Windows, or Wine): `cargo test audio::windows -- --ignored`.
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "needs an audio device"]
    fn loopback_opens_and_runs() {
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let mut calls = 0u32;
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                stop.store(true, Ordering::Relaxed);
            });
            wasapi::loopback(&stop, &mut |pcm, _| {
                assert_eq!(pcm.len() % 2, 0, "interleaved stereo");
                calls += 1;
            })
        });
        assert!(r.is_ok(), "{r:?}");
        assert!(started.elapsed() >= Duration::from_millis(400));
        eprintln!("loopback delivered {calls} packets");
        let devices = wasapi::playback_devices().unwrap();
        assert!(!devices.is_empty());
        eprintln!("playback devices: {devices:?}");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    /// Plays 300 ms of a tone into the default playback device the way the
    /// microphone path plays into a cable: 48 kHz mono float, engine-converted.
    #[test]
    #[ignore = "needs an audio device"]
    fn mono_render_plays_into_a_device() {
        let devices = wasapi::playback_devices().unwrap();
        eprintln!("playback devices: {devices:?}");
        let (id, _) = devices.first().cloned().expect("a playback device");
        let (tx, rx) = crossbeam_channel::bounded(64);
        for chunk in 0..30 {
            let tone: Vec<f32> = (0..480).map(|i| ((chunk * 480 + i) as f32 * 0.0576).sin() * 0.2).collect();
            tx.send(tone).unwrap();
        }
        let stop = AtomicBool::new(false);
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(400));
                stop.store(true, Ordering::Relaxed);
            });
            wasapi::render_mono(&id, &rx, &stop)
        });
        assert!(r.is_ok(), "{r:?}");
        assert!(rx.is_empty(), "all queued audio was played");
    }
}
