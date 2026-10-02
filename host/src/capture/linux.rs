use super::scale::Scaler;
use super::{
    mock::MockCapturer, native, portal, CaptureState, CaptureTarget, Frame, PixelFormat,
    ScreenCapturer,
};
use crate::protocol::host_monotonic_us;
use crossbeam_channel as cb;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum spacing between frames for the polling backends (X11, xcap, mock).
///
/// Those have no "new frame" event to block on, so `next_frame` paces them
/// itself; without this a `next_frame(100 ms)` loop would spin the CPU and
/// encode as fast as the grab can run.
const POLLING_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Sleeps out whatever is left of [`POLLING_FRAME_INTERVAL`] since `last`, but
/// never longer than `timeout`.
fn pace(last: &mut Option<Instant>, timeout: Duration) {
    if let Some(prev) = *last {
        let remaining = POLLING_FRAME_INTERVAL.saturating_sub(prev.elapsed());
        std::thread::sleep(remaining.min(timeout));
    }
    *last = Some(Instant::now());
}

// ============================================================================
// X11 and XShm C FFI Declarations & Dynamic Loader
// ============================================================================

const ZPIXMAP: c_int = 2;
const ALL_PLANES: c_ulong = !0;

#[repr(C)]
struct XImage {
    width: c_int,
    height: c_int,
    xoffset: c_int,
    format: c_int,
    data: *mut c_char,
    byte_order: c_int,
    bitmap_unit: c_int,
    bitmap_bit_order: c_int,
    bitmap_pad: c_int,
    depth: c_int,
    bytes_per_line: c_int,
    bits_per_pixel: c_int,
    red_mask: c_ulong,
    green_mask: c_ulong,
    blue_mask: c_ulong,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XShmSegmentInfo {
    shmseg: c_ulong,
    shmid: c_int,
    shmaddr: *mut c_char,
    read_only: c_int,
}

/// `XFixesCursorImage` (X11/extensions/Xfixes.h). `pixels` are premultiplied
/// ARGB, one 32-bit value per `c_ulong`.
#[repr(C)]
struct XFixesCursorImage {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    cursor_serial: c_ulong,
    pixels: *mut c_ulong,
    atom: c_ulong,
    name: *const c_char,
}

struct X11Symbols {
    _lib_x11: *mut c_void,
    _lib_xext: *mut c_void,
    /// libXfixes, for the pointer image: XGetImage/XShm never include it.
    _lib_xfixes: *mut c_void,
    xfixes_cursor_image: Option<unsafe extern "C" fn(*mut c_void) -> *mut XFixesCursorImage>,
    xfree: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,

    open_display: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    close_display: unsafe extern "C" fn(*mut c_void) -> c_int,
    default_root_window: unsafe extern "C" fn(*mut c_void) -> c_ulong,
    default_screen: unsafe extern "C" fn(*mut c_void) -> c_int,
    display_width: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    display_height: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    default_visual: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
    default_depth: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    get_image: unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int, c_uint, c_uint, c_ulong, c_int) -> *mut XImage,
    destroy_image: unsafe extern "C" fn(*mut XImage) -> c_int,
    sync: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    set_error_handler: Option<unsafe extern "C" fn(Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>) -> Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>>,

    shm_query_extension: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    shm_create_image: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, c_uint, c_int, *mut c_char, *mut XShmSegmentInfo, c_uint, c_uint) -> *mut XImage>,
    shm_attach: Option<unsafe extern "C" fn(*mut c_void, *mut XShmSegmentInfo) -> c_int>,
    shm_detach: Option<unsafe extern "C" fn(*mut c_void, *mut XShmSegmentInfo) -> c_int>,
    shm_get_image: Option<unsafe extern "C" fn(*mut c_void, c_ulong, *mut XImage, c_int, c_int, c_ulong) -> c_int>,
}

unsafe impl Send for X11Symbols {}
unsafe impl Sync for X11Symbols {}

impl X11Symbols {
    fn load() -> Option<Self> {
        unsafe {
            // Try loading libX11.so.6 or libX11.so
            let x11_names = [b"libX11.so.6\0".as_ptr(), b"libX11.so\0".as_ptr()];
            let mut lib_x11 = std::ptr::null_mut();
            for name in x11_names {
                lib_x11 = libc::dlopen(name as *const c_char, libc::RTLD_LAZY);
                if !lib_x11.is_null() {
                    break;
                }
            }
            if lib_x11.is_null() {
                log::debug!("Failed to dlopen libX11.so");
                return None;
            }

            macro_w_sym!(lib_x11, open_display, b"XOpenDisplay\0");
            macro_w_sym!(lib_x11, close_display, b"XCloseDisplay\0");
            macro_w_sym!(lib_x11, default_root_window, b"XDefaultRootWindow\0");
            macro_w_sym!(lib_x11, default_screen, b"XDefaultScreen\0");
            macro_w_sym!(lib_x11, display_width, b"XDisplayWidth\0");
            macro_w_sym!(lib_x11, display_height, b"XDisplayHeight\0");
            macro_w_sym!(lib_x11, default_visual, b"XDefaultVisual\0");
            macro_w_sym!(lib_x11, default_depth, b"XDefaultDepth\0");
            macro_w_sym!(lib_x11, get_image, b"XGetImage\0");
            macro_w_sym!(lib_x11, destroy_image, b"XDestroyImage\0");
            macro_w_sym!(lib_x11, sync, b"XSync\0");

            let set_error_handler: Option<unsafe extern "C" fn(Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>) -> Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>> = {
                let sym_ptr = libc::dlsym(lib_x11, b"XSetErrorHandler\0".as_ptr() as *const c_char);
                if !sym_ptr.is_null() {
                    Some(std::mem::transmute(sym_ptr))
                } else {
                    None
                }
            };

            // Try loading libXext.so.6 or libXext.so for MIT-SHM
            let xext_names = [b"libXext.so.6\0".as_ptr(), b"libXext.so\0".as_ptr()];
            let mut lib_xext = std::ptr::null_mut();
            for name in xext_names {
                lib_xext = libc::dlopen(name as *const c_char, libc::RTLD_LAZY);
                if !lib_xext.is_null() {
                    break;
                }
            }

            let (shm_query, shm_create, shm_attach, shm_detach, shm_get) = if !lib_xext.is_null() {
                let q: Option<unsafe extern "C" fn(*mut c_void) -> c_int> =
                    std::mem::transmute(libc::dlsym(lib_xext, b"XShmQueryExtension\0".as_ptr() as *const c_char));
                let c: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, c_uint, c_int, *mut c_char, *mut XShmSegmentInfo, c_uint, c_uint) -> *mut XImage> =
                    std::mem::transmute(libc::dlsym(lib_xext, b"XShmCreateImage\0".as_ptr() as *const c_char));
                let a: Option<unsafe extern "C" fn(*mut c_void, *mut XShmSegmentInfo) -> c_int> =
                    std::mem::transmute(libc::dlsym(lib_xext, b"XShmAttach\0".as_ptr() as *const c_char));
                let d: Option<unsafe extern "C" fn(*mut c_void, *mut XShmSegmentInfo) -> c_int> =
                    std::mem::transmute(libc::dlsym(lib_xext, b"XShmDetach\0".as_ptr() as *const c_char));
                let g: Option<unsafe extern "C" fn(*mut c_void, c_ulong, *mut XImage, c_int, c_int, c_ulong) -> c_int> =
                    std::mem::transmute(libc::dlsym(lib_xext, b"XShmGetImage\0".as_ptr() as *const c_char));
                (q, c, a, d, g)
            } else {
                (None, None, None, None, None)
            };

            let lib_xfixes = libc::dlopen(b"libXfixes.so.3\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
            let (xfixes_cursor_image, xfree) = if lib_xfixes.is_null() {
                (None, None)
            } else {
                (
                    std::mem::transmute(libc::dlsym(lib_xfixes, b"XFixesGetCursorImage\0".as_ptr() as *const c_char)),
                    std::mem::transmute(libc::dlsym(lib_x11, b"XFree\0".as_ptr() as *const c_char)),
                )
            };

            Some(Self {
                _lib_x11: lib_x11,
                _lib_xext: lib_xext,
                _lib_xfixes: lib_xfixes,
                xfixes_cursor_image,
                xfree,
                open_display,
                close_display,
                default_root_window,
                default_screen,
                display_width,
                display_height,
                default_visual,
                default_depth,
                get_image,
                destroy_image,
                sync,
                set_error_handler,
                shm_query_extension: shm_query,
                shm_create_image: shm_create,
                shm_attach: shm_attach,
                shm_detach: shm_detach,
                shm_get_image: shm_get,
            })
        }
    }
}

macro_rules! macro_w_sym {
    ($lib:expr, $field:ident, $sym:expr) => {
        let sym_ptr = libc::dlsym($lib, $sym.as_ptr() as *const c_char);
        if sym_ptr.is_null() {
            log::debug!("Failed to find symbol in X11");
            return None;
        }
        let $field = std::mem::transmute(sym_ptr);
    };
}
use macro_w_sym;

impl Drop for X11Symbols {
    fn drop(&mut self) {
        unsafe {
            if !self._lib_xfixes.is_null() {
                libc::dlclose(self._lib_xfixes);
            }
            if !self._lib_xext.is_null() {
                libc::dlclose(self._lib_xext);
            }
            if !self._lib_x11.is_null() {
                libc::dlclose(self._lib_x11);
            }
        }
    }
}

// ============================================================================
// X11 Screen Capturer (XShm with XGetImage fallback)
// ============================================================================

// Silent error handler to suppress non-fatal X11 errors (e.g. BadShmSeg under XWayland)
unsafe extern "C" fn x11_silent_error_handler(_display: *mut c_void, _event: *mut c_void) -> c_int {
    0
}

pub struct X11Capturer {
    symbols: Arc<X11Symbols>,
    display: *mut c_void,
    root_window: c_ulong,
    screen_width: u32,
    screen_height: u32,
    visual: *mut c_void,
    depth: c_uint,
    /// The rectangle grabbed, in root-window pixels: one monitor, or the whole
    /// root when RandR could not name one.
    rect: crate::display::OutputRegion,
    /// The RandR output `rect` follows (the user can move or resize it), and
    /// the connection it is re-read on.
    follow: Option<(String, x11rb::rust_connection::RustConnection, u32)>,
    last_follow: Instant,
    warned_gone: bool,
    target_width: u32,
    target_height: u32,
    pixel_format: PixelFormat,
    scaler: Scaler,

    // XShm State (if available)
    shm_image: *mut XImage,
    /// Boxed: `XShmCreateImage` keeps a pointer to it in the image (`obdata`),
    /// and `XShmGetImage` reads the segment id through that pointer.
    shminfo: Option<Box<XShmSegmentInfo>>,
    is_shm: bool,

    /// The virtual monitor this capture shows (Extend on X11). Declared last so
    /// it is torn down after the display connection is closed.
    virtual_output: Option<crate::display::x11_virtual::VirtualOutput>,
}

unsafe impl Send for X11Capturer {}

/// How often a followed output's rectangle is re-read.
const X11_FOLLOW_EVERY: Duration = Duration::from_millis(500);

impl X11Capturer {
    /// The whole root window (every monitor), scaled to the target size.
    pub fn new(target_width: u32, target_height: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::open(target_width, target_height, None)
    }

    /// One monitor, found through RandR: `output` when it names a lit one, else
    /// the primary. Falls back to the whole root window when RandR cannot be
    /// read.
    pub fn for_output(
        target_width: u32,
        target_height: u32,
        output: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use x11rb::connection::Connection as _;
        let conn = x11rb::connect(None).ok().and_then(|(c, screen)| {
            let root = c.setup().roots.get(screen)?.root;
            Some((c, root))
        });
        let picked = conn.as_ref().and_then(|(c, root)| {
            let state = crate::display::randr::query_on(c, *root).ok()?;
            crate::display::randr::pick_capture_output(&state, output)
        });
        match (picked, conn) {
            (Some((name, rect)), Some((c, root))) => {
                log::info!("X11 capture: output {name} at {rect:?}");
                Self::open(target_width, target_height, Some((rect, (name, c, root))))
            }
            _ => {
                log::warn!("X11 capture: no RandR output to follow; capturing the whole screen");
                Self::open(target_width, target_height, None)
            }
        }
    }

    /// Lights a virtual monitor of `width`x`height` on a spare RandR output and
    /// captures it; both go away together.
    pub fn for_virtual(
        target_width: u32,
        target_height: u32,
        device_id: &str,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let v = crate::display::x11_virtual::VirtualOutput::create(device_id, width, height, refresh_mhz)
            .map_err(|e| e.summary())?;
        let name = v.output().to_string();
        // On error `v` drops here and the monitor is torn down again.
        let mut cap = Self::for_output(target_width, target_height, Some(&name))?;
        if cap.follow.as_ref().map(|f| f.0.as_str()) != Some(name.as_str()) {
            return Err(format!("the virtual monitor {name} is not in the RandR layout").into());
        }
        cap.virtual_output = Some(v);
        Ok(cap)
    }

