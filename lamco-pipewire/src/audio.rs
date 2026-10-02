//! PipeWire Audio Capture and Virtual Source
//!
//! Two symmetric directions, both running on a dedicated thread since
//! PipeWire types are not Send:
//!
//! - **Capture** ([`spawn_audio_capture`]): reads PCM from an existing
//!   PipeWire sink/source (e.g. desktop audio) and delivers it through a
//!   channel.
//! - **Virtual source** ([`spawn_virtual_microphone`]): the mirror image.
//!   Creates a new `Audio/Source` node that other applications can select
//!   as an input device (a microphone), fed by PCM pushed in through a
//!   channel. Carries no RDP knowledge; a consumer (e.g. an MS-RDPEAI
//!   backend) decodes the wire protocol and pushes decoded PCM in.
//!
//! # Usage
//!
//! ```rust,ignore
//! use lamco_pipewire::audio::{spawn_audio_capture, CaptureConfig, AudioFormat};
//!
//! let config = CaptureConfig {
//!     format: AudioFormat::F32,
//!     ..Default::default()
//! };
//!
//! let handle = spawn_audio_capture(config, None, 64)?;
//!
//! while let Some(samples) = handle.receiver.recv().await {
//!     // Process samples
//! }
//! ```

use std::collections::VecDeque;
use std::convert::TryInto;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::Pod;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Audio sample format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 32-bit float (native endian)
    F32,
    /// 16-bit signed integer (native endian)
    I16,
}

impl AudioFormat {
    fn to_spa_format(self) -> spa::param::audio::AudioFormat {
        match self {
            Self::F32 => spa::param::audio::AudioFormat::F32LE,
            Self::I16 => spa::param::audio::AudioFormat::S16LE,
        }
    }

    /// Bytes per single sample (one channel)
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::F32 => mem::size_of::<f32>(),
            Self::I16 => mem::size_of::<i16>(),
        }
    }
}

/// Audio capture configuration
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Sample rate in Hz (default: 48000)
    pub sample_rate: u32,
    /// Number of channels (default: 2)
    pub channels: u32,
    /// Output sample format
    pub format: AudioFormat,
    /// Frames per buffer (default: 1024, ~21ms at 48kHz)
    pub buffer_frames: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            format: AudioFormat::F32,
            buffer_frames: 1024,
        }
    }
}

/// Virtual microphone source configuration
///
/// Mirrors [`CaptureConfig`]'s fields exactly (same PCM shape, same
/// defaults); kept as its own type rather than reusing `CaptureConfig`
/// directly since the two engines are conceptually opposite ends of the
/// pipe and a `CaptureConfig` passed to [`spawn_virtual_microphone`] would
/// read backwards at every call site.
#[derive(Debug, Clone)]
pub struct PlaybackConfig {
    /// Sample rate in Hz (default: 48000)
    pub sample_rate: u32,
    /// Number of channels (default: 2)
    pub channels: u32,
    /// Input sample format (the format callers push in via the channel)
    pub format: AudioFormat,
    /// Frames per buffer (default: 1024, ~21ms at 48kHz)
    pub buffer_frames: u32,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            format: AudioFormat::F32,
            buffer_frames: 1024,
        }
    }
}

/// Typed audio sample buffer
#[derive(Debug, Clone)]
pub enum AudioSamples {
    /// 32-bit float samples
    F32(Vec<f32>),
    /// 16-bit signed integer samples
    I16(Vec<i16>),
}

impl AudioSamples {
    /// Number of samples (all channels combined)
    pub fn len(&self) -> usize {
        match self {
            Self::F32(s) => s.len(),
            Self::I16(s) => s.len(),
        }
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert to i16 samples
    pub fn to_i16(&self) -> Vec<i16> {
        match self {
            Self::F32(samples) => samples.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect(),
            Self::I16(samples) => samples.clone(),
        }
    }

    /// Convert to f32 samples
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::F32(samples) => samples.clone(),
            Self::I16(samples) => samples.iter().map(|&s| s as f32 / 32768.0).collect(),
        }
    }
}

