//! Windows system audio: a WASAPI loopback capture of the default output.
//!
//! Shared mode, event driven, in the endpoint's mix format; [`ToStereo48`]
//! converts it to what the audio channel carries. WASAPI delivers nothing
//! while nothing plays, so the gaps are filled with silence to keep the
//! timeline continuous (the [`super::codec::SilenceGate`] stops sending it
//! after a short while). When the default output changes or the device goes
//! away the capture is opened again on the new default.
//!
//! The COM objects live on their own thread; dropping [`Loopback`] stops it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};

use super::codec::SAMPLE_RATE;
use super::pcm::{SampleKind, SrcFormat, ToStereo48};
use crate::protocol::host_monotonic_us;

/// Called with interleaved stereo f32 samples at 48 kHz and the host-clock
/// time at which they ended. Runs on the capture thread: keep it short.
pub type PcmCallback = Box<dyn FnMut(&[f32], u64) + Send>;

/// How long the capture waits for data before it fills the gap with silence.
const WAIT_MS: u32 = 20;
/// Silence is only inserted for gaps at least this long.
const FILL_AFTER: Duration = Duration::from_millis(10);
/// The default output is looked up again this often.
const DEFAULT_CHECK: Duration = Duration::from_secs(1);
/// Wait before trying to open the output again after a failure.
const RETRY: Duration = Duration::from_secs(1);

pub struct Loopback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loopback {
    /// Starts capturing the default output. A machine without any output is
    /// not an error: the thread keeps looking for one.
    pub fn start(on_pcm: PcmCallback) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || unsafe { run(&stop_t, on_pcm) })
            .map_err(|e| e.to_string())?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// An open loopback capture of one output.
struct Capture {
    id: String,
    client: IAudioClient,
    reader: IAudioCaptureClient,
    event: HANDLE,
    conv: ToStereo48,
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

unsafe fn default_output_id(en: &IMMDeviceEnumerator) -> windows::core::Result<String> {
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let id: PWSTR = dev.GetId()?;
    let s = id.to_string().unwrap_or_default();
    CoTaskMemFree(Some(id.0 as _));
    Ok(s)
}

/// The sample layout of a mix format.
unsafe fn parse_format(f: *const WAVEFORMATEX) -> Result<SrcFormat, String> {
    let w = std::ptr::read_unaligned(f);
    let (tag, bits, channels, rate) = (w.wFormatTag, w.wBitsPerSample, w.nChannels, w.nSamplesPerSec);
    let (mut float, mut mask) = (tag == 3, 0);
    if tag == 0xFFFE {
        let x = std::ptr::read_unaligned(f as *const WAVEFORMATEXTENSIBLE);
        let sub: GUID = x.SubFormat;
        // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT / _PCM share the rest of the GUID.
        float = sub.data1 == 3;
        if !float && sub.data1 != 1 {
            return Err(format!("unsupported sample format {sub:?}"));
        }
        mask = x.dwChannelMask;
    } else if tag != 1 && tag != 3 {
        return Err(format!("unsupported format tag {tag}"));
    }
    let kind = match (float, bits) {
        (true, 32) => SampleKind::F32,
        (false, 16) => SampleKind::I16,
        (false, 24) => SampleKind::I24,
        (false, 32) => SampleKind::I32,
        _ => return Err(format!("unsupported sample size {bits} bits")),
    };
    if channels == 0 || rate == 0 {
        return Err("empty mix format".into());
    }
    Ok(SrcFormat { rate, channels, kind, mask })
}

unsafe fn open(en: &IMMDeviceEnumerator) -> Result<Capture, String> {
    let e = |what: &'static str| move |err: windows::core::Error| format!("{what}: {err}");
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).map_err(e("default output"))?;
    let id = default_output_id(en).map_err(e("output id"))?;
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(e("activate"))?;
    let mix = client.GetMixFormat().map_err(e("mix format"))?;
    let fmt = parse_format(mix);
    // Loopback has to use the mix format as it is. 200 ms of buffer; the event wakes us far sooner.
    let init = fmt.is_ok().then(|| client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 2_000_000, 0, mix, None));
    CoTaskMemFree(Some(mix as _));
    let fmt = fmt?;
    init.unwrap().map_err(e("initialize loopback"))?;
    let event = CreateEventW(None, false, false, None).map_err(e("event"))?;
    // From here `Capture::drop` cleans up.
    let reader = match client.SetEventHandle(event).and_then(|_| client.GetService::<IAudioCaptureClient>()) {
        Ok(r) => r,
        Err(err) => {
            let _ = CloseHandle(event);
            return Err(format!("capture client: {err}"));
        }
    };
    let cap = Capture { id, client, reader, event, conv: ToStereo48::new(fmt) };
    cap.client.Start().map_err(e("start"))?;
    log::info!("audio loopback: {} Hz, {} ch, {:?} -> 48000 Hz stereo", fmt.rate, fmt.channels, fmt.kind);
    Ok(cap)
}

