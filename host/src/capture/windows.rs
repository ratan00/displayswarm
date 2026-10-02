//! Windows screen capture.
//!
//! Two sources sit behind the [`RawSource`] trait:
//!
//! * [`super::windows_wgc`] - Windows.Graphics.Capture (Windows 10 1903+).
//!   Captures a monitor or one window, includes the cursor, is event driven,
//!   and works for IDD virtual monitors. Preferred.
//! * [`super::windows_dxgi`] - DXGI Desktop Duplication. Monitors only, no
//!   cursor in the picture; the fallback when WGC is unavailable or refuses.
//!
//! [`WindowsCapturer`] wraps a source and does the platform-independent part:
//! fitting the picture to the encoder size, monotonic timestamps, "nothing new"
//! handling and the polling `capture_frame` contract. That part is unit-tested
//! on Linux with a fake source.
//!
//! Targets ([`CaptureTarget`]): `Monitor` (a named `\\.\DISPLAYn`, else the
//! primary), `Virtual` (adds a monitor of the phone's mode through the
//! installed display driver, [`crate::display::virtual_monitor`], captures it
//! with [`VirtualMonitorSource`], and gives it back on drop) and
//! `Window` (`DISPLAYSWARM_CAPTURE_WINDOW` = title substring or `0x<hwnd>`, else the
//! top-most ordinary window that is not this process).
//!
//! Coordinates: [`ScreenCapturer::output_region`] is in virtual-screen pixels,
//! the space `InputInjector::set_output_region` and `SendInput` use. The
//! process is made per-monitor DPI aware first so all of them agree.

use super::{CaptureState, CaptureTarget, Frame, PixelFormat, ScreenCapturer};
use crate::display::virtual_monitor::{MonitorPlace, VirtualMonitor};
use crate::display::OutputRegion;
use crate::protocol::host_monotonic_us;
use std::time::{Duration, Instant};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// Platform-independent core
// ---------------------------------------------------------------------------

/// One captured picture at the source's native size, tightly packed BGRA.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// A producer of native-size frames (WGC, DXGI, or a test double).
pub trait RawSource: Send {
    /// Waits up to `timeout` for a picture newer than the last one returned.
    fn next_raw(&mut self, timeout: Duration) -> Result<Option<RawFrame>, BoxError>;
    fn describe(&self) -> String;
    /// `Some(reason)` once the source can never deliver again (window closed).
    fn failure(&self) -> Option<String> {
        None
    }
    /// The captured monitor's rectangle in virtual-screen pixels, if it is one.
    fn region(&self) -> Option<OutputRegion> {
        None
    }
    /// Still getting ready (a virtual monitor being added): no frames yet, but
    /// not failed either.
    fn pending(&self) -> bool {
        false
    }
}

/// Adds a virtual monitor (off the caller's thread: a driver reload takes
/// seconds and `create_capturer` must not block) and captures it once it is
/// there. When the capture dies because a driver reload re-created the monitor
/// (another phone joined or left), the monitor is looked up again and the
/// capture rebuilt.
pub struct VirtualMonitorSource {
    // Field order matters: the capture goes before the monitor it shows.
    inner: Option<Box<dyn RawSource>>,
    open: Box<dyn Fn(&MonitorPlace) -> Result<Box<dyn RawSource>, String> + Send>,
    monitor: Option<Box<dyn VirtualMonitor>>,
    arriving: Option<std::sync::mpsc::Receiver<Result<Box<dyn VirtualMonitor>, String>>>,
    failed: Option<String>,
    label: String,
    /// Rebuilds in a row that produced no frame.
    rebuilds: u32,
    next_try: Instant,
}

/// Rebuilds of a dead capture before giving up (a monitor that stays gone).
const MAX_REBUILDS: u32 = 20;
const REBUILD_GAP: Duration = Duration::from_millis(500);