/// Handle to a running audio capture session
pub struct AudioCaptureHandle {
    /// Receiver for captured audio samples
    pub receiver: mpsc::Receiver<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl AudioCaptureHandle {
    /// Signal the capture thread to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }

    /// Check if capture has been stopped
    pub fn is_stopped(&self) -> bool {
        self.stop_signal.load(Ordering::SeqCst)
    }
}

struct CaptureUserData {
    format: spa::param::audio::AudioInfoRaw,
    output_format: AudioFormat,
    sender: mpsc::Sender<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
    samples_captured: u64,
    samples_dropped: u64,
}

/// PipeWire audio capture engine
///
/// Captures desktop audio via PipeWire and sends PCM samples through a channel.
/// Must be run on a dedicated thread via [`spawn_audio_capture`].
pub struct AudioCapture {
    config: CaptureConfig,
    sender: mpsc::Sender<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl AudioCapture {
    /// Create a new capture instance and its handle
    pub fn new(config: CaptureConfig, channel_size: usize) -> (Self, AudioCaptureHandle) {
        let (sender, receiver) = mpsc::channel(channel_size);
        let stop_signal = Arc::new(AtomicBool::new(false));

        let capture = Self {
            config,
            sender,
            stop_signal: Arc::clone(&stop_signal),
        };

        let handle = AudioCaptureHandle { receiver, stop_signal };

        (capture, handle)
    }

    /// Run the PipeWire main loop for audio capture (blocking).
    ///
    /// Call from a dedicated thread. Connects to the PipeWire daemon,
    /// negotiates audio format, and delivers samples until stopped.
    pub fn start_capture(&self, node_id: Option<u32>) -> Result<()> {
        info!(
            "Starting audio capture: {}Hz, {} channels, format={:?}, node_id={:?}",
            self.config.sample_rate, self.config.channels, self.config.format, node_id
        );

        // PipeWire 0.9 Box types for owned resources
        let mainloop = pw::main_loop::MainLoopBox::new(None).context("Failed to create PipeWire MainLoop")?;
        let context =
            pw::context::ContextBox::new(mainloop.loop_(), None).context("Failed to create PipeWire Context")?;
        let core = context.connect(None).context("Failed to connect to PipeWire daemon")?;

        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "lamco-audio-capture",
            *pw::keys::APP_NAME => "lamco-pipewire",
        };

        if let Some(id) = node_id {
            props.insert("target.object", id.to_string());
        }

        props.insert("stream.capture.sink", "true");

        let stream = pw::stream::StreamBox::new(&core, "lamco-audio-capture", props)
            .context("Failed to create PipeWire stream")?;

        let user_data = CaptureUserData {
            format: spa::param::audio::AudioInfoRaw::default(),
            output_format: self.config.format,
            sender: self.sender.clone(),
            stop_signal: Arc::clone(&self.stop_signal),
            samples_captured: 0,
            samples_dropped: 0,
        };

        let stop_signal_for_callback = Arc::clone(&self.stop_signal);

        let _listener = stream
            .add_local_listener_with_user_data(user_data)
            .state_changed(move |_stream, _user_data, old, new| {
                debug!("Audio stream state: {:?} -> {:?}", old, new);

                match new {
                    // The session manager could not link the capture to a
                    // sink: the host has no audio output to record, which is
                    // a setup fact, not a fault in the stream.
                    pw::stream::StreamState::Error(err) if err.contains("no target node") => {
                        warn!("No audio output to capture: PipeWire has no sink on this host, so clients get no sound");
                        stop_signal_for_callback.store(true, Ordering::SeqCst);
                    }
                    pw::stream::StreamState::Error(err) => {
                        error!("Audio stream error: {}", err);
                        stop_signal_for_callback.store(true, Ordering::SeqCst);
                    }
                    pw::stream::StreamState::Streaming => {
                        info!("Audio capture streaming started");
                    }
                    pw::stream::StreamState::Paused => {
                        debug!("Audio stream paused");
                    }
                    _ => {}
                }
            })
            .param_changed(|_stream, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };

                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse audio format: {:?}", e);
                        return;
                    }
                };

                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    debug!("Ignoring non-raw audio format: {:?}/{:?}", media_type, media_subtype);
                    return;
                }

                if let Err(e) = user_data.format.parse(param) {
                    warn!("Failed to parse audio info: {:?}", e);
                    return;
                }

                info!(
                    "Audio format negotiated: rate={}, channels={}, format={:?}",
                    user_data.format.rate(),
                    user_data.format.channels(),
                    user_data.format.format()
                );
            })
            .process(|stream, user_data| {
                if user_data.stop_signal.load(Ordering::Relaxed) {
                    return;
                }

                let Some(mut buffer) = stream.dequeue_buffer() else {
                    trace!("No buffer available");
                    return;
                };

                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let chunk = data.chunk();
                let size = chunk.size() as usize;

                if size == 0 {
                    return;
                }

                let Some(slice) = data.data() else {
                    return;
                };

                let n_channels = user_data.format.channels() as usize;
                if n_channels == 0 {
                    return;
                }

                let samples = match user_data.format.format() {
                    spa::param::audio::AudioFormat::F32LE | spa::param::audio::AudioFormat::F32BE => {
                        let byte_count = size.min(slice.len());
                        let sample_count = byte_count / mem::size_of::<f32>();
                        let mut f32_samples = Vec::with_capacity(sample_count);

                        for i in 0..sample_count {
                            let start = i * mem::size_of::<f32>();
                            let end = start + mem::size_of::<f32>();
                            if end <= slice.len() {
                                let bytes: [u8; 4] = slice[start..end].try_into().unwrap_or([0; 4]);
                                let sample = if user_data.format.format() == spa::param::audio::AudioFormat::F32LE {
                                    f32::from_le_bytes(bytes)
                                } else {
                                    f32::from_be_bytes(bytes)
                                };
                                f32_samples.push(sample);
                            }
                        }

                        match user_data.output_format {
                            AudioFormat::F32 => AudioSamples::F32(f32_samples),
                            AudioFormat::I16 => {
                                let i16_samples: Vec<i16> = f32_samples
                                    .iter()
                                    .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                                    .collect();
                                AudioSamples::I16(i16_samples)
                            }
                        }
                    }
                    spa::param::audio::AudioFormat::S16LE | spa::param::audio::AudioFormat::S16BE => {
                        let byte_count = size.min(slice.len());
                        let sample_count = byte_count / mem::size_of::<i16>();
                        let mut i16_samples = Vec::with_capacity(sample_count);

                        for i in 0..sample_count {
                            let start = i * mem::size_of::<i16>();
                            let end = start + mem::size_of::<i16>();
                            if end <= slice.len() {
                                let bytes: [u8; 2] = slice[start..end].try_into().unwrap_or([0; 2]);
                                let sample = if user_data.format.format() == spa::param::audio::AudioFormat::S16LE {
                                    i16::from_le_bytes(bytes)
                                } else {
                                    i16::from_be_bytes(bytes)
                                };
                                i16_samples.push(sample);
                            }
                        }

                        match user_data.output_format {
                            AudioFormat::I16 => AudioSamples::I16(i16_samples),
                            AudioFormat::F32 => {
                                let f32_samples: Vec<f32> = i16_samples.iter().map(|&s| s as f32 / 32768.0).collect();
                                AudioSamples::F32(f32_samples)
                            }
                        }
                    }
                    other => {
                        trace!("Unsupported audio format: {:?}", other);
                        return;
                    }
                };

                let sample_count = samples.len();

                // Non-blocking send to maintain realtime performance
                match user_data.sender.try_send(samples) {
                    Ok(()) => {
                        user_data.samples_captured += sample_count as u64;
                        trace!("Captured {} samples", sample_count);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        user_data.samples_dropped += sample_count as u64;
                        trace!("Dropped {} samples (channel full)", sample_count);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        user_data.stop_signal.store(true, Ordering::SeqCst);
                        debug!("Audio sample channel closed");
                    }
                }
            })
            .register()
            .context("Failed to register stream listener")?;

        // Build format parameters for negotiation. Pin the rate and channel
        // count as well as the format: leaving them unset advertises "any", so
        // PipeWire delivers its graph-native rate (typically 48 kHz) and ignores
        // the requested sample_rate entirely. Setting the rate makes PipeWire
        // insert a resampler, so a consumer asking for 44.1 kHz actually receives
        // 44.1 kHz (needed for RDP clients whose endpoints resample poorly).
        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(self.config.format.to_spa_format());
        audio_info.set_rate(self.config.sample_rate);
        audio_info.set_channels(self.config.channels);

        let obj = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        };

        let pod_bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )
        .context("Failed to serialize audio format pod")?
        .0
        .into_inner();

        let pod = Pod::from_bytes(&pod_bytes).context("Failed to create pod from bytes")?;

        let mut params = [pod];

        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;

        stream
            .connect(spa::utils::Direction::Input, node_id, flags, &mut params)
            .context("Failed to connect PipeWire stream")?;

        info!("Audio capture stream connected, starting main loop");

        let loop_ref = mainloop.loop_();
        while !self.stop_signal.load(Ordering::Relaxed) {
            loop_ref.iterate(pw::loop_::Timeout::Finite(std::time::Duration::from_millis(100)));
        }

        info!("Audio capture stopped");
        Ok(())
    }

    /// Signal the capture to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }
}