    fn open(
        target_width: u32,
        target_height: u32,
        follow: Option<(crate::display::OutputRegion, (String, x11rb::rust_connection::RustConnection, u32))>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let symbols = Arc::new(X11Symbols::load().ok_or("Could not load libX11 / libXext symbols")?);

        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        let c_display_env = CString::new(display_env)?;

        let display = unsafe { (symbols.open_display)(c_display_env.as_ptr()) };
        if display.is_null() {
            return Err("Failed to open X11 Display".into());
        }

        let screen = unsafe { (symbols.default_screen)(display) };
        let root = unsafe { (symbols.default_root_window)(display) };
        let screen_width = unsafe { (symbols.display_width)(display, screen) } as u32;
        let screen_height = unsafe { (symbols.display_height)(display, screen) } as u32;
        let visual = unsafe { (symbols.default_visual)(display, screen) };
        let depth = unsafe { (symbols.default_depth)(display, screen) } as c_uint;

        log::info!(
            "X11 Display connected: Screen resolution {}x{}, Depth: {}, Target: {}x{}",
            screen_width, screen_height, depth, target_width, target_height
        );

        // Install error handler to intercept BadShmSeg or any X protocol errors without terminating
        if let Some(set_err) = symbols.set_error_handler {
            unsafe {
                set_err(Some(x11_silent_error_handler));
            }
        }

        let full = crate::display::OutputRegion { x: 0, y: 0, width: screen_width, height: screen_height };
        let (rect, follow) = match follow {
            Some((rect, f)) => (rect, Some(f)),
            None => (full, None),
        };

        // 1-pixel probe to verify root window pixmap capture is permitted by X server (e.g. not blocked by XWayland)
        let probe_img = unsafe {
            (symbols.get_image)(
                display,
                root,
                rect.x,
                rect.y,
                1,
                1,
                ALL_PLANES,
                ZPIXMAP,
            )
        };
        if probe_img.is_null() {
            unsafe {
                (symbols.close_display)(display);
            }
            return Err("X11 root window capture probe returned NULL (capture blocked or unsupported)".into());
        }
        unsafe {
            (symbols.destroy_image)(probe_img);
        }

        let mut cap = Self {
            symbols,
            display,
            root_window: root,
            screen_width,
            screen_height,
            visual,
            depth,
            rect,
            follow,
            last_follow: Instant::now(),
            warned_gone: false,
            target_width,
            target_height,
            pixel_format: PixelFormat::Bgra,
            scaler: Scaler::new(),
            shm_image: std::ptr::null_mut(),
            shminfo: None,
            is_shm: false,
            virtual_output: None,
        };
        cap.attach_shm();
        if !cap.is_shm {
            log::warn!("MIT-SHM unavailable or failed; falling back to XGetImage software capture");
        }
        Ok(cap)
    }

    /// Sets up an MIT-SHM image of `rect`'s size. Leaves `is_shm` false when the
    /// extension is missing or any step fails.
    fn attach_shm(&mut self) {
        let symbols = self.symbols.clone();
        let display = self.display;
        let (width, height) = (self.rect.width, self.rect.height);
        let (Some(query), Some(create), Some(attach), Some(_detach), Some(_get)) = (
            symbols.shm_query_extension,
            symbols.shm_create_image,
            symbols.shm_attach,
            symbols.shm_detach,
            symbols.shm_get_image,
        ) else {
            return;
        };
        if unsafe { query(display) } == 0 {
            return;
        }
        let mut shminfo = Box::new(XShmSegmentInfo {
            shmseg: 0,
            shmid: -1,
            shmaddr: std::ptr::null_mut(),
            read_only: 0,
        });

        let img = unsafe {
            create(
                display,
                self.visual,
                self.depth,
                ZPIXMAP,
                std::ptr::null_mut(),
                &mut *shminfo,
                width as c_uint,
                height as c_uint,
            )
        };
        if img.is_null() {
            return;
        }
        let total_bytes = unsafe { (*img).bytes_per_line as usize * height as usize };
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, total_bytes, libc::IPC_CREAT | 0o777) };
        if shmid < 0 {
            unsafe { (symbols.destroy_image)(img); }
            return;
        }
        let shmaddr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if shmaddr == (-1isize as *mut c_void) {
            unsafe {
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                (symbols.destroy_image)(img);
            }
            return;
        }
        shminfo.shmid = shmid;
        shminfo.shmaddr = shmaddr as *mut c_char;
        unsafe { (*img).data = shminfo.shmaddr; }

        // Attach shm segment to X server
        if unsafe { attach(display, &mut *shminfo) } != 0 {
            unsafe {
                // Mark for auto-destroy when detached
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                (symbols.sync)(display, 0);
            }
            log::info!("X11 MIT-SHM shared memory capture successfully initialized (zero-copy, {width}x{height})");
            self.is_shm = true;
            self.shm_image = img;
            self.shminfo = Some(shminfo);
        } else {
            unsafe {
                libc::shmdt(shmaddr);
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                (symbols.destroy_image)(img);
            }
        }
    }

    fn detach_shm(&mut self) {
        unsafe {
            if let (Some(detach_fn), Some(shminfo)) = (self.symbols.shm_detach, self.shminfo.take()) {
                let mut info = shminfo;
                detach_fn(self.display, &mut *info);
                (self.symbols.sync)(self.display, 0);
                libc::shmdt(info.shmaddr as *const c_void);
            }
            if !self.shm_image.is_null() {
                // The data pointer is the (now detached) segment; XDestroyImage
                // must not free() it.
                (*self.shm_image).data = std::ptr::null_mut();
                (self.symbols.destroy_image)(self.shm_image);
                self.shm_image = std::ptr::null_mut();
            }
        }
        self.is_shm = false;
    }

    /// Re-reads the followed output's rectangle now and then; a new size
    /// rebuilds the shared-memory image.
    fn refresh_rect(&mut self) {
        if self.last_follow.elapsed() < X11_FOLLOW_EVERY {
            return;
        }
        self.last_follow = Instant::now();
        let Some((name, conn, root)) = &self.follow else { return };
        let Ok(state) = crate::display::randr::query_on(conn, *root) else { return };
        let Some(now) = state.by_name(name).and_then(|o| o.region) else {
            if !self.warned_gone {
                self.warned_gone = true;
                log::warn!("X11 capture: {name} is off or gone; keeping its last rectangle");
            }
            return;
        };
        self.warned_gone = false;
        if now == self.rect {
            return;
        }
        log::info!("X11 capture: {name} moved from {:?} to {now:?}", self.rect);
        let resized = (now.width, now.height) != (self.rect.width, self.rect.height);
        self.rect = now;
        if resized {
            let had_shm = self.is_shm;
            self.detach_shm();
            if had_shm {
                self.attach_shm();
            }
        }
    }

    /// The followed output's rectangle, if any.
    pub fn output_region(&self) -> Option<crate::display::OutputRegion> {
        self.follow.as_ref().map(|_| self.rect)
    }

    pub fn describe(&self) -> String {
        match &self.follow {
            Some((name, _, _)) => format!(
                "X11 output {name} {}x{}+{}+{}{}",
                self.rect.width,
                self.rect.height,
                self.rect.x,
                self.rect.y,
                if self.virtual_output.is_some() { " (virtual monitor)" } else { "" }
            ),
            None => format!("X11 root window {}x{}", self.screen_width, self.screen_height),
        }
    }

    pub fn capture_raw_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        self.refresh_rect();
        // Stamped before the grab: it is the closest we get to "capture time".
        let now_us = host_monotonic_us();

        let total_pixels = (self.target_width * self.target_height) as usize;
        let mut out_buffer = vec![0u8; total_pixels * 4];
        let rect = self.rect;

        let mut captured = false;
        if self.is_shm {
            if let Some(get_fn) = self.symbols.shm_get_image {
                let success = unsafe {
                    get_fn(
                        self.display,
                        self.root_window,
                        self.shm_image,
                        rect.x,
                        rect.y,
                        ALL_PLANES,
                    )
                };
                if success != 0 {
                    let img = unsafe { &*self.shm_image };
                    let raw_src = unsafe {
                        std::slice::from_raw_parts(
                            img.data as *const u8,
                            (img.bytes_per_line as usize) * (rect.height as usize),
                        )
                    };

                    self.scaler.scale(
                        raw_src,
                        rect.width,
                        rect.height,
                        img.bytes_per_line as usize,
                        &mut out_buffer,
                        self.target_width,
                        self.target_height,
                    );
                    captured = true;
                } else {
                    log::warn!("XShmGetImage failed; disabling MIT-SHM and falling back to XGetImage");
                    self.detach_shm();
                }
            }
        }

        if !captured {
            // XGetImage fallback
            let ximage = unsafe {
                (self.symbols.get_image)(
                    self.display,
                    self.root_window,
                    rect.x,
                    rect.y,
                    rect.width as c_uint,
                    rect.height as c_uint,
                    ALL_PLANES,
                    ZPIXMAP,
                )
            };

            if ximage.is_null() {
                return Err("XGetImage failed to acquire root window pixmap".into());
            }

            let img = unsafe { &*ximage };
            let raw_src = unsafe {
                std::slice::from_raw_parts(
                    img.data as *const u8,
                    (img.bytes_per_line as usize) * (rect.height as usize),
                )
            };

            self.scaler.scale(
                raw_src,
                rect.width,
                rect.height,
                img.bytes_per_line as usize,
                &mut out_buffer,
                self.target_width,
                self.target_height,
            );

            unsafe {
                (self.symbols.destroy_image)(ximage);
            }
        }

        self.draw_cursor(&mut out_buffer);

        Ok(Frame {
            width: self.target_width,
            height: self.target_height,
            format: self.pixel_format,
            data: out_buffer,
            timestamp_us: now_us,
        })
    }

    /// Blends the pointer into the scaled BGRA frame where it is on `rect`.
    /// Does nothing without libXfixes or when the pointer is off this monitor.
    fn draw_cursor(&self, out: &mut [u8]) {
        let (Some(get), Some(free)) = (self.symbols.xfixes_cursor_image, self.symbols.xfree) else { return };
        let ci = unsafe { get(self.display) };
        if ci.is_null() {
            return;
        }
        let c = unsafe { &*ci };
        let rect = self.rect;
        let (tw, th) = (self.target_width as i64, self.target_height as i64);
        let sx = tw as f64 / rect.width.max(1) as f64;
        let sy = th as f64 / rect.height.max(1) as f64;
        // Top-left of the image on the monitor, in source pixels.
        let left = c.x as i64 - c.xhot as i64 - rect.x as i64;
        let top = c.y as i64 - c.yhot as i64 - rect.y as i64;
        let (cw, ch) = (c.width as i64, c.height as i64);
        if !c.pixels.is_null() && cw > 0 && ch > 0 {
            let dw = ((cw as f64 * sx).round() as i64).max(1);
            let dh = ((ch as f64 * sy).round() as i64).max(1);
            let dx0 = (left as f64 * sx).round() as i64;
            let dy0 = (top as f64 * sy).round() as i64;
            for dy in 0..dh {
                let y = dy0 + dy;
                if y < 0 || y >= th {
                    continue;
                }
                let srow = (dy * ch / dh).min(ch - 1);
                for dx in 0..dw {
                    let x = dx0 + dx;
                    if x < 0 || x >= tw {
                        continue;
                    }
                    let scol = (dx * cw / dw).min(cw - 1);
                    let px = unsafe { *c.pixels.add((srow * cw + scol) as usize) } as u32;
                    let a = (px >> 24) & 0xff;
                    if a == 0 {
                        continue;
                    }
                    let o = ((y * tw + x) * 4) as usize;
                    if o + 3 >= out.len() {
                        continue;
                    }
                    // Premultiplied ARGB over BGRA.
                    let inv = 255 - a;
                    let chan = |src: u32, dst: u8| (src + (dst as u32 * inv) / 255).min(255) as u8;
                    out[o] = chan(px & 0xff, out[o]);
                    out[o + 1] = chan((px >> 8) & 0xff, out[o + 1]);
                    out[o + 2] = chan((px >> 16) & 0xff, out[o + 2]);
                    out[o + 3] = 255;
                }
            }
        }
        unsafe { free(ci as *mut c_void) };
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        self.detach_shm();
        unsafe {
            if !self.display.is_null() {
                (self.symbols.close_display)(self.display);
            }
        }
    }
}

// ============================================================================
// PipeWire DMA-BUF Portal Architecture Structure
// ============================================================================

/// Returns true if currently running under a Wayland compositor
pub fn is_wayland_active() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|s| s.to_lowercase() == "wayland")
            .unwrap_or(false)
}

/// Which capture backend to use.
///
/// Defaults to [`CaptureBackend::Auto`]. Override with `DISPLAYSWARM_CAPTURE` set to
/// one of `auto`, `native`, `pipewire`, `x11`, `mock`.
///
/// The `auto` behaviour is what previously made this confusing: on KDE Plasma 6
/// Wayland it silently degraded to the synthetic test pattern (a scrolling rainbow
/// grid), which looks like a broken decoder rather than a missing capture source.
///
/// A *forced* value means "try this one first", not "use this one or nothing":
/// every forced value still falls through to the next candidate if it cannot be
/// initialised, exactly as it did before `native` existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Try each real backend in preference order, then the test pattern.
    Auto,
    /// [`super::native`]: our own XDG portal + PipeWire implementation. The only
    /// one that can capture a KWin 6 screencast, because it is the only one that
    /// imports DMA-BUF buffers.
    Native,
    /// `xcap` 0.9.8. Kept as a last-resort portal fallback: it works on GNOME
    /// and X11 sessions, but it receives zero buffers on KWin 6.
    PipeWire,
    X11,
    Mock,
}

impl CaptureBackend {
    pub fn from_env() -> Self {
        Self::parse(
            &std::env::var("DISPLAYSWARM_CAPTURE").unwrap_or_default(),
        )
    }

