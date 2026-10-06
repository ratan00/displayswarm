use std::sync::{Arc, Mutex};
use std::time::Duration;
use crossbeam_channel as cb;
use lamco_pipewire::{PipeWireManager, PipeWireConfig, PixelFormat, SourceType, StreamInfo};
use crate::capture::portal;
use crate::display::OutputRegion;
use crate::capture::PixelFormat as HostPixelFormat;
use crate::protocol::host_monotonic_us;

pub const FRAME_QUEUE_DEPTH: usize = 2;
pub const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// One frame as delivered by PipeWire, before any scaling.
///
/// Carried through the channel with everything the consumer needs to interpret
/// it correctly, instead of a bare `Vec<u8>` whose layout was implied:
///
/// * `data` is the very `Arc` PipeWire's frame holds, so handing a frame to the
///   consumer copies nothing. A consumer that ends up the sole owner can even
///   take the `Vec` without a copy (`Arc::try_unwrap`), which is what makes the
///   1:1 case allocation-free.
/// * `stride` is in bytes: PipeWire rows may be padded, so `width * 4` is not a
///   safe assumption.
/// * `format` is the byte order the compositor *actually negotiated* (see
///   `lamco-pipewire`'s `negotiated_pixel_format`), already reduced to what the
///   encoder path understands. BGRx/BGRA are both `Bgra` (the `x`/`A` byte is
///   the fourth one either way), RGBx/RGBA are `Rgba`.
/// * `timestamp_us` is `host_monotonic_us()` sampled when the frame came out of
///   PipeWire. It must be taken here, before any scaling or queueing, because
///   it is the capture time the phone uses for its latency estimate.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: HostPixelFormat,
    pub timestamp_us: u64,
}

pub fn frame_channel() -> (cb::Sender<RawFrame>, cb::Receiver<RawFrame>) {
    cb::bounded(FRAME_QUEUE_DEPTH)
}