/// Spawn audio capture on a dedicated thread
///
/// Returns a handle with a receiver for audio samples. The capture runs
/// until the handle is dropped or `stop()` is called.
///
/// # Arguments
///
/// * `config` - Audio capture configuration
/// * `node_id` - Optional PipeWire node ID (from portal session)
/// * `channel_size` - Bounded channel capacity for sample buffers
pub fn spawn_audio_capture(
    config: CaptureConfig,
    node_id: Option<u32>,
    channel_size: usize,
) -> Result<AudioCaptureHandle> {
    let (capture, handle) = AudioCapture::new(config, channel_size);

    std::thread::Builder::new()
        .name("pipewire-audio".into())
        .spawn(move || {
            // Shared, reference-counted acquire — see crate::pw_lifecycle.
            // See crate::pw_lifecycle for why this goes through the shared
            // acquire()/release() pair instead of pipewire::init() directly.
            crate::pw_lifecycle::acquire();

            if let Err(e) = capture.start_capture(node_id) {
                error!("Audio capture error: {:#}", e);
            }

            // Release this thread's share of the process-wide PipeWire user
            // count. See crate::pw_lifecycle: this is bookkeeping only, it
            // does not call the real pipewire::deinit().
            crate::pw_lifecycle::release();
        })
        .context("Failed to spawn audio capture thread")?;

    Ok(handle)
}

