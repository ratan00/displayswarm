pub mod mock;
pub mod scale;

#[cfg(target_os = "linux")]
pub mod linux;

// Windows: `windows` holds the platform-independent core and `windows_probe` the
// capability decision (both unit-tested on every platform); the two sources
// need the Win32/WinRT APIs.
pub mod windows;
pub mod windows_probe;
#[cfg(target_os = "windows")]
pub mod windows_dxgi;
#[cfg(target_os = "windows")]
pub mod windows_wgc;
#[cfg(all(test, target_os = "windows", feature = "encoders"))]
mod windows_bench;

// Native Wayland screencast (portal + PipeWire + DMA-BUF). This is the
// replacement for xcap on KWin 6, which only offers DMA-BUF buffers.
#[cfg(target_os = "linux")]
pub mod native;
#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod restore_token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>, // RGBA or BGRA 32-bit
    pub timestamp_us: u64,
    /// Zero-copy: the picture is still in GPU memory, at the source's own size
    /// (`width`/`height` are the encoder's), and `data` is empty. Only sent
    /// while [`gpu_frames_enabled`], i.e. to an encoder that imports it.
    pub gpu: Option<std::sync::Arc<GpuFrame>>,
}

/// A captured frame left where the compositor rendered it: a single-plane
/// DMA-BUF. The VA-API encoder imports it and scales and converts it on the
/// GPU, so no pixel crosses the CPU (before: the CPU read every frame out of
/// GPU memory, scaled it and converted it to NV12, then uploaded it again).
#[cfg(not(windows))]
#[derive(Debug)]
pub struct GpuFrame {
    #[cfg(unix)]
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    /// Bytes per row.
    pub stride: u32,
    /// DRM format modifier (layout); 0 is linear.
    pub modifier: u64,
    /// DRM fourcc, e.g. XRGB8888 for BGRx.
    pub drm_format: u32,
    pub width: u32,
    pub height: u32,
}

/// Windows: a Direct3D 11 BGRA texture on the capture device, holding the
/// newest Windows.Graphics.Capture frame (a copy out of the capture pool, so
/// the pool can keep recycling). The QSV encoder's video processor scales and
/// converts it to NV12 on the same device (`encoder/zerocopy_win.rs`).
#[cfg(windows)]
#[derive(Debug)]
pub struct GpuFrame {
    pub device: ::windows::Win32::Graphics::Direct3D11::ID3D11Device,
    pub context: ::windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    pub texture: ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// The picture's size inside the texture (top-left corner).
    pub width: u32,
    pub height: u32,
}