impl VirtualMonitorSource {
    /// Starts `add` on its own thread; `open` builds the capture of the monitor
    /// once it is on the desktop (and again after a reload).
    pub fn start(
        label: String,
        add: impl FnOnce() -> Result<Box<dyn VirtualMonitor>, String> + Send + 'static,
        open: impl Fn(&MonitorPlace) -> Result<Box<dyn RawSource>, String> + Send + 'static,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("displayswarm-virtual-add".into()).spawn(move || {
            // A receiver gone (the session ended first) drops the monitor here,
            // which gives it back.
            let _ = tx.send(add());
        });
        let failed = spawned.err().map(|e| format!("could not start adding the virtual monitor: {e}"));
        VirtualMonitorSource {
            inner: None,
            open: Box::new(open),
            monitor: None,
            arriving: Some(rx),
            failed,
            label,
            rebuilds: 0,
            next_try: Instant::now(),
        }
    }

    /// Takes the added monitor when it is ready; builds or rebuilds the capture.
    fn poll_ready(&mut self) {
        if let Some(rx) = &self.arriving {
            match rx.try_recv() {
                Ok(Ok(m)) => {
                    log::info!("Virtual monitor ready: {}", m.describe());
                    self.monitor = Some(m);
                    self.arriving = None;
                }
                Ok(Err(e)) => {
                    self.failed = Some(e);
                    self.arriving = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.failed = Some("adding the virtual monitor stopped without an answer".into());
                    self.arriving = None;
                }
            }
        }
        if self.inner.as_ref().is_some_and(|s| s.failure().is_some()) {
            let why = self.inner.as_ref().and_then(|s| s.failure()).unwrap_or_default();
            log::info!("Capture of the virtual monitor ended ({why}); looking for it again");
            self.inner = None;
        }
        let Some(monitor) = &self.monitor else { return };
        if self.inner.is_some() || Instant::now() < self.next_try {
            return;
        }
        if self.rebuilds >= MAX_REBUILDS {
            self.failed = Some(format!("the virtual monitor ({}) cannot be captured any more", monitor.describe()));
            return;
        }
        self.rebuilds += 1;
        self.next_try = Instant::now() + REBUILD_GAP;
        match monitor.locate() {
            Some(place) => match (self.open)(&place) {
                Ok(s) => {
                    log::info!("Capturing virtual monitor {} at {:?}", place.gdi_name, place.region);
                    self.inner = Some(s);
                }
                Err(e) => log::warn!("Could not capture virtual monitor {}: {e}", place.gdi_name),
            },
            None => log::debug!("Virtual monitor not on the desktop right now"),
        }
    }
}

impl RawSource for VirtualMonitorSource {
    fn next_raw(&mut self, timeout: Duration) -> Result<Option<RawFrame>, BoxError> {
        self.poll_ready();
        match &mut self.inner {
            Some(s) => {
                let frame = s.next_raw(timeout)?;
                if frame.is_some() {
                    self.rebuilds = 0;
                }
                Ok(frame)
            }
            None => {
                std::thread::sleep(timeout.min(Duration::from_millis(50)));
                Ok(None)
            }
        }
    }

    fn describe(&self) -> String {
        match (&self.inner, &self.monitor) {
            (Some(s), _) => s.describe(),
            (None, Some(m)) => m.describe(),
            (None, None) => format!("{} (being added)", self.label),
        }
    }

    fn failure(&self) -> Option<String> {
        self.failed.clone()
    }

    fn region(&self) -> Option<OutputRegion> {
        self.inner.as_ref().and_then(|s| s.region()).or_else(|| self.monitor.as_ref()?.locate().map(|p| p.region))
    }

    fn pending(&self) -> bool {
        self.failed.is_none() && self.inner.is_none()
    }
}

/// Copies `height` rows of `width` BGRA pixels out of a mapped texture whose
/// rows are `pitch` bytes apart into a tight buffer.
pub fn copy_rows(src: &[u8], pitch: usize, width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * pitch;
        match src.get(start..start + row) {
            Some(r) => out.extend_from_slice(r),
            None => out.resize(out.len() + row, 0), // short mapping: black rather than a panic
        }
    }
    out
}

pub struct WindowsCapturer {
    // Field order matters: the source must go before the virtual monitor it
    // captures.
    source: Box<dyn RawSource>,
    out_w: u32,
    out_h: u32,
    scaler: super::scale::Scaler,
    last: Option<Frame>,
    _virtual_monitor: Option<Box<dyn std::any::Any + Send>>,
}