/// How much PCM the virtual microphone will buffer before dropping the
/// oldest bytes rather than growing further.
///
/// [`spawn_virtual_microphone`]'s producer (an RDP client's AUDIN Data
/// PDUs, say) and consumer (PipeWire's own driver clock, calling
/// `process()` on its own schedule) are not lock-stepped, so short bursts
/// are normal and must be absorbed. Bounding the absorption window keeps a
/// slow or bursty producer from turning into unbounded latency: a live
/// microphone feed that silently falls behind is worse than one that
/// occasionally drops its oldest backlog to stay near real time.
const MAX_BUFFERED_MS: u32 = 200;

/// Fill one PipeWire cycle of the virtual microphone from `ring`.
///
/// Writes the frames the graph asked for this cycle (`requested_frames`,
/// from `pw_buffer.requested`), not the whole mapped buffer: the buffer can
/// hold many quanta, and filling all of it every cycle consumed the ring far
/// faster than real time, turning the rest of each chunk into silence.
/// Older PipeWire reports 0 there, in which case the whole buffer is used.
/// Any shortfall is silence-padded so the chunk never carries stale bytes.
///
/// Returns `(bytes taken from the ring, chunk length in bytes)`.
fn fill_cycle(
    ring: &mut VecDeque<u8>,
    dest: &mut [u8],
    requested_frames: u64,
    bytes_per_frame: usize,
) -> (usize, usize) {
    let wanted = usize::try_from(requested_frames)
        .ok()
        .filter(|&frames| frames > 0)
        .map_or(dest.len(), |frames| frames.saturating_mul(bytes_per_frame));
    let chunk_len = wanted.min(dest.len()) / bytes_per_frame * bytes_per_frame;
    let available = ring.len().min(chunk_len);

    for (slot, byte) in dest[..available].iter_mut().zip(ring.drain(..available)) {
        *slot = byte;
    }
    dest[available..chunk_len].fill(0);

    (available, chunk_len)
}