    /// Pure parser behind [`CaptureBackend::from_env`], split out so it can be
    /// tested without mutating the process environment (which the test harness
    /// runs in parallel with everything else).
    fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "native" | "portal-native" | "pwnative" => CaptureBackend::Native,
            "pipewire" | "pw" | "portal" => CaptureBackend::PipeWire,
            "x11" => CaptureBackend::X11,
            "mock" | "test" | "synthetic" => CaptureBackend::Mock,
            _ => CaptureBackend::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CaptureBackend::Auto => "auto",
            CaptureBackend::Native => "native (xdg portal + pipewire, dma-buf capable)",
            CaptureBackend::PipeWire => "pipewire (xcap 0.9.8, last-resort fallback)",
            CaptureBackend::X11 => "x11",
            CaptureBackend::Mock => "mock (synthetic test pattern)",
        }
    }
}

/// Check if PipeWire portal or Wayland screencast is available on the system
pub fn is_pipewire_portal_available() -> bool {
    if is_wayland_active() {
        return true;
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        if std::path::Path::new(&runtime_dir).join("pipewire-0").exists() {
            return true;
        }
    }

    let uid = unsafe { libc::getuid() };
    if std::path::Path::new(&format!("/run/user/{}/pipewire-0", uid)).exists() {
        return true;
    }

    false
}

/// Returns true if the user has explicitly opted in to trying X11 capture on Wayland.
///
/// This is a workaround for the KDE Plasma 6 portal bug where xdg-desktop-portal
/// crashes during screencast session creation. X11 capture on Wayland (via XWayland)
/// only captures X11/XWayland applications, NOT native Wayland applications, so it
/// is NOT a full solution. It is offered as a last-resort partial workaround.
///
/// Enable with `DISPLAYSWARM_FORCE_X11_ON_WAYLAND=1`.
pub fn force_x11_on_wayland() -> bool {
    std::env::var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

// ============================================================================
// Native Portal Screencast Capturer  (PRIMARY Wayland backend)
// ============================================================================
//
// A thin, `ScreenCapturer`-shaped adapter over [`native`], which owns the
// XDG Desktop Portal handshake and the PipeWire stream. The reason it exists is
// in `native.rs`: xcap 0.9.8 cannot capture a KWin 6 screencast (four
// independent defects), so the portal is driven directly instead.
//
// THREADING -- the load-bearing part
// ----------------------------------
// `native::start_screencast` BLOCKS. It opens the portal, waits for the user to
// approve the screen-share dialog (up to `native::READY_TIMEOUT` = 300 s), and
// then waits for the first buffer to arrive. `create_default_capturer` is called
// from the tokio server's async context, and the very next statements of that
// future are `.await`s on the client socket -- so calling `start_screencast`
// there would stall the whole runtime for the length of the prompt and the
// phone would not even finish its handshake. Worse, the server's video loop
// calls `capture_frame` on that same thread.
//
// So `NativePortalCapturer::new` does NOT call it. It spawns a dedicated
// startup thread, hands the thread the frame sender, and returns immediately;
// `capture_frame` picks the result up from a channel on a later tick. `new()` is
// therefore safe to call from an async context, which is the whole point.
//
// `native::NativeScreencast` is `Send` (asserted at compile time below): it
// holds an `Arc<AtomicBool>`, a `JoinHandle`, and two `Arc`/channel pairs, and
// no PipeWire `Rc`. The `Rc`-based PipeWire state stays on the capture thread
// that created it, as PipeWire requires.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<native::NativeScreencast>();
    assert_send::<native::CaptureError>();
    assert_send::<native::CaptureFormat>();
};

/// Environment variable naming the output to capture, e.g. `Virtual-1`.
///
/// That is a DRM connector name, as shown by `wlr-randr` or
/// `kscreen-doctor -j`. Unset (the default) means "do not restrict the
/// request", which is what makes the portal put its own screen picker in front
/// of the user.
///
/// Caveat, restated from [`native::start_screencast`]: the `ScreenCast` portal's
/// `SelectSources` has no connector-name option, so this name is recorded for
/// diagnostics and echoed into the log -- the compositor still shows the picker
/// and the user can override it. It narrows the request, it does not guarantee
/// the connector.
pub const CAPTURE_OUTPUT_ENV: &str = "DISPLAYSWARM_CAPTURE_OUTPUT";

/// How long the frame channel is polled per `capture_frame` call before
/// concluding there is nothing new.
///
/// Tiny on purpose: the server calls `capture_frame` at the target frame rate
/// from an async context, so every microsecond spent here is latency added to
/// the whole stream. The native producer `try_send`s, so nothing is ever lost by
/// not waiting -- the newest frame is still there on the next tick.
const FRAME_POLL_TIMEOUT: Duration = Duration::from_millis(2);

/// Minimum spacing between two "this is not your screen" warnings.
const NOT_YOUR_SCREEN_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Reads [`CAPTURE_OUTPUT_ENV`], treating an empty/whitespace value as unset.
fn capture_output_filter() -> Option<String> {
    parse_output_filter(&std::env::var(CAPTURE_OUTPUT_ENV).unwrap_or_default())
}

/// Pure parser behind [`capture_output_filter`], split out so it can be tested
/// without mutating the process environment.
fn parse_output_filter(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Captures the compositor's output through the XDG Desktop Portal, using
/// [`native`]'s PipeWire stream, and exposes it as a [`ScreenCapturer`].
pub struct NativePortalCapturer {
    /// The size the encoder was configured for. Frames are letterboxed into
    /// this rather than returned at the native size; see [`Self::render`].
    target_width: u32,
    target_height: u32,

    /// The newest raw frame, kept only for the legacy polling
    /// [`ScreenCapturer::capture_frame`], which replays it on ticks with nothing
    /// new. The event-driven `next_frame` never fills this (holding a second
    /// reference to the pixel `Arc` would defeat its zero-copy 1:1 path).
    latest: Option<native::RawFrame>,

    /// What the compositor actually negotiated, published by the capture thread
    /// once it answers `param_changed`. `None` until then.
    negotiated: Mutex<Option<native::CaptureFormat>>,

    /// Result of the blocking [`native::start_screencast`] call, once the
    /// startup thread has produced one. Taken (set to `None`) on first arrival.
    startup: Option<cb::Receiver<Result<native::NativeScreencast, native::CaptureError>>>,

    /// The live session. `None` while the portal dialog is open and after a
    /// failure has been latched.
    session: Option<native::NativeScreencast>,

    /// The startup thread. Deliberately not joined while it may be parked in a
    /// D-Bus wait -- see `Drop`.
    startup_thread: Option<std::thread::JoinHandle<()>>,

    /// Set on drop. The startup thread checks it when the portal handshake
    /// returns and closes the brand-new session instead of leaking it: a session
    /// nobody owns is what keeps the compositor's recording icon lit.
    cancel: Arc<AtomicBool>,

    /// Frames from the capture thread, with their format, stride and capture
    /// time.
    frames: cb::Receiver<native::RawFrame>,
    /// True once the frame channel has reported "disconnected"; it is then
    /// excluded from waits, since a closed channel is permanently "ready" and
    /// would turn `next_frame` into a busy loop.
    frames_closed: bool,

    /// Reused filter taps and row accumulator for the scaling path.
    scaler: Scaler,

    /// A terminal failure, latched. See `capture_frame` for why it is reported
    /// exactly once and then only rate-limited.
    failure: Option<String>,
    /// Whether the latched failure has already been returned as an `Err`.
    failure_reported: bool,

    /// True once at least one genuine frame of the desktop has arrived.
    got_real_frame: bool,
    /// When the last "not your screen" warning was emitted.
    last_not_your_screen_log: Option<Instant>,

    /// The portal stream's rectangle in the desktop layout, published by the
    /// startup thread once `Start` answers.
    region: native::SharedRegion,
    /// Which kind of target this captures, for `describe_source`.
    target_label: &'static str,
}

/// Short name of a target kind.
fn target_label(target: &CaptureTarget) -> &'static str {
    match target {
        CaptureTarget::Monitor { .. } => "monitor",
        CaptureTarget::Virtual { .. } => "virtual monitor",
        CaptureTarget::Window => "window",
    }
}

/// The portal request (and, for a virtual monitor, the mode to offer in the
/// PipeWire format) that captures `target`. `env_output` is the
/// `DISPLAYSWARM_CAPTURE_OUTPUT` override; it wins over the target's own output for
/// back-compat.
fn portal_request_for(
    target: &CaptureTarget,
    token_key: Option<&str>,
    env_output: Option<String>,
) -> (portal::ScreencastRequest, Option<native::VirtualMode>) {
    let token_key = token_key.map(str::to_string);
    match target {
        CaptureTarget::Monitor { output } => (
            portal::ScreencastRequest::monitor(env_output.or_else(|| output.clone()), token_key),
            None,
        ),
        CaptureTarget::Virtual { width, height, refresh_mhz } => (
            portal::ScreencastRequest {
                source_type: portal::SOURCE_TYPE_VIRTUAL,
                output_filter: None,
                token_key,
            },
            Some(native::VirtualMode {
                width: *width,
                height: *height,
                refresh_mhz: *refresh_mhz,
            }),
        ),
        CaptureTarget::Window => (
            portal::ScreencastRequest {
                source_type: portal::SOURCE_TYPE_WINDOW,
                output_filter: None,
                token_key,
            },
            None,
        ),
    }
}

impl NativePortalCapturer {
    /// Starts a native portal screencast and returns immediately.
    ///
    /// Safe to call from an async context -- the blocking part
    /// ([`native::start_screencast`], which waits for the user's portal prompt
    /// and then for the first buffer) runs on a dedicated startup thread and is
    /// collected later by [`ScreenCapturer::next_frame`].
    ///
    /// `width`/`height` are the size the *encoder* was configured for, not a
    /// capture request: the compositor picks its own capture size and the frames
    /// are scaled to this one. See [`Self::render`].
    pub fn new(
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_for(&CaptureTarget::Monitor { output: None }, None, width, height)
    }