impl WindowsCapturer {
    /// Wraps `source`; frames are fitted to `out_w` x `out_h`. `guard` is kept
    /// alive as long as the capturer (the IDD monitor).
    pub fn from_source(
        source: Box<dyn RawSource>,
        out_w: u32,
        out_h: u32,
        guard: Option<Box<dyn std::any::Any + Send>>,
    ) -> Self {
        Self {
            source,
            out_w: out_w.max(2),
            out_h: out_h.max(2),
            scaler: super::scale::Scaler::new(),
            last: None,
            _virtual_monitor: guard,
        }
    }

    fn fit(&mut self, raw: RawFrame) -> Frame {
        let ts = host_monotonic_us();
        if raw.width == self.out_w && raw.height == self.out_h {
            return Frame { width: raw.width, height: raw.height, format: PixelFormat::Bgra, data: raw.data, timestamp_us: ts };
        }
        let mut data = vec![0u8; self.out_w as usize * self.out_h as usize * 4];
        self.scaler.scale(&raw.data, raw.width, raw.height, raw.width as usize * 4, &mut data, self.out_w, self.out_h);
        Frame { width: self.out_w, height: self.out_h, format: PixelFormat::Bgra, data, timestamp_us: ts }
    }
}

impl ScreenCapturer for WindowsCapturer {
    fn capture_frame(&mut self) -> Result<Frame, BoxError> {
        if let Some(f) = self.next_frame(Duration::from_millis(100))? {
            return Ok(f);
        }
        // Nothing changed: the polling contract wants a frame, so re-stamp the last.
        match &self.last {
            Some(f) => Ok(Frame { timestamp_us: host_monotonic_us(), ..f.clone() }),
            None => Ok(Frame {
                width: self.out_w,
                height: self.out_h,
                format: PixelFormat::Bgra,
                data: vec![0; self.out_w as usize * self.out_h as usize * 4],
                timestamp_us: host_monotonic_us(),
            }),
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, BoxError> {
        if let Some(why) = self.source.failure() {
            return Err(why.into());
        }
        match self.source.next_raw(timeout)? {
            Some(raw) => {
                let frame = self.fit(raw);
                self.last = Some(frame.clone());
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }

    fn width(&self) -> u32 {
        self.out_w
    }

    fn height(&self) -> u32 {
        self.out_h
    }

    fn describe_source(&self) -> String {
        format!("{} -> {}x{}", self.source.describe(), self.out_w, self.out_h)
    }

    fn state(&self) -> CaptureState {
        match self.source.failure() {
            Some(why) => CaptureState::Failed(why),
            // A monitor being added is like a permission dialog: nothing to
            // show yet, nothing wrong either.
            None if self.source.pending() => CaptureState::AwaitingPermission,
            None => CaptureState::Live,
        }
    }

    fn output_region(&self) -> Option<OutputRegion> {
        self.source.region()
    }
}

/// True when the requested `output` names the device `\\.\DISPLAYn`. Accepts
/// `\\.\DISPLAY2`, `DISPLAY2` and a bare `2`, any case.
pub fn output_matches(want: &str, device_name: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.trim().to_ascii_lowercase();
        let s = s.trim_start_matches(r"\\.\");
        s.strip_prefix("display").unwrap_or(s).to_string()
    }
    let w = norm(want);
    !w.is_empty() && w == norm(device_name)
}

/// Which monitor to capture among `(device name, is primary)` pairs: the one
/// named `want`, else the primary one, else the first. The bool says whether
/// `want` was honoured (a Linux connector name such as `eDP-1` never is).
pub fn pick_monitor(monitors: &[(String, bool)], want: Option<&str>) -> Option<(usize, bool)> {
    let fallback = || monitors.iter().position(|m| m.1).or(if monitors.is_empty() { None } else { Some(0) });
    match want.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => match monitors.iter().position(|m| output_matches(w, &m.0)) {
            Some(i) => Some((i, true)),
            None => fallback().map(|i| (i, false)),
        },
        None => fallback().map(|i| (i, true)),
    }
}

/// How DXGI reports an output's rotation (`DXGI_MODE_ROTATION`): Desktop
/// Duplication hands out the picture un-rotated, so a rotated monitor has to be
/// turned upright here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    None,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Rotation {
    /// From the raw `DXGI_MODE_ROTATION` value (1 = unspecified/identity).
    pub fn from_dxgi(v: i32) -> Self {
        match v {
            2 => Rotation::Rotate90,
            3 => Rotation::Rotate180,
            4 => Rotation::Rotate270,
            _ => Rotation::None,
        }
    }
}

/// Turns a tight BGRA picture upright: the un-rotated texture is turned
/// `rotation` degrees counter-clockwise. A 90 or 270 turn swaps width and
/// height. UNVERIFIED: that `DXGI_MODE_ROTATION_ROTATE90` means counter-clockwise
/// (so 90 and 270 could be swapped) needs a rotated monitor to confirm; 180
/// and the size swap are certain.
pub fn unrotate_bgra(raw: RawFrame, rotation: Rotation) -> RawFrame {
    let (w, h) = (raw.width as usize, raw.height as usize);
    let (ow, oh) = match rotation {
        Rotation::None | Rotation::Rotate180 => (w, h),
        Rotation::Rotate90 | Rotation::Rotate270 => (h, w),
    };
    if rotation == Rotation::None || raw.data.len() < w * h * 4 {
        return raw;
    }
    let mut out = vec![0u8; ow * oh * 4];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match rotation {
                Rotation::Rotate90 => (y, w - 1 - x),
                Rotation::Rotate180 => (w - 1 - x, h - 1 - y),
                Rotation::Rotate270 => (h - 1 - y, x),
                Rotation::None => (x, y),
            };
            out[(ny * ow + nx) * 4..][..4].copy_from_slice(&raw.data[(y * w + x) * 4..][..4]);
        }
    }
    RawFrame { width: ow as u32, height: oh as u32, data: out }
}