/// Handle to a running virtual microphone source
pub struct VirtualMicrophoneHandle {
    /// Sender for PCM samples to push into the virtual source
    pub sender: mpsc::Sender<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl VirtualMicrophoneHandle {
    /// Signal the playback thread to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }

    /// Check if playback has been stopped
    pub fn is_stopped(&self) -> bool {
        self.stop_signal.load(Ordering::SeqCst)
    }
}

struct PlaybackUserData {
    receiver: mpsc::Receiver<AudioSamples>,
    format: AudioFormat,
    bytes_per_frame: usize,
    max_buffered_bytes: usize,
    ring: VecDeque<u8>,
    stop_signal: Arc<AtomicBool>,
    frames_written: u64,
    frames_silenced: u64,
}

/// PipeWire virtual microphone source
///
/// Creates an `Audio/Source` node that other applications can select as an
/// input device, fed by PCM pushed in through a channel. Must be run on a
/// dedicated thread via [`spawn_virtual_microphone`].
pub struct VirtualMicrophone {
    config: PlaybackConfig,
    receiver: mpsc::Receiver<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl VirtualMicrophone {
    /// Create a new virtual microphone instance and its handle
    pub fn new(config: PlaybackConfig, channel_size: usize) -> (Self, VirtualMicrophoneHandle) {
        let (sender, receiver) = mpsc::channel(channel_size);
        let stop_signal = Arc::new(AtomicBool::new(false));

        let mic = Self {
            config,
            receiver,
            stop_signal: Arc::clone(&stop_signal),
        };

        let handle = VirtualMicrophoneHandle { sender, stop_signal };

        (mic, handle)
    }

    /// Run the PipeWire main loop for the virtual microphone (blocking).
    ///
    /// Call from a dedicated thread. Connects to the PipeWire daemon,
    /// registers the source node, and streams whatever PCM has been pushed
    /// through the channel until stopped, padding with silence on
    /// underrun.
    ///
    /// `node_label` sets `node.description` (the human-readable name shown
    /// in mic pickers); defaults to "Lamco Virtual Microphone" when `None`.
    pub fn start_playback(self, node_label: Option<&str>) -> Result<()> {
        info!(
            "Starting virtual microphone: {}Hz, {} channels, format={:?}",
            self.config.sample_rate, self.config.channels, self.config.format
        );

        let mainloop = pw::main_loop::MainLoopBox::new(None).context("Failed to create PipeWire MainLoop")?;
        let context =
            pw::context::ContextBox::new(mainloop.loop_(), None).context("Failed to create PipeWire Context")?;
        let core = context.connect(None).context("Failed to connect to PipeWire daemon")?;

        let description = node_label.unwrap_or("Lamco Virtual Microphone");

        let props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            // "Capture" describes what CONSUMERS do with this node (they
            // capture from it, as they would a hardware microphone), not
            // the local `pw_stream` direction below -- this is the same
            // convention lamco's own xdg-desktop-portal-generic uses for
            // its Video/Source screen-capture node.
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => "lamco-rdp-microphone",
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::APP_NAME => "lamco-pipewire",
            "stream.is-live" => "true",
            // Root cause of a confirmed live bug: a follower node not
            // linked to a driver stays "suspended" and its `process()`
            // callback is never invoked (PipeWire's own docs, "Streams" --
            // this is the documented default since 0.3.51), even once a
            // real consumer links to it (verified via `pw-dump`: an active
            // Link to a running client, node still `suspended`, `process()`
            // never called). This node must always produce data --
            // including silence when nothing has been pushed yet, per
            // `spawn_virtual_microphone`'s own doc comment -- regardless of
            // whether anything is currently listening, so opt out of the
            // link-gated default entirely.
            *pw::keys::NODE_ALWAYS_PROCESS => "true",
        };

        let stream = pw::stream::StreamBox::new(&core, "lamco-rdp-microphone", props)
            .context("Failed to create PipeWire stream")?;

        let bytes_per_frame = self.config.channels as usize * self.config.format.bytes_per_sample();
        let max_buffered_bytes = (self.config.sample_rate as usize * bytes_per_frame * MAX_BUFFERED_MS as usize) / 1000;

        let user_data = PlaybackUserData {
            receiver: self.receiver,
            format: self.config.format,
            bytes_per_frame,
            max_buffered_bytes,
            ring: VecDeque::with_capacity(max_buffered_bytes),
            stop_signal: Arc::clone(&self.stop_signal),
            frames_written: 0,
            frames_silenced: 0,
        };

        let stop_signal_for_callback = Arc::clone(&self.stop_signal);

        let _listener = stream
            .add_local_listener_with_user_data(user_data)
            .state_changed(move |_stream, _user_data, old, new| {
                debug!("Virtual microphone stream state: {:?} -> {:?}", old, new);

                match new {
                    pw::stream::StreamState::Error(err) => {
                        error!("Virtual microphone stream error: {}", err);
                        stop_signal_for_callback.store(true, Ordering::SeqCst);
                    }
                    pw::stream::StreamState::Streaming => {
                        info!("Virtual microphone streaming started");
                    }
                    pw::stream::StreamState::Paused => {
                        debug!("Virtual microphone stream paused");
                    }
                    _ => {}
                }
            })
            .param_changed(|_stream, _user_data, id, param| {
                let Some(param) = param else {
                    return;
                };

                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse virtual microphone format: {:?}", e);
                        return;
                    }
                };

                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    debug!("Ignoring non-raw audio format: {:?}/{:?}", media_type, media_subtype);
                    return;
                }