    /// [`Self::new`] for any [`CaptureTarget`], with an optional restore-token
    /// key (one token file per device/target; `None` = the shared legacy one).
    pub fn new_for(
        target: &CaptureTarget,
        token_key: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Cheap pre-flight so the obviously-wrong cases (no Wayland session, no
        // PipeWire socket anywhere) fail instantly instead of spending the
        // portal's 300 s D-Bus timeout discovering it.
        if !is_pipewire_portal_available() {
            return Err("Neither Wayland session nor PipeWire portal is available".into());
        }

        let (request, virtual_mode) = portal_request_for(target, token_key, capture_output_filter());
        let output_filter = request.output_filter.clone();
        log::info!(
            "Requesting NATIVE XDG portal screencast (target: {}, requested output: {}, token key: {})",
            target_label(target),
            output_filter.as_deref().unwrap_or("<unset: the portal will ask the user>"),
            token_key.unwrap_or("<shared>")
        );
        let region: native::SharedRegion = Arc::new(Mutex::new(None));
        let thread_region = region.clone();

        let (frame_tx, frames) = native::frame_channel();
        let (startup_tx, startup_rx) =
            cb::bounded::<Result<native::NativeScreencast, native::CaptureError>>(1);
        let cancel = Arc::new(AtomicBool::new(false));

        let thread_cancel = cancel.clone();
        let startup_thread = std::thread::Builder::new()
            .name("displayswarm-portal-startup".into())
            .spawn(move || {
                // Blocking: the portal handshake waits for the user's dialogs,
                // then this waits for the first buffer or a clear failure. If
                // the capturer was dropped meanwhile, the cancellable variant
                // closes the session it just obtained instead of returning it.
                let mut result = native::start_screencast_request(
                    request.clone(),
                    virtual_mode,
                    frame_tx.clone(),
                    thread_cancel.clone(),
                    thread_region.clone(),
                );
                // KWin sometimes hands out a virtual-monitor stream that never
                // negotiates a format ("no more input formats", typically right
                // after another capture ended). A fresh session fixes it, so a
                // virtual stream with no frame after a few seconds is redone.
                if virtual_mode.is_some() {
                    for attempt in 2..=3 {
                        let Ok(session) = &result else { break };
                        let deadline = std::time::Instant::now() + Duration::from_secs(4);
                        while !session.has_frames()
                            && std::time::Instant::now() < deadline
                            && !thread_cancel.load(Ordering::Acquire)
                        {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        if session.has_frames() || thread_cancel.load(Ordering::Acquire) {
                            break;
                        }
                        log::warn!("native capture: the virtual monitor stream delivered no frame; starting a new session (attempt {attempt}/3)");
                        drop(result);
                        std::thread::sleep(Duration::from_millis(500));
                        result = native::start_screencast_request(
                            request.clone(),
                            virtual_mode,
                            frame_tx.clone(),
                            thread_cancel.clone(),
                            thread_region.clone(),
                        );
                    }
                }
                // If nobody is listening any more the send fails and the result
                // (a live session) is dropped here, which closes it.
                let _ = startup_tx.send(result);
            })
            .map_err(|e| format!("could not spawn the native capture startup thread: {e}"))?;

        log::info!(
            "Native portal screencast requested; waiting for the portal dialog. \
             Authorise the screen-share prompt, otherwise this will end in a timeout."
        );

        Ok(Self {
            target_width: width,
            target_height: height,
            latest: None,
            negotiated: Mutex::new(None),
            startup: Some(startup_rx),
            session: None,
            startup_thread: Some(startup_thread),
            cancel,
            frames,
            frames_closed: false,
            scaler: Scaler::new(),
            failure: None,
            failure_reported: false,
            got_real_frame: false,
            last_not_your_screen_log: None,
            region,
            target_label: target_label(target),
        })
    }

    /// True once at least one real frame of the desktop has been received.
    pub fn has_real_frames(&self) -> bool {
        self.got_real_frame
    }

    /// True if the session has definitively failed.
    pub fn has_failed(&self) -> bool {
        self.failure.is_some()
    }

    /// The negotiated capture size, or `None` before the compositor answers.
    fn negotiated(&self) -> Option<native::CaptureFormat> {
        // `unwrap_or_else(into_inner)` rather than `unwrap`: a poisoned lock here
        // only means some other thread panicked while holding a copy of a
        // `Copy` struct, and there is nothing to recover but still worth using.
        *self.negotiated.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a terminal failure. The first one wins; later ones are noise from
    /// the same dead session and are ignored.
    fn latch_failure(&mut self, msg: String) {
        if self.failure.is_some() {
            return;
        }
        log::error!("native screencast: {msg}");
        self.failure = Some(msg);
        // The handle is useless now; dropping it tears the PipeWire loop down
        // and closes the portal session.
        //
        // That drop joins the capture thread, so it blocks -- but the loop
        // notices the stop flag on its 100 ms tick and is normally already on its
        // way out (every path that latches a failure has just seen the loop end),
        // so this is a bounded wait on a terminal path, not a stall on the hot
        // one.
        self.session = None;
    }

    /// Re-reads the negotiated format from the live session.
    ///
    /// Called on every received frame, not just once at startup: the compositor
    /// legitimately renegotiates, and `native` publishes the new format before
    /// any buffer of the new size arrives.
    fn refresh_negotiated(&self) {
        if let Some(fmt) = self.session.as_ref().and_then(|s| s.format()) {
            *self.negotiated.lock().unwrap_or_else(|e| e.into_inner()) = Some(fmt);
        }
    }

    /// Picks up whatever the startup thread has produced: a live session, a
    /// failure, or nothing yet.
    fn absorb_startup(&mut self) {
        let Some(rx) = self.startup.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(session)) => {
                self.startup = None;
                let fmt = session.format();
                match fmt {
                    Some(f) => log::info!(
                        "native screencast: portal session LIVE, {}x{} @ {}/{} fps, fourcc {}, {} buffers",
                        f.width,
                        f.height,
                        f.frame_rate.num,
                        f.frame_rate.denom,
                        f.fourcc_str(),
                        if f.is_dmabuf { "DMA-BUF" } else { "shared-memory" }
                    ),
                    None => log::warn!(
                        "native screencast: session started but the compositor has not published \
                         a format yet; frame sizes are taken from the frames themselves"
                    ),
                }
                *self.negotiated.lock().unwrap_or_else(|e| e.into_inner()) = fmt;
                self.session = Some(session);
            }
            Ok(Err(e)) => {
                self.startup = None;
                self.latch_failure(format!("could not start the native portal screencast: {e}"));
            }
            Err(cb::TryRecvError::Empty) => {}
            Err(cb::TryRecvError::Disconnected) => {
                // The startup thread is gone without reporting. The only way that
                // happens is a panic inside it; there is nothing to retry to.
                self.startup = None;
                self.latch_failure(
                    "the native capture startup thread exited without reporting a result \
                     (it panicked; see any panic message earlier in this log)"
                        .to_string(),
                );
            }
        }
    }

    /// Blocks for up to `timeout` until there is something to look at: a frame,
    /// or the startup thread's verdict. A zero timeout never blocks.
    ///
    /// Event-driven on purpose: waking on the channels instead of polling at the
    /// target frame rate is what lets a static desktop cost nothing and a busy
    /// one be encoded exactly once per real frame.
    fn wait_ready(&self, timeout: Duration) {
        if timeout.is_zero() || !self.frames.is_empty() {
            return;
        }
        let mut sel = cb::Select::new();
        let mut watching = false;
        if !self.frames_closed {
            sel.recv(&self.frames);
            watching = true;
        }
        if let Some(rx) = self.startup.as_ref() {
            sel.recv(rx);
            watching = true;
        }
        if watching {
            // The result (ready index or timeout) does not matter: whichever it
            // was, the caller now does a non-blocking pass over everything.
            let _ = sel.ready_timeout(timeout);
        } else {
            // Failed and drained: nothing can ever arrive, so behave like an
            // idle source instead of spinning.
            std::thread::sleep(timeout);
        }
    }

    /// Drains the frame channel down to the newest frame, without blocking.
    fn drain_newest(&mut self) -> Option<native::RawFrame> {
        let mut newest = None;
        loop {
            match self.frames.try_recv() {
                Ok(f) => {
                    if !self.got_real_frame {
                        self.got_real_frame = true;
                        log::info!(
                            "native screencast: first REAL frame received ({}x{}, {:?}, {} bytes) - \
                             live capture is live",
                            f.width,
                            f.height,
                            f.format,
                            f.data.len()
                        );
                    }
                    self.refresh_negotiated();
                    // Keep only the newest: for a remote display the most recent
                    // view is the only one that has any value.
                    newest = Some(f);
                }
                Err(cb::TryRecvError::Empty) => break,
                Err(cb::TryRecvError::Disconnected) => {
                    // The sender lives inside the capture thread, so a closed
                    // channel only means "that thread is finishing". The startup
                    // thread publishes its result immediately afterwards, so a
                    // closed channel is NOT on its own a failure -- reporting it
                    // here would invent an error *and* latch it in place of the
                    // real one. Re-check the startup channel instead, and only
                    // complain if a session had genuinely been established and
                    // then died: a live session's capture thread does not return
                    // while its loop is running, so this really is a failure.
                    self.frames_closed = true;
                    self.absorb_startup();
                    if self.session.is_some() {
                        self.latch_failure(
                            "the native capture frame channel closed: the capture thread is gone"
                                .to_string(),
                        );
                    }
                    break;
                }
            }
        }
        newest
    }

    /// The shared front half of `next_frame` and `capture_frame`: waits up to
    /// `timeout`, updates session state, and returns the newest frame that
    /// arrived (if any).
    ///
    /// A brand-new failure is returned as `Err` exactly once, so the caller's own
    /// "capture failed" log carries the full cause. After that the failure stays
    /// latched (visible through `state()`) and this returns `Ok(None)`; repeating
    /// it every call would put dozens of identical errors a second in the log.
    fn poll_newest(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<native::RawFrame>, Box<dyn std::error::Error + Send + Sync>> {
        self.wait_ready(timeout);

        // Anything that changed since the last call: a session that finished
        // starting, or a failure. Checked around the frames because these are the
        // reason there may be no frame at all.
        self.absorb_startup();
        if let Some(err) = self.session.as_ref().and_then(|s| s.try_take_error()) {
            self.latch_failure(format!("native portal screencast failed after start: {err}"));
        }
        let newest = self.drain_newest();

        if let Some(msg) = self.failure.clone() {
            if !self.failure_reported {
                self.failure_reported = true;
                return Err(msg.into());
            }
        }
        Ok(newest)
    }

    /// Whether `raw`'s buffer really holds the `width`x`height` picture it
    /// claims. A short buffer must never reach the scaler.
    fn frame_is_sane(raw: &native::RawFrame) -> bool {
        raw.width > 0
            && raw.height > 0
            && raw.stride >= raw.width as usize * 4
            && raw.data.len() >= (raw.height as usize - 1) * raw.stride + raw.width as usize * 4
    }

    /// Turns a raw frame into a [`Frame`] at the encoder's size.
    ///
    /// * Same size as the encoder, tightly packed: no scaling and, when the
    ///   caller holds the only reference to the pixels, no copy at all -- the
    ///   `Vec` PipeWire produced is moved straight through. Otherwise one plain
    ///   row copy.
    /// * Different size: fitted inside the encoder's rectangle with the aspect
    ///   ratio preserved (black bars) and area-averaged; see
    ///   [`crate::capture::scale`]. The encoder reads `frame.data` with its own
    ///   fixed width/height, so the output must always be exactly that size --
    ///   but squashing a 16:10 desktop into a 16:9 stream is not acceptable, and
    ///   nearest-neighbour makes text shimmer.
    ///
    /// The frame's own timestamp is passed through untouched: it is the capture
    /// time, taken before any of this work.
    fn render(&mut self, raw: native::RawFrame, timestamp_us: u64) -> Frame {
        let (tw, th) = (self.target_width, self.target_height);
        let tight = raw.width as usize * 4;
        let out_len = tw as usize * th as usize * 4;
        let same_size = raw.width == tw && raw.height == th;

        let data = if same_size && raw.stride == tight && raw.data.len() >= out_len {
            match Arc::try_unwrap(raw.data) {
                Ok(mut v) => {
                    v.truncate(out_len);
                    v
                }
                Err(shared) => shared[..out_len].to_vec(),
            }
        } else {
            let mut out = vec![0u8; out_len];
            self.scaler.scale(
                &raw.data,
                raw.width,
                raw.height,
                raw.stride,
                &mut out,
                tw,
                th,
            );
            out
        };
        Frame {
            width: tw,
            height: th,
            format: raw.format,
            data,
            timestamp_us,
        }
    }

    /// An opaque black frame of the encoder's size, for the legacy polling API
    /// only (`next_frame` returns `None` instead of faking a picture).
    ///
    /// Black rather than a colourful pattern on purpose: a rainbow grid is exactly
    /// the thing that made this bug look like a decoder fault, whereas a black
    /// screen reads as "no source" everywhere, and the accompanying log line says
    /// so in capitals anyway.
    fn placeholder_frame(&self) -> Frame {
        let mut data = vec![0u8; (self.target_width as usize) * (self.target_height as usize) * 4];
        for px in data.chunks_exact_mut(4) {
            px[3] = 0xff;
        }
        Frame {
            width: self.target_width,
            height: self.target_height,
            format: PixelFormat::Rgba,
            data,
            timestamp_us: host_monotonic_us(),
        }
    }

    /// Says, at a bounded rate, that the caller is not looking at the desktop.
    fn warn_not_your_screen(&mut self, why: &str) {
        let now = Instant::now();
        if let Some(last) = self.last_not_your_screen_log {
            if now.duration_since(last) < NOT_YOUR_SCREEN_LOG_INTERVAL {
                return;
            }
        }
        self.last_not_your_screen_log = Some(now);
        log::warn!(
            "*** NOT YOUR SCREEN *** {why}. Approve the screen-share prompt, or set \
             DISPLAYSWARM_CAPTURE=pipewire to retry with xcap, or DISPLAYSWARM_CAPTURE=mock to \
             silence this."
        );
    }
}

impl ScreenCapturer for NativePortalCapturer {
    /// Event-driven capture: blocks up to `timeout` for a frame newer than the
    /// last one returned, and returns `Ok(None)` if none came.
    ///
    /// Deliberately never replays the previous frame. The old behaviour (reuse
    /// the last frame on every tick) made the encoder re-encode identical
    /// pictures at 60 fps on a static desktop; now the encoder only runs when the
    /// compositor actually produced something. While the portal dialog is open
    /// there is no frame at all, so this returns `Ok(None)` rather than a black
    /// placeholder, and `state()` says `AwaitingPermission`.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Frame>, Box<dyn std::error::Error + Send + Sync>> {
        let Some(raw) = self.poll_newest(timeout)? else {
            return Ok(None);
        };
        if !Self::frame_is_sane(&raw) {
            log::warn!(
                "native screencast: discarding a {}-byte frame that does not hold the \
                 {}x{} (stride {}) picture it claims",
                raw.data.len(),
                raw.width,
                raw.height,
                raw.stride
            );
            self.refresh_negotiated();
            return Ok(None);
        }
        let ts = raw.timestamp_us;
        Ok(Some(self.render(raw, ts)))
    }

    /// AwaitingPermission until the first real frame, Live afterwards, Failed
    /// once a failure is latched.
    fn state(&self) -> CaptureState {
        if let Some(msg) = &self.failure {
            CaptureState::Failed(msg.clone())
        } else if self.got_real_frame {
            CaptureState::Live
        } else {
            CaptureState::AwaitingPermission
        }
    }

    /// Legacy polling API: always yields a frame, replaying the last real one (or
    /// a black placeholder before the first). New code should use `next_frame`.
    fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        let fresh = self.poll_newest(FRAME_POLL_TIMEOUT)?;
        let fresh_frame = fresh.is_some();
        if let Some(raw) = fresh {
            self.latest = Some(raw);
        }

        // FRAME REUSE -- deliberate, for this legacy caller only: the polling
        // loop wants a frame per tick, and a reused frame is a perfectly good
        // one to a video encoder (it codes as a near-zero-delta picture).
        if let Some(raw) = self.latest.clone() {
            if !Self::frame_is_sane(&raw) {
                // The buffer is not something to scale, and retrying it every
                // tick would put the same error in the log sixty times a second.
                // Throw it away and say so at the usual rate.
                log::warn!(
                    "native screencast: discarding a {}-byte frame that does not hold the \
                     {}x{} (stride {}) picture it claims",
                    raw.data.len(),
                    raw.width,
                    raw.height,
                    raw.stride
                );
                self.latest = None;
                self.refresh_negotiated();
                self.warn_not_your_screen("a frame did not match its declared format and was \
                                           discarded");
                return Ok(self.placeholder_frame());
            }

            // The clone above keeps the Arc shared, so this always takes the copy
            // path: the stored `latest` must stay intact for the next replay.
            let ts = if fresh_frame { raw.timestamp_us } else { host_monotonic_us() };
            let frame = self.render(raw, ts);
            if self.failure.is_some() {
                // A dead session still has a last frame, and replaying it is the
                // least-bad thing to show -- but it is frozen, so say so instead
                // of letting it read as a live static desktop.
                self.warn_not_your_screen("the native screencast session has FAILED; showing the \
                                          last frame it produced, which is now FROZEN");
            }
            return Ok(frame);
        }

        // No real frame has ever arrived. Emit black and say why, loudly.
        let why = match self.failure.as_deref() {
            Some(m) => format!("native screencast failed: {m}"),
            None if self.startup.is_some() => "no frames from the native portal screencast yet; \
                 the portal dialog is still waiting for you, or the session is still starting"
                .to_string(),
            None => "no frames from the native portal screencast".to_string(),
        };
        self.warn_not_your_screen(&why);
        Ok(self.placeholder_frame())
    }