/// Reads and delivers until the output goes away, changes, or `stop` is set.
unsafe fn pump(cap: &mut Capture, en: &IMMDeviceEnumerator, stop: &AtomicBool, on_pcm: &mut PcmCallback) {
    let mut out: Vec<f32> = Vec::new();
    let mut last_end = Instant::now();
    let mut last_check = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let woke = WaitForSingleObject(cap.event, WAIT_MS) == WAIT_OBJECT_0;
        let mut got = false;
        loop {
            let avail = match cap.reader.GetNextPacketSize() {
                Ok(n) => n,
                Err(e) => return log::info!("audio loopback: output lost ({e})"),
            };
            if avail == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            if let Err(e) = cap.reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) {
                return log::info!("audio loopback: output lost ({e})");
            }
            out.clear();
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let bytes = (!silent).then(|| std::slice::from_raw_parts(data, frames as usize * cap.conv.frame_bytes()));
            cap.conv.convert(bytes, frames as usize, &mut out);
            let _ = cap.reader.ReleaseBuffer(frames);
            got = true;
            on_pcm(&out, host_monotonic_us());
        }
        let now = Instant::now();
        if got {
            last_end = now;
        } else if !woke || now - last_end >= FILL_AFTER {
            // Nothing is playing: keep the timeline running with silence.
            let gap = now - last_end;
            if gap >= FILL_AFTER {
                out.clear();
                out.resize((gap.as_micros() as usize * SAMPLE_RATE as usize / 1_000_000) * 2, 0.0);
                on_pcm(&out, host_monotonic_us());
                last_end = now;
            }
        }
        if now - last_check >= DEFAULT_CHECK {
            last_check = now;
            match default_output_id(en) {
                Ok(id) if id != cap.id => return log::info!("audio loopback: default output changed"),
                Err(_) => return log::info!("audio loopback: no default output"),
                _ => {}
            }
        }
    }
}

unsafe fn run(stop: &AtomicBool, mut on_pcm: PcmCallback) {
    let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
    let mut task = 0u32;
    let mmcss = AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task).ok();
    match CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
        Ok(en) => {
            let mut warned = false;
            while !stop.load(Ordering::Relaxed) {
                match open(&en) {
                    Ok(mut cap) => {
                        warned = false;
                        pump(&mut cap, &en, stop, &mut on_pcm);
                    }
                    Err(e) => {
                        if !std::mem::replace(&mut warned, true) {
                            log::warn!("audio loopback unavailable, retrying: {e}");
                        }
                        let until = Instant::now() + RETRY;
                        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            }
        }
        Err(e) => log::warn!("audio loopback: no device enumerator: {e}"),
    }
    if let Some(h) = mmcss {
        let _ = AvRevertMmThreadCharacteristics(h);
    }
    if com {
        CoUninitialize();
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use std::sync::Mutex;

    /// Needs an output device and something playing during the 2 s it listens
    /// (e.g. `[console]::beep(440,1500)`): prints the delivered frame count and peak level
    /// of the raw (pre-Opus) stereo 48 kHz stream.
    #[test]
    #[ignore]
    fn live_raw_loopback_reports_peak_and_frames() {
        let stats = Arc::new(Mutex::new((0usize, 0usize, 0f32)));
        let s = stats.clone();
        let lb = Loopback::start(Box::new(move |pcm, _| {
            let mut g = s.lock().unwrap();
            g.0 += 1;
            g.1 += pcm.len() / 2;
            g.2 = pcm.iter().fold(g.2, |m, v| m.max(v.abs()));
        }))
        .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        drop(lb);
        let (calls, frames, peak) = *stats.lock().unwrap();
        println!("raw loopback: {calls} callbacks, {frames} frames ({:.2} s), peak {peak:.4}", frames as f64 / 48_000.0);
        assert!(frames > 48_000, "timeline should cover ~2 s");
    }
}