static GPU_FRAMES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether captures should hand over [`GpuFrame`]s: an encoder that can import
/// them is running and its import works.
pub fn gpu_frames_enabled() -> bool {
    GPU_FRAMES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set by the encoder: on when its GPU import is ready, off when it failed
/// (captures then go back to CPU frames by themselves).
pub fn set_gpu_frames(on: bool) {
    GPU_FRAMES.store(on, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
impl GpuFrame {
    /// The pixels as a CPU copy (`stride * height` bytes), for when the GPU
    /// path is off. Linear buffers only; `None` for tiled ones or on failure.
    pub fn read_linear(&self) -> Option<Vec<u8>> {
        use std::os::fd::AsRawFd;
        if self.modifier != 0 {
            return None;
        }
        let len = self.stride as usize * self.height as usize;
        let map_len = self.offset as usize + len;
        let fd = self.fd.as_raw_fd();
        // DMA_BUF_IOCTL_SYNC: _IOW('b', 0, struct dma_buf_sync { u64 flags; })
        const SYNC: libc::c_ulong = 0x4008_6200;
        const SYNC_READ: u64 = 1;
        const SYNC_START: u64 = 0;
        const SYNC_END: u64 = 4;
        // Safety: a read-only shared mapping of a dma-buf we hold an fd to,
        // bracketed by the cache-coherency ioctls, unmapped before returning.
        unsafe {
            let p = libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            if p == libc::MAP_FAILED {
                return None;
            }
            let start: u64 = SYNC_START | SYNC_READ;
            libc::ioctl(fd, SYNC, &start);
            let out = std::slice::from_raw_parts((p as *const u8).add(self.offset as usize), len).to_vec();
            let end: u64 = SYNC_END | SYNC_READ;
            libc::ioctl(fd, SYNC, &end);
            libc::munmap(p, map_len);
            Some(out)
        }
    }
}

impl Frame {
    #[allow(dead_code)]
    pub fn new_rgba(width: u32, height: u32, data: Vec<u8>, timestamp_us: u64) -> Self {
        Self {
            width,
            height,
            format: PixelFormat::Rgba,
            data,
            timestamp_us,
            gpu: None,
        }
    }

    #[allow(dead_code)]
    pub fn new_bgra(width: u32, height: u32, data: Vec<u8>, timestamp_us: u64) -> Self {
        Self {
            width,
            height,
            format: PixelFormat::Bgra,
            data,
            timestamp_us,
            gpu: None,
        }
    }
}

pub trait ScreenCapturer: Send {
    fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>>;
    fn width(&self) -> u32;
    fn height(&self) -> u32;

    /// Human-readable description of what is actually being captured.
    ///
    /// Exists because "is this my real screen or a placeholder?" is otherwise
    /// invisible: a synthetic pattern decodes and renders perfectly, so a failure
    /// to capture looks identical to success unless it is stated explicitly.
    fn describe_source(&self) -> String {
        format!("{}x{} (unspecified source)", self.width(), self.height())
    }

    /// Event-driven capture: waits up to `timeout` for a frame that is NEWER
    /// than the last one returned.
    ///
    /// Returns `Ok(None)` when nothing new arrived in time (the screen is
    /// unchanged), so the caller can skip encoding instead of re-encoding a
    /// duplicate. `Frame::timestamp_us` is the capture time on the host
    /// monotonic clock (`protocol::host_monotonic_us`).
    ///
    /// The default wraps the legacy polling `capture_frame`, which always
    /// yields a frame; backends that can block on their producer override it.
    fn next_frame(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<Frame>, Box<dyn std::error::Error + Send + Sync>> {
        let _ = timeout;
        self.capture_frame().map(Some)
    }

    /// Lifecycle state, for the session state machine and the UIs.
    fn state(&self) -> CaptureState {
        CaptureState::Live
    }

    /// Where the captured monitor sits in the host's global desktop layout,
    /// in logical pixels (the portal's stream `position`/`size`), once known.
    /// `None` for a window, a mock, or before the portal has answered. The
    /// session hands it to [`crate::input::InputInjector::set_output_region`]
    /// so touch and pen land on this monitor.
    fn output_region(&self) -> Option<crate::display::OutputRegion> {
        None
    }
}

/// What to capture for a device (Phase 3 roles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    /// An existing monitor. `output` is a connector name (`eDP-1`) to preselect;
    /// `None` lets the portal ask the user. (Role::Mirror)
    Monitor { output: Option<String> },
    /// A new virtual monitor of this size and refresh (portal source type
    /// VIRTUAL); it disappears when the capturer is dropped.
    /// (Role::Extend, Role::PhonePrimary)
    Virtual { width: u32, height: u32, refresh_mhz: u32, density_dpi: u16 },
    /// One application window, chosen in the portal dialog. (Role::MirrorWindow)
    Window,
}

/// Builds a capturer for `target`.
///
/// `token_key` names the portal restore token to use, so each device and
/// target keeps its own grant (`None` = the shared legacy token). `width` and
/// `height` are the encoder size. Must not block, like [`create_default_capturer`].
///
/// On Linux every target goes through the native portal backend (a monitor
/// keeps the full fallback chain). A virtual monitor or window that cannot be
/// started yields the synthetic pattern with an error in the log, never a
/// silent mirror of the wrong thing. `DISPLAYSWARM_CAPTURE=mock` forces the mock
/// for every target. Other platforms only support `Monitor` and mirror.
pub fn create_capturer(
    target: &CaptureTarget,
    token_key: Option<&str>,
    width: u32,
    height: u32,
) -> Box<dyn ScreenCapturer> {
    #[cfg(target_os = "linux")]
    {
        match linux::LinuxCapturer::new_for(target, token_key, width, height) {
            Ok(cap) => return Box::new(cap),
            Err(e) => {
                log::error!("could not start capture for {target:?}: {e}; using the synthetic pattern");
                return Box::new(mock::MockCapturer::new(width, height));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = token_key;
        match windows::WindowsCapturer::new_for(target, width, height) {
            Ok(cap) => return Box::new(cap),
            Err(e) => {
                log::error!("could not start capture for {target:?}: {e}; using the synthetic pattern");
                return Box::new(mock::MockCapturer::new(width, height));
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (target, token_key);
        create_default_capturer(width, height)
    }
}

/// Where a capturer is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    /// Waiting for the user to answer the screen-share permission dialog.
    AwaitingPermission,
    /// Delivering real frames.
    Live,
    /// Terminally failed; the message says why.
    Failed(String),
}

/// Builds the best capture backend available for this platform.
///
/// Called from the tokio server's async context, so on Linux it **must not
/// block**. It does not: the Wayland path (`capture::linux`) defers the XDG
/// portal handshake -- which waits on a user-facing screen-share dialog for up to
/// five minutes -- to a dedicated thread and returns immediately, so the first
/// `capture_frame` may well be a placeholder until that dialog is answered.
/// `ScreenCapturer::describe_source` exists precisely to make that state
/// visible instead of leaving the caller to guess.
pub fn create_default_capturer(width: u32, height: u32) -> Box<dyn ScreenCapturer> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(cap) = linux::LinuxCapturer::new(width, height) {
            return Box::new(cap);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(cap) = windows::WindowsCapturer::new_for(&CaptureTarget::Monitor { output: None }, width, height) {
            return Box::new(cap);
        }
    }

    // High performance synthetic test pattern fallback
    Box::new(mock::MockCapturer::new(width, height))
}

#[cfg(all(test, target_os = "linux"))]
mod gpu_frame_tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// The CPU fallback reads a linear buffer row by row from its offset. A
    /// memfd stands in for the dma-buf (the sync ioctl just fails on it).
    #[test]
    fn read_linear_copies_the_picture_from_its_offset() {
        let (w, h, stride, offset) = (4u32, 3u32, 20u32, 8u32);
        let len = (offset + stride * h) as usize;
        let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
        // Safety: a fresh memfd we own.
        let fd = unsafe {
            let fd = libc::memfd_create(c"gpu-frame-test".as_ptr(), 0);
            assert!(fd >= 0);
            assert_eq!(libc::write(fd, bytes.as_ptr() as *const _, len), len as isize);
            std::os::fd::OwnedFd::from_raw_fd(fd)
        };
        let g = GpuFrame { fd, offset, stride, modifier: 0, drm_format: 0x3432_5258, width: w, height: h };
        assert_eq!(g.read_linear().unwrap(), bytes[offset as usize..].to_vec());
        let tiled = GpuFrame { modifier: 1 << 56, ..g };
        assert!(tiled.read_linear().is_none(), "tiled layouts are not readable linearly");
    }
}