    /// The real negotiated capture size, falling back to the requested target
    /// until the compositor has answered.
    fn width(&self) -> u32 {
        self.negotiated().map(|f| f.width).unwrap_or(self.target_width)
    }

    fn height(&self) -> u32 {
        self.negotiated().map(|f| f.height).unwrap_or(self.target_height)
    }

    fn output_region(&self) -> Option<crate::display::OutputRegion> {
        // A window has no place in the monitor layout, whatever the portal says.
        if self.target_label == "window" {
            return None;
        }
        *self.region.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn describe_source(&self) -> String {
        let kind = self.target_label;
        let base = self.describe_source_inner();
        format!("[{kind}] {base}")
    }
}

impl NativePortalCapturer {
    fn describe_source_inner(&self) -> String {
        let target = format!("{}x{}", self.target_width, self.target_height);
        match (self.failure.as_deref(), self.negotiated()) {
            (Some(m), _) => format!("Portal screencast (native) FAILED: {m} -- NOT your screen"),
            (None, Some(f)) if self.got_real_frame => format!(
                "Portal screencast (native, {}) {}x{} @ {}/{} fps {} -- REAL frames, scaled to {target}",
                if f.is_dmabuf { "dmabuf" } else { "shm" },
                f.width,
                f.height,
                f.frame_rate.num,
                f.frame_rate.denom,
                f.fourcc_str(),
            ),
            (None, Some(f)) => format!(
                "Portal screencast (native, {}) {}x{} negotiated but NOT YET DELIVERING FRAMES \
                 -- not your screen yet",
                if f.is_dmabuf { "dmabuf" } else { "shm" },
                f.width,
                f.height,
            ),
            // No format published, but frames are arriving: still real capture.
            (None, None) if self.got_real_frame => format!(
                "Portal screencast (native) delivering REAL frames, scaled to {target} \
                 (capture size not reported)"
            ),
            (None, None) => format!(
                "Portal screencast (native) starting, waiting for the portal dialog \
                 -- NOT your screen yet (target {target})"
            ),
        }
    }
}

impl Drop for NativePortalCapturer {
    fn drop(&mut self) {
        // Tell a still-parked startup thread that nobody wants its session.
        self.cancel.store(true, Ordering::Release);

        // A live session: stop() joins the PipeWire thread, THEN closes the
        // portal session explicitly (Session.Close), which is what switches the
        // compositor's recording indicator off. Bounded by ~100 ms for the join
        // plus the close timeout.
        if let Some(mut session) = self.session.take() {
            log::info!("native screencast: stopping the capture session");
            session.stop();
        }

        // A session the startup thread finished but nobody collected yet: take
        // it out of the channel and stop it now, rather than leaving it to
        // whenever the channel's last endpoint happens to drop.
        if let Some(rx) = self.startup.take() {
            if let Ok(Ok(mut session)) = rx.try_recv() {
                log::info!("native screencast: closing a session that was never collected");
                session.stop();
            }
        }

        // The startup thread may still be parked inside `start_screencast`,
        // waiting on a portal dialog nobody has answered. Joining it here would
        // trade a bounded teardown for an unbounded hang, so it is detached
        // instead. That is safe now: `cancel` is set, so when the dialog is
        // answered the thread closes the session it gets instead of leaking it.
        if let Some(handle) = self.startup_thread.take() {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                log::warn!(
                    "native screencast: the portal dialog is still open; the startup thread is \
                     detached and will close its session as soon as the dialog is answered"
                );
                drop(handle);
            }
        }
    }
}

// ============================================================================
// PipeWire DMA-BUF Portal Architecture Structure
// ============================================================================

/// Represents PipeWire DMA-BUF Screencast portal configuration and buffer management
pub struct PipeWirePortalCapturer {
    pub width: u32,
    pub height: u32,
    pub session_handle: Option<String>,
    pub pipewire_node_id: Option<u32>,
    pub is_active: bool,
    frame_count: u32,

    // Real capture components via xcap ScreenCast portal & PipeWire stream
    monitor: Option<xcap::Monitor>,
    recorder: Option<xcap::VideoRecorder>,
    receiver: Option<std::sync::mpsc::Receiver<xcap::Frame>>,
    pending_init: Option<std::sync::mpsc::Receiver<Result<(xcap::VideoRecorder, std::sync::mpsc::Receiver<xcap::Frame>), String>>>,
    cached_frame: Option<Vec<u8>>,
    cached_width: u32,
    cached_height: u32,
    /// Set once the portal has produced at least one real frame.
    got_first_real_frame: bool,
    /// Ensures the "this is a test pattern" warning is logged at most once.
    warned_test_pattern: bool,
    /// Capture ticks elapsed while a live session has produced no frames.
    frames_poll_count: u32,
    scaler: Scaler,
}

impl PipeWirePortalCapturer {
    /// True once the portal has delivered at least one real frame. Used to tell a
    /// live capture apart from the synthetic fallback shown while the portal is
    /// still awaiting the user's authorisation.
    pub fn has_real_frames(&self) -> bool {
        self.cached_frame.is_some()
    }

    pub fn new(width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        log::info!(
            "Initializing Linux Wayland PipeWire Portal Capturer (Target: {}x{})",
            width, height
        );

        if !is_pipewire_portal_available() {
            return Err("Neither Wayland session nor PipeWire portal is available".into());
        }

        // 1. Discover active monitors via xcap
        let monitors = xcap::Monitor::all().map_err(|e| format!("Failed to enumerate monitors: {e}"))?;
        if monitors.is_empty() {
            return Err("No active displays found via xcap".into());
        }

        // Prefer primary monitor, fallback to first monitor
        let monitor = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .cloned()
            .unwrap_or_else(|| monitors[0].clone());

        log::info!(
            "Selected monitor for Wayland capture: name={:?}, size={}x{}",
            monitor.name().unwrap_or_default(),
            monitor.width().unwrap_or(0),
            monitor.height().unwrap_or(0)
        );

        // 2. Initiate XDG Desktop Portal ScreenCast session asynchronously
        // Spawning on a background thread ensures the host application never freezes
        // while the desktop environment displays the screen-sharing authorization dialog.
        let (init_tx, init_rx) = std::sync::mpsc::channel();
        let mon_clone = monitor.clone();

        let spawn_res = std::thread::Builder::new()
            .name("displayswarm-wayland-init".into())
            .spawn(move || {
                log::info!("Connecting to XDG Desktop Portal ScreenCast and PipeWire stream...");
                let res = match mon_clone.video_recorder() {
                    Ok((recorder, receiver)) => {
                        if let Err(e) = recorder.start() {
                            log::error!("Failed to start xcap VideoRecorder: {:?}", e);
                            Err(format!("VideoRecorder start error: {e:?}"))
                        } else {
                            log::info!("xcap ScreenCast VideoRecorder started successfully");
                            Ok((recorder, receiver))
                        }
                    }
                    Err(e) => {
                        log::error!("xcap video_recorder initialization failed: {:?}", e);
                        Err(format!("Portal video_recorder failed: {e:?}"))
                    }
                };
                let _ = init_tx.send(res);
            });

        let (recorder, receiver, pending_init) = if spawn_res.is_ok() {
            // Check if portal authorized immediately or already authorized
            match init_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(Ok((rec, rx))) => {
                    log::info!("Wayland screencast stream established immediately");
                    (Some(rec), Some(rx), None)
                }
                Ok(Err(e)) => {
                    log::warn!("Immediate screencast initialization error: {}. Will use fallback/retry.", e);
                    (None, None, None)
                }
                Err(_) => {
                    log::info!("Screencast initialization continuing asynchronously via portal dialog");
                    (None, None, Some(init_rx))
                }
            }
        } else {
            (None, None, None)
        };

        Ok(Self {
            width,
            height,
            session_handle: Some("portal_screencast_session".into()),
            pipewire_node_id: Some(42),
            is_active: true,
            frame_count: 0,
            monitor: Some(monitor),
            recorder,
            receiver,
            pending_init,
            cached_frame: None,
            cached_width: 0,
            cached_height: 0,
            got_first_real_frame: false,
            warned_test_pattern: false,
            frames_poll_count: 0,
            scaler: Scaler::new(),
        })
    }

    pub fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        self.frame_count = self.frame_count.wrapping_add(1);
        let now_us = host_monotonic_us();

        // 1. Check if asynchronous portal initialization has completed
        if let Some(ref pending) = self.pending_init {
            match pending.try_recv() {
                Ok(Ok((recorder, receiver))) => {
                    log::info!("Asynchronous Wayland screencast session activated!");
                    self.recorder = Some(recorder);
                    self.receiver = Some(receiver);
                    self.pending_init = None;
                }
                Ok(Err(e)) => {
                    log::warn!("Asynchronous screencast initialization failed: {}", e);
                    self.pending_init = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending_init = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // 2. Drain latest frames from receiver
        let mut stream_disconnected = false;

        if let Some(ref rx) = self.receiver {
            // Distinguish "portal granted but producing nothing" from "producing
            // frames that we are consuming". Without this the only symptom is a
            // static image on the client that looks like a frozen stream.
            if !self.got_first_real_frame {
                self.frames_poll_count += 1;
                if self.frames_poll_count % 120 == 0 {
                    log::warn!(
                        "Portal session is live but has delivered 0 frames after {} capture ticks. \
                         The screencast stream is not producing data (PipeWire/KWin end).",
                        self.frames_poll_count
                    );
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(frame) => {
                        if !self.got_first_real_frame {
                            self.got_first_real_frame = true;
                            log::info!(
                                "PipeWire delivered its first REAL frame ({}x{}) - live capture is live",
                                frame.width,
                                frame.height
                            );
                        }
                        self.cached_frame = Some(frame.raw);
                        self.cached_width = frame.width;
                        self.cached_height = frame.height;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        log::warn!("Wayland PipeWire stream receiver disconnected");
                        stream_disconnected = true;
                        break;
                    }
                }
            }
        }

        // 3. Graceful error recovery: reconnect if stream disconnected with rate limiting
        if stream_disconnected {
            self.receiver = None;
            self.recorder = None;
            // Only retry if not already pending
            if self.pending_init.is_none() {
                log::warn!("Wayland screencast stream ended. Falling back to active pattern generator.");
            }
        }

        // 4. If we have a captured frame from PipeWire stream, scale and return it
        if let Some(ref raw) = self.cached_frame {
            let total_dst_bytes = (self.width * self.height * 4) as usize;
            let mut scaled_buffer = vec![0u8; total_dst_bytes];

            self.scaler.scale(
                raw,
                self.cached_width,
                self.cached_height,
                (self.cached_width * 4) as usize,
                &mut scaled_buffer,
                self.width,
                self.height,
            );

            return Ok(Frame {
                width: self.width,
                height: self.height,
                format: PixelFormat::Rgba,
                data: scaled_buffer,
                timestamp_us: now_us,
            });
        }

        // 5. Active dynamic pattern fallback while portal is awaiting authorization or if unavailable
        if self.cached_frame.is_none() && !self.warned_test_pattern {
            self.warned_test_pattern = true;
            log::warn!(
                "No real frames from the capture portal yet. Emitting the SYNTHETIC TEST PATTERN \
                 (a scrolling rainbow grid) instead of your screen. Authorise the screen-share \
                 prompt, or set DISPLAYSWARM_CAPTURE=mock to silence this."
            );
        }
        let total_pixels = (self.width * self.height) as usize;
        let mut buffer = vec![0u8; total_pixels * 4];
        let offset = (self.frame_count * 4) as usize;
        let w = self.width as usize;
        for y in 0..self.height as usize {
            let row_offset = y * w * 4;
            let r = ((y + offset) % 256) as u8;
            for x in 0..w {
                let idx = row_offset + (x * 4);
                let g = ((x + offset) % 256) as u8;
                buffer[idx] = r;
                buffer[idx + 1] = g;
                buffer[idx + 2] = 200u8;
                buffer[idx + 3] = 255u8;
            }
        }

        Ok(Frame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgba,
            data: buffer,
            timestamp_us: now_us,
        })
    }
}

impl Drop for PipeWirePortalCapturer {
    fn drop(&mut self) {
        if let Some(ref recorder) = self.recorder {
            log::info!("Stopping xcap VideoRecorder portal stream");
            let _ = recorder.stop();
        }
    }
}

impl ScreenCapturer for PipeWirePortalCapturer {
    fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        PipeWirePortalCapturer::capture_frame(self)
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn describe_source(&self) -> String {
        if self.has_real_frames() {
            format!(
                "PipeWire screencast via xcap 0.9.8 {}x{} -- REAL frames, scaled to {}x{}",
                self.cached_width, self.cached_height, self.width, self.height
            )
        } else {
            format!(
                "PipeWire screencast via xcap 0.9.8 (target {}x{}) -- NO REAL FRAMES YET, \
                 showing the SYNTHETIC TEST PATTERN, this is NOT your screen",
                self.width, self.height
            )
        }
    }
}

// ============================================================================
// Linux Unified Capturer Backend Dispatcher
// ============================================================================

