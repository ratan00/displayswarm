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
    Virtual { width: u32, height: u32, refresh_mhz: u32 },
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