/// Maps the negotiated PipeWire format to the host's byte-order enum, or `None`
/// for formats the 32-bit packed pipeline cannot represent.
fn host_format(f: PixelFormat) -> Option<HostPixelFormat> {
    match f {
        PixelFormat::BGRA | PixelFormat::BGRx => Some(HostPixelFormat::Bgra),
        PixelFormat::RGBA | PixelFormat::RGBx => Some(HostPixelFormat::Rgba),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fraction {
    pub num: u32,
    pub denom: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureFormat {
    pub width: u32,
    pub height: u32,
    pub format: u32, // dummy format
    pub frame_rate: Fraction,
    pub is_dmabuf: bool,
}

impl CaptureFormat {
    pub fn bytes_per_pixel(&self) -> usize { 4 }
    pub fn fourcc_str(&self) -> String { "BGRA".into() }
}

#[derive(Debug)]
pub enum CaptureError {
    Portal(String),
    PipeWire(String),
    NoFramesAfterStart(Duration),
    UnsupportedFormat,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Portal(m) => write!(f, "portal error: {m}"),
            CaptureError::PipeWire(m) => write!(f, "pipewire error: {m}"),
            CaptureError::NoFramesAfterStart(d) => write!(f, "no frames after start: {d:?}"),
            CaptureError::UnsupportedFormat => write!(f, "unsupported buffer format"),
        }
    }
}
impl std::error::Error for CaptureError {}

pub struct NativeScreencast {
    rt: Option<tokio::runtime::Runtime>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    format: Arc<Mutex<Option<CaptureFormat>>>,
    errors: cb::Receiver<CaptureError>,
    /// Frames the PipeWire stream has delivered so far.
    frames_seen: Arc<std::sync::atomic::AtomicU64>,
    /// The portal session, including the D-Bus connection that owns it.
    /// xdg-desktop-portal closes the session (and the compositor destroys the
    /// PipeWire node) as soon as that connection drops off the bus, so it must
    /// live exactly as long as the capture does. Released in `stop()` only
    /// after the capture thread has finished.
    session: Option<portal::ScreencastSession>,
}

impl NativeScreencast {
    pub fn format(&self) -> Option<CaptureFormat> {
        *self.format.lock().unwrap()
    }
    /// Whether the stream has delivered at least one frame.
    pub fn has_frames(&self) -> bool {
        self.frames_seen.load(std::sync::atomic::Ordering::Relaxed) > 0
    }
    pub fn try_take_error(&self) -> Option<CaptureError> {
        self.errors.try_recv().ok()
    }
    pub fn stop(&mut self) {
        self.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
        // Only now, with the capture thread gone: end the portal session
        // explicitly. Dropping the connection alone would (eventually) do it
        // too, but `Close` is what makes the compositor's recording indicator
        // go away promptly. Releasing earlier would kill the stream mid-capture
        // (the "one frame then nothing" bug).
        if let Some(mut session) = self.session.take() {
            session.close();
        }
    }
}

impl Drop for NativeScreencast {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start_screencast(
    output_filter: Option<String>,
    frame_tx: cb::Sender<RawFrame>,
) -> Result<NativeScreencast, CaptureError> {
    start_screencast_cancellable(output_filter, frame_tx, Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// [`start_screencast`] with a cancellation flag.
///
/// The portal handshake can sit in a user-facing dialog for minutes. If the
/// owner gives up meanwhile it sets `cancel`; when the handshake then returns,
/// the freshly created session is closed on the spot instead of being handed to
/// nobody -- a leaked session is exactly what keeps the recording indicator on.
pub fn start_screencast_cancellable(
    output_filter: Option<String>,
    frame_tx: cb::Sender<RawFrame>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<NativeScreencast, CaptureError> {
    start_screencast_request(
        portal::ScreencastRequest::monitor(output_filter, None),
        None,
        frame_tx,
        cancel,
        Arc::new(Mutex::new(None)),
    )
}

/// The stream's rectangle in the desktop layout, published once the portal
/// answers `Start`.
pub type SharedRegion = Arc<Mutex<Option<OutputRegion>>>;

/// A virtual monitor's requested mode: what to offer in the PipeWire format
/// negotiation (Mutter sizes the monitor from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl VirtualMode {
    /// Frame rate to offer, rounded to whole fps and at least 1.
    pub fn fps(&self) -> u32 {
        ((self.refresh_mhz + 500) / 1000).max(1)
    }
}

/// Polls the display layout (off-thread, up to [`VIRTUAL_MONITOR_WAIT`]) for
/// the monitor a VIRTUAL stream creates and publishes its region.
///
/// GNOME: the monitor only appears once the PipeWire stream has negotiated its
/// format, already at the offered size. KDE: the portal creates it at 1920x1080
/// whatever the stream offers, so it is resized to `mode` here with a custom
/// mode and KWin renegotiates the stream.
fn watch_for_virtual_monitor(
    backend: Arc<dyn crate::display::backend::LayoutBackend>,
    before: crate::display::model::Layout,
    mode: VirtualMode,
    region_out: SharedRegion,
) {
    use crate::display::model::{Desktop, LayoutOp, Mode};
    use crate::display::{backend::find_new_output, primary};
    let is_kde = crate::display::env::current_desktop() == Desktop::Kde;
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + VIRTUAL_MONITOR_WAIT;
        while std::time::Instant::now() < deadline {
            let after = backend.snapshot().ok();
            let found = after
                .as_ref()
                .and_then(|after| find_new_output(&before, after, Some((mode.width, mode.height))))
                .and_then(|o| Some((o.name.clone(), o.region?)));
            if let Some((name, mut region)) = found {
                if is_kde {
                    primary::recover_for_output(&name);
                    let want = Mode { width: mode.width, height: mode.height, refresh_mhz: mode.refresh_mhz };
                    match backend.apply(&[LayoutOp::SetMode { name: name.clone(), mode: want }]) {
                        Ok(()) => {
                            if let Some(r) = backend.snapshot().ok().and_then(|l| l.output(&name)?.region) {
                                region = r;
                            }
                        }
                        Err(e) => log::warn!("native capture: could not give {name} the phone's size ({}); streaming it as it is", e.summary()),
                    }
                }
                log::info!("native capture: virtual monitor found at {region:?}");
                *region_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(region);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::warn!("native capture: the virtual monitor did not show up in the display layout; touch maps to the whole desktop");
    });
}

/// How long to look for a new virtual monitor in the layout.
const VIRTUAL_MONITOR_WAIT: Duration = Duration::from_secs(10);

/// [`start_screencast_cancellable`] for any [`portal::ScreencastRequest`].
///
/// With `virtual_mode` the PipeWire stream offers exactly that size and rate
/// (fixed, not a range), which is how the compositor decides how big the new
/// virtual monitor is. The portal-reported region is published to `region_out`
/// as soon as the handshake is done.
pub fn start_screencast_request(
    request: portal::ScreencastRequest,
    virtual_mode: Option<VirtualMode>,
    frame_tx: cb::Sender<RawFrame>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    region_out: SharedRegion,
) -> Result<NativeScreencast, CaptureError> {
    // Neither GNOME's nor KDE's Start response carries a position/size for a
    // VIRTUAL stream, so remember the layout now and find the monitor that appears.
    let layout_backend = crate::display::layout::current();
    let layout_before = virtual_mode.and_then(|_| layout_backend.snapshot().ok());
    let desktop = crate::display::env::current_desktop();
    let kscreen_before = virtual_mode
        .filter(|_| desktop == crate::display::model::Desktop::Kde)
        .and_then(|_| crate::display::kscreen::state().ok());
    let mut session = portal::request_screencast(&request).map_err(CaptureError::Portal)?;
    // KWin can bring the new virtual monitor up as a mirror of the panel (a
    // saved setup says so), and then streams the panel instead. Unmirror it
    // and ask again: the stream is bound to its source when it is created.
    if let Some(before) = &kscreen_before {
        match crate::display::kscreen::unmirror_new_outputs(before) {
            Ok(names) if !names.is_empty() => {
                log::warn!("native capture: KWin made {names:?} a mirror, so the stream showed another screen; unmirrored it, starting again");
                session.close();
                drop(session);
                session = portal::request_screencast(&request).map_err(CaptureError::Portal)?;
                if let Ok(names) = crate::display::kscreen::unmirror_new_outputs(before) {
                    if !names.is_empty() {
                        log::error!("native capture: {names:?} is still a mirror; the phone will show the screen it mirrors");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => log::warn!("native capture: could not check the virtual monitor for mirroring ({})", e.summary()),
        }
    }
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        log::info!("native capture: startup was cancelled while the portal dialog was open; closing the new session");
        session.close();
        return Err(CaptureError::Portal("cancelled".into()));
    }
    if let Some(region) = session.region() {
        log::info!("native capture: stream region {region:?} ({})", session.describe());
        *region_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(region);
    } else if let (Some(before), Some(mode)) = (layout_before, virtual_mode) {
        log::info!("native capture: portal reported no stream region; looking for the new virtual monitor in the display layout");
        watch_for_virtual_monitor(layout_backend, before, mode, region_out.clone());
    } else {
        log::info!("native capture: portal reported no usable stream position/size ({})", session.describe());
    }
    let portal_fd = session.take_fd().ok_or_else(|| CaptureError::Portal("No fd".into()))?;

    let (err_tx, err_rx) = cb::unbounded();
    let format = Arc::new(Mutex::new(None));
    let format_out = format.clone();

    let node_id = session.node_id;
    let size = session.size.unwrap_or((1920, 1080));
    let size_u32 = match virtual_mode {
        Some(m) => (m.width, m.height),
        None => (size.0 as u32, size.1 as u32),
    };

    let frames_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let frames_seen_thread = frames_seen.clone();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();
    
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        
        local.block_on(&rt, async move {
            lamco_pipewire::init();

            // COSMIC's screencast stream is the graph's only clock: a consumer that
            // also drives it never ticks and no frame arrives. Everywhere else the
            // consumer drives, so a static desktop still delivers frames.
            // DISPLAYSWARM_DMABUF=0 forces shared-memory buffers.
            // Buffer size/stride are offered as ranges everywhere but GNOME: KWin and COSMIC
            // fix their own and never intersect with one exact value ("no more input
            // formats" / "error alloc buffers"), while mutter crashed on ranges.
            let drive_graph = desktop != crate::display::model::Desktop::Cosmic;
            let use_dmabuf = std::env::var("DISPLAYSWARM_DMABUF").map(|v| !(v == "0" || v.eq_ignore_ascii_case("false"))).unwrap_or(true);
            let mut cfg = PipeWireConfig::builder()
                .buffer_count(3)
                .preferred_format(PixelFormat::BGRA)
                .use_dmabuf(use_dmabuf)
                .drive_graph(drive_graph)
                .ranged_buffers(desktop != crate::display::model::Desktop::Gnome);
            if let Some(m) = virtual_mode {
                // Mutter sizes the monitor from a fixed-size offer. KWin offers
                // its own fixed size and changes it when the monitor is resized,
                // so there the offer must stay a range.
                let fixed = desktop == crate::display::model::Desktop::Gnome;
                cfg = cfg.fixed_size(fixed).framerate(m.fps());
            }
            let config = cfg.build();
                
            let mut manager = match PipeWireManager::new(config) {
                Ok(m) => m,
                Err(e) => {
                    let _ = err_tx.send(CaptureError::PipeWire(e.to_string()));
                    return;
                }
            };

            if let Err(e) = manager.connect(portal_fd).await {
                let _ = err_tx.send(CaptureError::PipeWire(e.to_string()));
                return;
            }

            let info = StreamInfo {
                node_id,
                position: (0, 0),
                size: size_u32,
                source_type: SourceType::Monitor,
            };

            let handle = match manager.create_stream(&info).await {
                Ok(h) => h,
                Err(e) => {
                    let _ = err_tx.send(CaptureError::PipeWire(e.to_string()));
                    return;
                }
            };

            *format_out.lock().unwrap() = Some(CaptureFormat {
                width: size_u32.0,
                height: size_u32.1,
                format: 0,
                frame_rate: Fraction { num: 60, denom: 1 },
                is_dmabuf: false,
            });

            if let Some(mut rx) = manager.frame_receiver(handle.id).await {
                log::info!("native capture: frame_receiver obtained for stream {}", handle.id);
                let mut frame_count: u64 = 0;
                let mut dmabuf_count: u64 = 0;
                let mut memory_count: u64 = 0;
                let mut send_ok: u64 = 0;
                let mut send_fail: u64 = 0;
                let mut warned_format = false;
                loop {
                    if stop_flag_clone.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    tokio::select! {
                        frame_opt = rx.recv() => {
                            if let Some(frame) = frame_opt {
                                // Stamp at the moment the frame comes out of
                                // PipeWire: before any copying, scaling or
                                // queueing, so the timestamp is the capture time.
                                let stamp = host_monotonic_us();
                                frame_count += 1;
                                frames_seen_thread.store(frame_count, std::sync::atomic::Ordering::Relaxed);
                                let Some(format) = host_format(frame.format) else {
                                    if !warned_format {
                                        warned_format = true;
                                        log::error!(
                                            "native capture: negotiated format {:?} is not a \
                                             32-bit packed RGB format; frames are dropped",
                                            frame.format
                                        );
                                    }
                                    continue;
                                };
                                // No pixel copy on the memory path: the Arc that
                                // PipeWire's frame holds is shared with the
                                // consumer. Only true DMA-BUF frames need
                                // `clone_data` (empty for un-mmappable ones).
                                let data: Option<Arc<Vec<u8>>> = if frame.is_dmabuf() {
                                    dmabuf_count += 1;
                                    if dmabuf_count <= 3 || dmabuf_count % 300 == 0 {
                                        log::warn!(
                                            "native capture: DMA-BUF frame #{} ({}x{}, {} bytes est) — \
                                             extracting via clone_data",
                                            dmabuf_count, frame.width, frame.height, frame.data_size()
                                        );
                                    }
                                    let d = frame.clone_data();
                                    if d.is_empty() { None } else { Some(Arc::new(d)) }
                                } else {
                                    memory_count += 1;
                                    frame.data().cloned()
                                };
                                if let Some(data) = data {
                                    let raw = RawFrame {
                                        data,
                                        width: frame.width,
                                        height: frame.height,
                                        stride: if frame.stride as usize >= frame.width as usize * 4 {
                                            frame.stride as usize
                                        } else {
                                            frame.width as usize * 4
                                        },
                                        format,
                                        timestamp_us: stamp,
                                    };
                                    match frame_tx.try_send(raw) {
                                        Ok(()) => { send_ok += 1; }
                                        Err(_) => { send_fail += 1; }
                                    }
                                }
                                if frame_count <= 5 || frame_count % 300 == 0 {
                                    log::info!(
                                        "native capture: frame #{}: memory={}, dmabuf={}, \
                                         sent_ok={}, sent_fail={}, size={}x{}, stride={}, format={:?}",
                                        frame_count, memory_count, dmabuf_count,
                                        send_ok, send_fail, frame.width, frame.height,
                                        frame.stride, frame.format
                                    );
                                }
                            } else {
                                log::warn!(
                                    "native capture: rx.recv() returned None — stream ended \
                                     (total frames: {}, memory: {}, dmabuf: {}, sent: {}, dropped: {})",
                                    frame_count, memory_count, dmabuf_count, send_ok, send_fail
                                );
                                break; // stream ended
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {
                            // periodically check stop_flag
                        }
                    }
                }
            } else {
                log::error!("native capture: frame_receiver returned None for stream {} — no frames will be delivered!", handle.id);
            }
            
            lamco_pipewire::deinit();
        });
    });

    Ok(NativeScreencast {
        rt: None, // We don't use the Runtime field anymore
        stop_flag,
        thread: Some(handle),
        format,
        errors: err_rx,
        frames_seen,
        session: Some(session),
    })
}