enum LinuxBackend {
    X11(X11Capturer),
    Native(NativePortalCapturer),
    PipeWire(PipeWirePortalCapturer),
    Fallback(MockCapturer),
}

pub struct LinuxCapturer {
    backend: LinuxBackend,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    /// Whether the definitive "is this delivering REAL frames" line has been
    /// logged. It can only be answered after the first frame, which is why it is
    /// not logged in `new`.
    verdict_logged: bool,
    /// When the polling backends last produced a frame; see [`pace`].
    last_poll: Option<Instant>,
}

impl LinuxCapturer {
    /// Builds the best available capturer.
    ///
    /// Preference order under [`CaptureBackend::Auto`]:
    /// `native` -> `pipewire` (xcap) -> `x11` -> `mock`.
    ///
    /// Every step logs the reason it was tried and, if it failed, the reason it
    /// was abandoned, so "why am I looking at a rainbow grid" is answerable from
    /// the log alone.
    ///
    /// This does **not** block on the portal: `NativePortalCapturer::new` only
    /// spawns a thread, so this is safe to call from the tokio runtime even
    /// though the capture session it starts is not ready yet.
    pub fn new(width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_for(&CaptureTarget::Monitor { output: None }, None, width, height)
    }

    /// [`Self::new`] for any [`CaptureTarget`].
    ///
    /// A monitor target follows the full fallback chain. A virtual monitor or a
    /// window has no meaningful fallback (xcap/X11/mock would silently show
    /// something else), so those use the native portal backend only and return
    /// an error if it cannot start; `DISPLAYSWARM_CAPTURE=mock` still forces the
    /// synthetic pattern for every target.
    pub fn new_for(
        target: &CaptureTarget,
        token_key: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let backend_choice = CaptureBackend::from_env();
        let with = |backend| Self { backend, width, height, verdict_logged: false, last_poll: None };
        // An X11 session reads the screen directly: a PipeWire socket exists on
        // most X11 desktops too, so the portal path below would otherwise be
        // picked and wait for a screen-share service that is usually missing.
        // Decided by the session type, never by DISPLAY (Xwayland sets it on
        // Wayland sessions).
        let x11_session = crate::display::env::current_session() == crate::display::model::SessionType::X11;
        if x11_session && matches!(backend_choice, CaptureBackend::Auto | CaptureBackend::X11) {
            match target {
                CaptureTarget::Monitor { output } => {
                    let output = output.clone().or_else(capture_output_filter);
                    match X11Capturer::for_output(width, height, output.as_deref()) {
                        Ok(cap) => {
                            log::info!("X11 session: capturing {}", cap.describe());
                            return Ok(with(LinuxBackend::X11(cap)));
                        }
                        Err(e) => log::warn!("X11 session: direct screen capture failed ({e}); trying the other backends"),
                    }
                }
                CaptureTarget::Virtual { width: vw, height: vh, refresh_mhz } => {
                    // The device id is the first part of the token key ("<id>/virtual").
                    let device = token_key.and_then(|k| k.split('/').next()).unwrap_or("phone");
                    match X11Capturer::for_virtual(width, height, device, *vw, *vh, *refresh_mhz) {
                        Ok(cap) => {
                            log::info!("X11 session: capturing {}", cap.describe());
                            return Ok(with(LinuxBackend::X11(cap)));
                        }
                        // The portal only when it says it can make one; otherwise
                        // its failure would hide the real reason.
                        Err(e) => {
                            let portal_virtual = crate::display::env::portal_source_types()
                                .is_some_and(|b| b & crate::display::env::SOURCE_VIRTUAL != 0);
                            if !portal_virtual {
                                return Err(format!("no X11 virtual monitor: {e}").into());
                            }
                            log::warn!("X11 session: no RandR virtual monitor ({e}); trying the portal's");
                        }
                    }
                }
                // Single windows only come from a portal.
                CaptureTarget::Window => {}
            }
        }
        if !matches!(target, CaptureTarget::Monitor { .. }) && backend_choice != CaptureBackend::Mock {
            let native_cap = NativePortalCapturer::new_for(target, token_key, width, height)?;
            return Ok(Self {
                backend: LinuxBackend::Native(native_cap),
                width,
                height,
                verdict_logged: false,
                last_poll: None,
            });
        }
        log::info!(
            "Initializing Linux screen capturer (Target: {}x{}, requested backend: {})",
            width,
            height,
            backend_choice.as_str()
        );

        let wayland = is_wayland_active();
        let is_kde = std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_uppercase().contains("KDE"))
            .unwrap_or(false);
        let portal_reachable = wayland || is_pipewire_portal_available();

        let use_native =
            matches!(backend_choice, CaptureBackend::Auto | CaptureBackend::Native)
                && portal_reachable;
        let use_pipewire =
            matches!(backend_choice, CaptureBackend::Auto | CaptureBackend::PipeWire)
                && portal_reachable;
        let use_x11 = matches!(backend_choice, CaptureBackend::Auto | CaptureBackend::X11)
            && (!wayland || force_x11_on_wayland())
            && std::env::var("DISPLAY").is_ok();

        if wayland && force_x11_on_wayland() {
            log::warn!(
                "DISPLAYSWARM_FORCE_X11_ON_WAYLAND=1 set: attempting X11 capture on Wayland. \
                 THIS ONLY CAPTURES X11/XWAYLAND WINDOWS, NOT NATIVE WAYLAND APPS. \
                 The virtual monitor (a Wayland surface) will likely NOT be captured."
            );
        }

        // 1. NATIVE portal screencast -- our own portal + PipeWire client.
        //
        //    This is first because it is the only one that can capture a KWin 6
        //    screencast: xcap 0.9.8 receives zero buffers there (it never uses the
        //    portal's `OpenPipeWireRemote` fd, never calls `update_params`, reads
        //    its frames from `datas[0].data()` which is `None` for DMA-BUF, and
        //    offers only `Choice` formats so KWin computes stride 0). See
        //    `native.rs` for the long version.
        if use_native {
            log::info!(
                "Attempting NATIVE portal screencast (Wayland session: {})",
                wayland
            );
            match NativePortalCapturer::new_for(target, token_key, width, height) {
                Ok(native_cap) => {
                    log::info!(
                        "Selected NATIVE portal screencast backend: {}",
                        native_cap.describe_source()
                    );
                    return Ok(Self {
                        backend: LinuxBackend::Native(native_cap),
                        width,
                        height,
                        verdict_logged: false,
                        last_poll: None,
                    });
                }
                Err(e) => {
                    let err_str = format!("{e:?}");
                    let is_portal_crash = crate::capture::portal::is_portal_crash_error(&err_str);
                    if is_portal_crash {
                        log::error!(
                            "FALLBACK STEP 1 (native portal screencast) failed due to KNOWN KDE PLASMA 6 PORTAL BUG:\n{e}"
                        );
                        log::error!(
                            "The xdg-desktop-portal daemon crashed (assertion failed: session->token != NULL).\n\
                             This is an UPSTREAM BUG in xdg-desktop-portal-kde.\n\
                             WORKAROUNDS:\n\
                             1. Use an X11 session (log out, select 'Plasma (X11)' at login)\n\
                             2. Use a different compositor (GNOME, sway, Hyprland, etc.)\n\
                             3. Set DISPLAYSWARM_FORCE_X11_ON_WAYLAND=1 for partial X11-only capture\n\
                             4. Wait for upstream fix"
                        );
                    } else {
                        log::warn!("FALLBACK STEP 1 (native portal screencast) failed: {e:?}");
                    }
                    if use_pipewire {
                        log::warn!("  -> falling back to the xcap PipeWire portal backend");
                    } else {
                        log::warn!(
                            "  -> DISPLAYSWARM_CAPTURE=native pinned this backend; falling through to test pattern"
                        );
                    }
                }
            }
        }

        // 2. Real capture via the XDG Desktop Portal, using xcap.
        //
        //    Kept as a last-resort portal path: it is the only thing that worked
        //    before, and it still works where the compositor offers shared-memory
        //    buffers (GNOME, most non-KDE Wayland sessions). On KWin 6 it will
        //    initialise and then never deliver a frame, which is why the native
        //    backend above is tried first.
        //
        //    This used to be skipped outright on KDE Plasma 6 because
        //    xdg-desktop-portal-kde could crash in PipeWireDelegate.qml. That
        //    silently substituted a synthetic rainbow test pattern, which is
        //    indistinguishable on-screen from a broken video pipeline. It is now
        //    attempted by default and any failure is reported loudly.
        if use_pipewire {
            log::info!("Attempting PipeWire portal capture via xcap (Wayland session: {})", wayland);
            match PipeWirePortalCapturer::new(width, height) {
                Ok(pw_cap) => {
                    log::info!(
                        "Selected PipeWire Portal capture backend (xcap, awaiting first real frame)"
                    );
                    return Ok(Self {
                        backend: LinuxBackend::PipeWire(pw_cap),
                        width,
                        height,
                        verdict_logged: false,
                        last_poll: None,
                    });
                }
                Err(e) => {
                    let err_str = format!("{e:?}");
                    let is_portal_crash = crate::capture::portal::is_portal_crash_error(&err_str);
                    if is_portal_crash {
                        log::error!(
                            "FALLBACK STEP 2 (xcap PipeWire portal) failed due to KNOWN KDE PLASMA 6 PORTAL BUG:\n{e}"
                        );
                    } else {
                        log::warn!("FALLBACK STEP 2 (xcap PipeWire portal) failed: {e:?}");
                    }
                    if is_kde && backend_choice == CaptureBackend::Auto {
                        log::warn!(
                            "KDE Plasma 6 Wayland: the portal is required for real screen capture. \
                             Until it is granted, frames will be the SYNTHETIC TEST PATTERN, not your desktop."
                        );
                    }
                    log::warn!("  -> falling back to X11, then to the synthetic test pattern");
                }
            }
        }

        // 3. X11 / XShm capture (non-Wayland sessions, or forced via DISPLAYSWARM_CAPTURE=x11 or DISPLAYSWARM_FORCE_X11_ON_WAYLAND=1)
        if use_x11 {
            if wayland && force_x11_on_wayland() {
                log::warn!(
                    "Attempting X11 capture on Wayland (DISPLAYSWARM_FORCE_X11_ON_WAYLAND=1). \
                     THIS ONLY CAPTURES X11/XWAYLAND WINDOWS, NOT NATIVE WAYLAND APPS."
                );
            }
            match X11Capturer::new(width, height) {
                Ok(x11_cap) => {
                    log::info!("Selected X11 capture backend");
                    return Ok(Self {
                        backend: LinuxBackend::X11(x11_cap),
                        width,
                        height,
                        verdict_logged: false,
                        last_poll: None,
                    });
                }
                Err(e) => {
                    log::warn!("FALLBACK STEP 3 (X11 / XShm) failed or was rejected: {e:?}");
                    if wayland && force_x11_on_wayland() {
                        log::warn!(
                            "X11 capture on Wayland failed (expected: XWayland root window capture is typically blocked). \
                             Falling back to the synthetic test pattern."
                        );
                    } else {
                        log::warn!("  -> falling back to the synthetic test pattern");
                    }
                }
            }
        }

        // 4. Synthetic pattern. Make it obvious that this is not your desktop.
        log::warn!(
            "*** USING SYNTHETIC TEST PATTERN - THIS IS NOT YOUR SCREEN *** \
             (reason: no real capture backend available; see the warnings above)"
        );
        Ok(Self {
            backend: LinuxBackend::Fallback(MockCapturer::new(width, height)),
            width,
            height,
            verdict_logged: false,
                        last_poll: None,
        })
    }
}

impl LinuxCapturer {
    pub fn backend_name(&self) -> &'static str {
        match &self.backend {
            LinuxBackend::X11(x11) => {
                if x11.is_shm {
                    "X11 (MIT-SHM)"
                } else {
                    "X11 (XGetImage)"
                }
            }
            LinuxBackend::Native(n) => {
                if n.has_real_frames() {
                    "Native Portal Screencast (live, real frames)"
                } else if n.has_failed() {
                    "Native Portal Screencast (FAILED - see the error above)"
                } else {
                    "Native Portal Screencast (awaiting the portal dialog - NOT your screen)"
                }
            }
            LinuxBackend::PipeWire(pw) => {
                if pw.has_real_frames() {
                    "PipeWire (Portal via xcap, live)"
                } else {
                    "PipeWire (Portal via xcap, awaiting frames - showing test pattern)"
                }
            }
            LinuxBackend::Fallback(_) => "Synthetic Test Pattern (NOT your screen)",
        }
    }

    /// The single most useful line in the whole capture path: which backend is
    /// running, and whether the user is about to see their desktop or a
    /// placeholder. Emitted once, on the first frame the backend manages to
    /// produce, because that is the earliest moment the answer is knowable.
    fn log_verdict_once(&mut self) {
        if self.verdict_logged {
            return;
        }
        self.verdict_logged = true;
        let name = self.backend_name();
        let real = match &self.backend {
            // The xcap and native paths both finish their portal handshake
            // asynchronously, so "produced a frame" and "is showing the desktop"
            // are not the same statement here. Only claim what is true.
            LinuxBackend::Native(n) => n.has_real_frames(),
            LinuxBackend::PipeWire(pw) => pw.has_real_frames(),
            LinuxBackend::X11(_) => true,
            LinuxBackend::Fallback(_) => false,
        };
        if real {
            log::info!("CAPTURE VERDICT: backend = {name} -- DELIVERING REAL FRAMES of your screen");
        } else {
            log::warn!(
                "CAPTURE VERDICT: backend = {name} -- NOT DELIVERING REAL FRAMES. \
                 YOU ARE NOT SEEING YOUR SCREEN (see the reason in the lines above)."
            );
        }
    }
}