                let mut info = spa::param::audio::AudioInfoRaw::default();
                if let Err(e) = info.parse(param) {
                    warn!("Failed to parse virtual microphone audio info: {:?}", e);
                    return;
                }

                info!(
                    "Virtual microphone format negotiated: rate={}, channels={}, format={:?}",
                    info.rate(),
                    info.channels(),
                    info.format()
                );
            })
            .process(|stream, user_data| {
                if user_data.stop_signal.load(Ordering::Relaxed) {
                    return;
                }

                // Drain everything currently queued from the producer without
                // blocking. PipeWire calls process() on the graph's own
                // cadence, not the producer's, so samples arrive in bursts
                // (e.g. a batch of decoded AUDIN Data PDUs) and must be
                // buffered here rather than dropped.
                while let Ok(samples) = user_data.receiver.try_recv() {
                    let bytes: Vec<u8> = match user_data.format {
                        AudioFormat::F32 => samples.to_f32().iter().flat_map(|s| s.to_le_bytes()).collect(),
                        AudioFormat::I16 => samples.to_i16().iter().flat_map(|s| s.to_le_bytes()).collect(),
                    };
                    user_data.ring.extend(bytes);
                }

                while user_data.ring.len() > user_data.max_buffered_bytes {
                    user_data.ring.pop_front();
                }

                let Some(mut buffer) = stream.dequeue_buffer() else {
                    trace!("No buffer available");
                    return;
                };
                let requested_frames = buffer.requested();

                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let Some(dest) = data.data() else {
                    return;
                };

                let bytes_per_frame = user_data.bytes_per_frame.max(1);
                let (available, chunk_len) = fill_cycle(&mut user_data.ring, dest, requested_frames, bytes_per_frame);
                user_data.frames_silenced += ((chunk_len - available) / bytes_per_frame) as u64;
                user_data.frames_written += (available / bytes_per_frame) as u64;

                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = user_data.bytes_per_frame as i32;
                *chunk.size_mut() = chunk_len as u32;
            })
            .register()
            .context("Failed to register stream listener")?;

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(self.config.format.to_spa_format());
        audio_info.set_rate(self.config.sample_rate);
        audio_info.set_channels(self.config.channels);

        let obj = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        };

        let pod_bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )
        .context("Failed to serialize audio format pod")?
        .0
        .into_inner();

        let pod = Pod::from_bytes(&pod_bytes).context("Failed to create pod from bytes")?;

        let mut params = [pod];

        // AUTOCONNECT: without it, this stream is registered but never
        // linked into the graph. DRIVER was also tried here and rejected --
        // PipeWire returned Error("Start error: Invalid argument") for this
        // stream shape, so it is not a viable fix for this node.
        //
        // No ALLOC_BUFFERS: native PipeWire trace (PIPEWIRE_DEBUG=3) showed
        // the real failure underneath that same "Invalid argument" --
        // `pw.stream impl_port_use_buffers(): invalid buffer mem` followed
        // by `pw.node start_node(): start node error -22`, right after the
        // spa.audioadapter (auto-inserted for our S16LE-mono -> graph-native
        // format conversion) negotiated buffers. ALLOC_BUFFERS asks this
        // stream to own the buffer memory itself, which the adapter
        // apparently cannot accept for this node shape. The proven-working
        // capture-direction sibling in this same file (`AudioCapture`,
        // above) uses AUTOCONNECT | MAP_BUFFERS | RT_PROCESS with no
        // ALLOC_BUFFERS -- match it.
        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;

        stream
            .connect(spa::utils::Direction::Output, None, flags, &mut params)
            .context("Failed to connect PipeWire stream")?;

        info!("Virtual microphone stream connected, starting main loop");

        let loop_ref = mainloop.loop_();
        while !self.stop_signal.load(Ordering::Relaxed) {
            loop_ref.iterate(pw::loop_::Timeout::Finite(std::time::Duration::from_millis(100)));
        }

        info!("Virtual microphone stopped");
        Ok(())
    }

    /// Signal the playback to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }
}