/// A top-level window as far as choosing one goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    pub visible: bool,
    pub cloaked: bool,
    pub minimized: bool,
    pub tool_window: bool,
}

/// Which of `windows` (top of the Z-order first) to capture: the one whose
/// title contains `hint` (case-insensitive) or whose handle is `0x<hex>`, else
/// the first ordinary window that is not ours. Returns an index.
pub fn pick_window(windows: &[WindowInfo], hint: Option<&str>, own_pid: u32) -> Option<usize> {
    let usable = |w: &WindowInfo| w.visible && !w.cloaked && !w.minimized && !w.tool_window && !w.title.is_empty();
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    if let Some(h) = hint {
        if let Some(hex) = h.strip_prefix("0x").or_else(|| h.strip_prefix("0X")) {
            if let Ok(v) = isize::from_str_radix(hex, 16) {
                if let Some(i) = windows.iter().position(|w| w.hwnd == v) {
                    return Some(i);
                }
            }
        }
        let needle = h.to_lowercase();
        return windows.iter().position(|w| usable(w) && w.title.to_lowercase().contains(&needle));
    }
    windows.iter().position(|w| usable(w) && w.pid != own_pid && w.title != "Program Manager")
}

/// Shared by the sources: polls `try_get` until it yields or `timeout` passes.
pub fn poll_until<T>(timeout: Duration, mut try_get: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = try_get() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl WindowsCapturer {
    #[cfg(not(target_os = "windows"))]
    pub fn new_for(_target: &CaptureTarget, _width: u32, _height: u32) -> Result<Self, BoxError> {
        Err("Windows capture is only available on Windows".into())
    }

    /// Builds the capturer for `target`, fitted to `width` x `height`.
    ///
    /// `Virtual` asks the installed display driver for a monitor of the phone's
    /// mode ([`crate::display::windows_caps::virtual_monitor_provider`]) and
    /// returns at once; frames start when the monitor is on the desktop. No
    /// driver: an error naming what to install.
    #[cfg(target_os = "windows")]
    pub fn new_for(target: &CaptureTarget, width: u32, height: u32) -> Result<Self, BoxError> {
        use super::{windows_dxgi::DxgiSource, windows_wgc::WgcSource};
        sys::ensure_dpi_aware();
        let source: Box<dyn RawSource> = match target {
            CaptureTarget::Window => {
                let hint = std::env::var("DISPLAYSWARM_CAPTURE_WINDOW").ok();
                let win = sys::choose_window(hint.as_deref())?;
                Box::new(WgcSource::for_window(win)?)
            }
            CaptureTarget::Monitor { output } => {
                let mon = sys::find_monitor(output.as_deref())?;
                match WgcSource::for_monitor(&mon) {
                    Ok(s) => Box::new(s),
                    Err(e) => {
                        log::warn!("Windows.Graphics.Capture unavailable for {} ({e}); using DXGI duplication", mon.name);
                        Box::new(DxgiSource::new(&mon.name)?)
                    }
                }
            }
            CaptureTarget::Virtual { width: vw, height: vh, refresh_mhz } => {
                let provider = crate::display::windows_caps::virtual_monitor_provider()?;
                let mode = crate::display::model::Mode { width: *vw, height: *vh, refresh_mhz: *refresh_mhz };
                let label = format!(
                    "{} monitor {}",
                    provider.kind().label(),
                    crate::display::virtual_monitor::mode_label(mode)
                );
                Box::new(VirtualMonitorSource::start(
                    label,
                    move || provider.add(mode).map_err(|e| e.summary()),
                    |place: &MonitorPlace| {
                        let mon = sys::monitor_named(&place.gdi_name)
                            .ok_or_else(|| format!("{} is not on the desktop", place.gdi_name))?;
                        match WgcSource::for_monitor(&mon) {
                            Ok(s) => Ok(Box::new(s) as Box<dyn RawSource>),
                            Err(e) => {
                                log::warn!("Windows.Graphics.Capture unavailable for {} ({e}); using DXGI duplication", mon.name);
                                Ok(Box::new(DxgiSource::new(&mon.name)?) as Box<dyn RawSource>)
                            }
                        }
                    },
                ))
            }
        };
        log::info!("Windows capture started: {}", source.describe());
        Ok(Self::from_source(source, width, height, None))
    }
}

/// Win32 lookups shared by both sources.
#[cfg(target_os = "windows")]
pub mod sys {
    use super::{pick_monitor, pick_window, WindowInfo};
    use crate::display::OutputRegion;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    #[derive(Debug, Clone)]
    pub struct MonitorInfo {
        pub handle: HMONITOR,
        /// `\\.\DISPLAYn`
        pub name: String,
        /// Virtual-screen rectangle.
        pub rect: RECT,
        pub primary: bool,
    }

    impl MonitorInfo {
        pub fn region(&self) -> OutputRegion {
            OutputRegion {
                x: self.rect.left,
                y: self.rect.top,
                width: (self.rect.right - self.rect.left).max(0) as u32,
                height: (self.rect.bottom - self.rect.top).max(0) as u32,
            }
        }
    }

    pub fn monitors() -> Vec<MonitorInfo> {
        unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
            let out = &mut *(data.0 as *mut Vec<MonitorInfo>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(h, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
                let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
                out.push(MonitorInfo {
                    handle: h,
                    name: String::from_utf16_lossy(&info.szDevice[..end]),
                    rect: info.monitorInfo.rcMonitor,
                    primary: info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
                });
            }
            TRUE
        }
        let mut out: Vec<MonitorInfo> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
        }
        out
    }

    /// The monitor named exactly `name` (`\\.\DISPLAYn`), with no fallback.
    pub fn monitor_named(name: &str) -> Option<MonitorInfo> {
        monitors().into_iter().find(|m| m.name.eq_ignore_ascii_case(name))
    }

    /// The monitor named `want`, else the primary one.
    pub fn find_monitor(want: Option<&str>) -> Result<MonitorInfo, String> {
        let all = monitors();
        let keys: Vec<(String, bool)> = all.iter().map(|m| (m.name.clone(), m.primary)).collect();
        let (i, honoured) = pick_monitor(&keys, want).ok_or("no monitor is attached to this desktop")?;
        if !honoured {
            log::warn!("No monitor {:?} on this desktop; capturing {} instead", want, all[i].name);
        }
        Ok(all[i].clone())
    }

    /// Makes the process per-monitor DPI aware (once), so monitor rectangles,
    /// capture sizes and injected pointer positions all use physical pixels.
    /// Fails quietly when the awareness was already set (manifest, earlier call).
    pub fn ensure_dpi_aware() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
                log::debug!("SetProcessDpiAwarenessContext: {e} (already set?)");
            }
        });
    }

    pub fn windows_in_z_order() -> Vec<WindowInfo> {
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            let out = &mut *(data.0 as *mut Vec<WindowInfo>);
            let mut title = [0u16; 256];
            let n = GetWindowTextW(h, &mut title).max(0) as usize;
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(
                h,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            );
            out.push(WindowInfo {
                hwnd: h.0 as isize,
                title: String::from_utf16_lossy(&title[..n]),
                pid,
                visible: IsWindowVisible(h).as_bool(),
                cloaked: cloaked != 0,
                minimized: IsIconic(h).as_bool(),
                tool_window: GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0,
            });
            TRUE
        }
        let mut out: Vec<WindowInfo> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
        }
        out
    }

    pub fn choose_window(hint: Option<&str>) -> Result<HWND, String> {
        let all = windows_in_z_order();
        let own = unsafe { GetCurrentProcessId() };
        let i = pick_window(&all, hint, own)
            .ok_or_else(|| format!("no capturable window found{}", hint.map(|h| format!(" matching {h:?}")).unwrap_or_default()))?;
        log::info!("Capturing window {:?} (0x{:x})", all[i].title, all[i].hwnd);
        Ok(HWND(all[i].hwnd as *mut _))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake {
        frames: VecDeque<Option<RawFrame>>,
        failed: Option<String>,
    }

    impl RawSource for Fake {
        fn next_raw(&mut self, _t: Duration) -> Result<Option<RawFrame>, BoxError> {
            Ok(self.frames.pop_front().flatten())
        }
        fn describe(&self) -> String {
            "fake".into()
        }
        fn failure(&self) -> Option<String> {
            self.failed.clone()
        }
        fn region(&self) -> Option<OutputRegion> {
            Some(OutputRegion { x: 1920, y: 0, width: 4, height: 4 })
        }
    }

    fn solid(w: u32, h: u32, v: u8) -> RawFrame {
        RawFrame { width: w, height: h, data: vec![v; (w * h * 4) as usize] }
    }

    fn capturer(frames: Vec<Option<RawFrame>>, w: u32, h: u32) -> WindowsCapturer {
        WindowsCapturer::from_source(Box::new(Fake { frames: frames.into(), failed: None }), w, h, None)
    }

    #[test]
    fn same_size_frames_pass_through_bgra_with_increasing_timestamps() {
        let mut c = capturer(vec![Some(solid(4, 4, 7)), Some(solid(4, 4, 9))], 4, 4);
        let a = c.next_frame(Duration::ZERO).unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let b = c.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((a.width, a.height, a.format), (4, 4, PixelFormat::Bgra));
        assert_eq!(a.data[0], 7);
        assert_eq!(b.data[0], 9);
        assert!(b.timestamp_us > a.timestamp_us);
        assert_eq!(c.output_region(), Some(OutputRegion { x: 1920, y: 0, width: 4, height: 4 }));
    }

    #[test]
    fn other_sizes_are_fitted_to_the_encoder_size() {
        let mut c = capturer(vec![Some(solid(16, 8, 200))], 8, 8);
        let f = c.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height), (8, 8));
        assert_eq!(f.data.len(), 8 * 8 * 4);
        // The 16x8 picture is letterboxed: 8x4 of colour between black bars.
        assert_eq!(&f.data[(4 * 8 + 4) * 4..][..3], &[200, 200, 200]);
        assert_eq!(&f.data[..3], &[0, 0, 0]);
    }

    #[test]
    fn no_new_frame_is_none_but_polling_still_gets_the_last_picture() {
        let mut c = capturer(vec![Some(solid(4, 4, 5)), None], 4, 4);
        assert!(c.next_frame(Duration::ZERO).unwrap().is_some());
        assert!(c.next_frame(Duration::ZERO).unwrap().is_none());
        let again = c.capture_frame().unwrap();
        assert_eq!(again.data[0], 5);
    }

    #[test]
    fn a_dead_source_is_an_error_and_a_failed_state() {
        let mut c = WindowsCapturer::from_source(
            Box::new(Fake { frames: VecDeque::new(), failed: Some("window closed".into()) }),
            4,
            4,
            None,
        );
        assert!(c.next_frame(Duration::ZERO).is_err());
        assert_eq!(c.state(), CaptureState::Failed("window closed".into()));
    }

    #[test]
    fn copy_rows_drops_row_padding() {
        // 2 pixels wide, 3 rows, pitch 12 (4 bytes of padding per row).
        let mut src = Vec::new();
        for y in 0..3u8 {
            src.extend_from_slice(&[y; 8]);
            src.extend_from_slice(&[0xEE; 4]);
        }
        let out = copy_rows(&src, 12, 2, 3);
        assert_eq!(out.len(), 24);
        assert_eq!(&out[8..16], &[1u8; 8]);
        assert!(!out.contains(&0xEE));
        // A short mapping yields black, not a panic.
        assert_eq!(copy_rows(&src[..10], 12, 2, 3).len(), 24);
    }

    #[test]
    fn output_names_match_loosely() {
        assert!(output_matches(r"\\.\DISPLAY2", r"\\.\DISPLAY2"));
        assert!(output_matches("display2", r"\\.\DISPLAY2"));
        assert!(output_matches("2", r"\\.\DISPLAY2"));
        assert!(!output_matches("3", r"\\.\DISPLAY2"));
        assert!(!output_matches("", r"\\.\DISPLAY2"));
    }

    #[test]
    fn monitor_choice_prefers_the_name_then_the_primary() {
        let m = |n: &str, p: bool| (n.to_string(), p);
        let two = vec![m(r"\\.\DISPLAY1", false), m(r"\\.\DISPLAY2", true)];
        assert_eq!(pick_monitor(&two, Some("1")), Some((0, true)));
        assert_eq!(pick_monitor(&two, None), Some((1, true)));
        assert_eq!(pick_monitor(&two, Some("  ")), Some((1, true)));
        // A Linux connector name matches nothing: the primary, flagged as not honoured.
        assert_eq!(pick_monitor(&two, Some("eDP-1")), Some((1, false)));
        assert_eq!(pick_monitor(&[m("a", false), m("b", false)], None), Some((0, true)));
        assert_eq!(pick_monitor(&[], Some("1")), None);
    }

    fn numbered(w: u32, h: u32) -> RawFrame {
        // Pixel i carries the value i in its first byte.
        let mut data = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            data[i * 4] = i as u8;
        }
        RawFrame { width: w, height: h, data }
    }

    fn firsts(f: &RawFrame) -> Vec<u8> {
        f.data.chunks(4).map(|p| p[0]).collect()
    }

    #[test]
    fn unrotating_turns_the_picture_and_swaps_the_size() {
        // 3x2:  0 1 2
        //       3 4 5
        assert_eq!(firsts(&unrotate_bgra(numbered(3, 2), Rotation::None)), [0, 1, 2, 3, 4, 5]);
        let r180 = unrotate_bgra(numbered(3, 2), Rotation::Rotate180);
        assert_eq!((r180.width, r180.height), (3, 2));
        assert_eq!(firsts(&r180), [5, 4, 3, 2, 1, 0]);
        // Counter-clockwise: the right column becomes the top row.
        let r90 = unrotate_bgra(numbered(3, 2), Rotation::Rotate90);
        assert_eq!((r90.width, r90.height), (2, 3));
        assert_eq!(firsts(&r90), [2, 5, 1, 4, 0, 3]);
        let r270 = unrotate_bgra(numbered(3, 2), Rotation::Rotate270);
        assert_eq!((r270.width, r270.height), (2, 3));
        assert_eq!(firsts(&r270), [3, 0, 4, 1, 5, 2]);
        // Four quarter turns are the identity; a short buffer is passed through.
        let mut f = numbered(3, 2);
        for _ in 0..4 {
            f = unrotate_bgra(f, Rotation::Rotate90);
        }
        assert_eq!(firsts(&f), [0, 1, 2, 3, 4, 5]);
        let short = unrotate_bgra(RawFrame { width: 3, height: 2, data: vec![0; 8] }, Rotation::Rotate90);
        assert_eq!((short.width, short.height), (3, 2));
        assert_eq!(Rotation::from_dxgi(1), Rotation::None);
        assert_eq!(Rotation::from_dxgi(4), Rotation::Rotate270);
    }

    struct FakeMonitor {
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl VirtualMonitor for FakeMonitor {
        fn locate(&self) -> Option<MonitorPlace> {
            Some(MonitorPlace { gdi_name: r"\\.\DISPLAY7".into(), region: OutputRegion { x: 1920, y: 0, width: 4, height: 4 } })
        }
        fn describe(&self) -> String {
            "fake monitor".into()
        }
    }

    impl Drop for FakeMonitor {
        fn drop(&mut self) {
            self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Polls `src` until it yields a frame or `tries` run out.
    fn first_frame(src: &mut VirtualMonitorSource, tries: usize) -> Option<RawFrame> {
        for _ in 0..tries {
            if let Some(f) = src.next_raw(Duration::from_millis(5)).unwrap() {
                return Some(f);
            }
            std::thread::sleep(Duration::from_millis(REBUILD_GAP.as_millis() as u64 / 5));
        }
        None
    }

    #[test]
    fn a_virtual_monitor_is_added_off_thread_then_captured_and_given_back_on_drop() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dropped.clone();
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let o = opened.clone();
        let src = VirtualMonitorSource::start(
            "fake".into(),
            move || {
                std::thread::sleep(Duration::from_millis(30));
                Ok(Box::new(FakeMonitor { dropped: d }) as Box<dyn VirtualMonitor>)
            },
            move |place: &MonitorPlace| {
                o.lock().unwrap().push(place.gdi_name.clone());
                Ok(Box::new(Fake { frames: vec![Some(solid(4, 4, 3))].into(), failed: None }) as Box<dyn RawSource>)
            },
        );
        assert!(src.pending(), "nothing to show while the monitor is being added");
        assert!(src.describe().contains("being added"));
        let mut cap = WindowsCapturer::from_source(Box::new(src), 4, 4, None);
        assert_eq!(cap.state(), CaptureState::AwaitingPermission);
        let mut got = None;
        for _ in 0..100 {
            if let Some(f) = cap.next_frame(Duration::from_millis(5)).unwrap() {
                got = Some(f);
                break;
            }
        }
        assert_eq!(got.unwrap().data[0], 3);
        assert_eq!(cap.state(), CaptureState::Live);
        assert_eq!(opened.lock().unwrap().as_slice(), [r"\\.\DISPLAY7".to_string()]);
        assert_eq!(cap.output_region(), Some(OutputRegion { x: 1920, y: 0, width: 4, height: 4 }));
        drop(cap);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the monitor is given back");
    }

    #[test]
    fn a_failed_add_is_a_failed_capture() {
        let mut src = VirtualMonitorSource::start(
            "fake".into(),
            || Err("no driver".to_string()),
            |_: &MonitorPlace| Err("never".to_string()),
        );
        for _ in 0..100 {
            let _ = src.next_raw(Duration::from_millis(2));
            if src.failure().is_some() {
                break;
            }
        }
        assert_eq!(src.failure().as_deref(), Some("no driver"));
        assert!(!src.pending());
    }

    #[test]
    fn a_capture_killed_by_a_driver_reload_is_rebuilt() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dropped.clone();
        let opens = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let n = opens.clone();
        let mut src = VirtualMonitorSource::start(
            "fake".into(),
            move || Ok(Box::new(FakeMonitor { dropped: d }) as Box<dyn VirtualMonitor>),
            move |_: &MonitorPlace| {
                // The first capture dies (the monitor was re-created); the second works.
                let first = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                Ok(Box::new(Fake {
                    frames: if first { Default::default() } else { vec![Some(solid(4, 4, 9))].into() },
                    failed: first.then(|| "monitor re-created".to_string()),
                }) as Box<dyn RawSource>)
            },
        );
        let f = first_frame(&mut src, 50).expect("a frame after the rebuild");
        assert_eq!(f.data[0], 9);
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(src.failure().is_none());
    }

    fn win(hwnd: isize, title: &str, pid: u32) -> WindowInfo {
        WindowInfo {
            hwnd,
            title: title.into(),
            pid,
            visible: true,
            cloaked: false,
            minimized: false,
            tool_window: false,
        }
    }

    #[test]
    fn window_choice_prefers_the_hint_then_the_top_foreign_window() {
        let mut ws = vec![win(1, "DisplaySwarm", 10), win(2, "Docs - Chrome", 20), win(3, "Krita", 30)];
        ws.insert(0, WindowInfo { tool_window: true, ..win(9, "Tooltip", 40) });
        assert_eq!(pick_window(&ws, None, 10), Some(2), "skips tool windows and ourselves");
        assert_eq!(pick_window(&ws, Some("krita"), 10), Some(3));
        assert_eq!(pick_window(&ws, Some("0x3"), 10), Some(3));
        assert_eq!(pick_window(&ws, Some("nothing"), 10), None);
        ws[2].minimized = true;
        assert_eq!(pick_window(&ws, Some("chrome"), 10), None, "minimized windows cannot be captured");
        assert_eq!(pick_window(&ws, None, 10), Some(3));
    }
}