impl ScreenCapturer for LinuxCapturer {
    fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        let frame = match &mut self.backend {
            LinuxBackend::X11(x11) => x11.capture_raw_frame(),
            LinuxBackend::Native(n) => ScreenCapturer::capture_frame(n),
            LinuxBackend::PipeWire(pw) => pw.capture_frame(),
            LinuxBackend::Fallback(mock) => mock.capture_frame(),
        };
        if frame.is_ok() {
            self.log_verdict_once();
        }
        frame
    }

    /// Event-driven where the backend can be (the native portal path blocks on
    /// its frame channel); the polling backends are paced to ~60 fps instead so
    /// a `next_frame` loop neither spins nor encodes faster than the screen.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Frame>, Box<dyn std::error::Error + Send + Sync>> {
        let frame = match &mut self.backend {
            LinuxBackend::Native(n) => ScreenCapturer::next_frame(n, timeout),
            LinuxBackend::X11(x11) => {
                pace(&mut self.last_poll, timeout);
                x11.capture_raw_frame().map(Some)
            }
            LinuxBackend::PipeWire(pw) => {
                pace(&mut self.last_poll, timeout);
                pw.capture_frame().map(Some)
            }
            LinuxBackend::Fallback(mock) => ScreenCapturer::next_frame(mock, timeout),
        };
        if matches!(frame, Ok(Some(_))) {
            self.log_verdict_once();
        }
        frame
    }

    fn state(&self) -> CaptureState {
        match &self.backend {
            LinuxBackend::Native(n) => ScreenCapturer::state(n),
            _ => CaptureState::Live,
        }
    }

    fn output_region(&self) -> Option<crate::display::OutputRegion> {
        match &self.backend {
            LinuxBackend::Native(n) => ScreenCapturer::output_region(n),
            LinuxBackend::X11(x11) => x11.output_region(),
            _ => None,
        }
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn describe_source(&self) -> String {
        match &self.backend {
            // `X11Capturer` predates the trait and exposes an inherent method, so
            // it is spelled out here rather than going through a blanket impl.
            LinuxBackend::X11(x11) => format!(
                "{} ({}), scaled to {}x{} -- REAL frames",
                x11.describe(),
                if x11.is_shm { "MIT-SHM" } else { "XGetImage" },
                self.width,
                self.height
            ),
            LinuxBackend::Native(n) => ScreenCapturer::describe_source(n),
            LinuxBackend::PipeWire(pw) => ScreenCapturer::describe_source(pw),
            LinuxBackend::Fallback(_) => format!(
                "SYNTHETIC TEST PATTERN {}x{} -- THIS IS NOT YOUR SCREEN",
                self.width, self.height
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scaler_1to1() {
        let w = 4;
        let h = 4;
        let mut src = vec![0u8; (w * h * 4) as usize];
        src[0] = 10;
        src[1] = 20;
        src[2] = 30;
        src[3] = 40;

        let mut dst = vec![0u8; (w * h * 4) as usize];
        Scaler::new().scale(&src, w, h, (w * 4) as usize, &mut dst, w, h);
        assert_eq!(&dst[0..4], &[10, 20, 30, 40]);
    }

    #[test]
    fn test_scaler_downscale_averages() {
        let src_w = 8;
        let src_h = 8;
        let mut src = vec![128u8; (src_w * src_h * 4) as usize];
        src[0] = 255; // Top-left pixel red

        let dst_w = 4;
        let dst_h = 4;
        let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
        Scaler::new().scale(&src, src_w, src_h, (src_w * 4) as usize, &mut dst, dst_w, dst_h);

        assert_eq!(dst.len(), (4 * 4 * 4) as usize);
        // The top-left output pixel averages a 2x2 box of one 255 and three 128s
        // (~160); nearest-neighbour would have returned 255 and lost the mix.
        assert!((dst[0] as i32 - 160).abs() <= 2, "got {}", dst[0]);
        assert_eq!(dst[4], 128);
    }

    #[test]
    fn test_wayland_detection_and_pipewire_portal() {
        assert!(is_pipewire_portal_available());
        let mut pw = PipeWirePortalCapturer::new(640, 480).expect("PipeWire capturer init failed");
        let frame = pw.capture_frame().expect("PipeWire capture failed");
        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 480);
        assert_eq!(frame.format, PixelFormat::Rgba);
        assert_eq!(frame.data.len(), 640 * 480 * 4);
    }

    #[test]
    fn test_x11_probe_rejects_on_xwayland_or_succeeds() {
        // Under Wayland / XWayland, X11Capturer::new must be rejected cleanly by the 1-pixel probe
        if is_wayland_active() {
            let res = X11Capturer::new(320, 240);
            assert!(
                res.is_err(),
                "X11 capture should be rejected under Wayland/XWayland by the 1-pixel probe"
            );
        }
    }

    #[test]
    fn test_linux_capturer_fallback_or_live() {
        let mut cap = LinuxCapturer::new(320, 240).expect("Capturer init failed");
        println!("Active Linux capture backend: {}", cap.backend_name());

        // A freshly selected backend is allowed to report a failure on its first
        // tick: the native portal path finishes its handshake asynchronously, so
        // a portal that is broken or absent is only known one or two frames in.
        // The dispatcher must keep producing a well-formed frame after that, so
        // drive a few ticks and check the first frame it does hand back.
        let mut frame = None;
        for tick in 0..4 {
            match cap.capture_frame() {
                Ok(f) => {
                    frame = Some(f);
                    break;
                }
                Err(e) => println!("tick {tick}: backend reported {e}"),
            }
        }
        let frame = frame.expect("no frame after 4 capture ticks");

        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.data.len(), 320 * 240 * 4);
        assert!(frame.format == PixelFormat::Rgba || frame.format == PixelFormat::Bgra);
    }

    // --- backend selection --------------------------------------------------

    #[test]
    fn test_capture_backend_parse() {
        assert_eq!(CaptureBackend::parse("native"), CaptureBackend::Native);
        assert_eq!(CaptureBackend::parse("NATIVE"), CaptureBackend::Native);
        assert_eq!(CaptureBackend::parse(" Portal-Native "), CaptureBackend::Native);
        // The xcap path keeps every spelling it had before, including "portal".
        assert_eq!(CaptureBackend::parse("pipewire"), CaptureBackend::PipeWire);
        assert_eq!(CaptureBackend::parse("portal"), CaptureBackend::PipeWire);
        assert_eq!(CaptureBackend::parse("x11"), CaptureBackend::X11);
        assert_eq!(CaptureBackend::parse("mock"), CaptureBackend::Mock);
        assert_eq!(CaptureBackend::parse(""), CaptureBackend::Auto);
        assert_eq!(CaptureBackend::parse("nonsense"), CaptureBackend::Auto);
    }

    #[test]
    fn test_capture_output_filter_parse() {
        assert_eq!(parse_output_filter("Virtual-1").as_deref(), Some("Virtual-1"));
        assert_eq!(parse_output_filter("  eDP-1  ").as_deref(), Some("eDP-1"));
        // Unset and blank are the same thing: let the portal ask the user.
        assert_eq!(parse_output_filter(""), None);
        assert_eq!(parse_output_filter("   "), None);
    }

    // --- native portal capturer --------------------------------------------
    //
    // These drive the state machine directly instead of going through `new`,
    // which would block on a real portal handshake. `new` is exercised by
    // `test_linux_capturer_fallback_or_live` above, on whatever this machine
    // actually has.

    /// DRM_FORMAT_XRGB8888, the fourcc KWin picks first for a DRM capture.
    const TEST_FOURCC_XR24: u32 = 0x3432_5258;

    fn test_format(width: u32, height: u32) -> native::CaptureFormat {
        native::CaptureFormat {
            width,
            height,
            format: TEST_FOURCC_XR24,
            frame_rate: native::Fraction {
                num: 60,
                denom: 1,
            },
            is_dmabuf: true,
        }
    }

    /// A `NativePortalCapturer` in the state it is in while the portal dialog is
    /// still open: no session, no frames, no failure. Returns the capturer plus
    /// the two senders, which the caller must keep alive or the channels read as
    /// disconnected.
    fn pending_capturer(
        target_width: u32,
        target_height: u32,
    ) -> (
        NativePortalCapturer,
        cb::Sender<native::RawFrame>,
        cb::Sender<Result<native::NativeScreencast, native::CaptureError>>,
    ) {
        let (frame_tx, frames) = native::frame_channel();
        let (startup_tx, startup_rx) =
            cb::bounded::<Result<native::NativeScreencast, native::CaptureError>>(1);
        let cap = NativePortalCapturer {
            target_width,
            target_height,
            latest: None,
            negotiated: Mutex::new(None),
            startup: Some(startup_rx),
            session: None,
            startup_thread: None,
            cancel: Arc::new(AtomicBool::new(false)),
            frames,
            frames_closed: false,
            scaler: Scaler::new(),
            failure: None,
            failure_reported: false,
            got_real_frame: false,
            last_not_your_screen_log: None,
            region: Arc::new(Mutex::new(None)),
            target_label: "monitor",
        };
        (cap, frame_tx, startup_tx)
    }

    #[test]
    fn portal_request_per_target() {
        let (r, m) = portal_request_for(
            &CaptureTarget::Monitor { output: Some("eDP-1".into()) },
            Some("d/monitor"),
            None,
        );
        assert_eq!(r.source_type, portal::SOURCE_TYPE_MONITOR);
        assert_eq!(r.output_filter.as_deref(), Some("eDP-1"));
        assert_eq!(r.token_key.as_deref(), Some("d/monitor"));
        assert!(m.is_none());

        // The env override wins for back-compat.
        let (r, _) = portal_request_for(
            &CaptureTarget::Monitor { output: Some("eDP-1".into()) },
            None,
            Some("HDMI-A-1".into()),
        );
        assert_eq!(r.output_filter.as_deref(), Some("HDMI-A-1"));
        assert_eq!(r.token_key, None);

        let (r, m) = portal_request_for(
            &CaptureTarget::Virtual { width: 1280, height: 800, refresh_mhz: 59_940 },
            Some("d/virtual"),
            Some("ignored".into()),
        );
        assert_eq!(r.source_type, portal::SOURCE_TYPE_VIRTUAL);
        assert_eq!(r.output_filter, None);
        let m = m.expect("virtual mode");
        assert_eq!((m.width, m.height, m.fps()), (1280, 800, 60));

        let (r, m) = portal_request_for(&CaptureTarget::Window, None, None);
        assert_eq!(r.source_type, portal::SOURCE_TYPE_WINDOW);
        assert!(m.is_none());
    }

    /// Live check against the real desktop portal: `cargo test --release --lib
    /// live_virtual_monitor -- --ignored --nocapture`. Creates a 1600x720
    /// virtual monitor (a portal dialog may need approving), waits until its
    /// region is known and frames arrive at that size, then drops the
    /// capturer, which must remove it.
    #[test]
    #[ignore]
    fn live_virtual_monitor() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
        let (w, h) = (1600, 720);
        let mut cap = LinuxCapturer::new_for(
            &CaptureTarget::Virtual { width: w, height: h, refresh_mhz: 60_000 },
            Some("livetest/virtual"),
            w,
            h,
        )
        .expect("start");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut sized = 0;
        let mut last = (0, 0);
        while Instant::now() < deadline && sized < 5 {
            match cap.next_frame(Duration::from_millis(500)) {
                Ok(Some(f)) => {
                    if (f.width, f.height) != last {
                        println!("frame {}x{}", f.width, f.height);
                        last = (f.width, f.height);
                    }
                    if (f.width, f.height) == (w, h) {
                        sized += 1;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("capture failed: {e}"),
            }
        }
        println!("source: {}", cap.describe_source());
        println!("region: {:?}", cap.output_region());
        assert!(sized > 0, "no {w}x{h} frames (was the portal dialog approved?), last {last:?}");
        let r = cap.output_region().expect("region");
        assert_eq!((r.width, r.height), (w, h));
    }

    /// Live "phone as main screen" and input binding on the running desktop:
    /// `cargo test --release --lib live_primary_and_input -- --ignored
    /// --nocapture`. Set `DISPLAYSWARM_LIVE_PANEL_OFF=1` to also turn the built-in
    /// panel off (the screen goes dark for a few seconds). Checks that the
    /// virtual monitor becomes primary, that input is bound to it (KDE), and
    /// that the layout is back afterwards.
    #[test]
    #[ignore]
    fn live_primary_and_input() {
        use crate::display::{kscreen, layout, primary};
        use crate::input::InputInjector;
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
        let panel_off = std::env::var("DISPLAYSWARM_LIVE_PANEL_OFF").as_deref() == Ok("1");
        let (w, h) = (1600, 720);
        let mut cap = LinuxCapturer::new_for(
            &CaptureTarget::Virtual { width: w, height: h, refresh_mhz: 60_000 },
            Some("livetest/virtual"),
            w,
            h,
        )
        .expect("start");
        let deadline = Instant::now() + Duration::from_secs(30);
        let region = loop {
            let _ = cap.next_frame(Duration::from_millis(200));
            if let Some(r) = cap.output_region().filter(|r| (r.width, r.height) == (w, h)) {
                break r;
            }
            assert!(Instant::now() < deadline, "virtual monitor region never showed up");
        };
        let is_kde = crate::display::env::current_desktop() == crate::display::model::Desktop::Kde;
        let before = layout::current().snapshot().expect("layout");
        println!("region {region:?}; layout before: {before:?}");

        let guard = primary::make_primary(region, panel_off).expect("make_primary");
        println!("primary; phone now at {:?}", guard.region());
        if is_kde {
            let s = kscreen::state().unwrap();
            let phone = kscreen::find_by_region(&s, guard.region()).expect("phone output after make_primary");
            assert_eq!(phone.priority, 1, "phone output is primary");
            if panel_off {
                assert!(s.outputs.iter().filter(|o| o.builtin).all(|o| !o.enabled), "panel off");
            }
            let mut inj = crate::input::linux::LinuxUinputInjector::new(w, h).expect("uinput");
            inj.set_output_region(Some(guard.region()));
            let bound: Vec<String> = inj
                .digitizer_event_nodes()
                .iter()
                .map(|e| crate::input::kwin::bound_output(e).unwrap())
                .collect();
            println!("input bound to {bound:?}");
            assert!(!bound.is_empty() && bound.iter().all(|b| *b == phone.name), "touch/pen bound to the phone");
        }
        std::thread::sleep(Duration::from_secs(2));
        drop(guard);
        if is_kde {
            let now = layout::current().snapshot().unwrap();
            for o in &before.outputs {
                let n = now.output(&o.name).expect("output still there");
                assert_eq!((n.enabled, n.region, n.primary), (o.enabled, o.region, o.primary), "{} restored", o.name);
            }
        }
        assert!(!primary::journal_path().exists(), "journal removed after restore");
        drop(cap);
    }

    #[test]
    fn output_region_and_source_kind_are_reported() {
        let (mut cap, _f, _s) = pending_capturer(1280, 800);
        assert_eq!(cap.output_region(), None, "unknown before the portal answers");
        *cap.region.lock().unwrap() =
            Some(crate::display::OutputRegion { x: 10, y: 20, width: 1280, height: 800 });
        assert_eq!(cap.output_region().unwrap().x, 10);
        assert!(cap.describe_source().starts_with("[monitor]"), "{}", cap.describe_source());
        cap.target_label = "window";
        assert_eq!(cap.output_region(), None, "a window has no monitor region");
        cap.target_label = "virtual monitor";
        assert!(cap.describe_source().starts_with("[virtual monitor]"));
    }

    /// A tightly packed solid-colour raw frame, as the capture thread sends it.
    fn raw(w: u32, h: u32, px: [u8; 4], format: PixelFormat, ts: u64) -> native::RawFrame {
        native::RawFrame {
            data: Arc::new(px.iter().copied().cycle().take((w * h * 4) as usize).collect()),
            width: w,
            height: h,
            stride: (w * 4) as usize,
            format,
            timestamp_us: ts,
        }
    }

    fn is_opaque_black(frame: &Frame) -> bool {
        frame.data.chunks_exact(4).all(|px| px == [0, 0, 0, 0xff])
    }

    #[test]
    fn test_native_capturer_never_blocks_and_never_fakes_a_screen() {
        // The portal dialog can stay open for minutes. `capture_frame` is called
        // from the async video loop, so it must return promptly and must not
        // pretend to be showing a desktop.
        let (mut cap, _frame_tx, _startup_tx) = pending_capturer(64, 48);

        let started = Instant::now();
        let frame = cap.capture_frame().expect("must not error while waiting");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "capture_frame blocked for {:?} while the portal dialog was open",
            started.elapsed()
        );

        // Black, at the encoder's size, and explicitly not the desktop.
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 48);
        assert_eq!(frame.format, PixelFormat::Rgba);
        assert_eq!(frame.data.len(), 64 * 48 * 4);
        assert!(is_opaque_black(&frame), "expected an opaque black placeholder");
        assert!(!cap.has_real_frames());

        // Before a format is negotiated, size falls back to the requested target.
        assert_eq!(cap.width(), 64);
        assert_eq!(cap.height(), 48);

        // And `describe_source` -- the line the host log prints at startup --
        // must not claim a working capture.
        let described = cap.describe_source();
        assert!(described.contains("native"), "{described}");
        assert!(
            described.contains("NOT your screen"),
            "describe_source must admit this is not the desktop: {described}"
        );
    }

    #[test]
    fn test_native_capturer_legacy_reuses_last_frame_and_scales_to_encoder_size() {
        // An 8x8 desktop fed into a 4x4 encoder.
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(4, 4);
        *cap.negotiated.lock().unwrap() = Some(test_format(8, 8));

        frame_tx.send(raw(8, 8, [200, 100, 50, 0xff], PixelFormat::Bgra, 1234)).unwrap();

        let first = cap.capture_frame().expect("capture failed");
        assert!(cap.has_real_frames());
        // Scaled to the size the encoder was built with, not the capture size.
        assert_eq!(first.width, 4);
        assert_eq!(first.height, 4);
        assert_eq!(first.data.len(), 4 * 4 * 4);
        // The format is the one the frame carried, not a hard-coded RGBA.
        assert_eq!(first.format, PixelFormat::Bgra);
        assert_eq!(first.timestamp_us, 1234, "capture timestamp must be carried through");
        assert!(first.data.chunks_exact(4).all(|px| px == [200, 100, 50, 0xff]));

        // `width`/`height` report the real negotiated size, not the encoder's.
        assert_eq!(cap.width(), 8);
        assert_eq!(cap.height(), 8);
        let described = cap.describe_source();
        assert!(described.contains("REAL frames"), "{described}");
        assert!(described.contains("8x8"), "{described}");
        assert!(described.contains("dmabuf"), "{described}");
        assert!(described.contains("BGRA"), "{described}");

        // Legacy polling API: nothing new replays the last frame.
        let second = cap.capture_frame().expect("frame reuse must not error");
        assert_eq!(second.data, first.data, "the last real frame must be replayed");
        assert!(second.timestamp_us >= first.timestamp_us);
    }

    #[test]
    fn test_native_next_frame_is_event_driven_and_never_replays() {
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(8, 8);
        assert_eq!(cap.state(), CaptureState::AwaitingPermission);

        // Portal dialog open: no frame, no placeholder, and it honours the timeout.
        let started = Instant::now();
        assert!(cap.next_frame(Duration::from_millis(30)).unwrap().is_none());
        assert!(started.elapsed() >= Duration::from_millis(25), "must wait for the timeout");
        assert_eq!(cap.state(), CaptureState::AwaitingPermission);

        // Two queued frames: only the newest is returned, and it is not scaled
        // (same size) and keeps its own capture timestamp and format.
        frame_tx.send(raw(8, 8, [1, 1, 1, 255], PixelFormat::Bgra, 10)).unwrap();
        frame_tx.send(raw(8, 8, [2, 2, 2, 255], PixelFormat::Rgba, 20)).unwrap();
        let f = cap.next_frame(Duration::from_millis(100)).unwrap().expect("a frame");
        assert_eq!((f.width, f.height), (8, 8));
        assert_eq!(f.timestamp_us, 20);
        assert_eq!(f.format, PixelFormat::Rgba);
        assert!(f.data.chunks_exact(4).all(|px| px == [2, 2, 2, 255]));
        assert_eq!(cap.state(), CaptureState::Live);

        // Nothing new: None, not the previous frame again.
        assert!(cap.next_frame(Duration::from_millis(10)).unwrap().is_none());
        assert_eq!(cap.state(), CaptureState::Live);
    }

    #[test]
    fn test_native_next_frame_wakes_when_a_frame_arrives() {
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(4, 4);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            frame_tx.send(raw(4, 4, [9, 9, 9, 255], PixelFormat::Bgra, 5)).unwrap();
            frame_tx
        });
        let started = Instant::now();
        let f = cap.next_frame(Duration::from_secs(5)).unwrap();
        assert!(f.is_some());
        assert!(started.elapsed() < Duration::from_secs(2), "must wake on arrival, not the timeout");
        let _keep = t.join().unwrap();
    }

    #[test]
    fn test_native_next_frame_letterboxes_instead_of_stretching() {
        // 16:9 desktop into a 4:3 encoder: bars top and bottom, colour intact.
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(80, 60);
        frame_tx.send(raw(160, 90, [200, 10, 20, 255], PixelFormat::Bgra, 1)).unwrap();
        let f = cap.next_frame(Duration::from_millis(100)).unwrap().expect("frame");
        assert_eq!((f.width, f.height), (80, 60));
        assert_eq!(&f.data[0..4], &[0, 0, 0, 255], "top bar must be black");
        let mid = (30 * 80 + 40) * 4;
        assert_eq!(&f.data[mid..mid + 4], &[200, 10, 20, 255]);
    }

    #[test]
    fn test_native_next_frame_one_to_one_moves_the_pixels() {
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(8, 8);
        let r = raw(8, 8, [5, 6, 7, 255], PixelFormat::Bgra, 3);
        let ptr = r.data.as_ptr();
        frame_tx.send(r).unwrap();
        let f = cap.next_frame(Duration::from_millis(100)).unwrap().expect("frame");
        assert_eq!(f.data.as_ptr(), ptr, "1:1 with a sole owner must not copy the pixels");
    }

    #[test]
    fn test_native_next_frame_handles_padded_stride() {
        // 6x2 picture with 32-byte rows (8 px) into a 6x2 encoder.
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(6, 2);
        let mut data = vec![0xEEu8; 32 * 2];
        for y in 0..2 {
            for x in 0..6 {
                data[y * 32 + x * 4..y * 32 + x * 4 + 4].copy_from_slice(&[1, 2, 3, 255]);
            }
        }
        frame_tx
            .send(native::RawFrame {
                data: Arc::new(data),
                width: 6,
                height: 2,
                stride: 32,
                format: PixelFormat::Bgra,
                timestamp_us: 1,
            })
            .unwrap();
        let f = cap.next_frame(Duration::from_millis(100)).unwrap().expect("frame");
        assert_eq!(f.data.len(), 6 * 2 * 4);
        assert!(f.data.chunks_exact(4).all(|px| px == [1, 2, 3, 255]));
    }

    #[test]
    fn test_native_next_frame_reports_failure_once_then_state_stays_failed() {
        let (mut cap, _frame_tx, _startup_tx) = pending_capturer(8, 8);
        cap.failure = Some("portal did not answer".to_string());
        assert!(cap.next_frame(Duration::from_millis(5)).is_err());
        assert!(cap.next_frame(Duration::from_millis(5)).unwrap().is_none());
        assert!(matches!(cap.state(), CaptureState::Failed(m) if m.contains("portal")));
    }

    #[test]
    fn test_native_capturer_reports_a_failure_once_then_keeps_streaming_black() {
        let (mut cap, _frame_tx, _startup_tx) = pending_capturer(8, 8);
        cap.failure = Some("portal did not answer within 300s".to_string());

        // The caller sees the full cause exactly once...
        let err = cap
            .capture_frame()
            .expect_err("the first tick after a failure must report it");
        assert!(err.to_string().contains("300s"), "{err}");

        // ...and is not then buried under 60 identical warnings a second.
        let frame = cap.capture_frame().expect("must not error repeatedly");
        assert!(is_opaque_black(&frame));
        assert!(cap.has_failed());
        assert!(cap.describe_source().contains("FAILED"));
        assert!(cap.describe_source().contains("NOT your screen"));
    }

    #[test]
    fn test_native_capturer_discards_a_frame_that_does_not_match_its_dimensions() {
        let (mut cap, frame_tx, _startup_tx) = pending_capturer(8, 8);
        // Claims 8x8 but carries 4x4 worth of bytes: must be discarded rather
        // than read past the end of the buffer, and must not be retried.
        let mut bad = raw(8, 8, [7, 7, 7, 255], PixelFormat::Bgra, 1);
        bad.data = Arc::new(vec![7u8; 4 * 4 * 4]);
        frame_tx.send(bad).unwrap();

        let frame = cap
            .capture_frame()
            .expect("a bad frame must not surface as an error every tick");
        assert!(is_opaque_black(&frame), "expected the placeholder, not garbage");
        assert!(cap.latest.is_none(), "the mismatched frame must be dropped");

        // A correctly-sized frame on the next tick works as normal.
        frame_tx.send(raw(8, 8, [9, 9, 9, 255], PixelFormat::Bgra, 2)).unwrap();
        let good = cap.capture_frame().expect("capture failed");
        assert_eq!(good.data.len(), 8 * 8 * 4);
        assert!(good.data.chunks_exact(4).all(|px| px[0] == 9));

        // And the event-driven API drops the same kind of frame silently.
        let mut bad = raw(8, 8, [7, 7, 7, 255], PixelFormat::Bgra, 3);
        bad.data = Arc::new(vec![7u8; 16]);
        frame_tx.send(bad).unwrap();
        assert!(cap.next_frame(Duration::from_millis(50)).unwrap().is_none());
    }

    #[test]
    fn test_force_x11_on_wayland_env_var() {
        // Test default (unset) returns false
        std::env::remove_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND");
        assert!(!force_x11_on_wayland());

        // Test "1" returns true
        std::env::set_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND", "1");
        assert!(force_x11_on_wayland());

        // Test "true" returns true (case insensitive)
        std::env::set_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND", "true");
        assert!(force_x11_on_wayland());
        std::env::set_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND", "TRUE");
        assert!(force_x11_on_wayland());

        // Test "false" returns false
        std::env::set_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND", "false");
        assert!(!force_x11_on_wayland());

        // Test other values return false
        std::env::set_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND", "yes");
        assert!(!force_x11_on_wayland());

        std::env::remove_var("DISPLAYSWARM_FORCE_X11_ON_WAYLAND");
    }
}