/// Spawn a virtual microphone source on a dedicated thread
///
/// Returns a handle whose `sender` accepts PCM samples to feed into the
/// virtual source. The node exists and streams (silence, if nothing has
/// been pushed yet) until the handle is dropped or `stop()` is called.
///
/// # Arguments
///
/// * `config` - Virtual microphone configuration
/// * `node_label` - Human-readable node description (mic-picker display name)
/// * `channel_size` - Bounded channel capacity for pushed sample batches
pub fn spawn_virtual_microphone(
    config: PlaybackConfig,
    node_label: Option<String>,
    channel_size: usize,
) -> Result<VirtualMicrophoneHandle> {
    let (mic, handle) = VirtualMicrophone::new(config, channel_size);

    std::thread::Builder::new()
        .name("pipewire-mic".into())
        .spawn(move || {
            // Shared, reference-counted acquire -- see crate::pw_lifecycle.
            crate::pw_lifecycle::acquire();

            if let Err(e) = mic.start_playback(node_label.as_deref()) {
                error!("Virtual microphone error: {:#}", e);
            }

            crate::pw_lifecycle::release();
        })
        .context("Failed to spawn virtual microphone thread")?;

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_config_default() {
        let config = CaptureConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 2);
        assert_eq!(config.format, AudioFormat::F32);
    }

    #[test]
    fn test_audio_samples_conversion() {
        let f32_samples = AudioSamples::F32(vec![0.0, 0.5, -0.5, 1.0, -1.0]);
        let i16_converted = f32_samples.to_i16();
        assert_eq!(i16_converted.len(), 5);
        assert_eq!(i16_converted[0], 0);
        assert!((i16_converted[1] - 16383).abs() <= 1);

        let i16_samples = AudioSamples::I16(vec![0, 16384, -16384, 32767, -32768]);
        let f32_converted = i16_samples.to_f32();
        assert_eq!(f32_converted.len(), 5);
        assert!((f32_converted[0] - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_audio_format_bytes_per_sample() {
        assert_eq!(AudioFormat::F32.bytes_per_sample(), 4);
        assert_eq!(AudioFormat::I16.bytes_per_sample(), 2);
    }

    #[test]
    fn test_audio_samples_empty() {
        let empty = AudioSamples::F32(vec![]);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_audio_capture_handle_stop() {
        let config = CaptureConfig::default();
        let (_capture, handle) = AudioCapture::new(config, 10);

        assert!(!handle.is_stopped());
        handle.stop();
        assert!(handle.is_stopped());
    }

    #[test]
    fn test_playback_config_default() {
        let config = PlaybackConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 2);
        assert_eq!(config.format, AudioFormat::F32);
    }

    #[test]
    fn test_virtual_microphone_handle_stop() {
        let config = PlaybackConfig::default();
        let (_mic, handle) = VirtualMicrophone::new(config, 10);

        assert!(!handle.is_stopped());
        handle.stop();
        assert!(handle.is_stopped());
    }

    #[test]
    fn test_max_buffered_bytes_matches_two_hundred_ms() {
        // 48000 Hz * 2 channels * 4 bytes (F32) * 200ms / 1000 = 76800 bytes.
        let config = PlaybackConfig::default();
        let bytes_per_frame = config.channels as usize * config.format.bytes_per_sample();
        let expected = (config.sample_rate as usize * bytes_per_frame * MAX_BUFFERED_MS as usize) / 1000;
        assert_eq!(expected, 76_800);
    }

    #[test]
    fn test_virtual_microphone_underrun_pads_silence() {
        // A ring shorter than the requested chunk must silence-pad the
        // shortfall rather than leave stale bytes.
        let bytes_per_frame = 2 * AudioFormat::I16.bytes_per_sample();
        let mut ring: VecDeque<u8> = VecDeque::from(vec![1u8, 2, 3, 4]); // one I16 stereo frame
        let mut dest = vec![0xFFu8; 16]; // four frames

        let (available, chunk_len) = fill_cycle(&mut ring, &mut dest, 4, bytes_per_frame);

        assert_eq!((available, chunk_len), (4, 16));
        assert_eq!(&dest[..4], &[1, 2, 3, 4]);
        assert!(dest[4..].iter().all(|&b| b == 0));
        assert!(ring.is_empty());
    }

    #[test]
    fn test_virtual_microphone_writes_only_the_requested_frames() {
        // PipeWire maps a buffer several quanta long but asks for one
        // quantum per cycle. Filling the whole buffer drained the ring
        // faster than real time and padded the rest with silence.
        let bytes_per_frame = AudioFormat::I16.bytes_per_sample(); // mono
        let mut ring: VecDeque<u8> = (0..=255u8).cycle().take(8192).collect();
        let mut dest = vec![0u8; 24576]; // 12288 frames mapped

        let (available, chunk_len) = fill_cycle(&mut ring, &mut dest, 1024, bytes_per_frame);

        assert_eq!((available, chunk_len), (2048, 2048));
        assert_eq!(ring.len(), 8192 - 2048, "only one quantum is consumed");
    }

    #[test]
    fn test_virtual_microphone_without_requested_fills_the_buffer() {
        let bytes_per_frame = AudioFormat::I16.bytes_per_sample();
        let mut ring: VecDeque<u8> = VecDeque::from(vec![7u8; 64]);
        let mut dest = vec![0xFFu8; 32];

        let (available, chunk_len) = fill_cycle(&mut ring, &mut dest, 0, bytes_per_frame);

        assert_eq!((available, chunk_len), (32, 32));
        assert!(dest.iter().all(|&b| b == 7));
    }
}
