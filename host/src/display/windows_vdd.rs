//! Per-phone virtual monitors on Windows through the open-source Virtual
//! Display Driver ("VDD", VirtualDrivers/Virtual-Display-Driver, MIT; hardware
//! id `Root\MttVDD`). The user installs it (winget or the project's release);
//! DisplaySwarm does not bundle it.
//!
//! # How the driver is controlled
//!
//! Protocol facts come from reading the driver's source at release tag
//! `25.7.23` (commit `d437ebc`) and `master` (`d724496`), not from running it;
//! Nothing here has been verified on Windows yet.
//!
//! * Settings: `vdd_settings.xml` in the directory named by the registry value
//!   `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`, else
//!   `C:\VirtualDisplayDriver`. `<monitors><count>` is how many monitors the
//!   driver creates (0 is read as 1); every `<resolution>` (width, height,
//!   `refresh_rate`, fractional allowed) and every `<g_refresh_rate>` (whole Hz,
//!   applied to every resolution) is a mode each monitor offers. **The driver
//!   reads the file only when its device is added** (`DeviceAdd`).
//! * Control pipe `\\.\pipe\MTTVirtualDisplayPipe`: message mode, one UTF-16LE
//!   command per connection, the driver answers (UTF-8, or log lines) and
//!   disconnects. `SETDISPLAYCOUNT n` rewrites `<count>` and re-initialises the
//!   adapter; `PING` answers `PONG`. `RELOAD_DRIVER` is **never sent**: it
//!   crashes the driver (upstream issue #351). `SETDISPLAYCOUNT` runs the same
//!   reload function, so it may restart the driver host too; the other
//!   project's control app treats it as a 2-8 s operation and spaces
//!   reload commands 3 s apart, and so does this module.
//!
//! # What that means here
//!
//! * The driver has no "add one monitor of this size": the phone's mode is
//!   written into the settings file, then the count is raised, and every VDD
//!   monitor is re-created. Other phones' monitors blink and their capture is
//!   rebuilt ([`VirtualMonitor::locate`] re-finds them).
//! * Lowering the count removes the *last* monitors. A phone that leaves while
//!   a later one still streams keeps its slot: its monitor is detached from the
//!   desktop instead, and the count drops once the later phones are gone.
//! * The user's settings file is backed up before the first change and put back
//!   byte for byte when the last phone leaves; a crash leaves the backup, which
//!   [`VddProvider::recover`] restores at the next start.
//!
//! Everything but [`VddSystem`]'s Windows implementation (`windows_vdd_sys`) is
//! plain logic, tested on Linux with [`fake::FakeVdd`].

use super::model::{Availability, Mode};
use super::virtual_monitor::{
    hz_text, mode_label, MonitorPlace, ProviderKind, VirtualMonitor, VirtualMonitorProvider, NOT_INSTALLED_PREFIX,
};
use super::{DisplayError, OutputRegion};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PIPE_NAME: &str = r"\\.\pipe\MTTVirtualDisplayPipe";
pub const HARDWARE_ID: &str = r"Root\MttVDD";
pub const REGISTRY_KEY: &str = r"SOFTWARE\MikeTheTech\VirtualDisplayDriver";
pub const REGISTRY_PATH_VALUE: &str = "VDDPATH";
pub const DEFAULT_DIR: &str = r"C:\VirtualDisplayDriver";
pub const SETTINGS_FILE: &str = "vdd_settings.xml";
/// The user's own settings, kept next to the file while DisplaySwarm has changed it.
pub const BACKUP_SUFFIX: &str = ".displayswarm-backup";

/// Driver builds DisplaySwarm was written against, by the `DriverDate` Windows
/// records at install. Source-reviewed, **not tested on a machine**: release
/// 25.7.23 and the signed package the project's control app ships (INF
/// `DriverVer = 08/14/2025,22.50.22.79`). `DISPLAYSWARM_VDD_ANY_VERSION=1` skips
/// the check.
pub const SUPPORTED_FROM: (u16, u8, u8) = (2025, 7, 1);
pub const SUPPORTED_UNTIL: (u16, u8, u8) = (2025, 12, 31);
pub const ANY_VERSION_ENV: &str = "DISPLAYSWARM_VDD_ANY_VERSION";

/// Spacing between reload-triggering commands (driver stability).
pub const RELOAD_COOLDOWN: Duration = Duration::from_secs(3);
/// How long a new monitor may take to show up after a reload.
pub const APPEAR_TIMEOUT: Duration = Duration::from_secs(15);
/// After this long present but detached, the monitor is attached by hand.
pub const ATTACH_AFTER: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Settings file (pure string editing, comment-aware)
// ---------------------------------------------------------------------------

/// `xml` with every comment's bytes blanked, same length, so tag searches skip
/// commented-out elements while offsets still match the original.
fn mask_comments(xml: &str) -> String {
    let mut out = xml.as_bytes().to_vec();
    let mut at = 0;
    while let Some(start) = xml[at..].find("<!--").map(|i| i + at) {
        let end = xml[start + 4..].find("-->").map(|i| i + start + 4 + 3).unwrap_or(xml.len());
        for b in &mut out[start..end] {
            *b = b' ';
        }
        at = end;
    }
    // Only ASCII bytes were written over whole comment spans.
    String::from_utf8(out).unwrap_or_else(|_| " ".repeat(xml.len()))
}

/// Byte ranges of the first `<tag>...</tag>` at or after `from` in the masked
/// text: (start of `<tag>`, start of content, end of content, end of `</tag>`).
fn element(masked: &str, tag: &str, from: usize) -> Option<(usize, usize, usize, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = masked.get(from..)?.find(&open)? + from;
    let content = start + open.len();
    let end = masked[content..].find(&close)? + content;
    Some((start, content, end, end + close.len()))
}

fn text_of<'a>(xml: &'a str, masked: &str, tag: &str, within: (usize, usize)) -> Option<&'a str> {
    let (_, a, b, close) = element(masked, tag, within.0)?;
    (close <= within.1).then(|| xml[a..b].trim())
}

/// The monitor count as the driver reads it: `<count>` in `<monitors>`, absent
/// or 0 meaning 1.
pub fn effective_count(xml: &str) -> u32 {
    let masked = mask_comments(xml);
    element(&masked, "monitors", 0)
        .and_then(|(_, a, b, _)| text_of(xml, &masked, "count", (a, b)))
        .and_then(|t| t.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1)
}

/// `xml` with the monitor count set to `count` (the element is created when
/// missing). Nothing else changes.
pub fn set_count(xml: &str, count: u32) -> String {
    let masked = mask_comments(xml);
    if let Some((_, ma, mb, _)) = element(&masked, "monitors", 0) {
        if let Some((_, a, b, close)) = element(&masked, "count", ma) {
            if close <= mb {
                return format!("{}{count}{}", &xml[..a], &xml[b..]);
            }
        }
        return format!("{}<count>{count}</count>{}", &xml[..ma], &xml[ma..]);
    }
    match masked.find("</vdd_settings>") {
        Some(i) => format!("{}  <monitors><count>{count}</count></monitors>\n{}", &xml[..i], &xml[i..]),
        None => xml.to_string(),
    }
}

/// One `<resolution>` entry. `refresh_mhz` is `None` when the entry has no
/// usable `refresh_rate` (the driver then pairs it with the globals only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: Option<u32>,
}

fn parse_hz_mhz(text: &str) -> Option<u32> {
    let v: f64 = text.trim().parse().ok()?;
    (v > 0.0 && v < 1000.0).then(|| (v * 1000.0).round() as u32)
}

pub fn listed_modes(xml: &str) -> Vec<ListedMode> {
    let masked = mask_comments(xml);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some((_, a, b, close)) = element(&masked, "resolution", at) {
        let num = |tag: &str| text_of(xml, &masked, tag, (a, b)).and_then(|t| t.parse::<u32>().ok());
        if let (Some(width), Some(height)) = (num("width"), num("height")) {
            let refresh_mhz = text_of(xml, &masked, "refresh_rate", (a, b)).and_then(parse_hz_mhz);
            out.push(ListedMode { width, height, refresh_mhz });
        }
        at = close;
    }
    out
}

/// Every `<g_refresh_rate>`, in whole Hz (the driver reads them with `stoi`).
pub fn global_rates_hz(xml: &str) -> Vec<u32> {
    let masked = mask_comments(xml);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some((_, a, b, close)) = element(&masked, "g_refresh_rate", at) {
        if let Some(v) = xml[a..b].trim().split('.').next().and_then(|t| t.parse().ok()) {
            out.push(v);
        }
        at = close;
    }
    out
}

/// Whether a monitor of the driver configured by `xml` offers `mode`.
pub fn offers(xml: &str, mode: Mode) -> bool {
    let listed = listed_modes(xml);
    let size_listed = listed.iter().any(|m| m.width == mode.width && m.height == mode.height);
    let exact = listed.iter().any(|m| {
        m.width == mode.width && m.height == mode.height && m.refresh_mhz.is_some_and(|r| r.abs_diff(mode.refresh_mhz) <= 10)
    });
    let by_global =
        size_listed && mode.refresh_mhz % 1000 == 0 && global_rates_hz(xml).contains(&(mode.refresh_mhz / 1000));
    exact || by_global
}

/// `xml` with `mode` listed among the resolutions (unchanged when already
/// offered). The new entry copies the file's indentation.
pub fn add_mode(xml: &str, mode: Mode) -> String {
    if offers(xml, mode) {
        return xml.to_string();
    }
    let masked = mask_comments(xml);
    let entry = |indent: &str| {
        format!(
            "{i}<resolution>\n{i}    <width>{}</width>\n{i}    <height>{}</height>\n{i}    <refresh_rate>{}</refresh_rate>\n{i}</resolution>\n",
            mode.width,
            mode.height,
            hz_text(mode.refresh_mhz),
            i = indent
        )
    };
    if let Some(close) = masked.find("</resolutions>") {
        // Indent like the line holding `</resolutions>`, one level deeper.
        let line_start = xml[..close].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let outer = &xml[line_start..close];
        let outer = if outer.trim().is_empty() { outer } else { "" };
        let inner = format!("{outer}    ");
        return format!("{}{}{}{}", &xml[..line_start], entry(&inner), outer, &xml[close..]);
    }
    match masked.find("</vdd_settings>") {
        Some(i) => format!("{}    <resolutions>\n{}    </resolutions>\n{}", &xml[..i], entry("        "), &xml[i..]),
        None => xml.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Driver version pin
// ---------------------------------------------------------------------------

/// The installed driver as Windows reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverInfo {
    /// Device instance id (`ROOT\DISPLAY\0000`).
    pub instance_id: String,
    /// `DriverVersion` (`22.50.22.79`).
    pub version: String,
    /// `DriverDate` as recorded (`8-14-2025`).
    pub date: String,
    /// The device is started (`DN_STARTED`) with no problem code.
    pub running: bool,
    /// Device Manager problem code, 0 = none.
    pub problem: u32,
}

/// `M-D-YYYY` or `MM/DD/YYYY` to (year, month, day).
pub fn parse_driver_date(s: &str) -> Option<(u16, u8, u8)> {
    let parts: Vec<&str> = s.trim().split(|c| c == '-' || c == '/').collect();
    let [m, d, y] = parts.as_slice() else { return None };
    let (m, d, y) = (m.parse::<u8>().ok()?, d.parse::<u8>().ok()?, y.parse::<u16>().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && y >= 2000).then_some((y, m, d))
}

fn ymd(d: (u16, u8, u8)) -> String {
    format!("{:04}-{:02}-{:02}", d.0, d.1, d.2)
}

/// Whether `driver` is one DisplaySwarm supports, and if not, what to do.
pub fn check_driver(driver: Option<&DriverInfo>, allow_any_version: bool) -> Availability {
    let Some(d) = driver else {
        return Availability::No(format!("{NOT_INSTALLED_PREFIX}the Virtual Display Driver is not installed."));
    };
    if !d.running {
        return Availability::No(format!(
            "The Virtual Display Driver is installed but not running{}. Enable it in Device Manager (Display adapters) or reinstall it.",
            if d.problem != 0 { format!(" (Device Manager problem code {})", d.problem) } else { String::new() }
        ));
    }
    if allow_any_version {
        return Availability::Yes;
    }
    let range = format!("{} to {}", ymd(SUPPORTED_FROM), ymd(SUPPORTED_UNTIL));
    match parse_driver_date(&d.date) {
        Some(date) if date >= SUPPORTED_FROM && date <= SUPPORTED_UNTIL => Availability::Yes,
        Some(date) => Availability::No(format!(
            "Virtual Display Driver {} (built {}) is {} than the builds DisplaySwarm supports ({range}). Install release 25.7.23, or set {ANY_VERSION_ENV}=1 to try this one anyway.",
            d.version,
            ymd(date),
            if date < SUPPORTED_FROM { "older" } else { "newer" }
        )),
        None => Availability::No(format!(
            "The Virtual Display Driver's build date ({:?}) could not be read, so DisplaySwarm cannot tell whether it supports it. Set {ANY_VERSION_ENV}=1 to try anyway.",
            d.date
        )),
    }
}

// ---------------------------------------------------------------------------
// Monitor identity
// ---------------------------------------------------------------------------

/// One display target (monitor connector) as `QueryDisplayConfig` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTarget {
    /// The target belongs to the Virtual Display Driver's adapter.
    pub vdd: bool,
    /// `DISPLAYCONFIG_PATH_TARGET_INFO::id`, unique per adapter.
    pub target_id: u32,
    /// Part of the desktop (an active path).
    pub active: bool,
    /// `\\.\DISPLAYn` while active.
    pub gdi_name: Option<String>,
    /// Virtual-screen rectangle while active.
    pub region: Option<OutputRegion>,
}

impl DisplayTarget {
    fn place(&self) -> Option<MonitorPlace> {
        match (&self.gdi_name, self.region, self.active) {
            (Some(n), Some(r), true) => Some(MonitorPlace { gdi_name: n.clone(), region: r }),
            _ => None,
        }
    }
}

/// The VDD targets in connector order. UNVERIFIED: that target ids follow the
/// driver's `ConnectorIndex` order; it is the natural reading of IddCx and is
/// only the fallback when the id itself changed across a reload.
fn vdd_targets(all: &[DisplayTarget]) -> Vec<&DisplayTarget> {
    let mut v: Vec<&DisplayTarget> = all.iter().filter(|t| t.vdd).collect();
    v.sort_by_key(|t| t.target_id);
    v
}

/// Which target is the monitor at `connector` (0-based) after a count change:
/// the only VDD target that was not there before, or, when a reload renumbered
/// several, the one at that position in connector order.
pub fn find_added(before: &[DisplayTarget], after: &[DisplayTarget], connector: u32) -> Option<DisplayTarget> {
    let known: Vec<u32> = before.iter().filter(|t| t.vdd).map(|t| t.target_id).collect();
    let fresh: Vec<&DisplayTarget> = vdd_targets(after).into_iter().filter(|t| !known.contains(&t.target_id)).collect();
    match fresh.as_slice() {
        [one] => Some((*one).clone()),
        [] => None,
        _ => vdd_targets(after).get(connector as usize).map(|t| (*t).clone()),
    }
}

/// The target of a known monitor now: by id, else by connector position.
pub fn relocate(all: &[DisplayTarget], target_id: u32, connector: u32) -> Option<DisplayTarget> {
    all.iter()
        .find(|t| t.vdd && t.target_id == target_id)
        .or_else(|| vdd_targets(all).get(connector as usize).copied())
        .cloned()
}

/// Where to put a monitor that Windows attached on top of another: right of
/// everything else.
pub fn right_of_desktop(all: &[DisplayTarget], except: u32) -> (i32, i32) {
    let right = all
        .iter()
        .filter(|t| t.active && !(t.vdd && t.target_id == except))
        .filter_map(|t| t.region)
        .map(|r| (r.x + r.width as i32, r.y))
        .max_by_key(|p| p.0);
    right.map(|(x, _)| (x, 0)).unwrap_or((0, 0))
}

// ---------------------------------------------------------------------------
// Slots: which connector belongs to which phone
// ---------------------------------------------------------------------------

/// The monitors DisplaySwarm added, by connector. Connectors below `baseline` are
/// the user's own; slot `k` is connector `baseline + k`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Slots {
    pub baseline: u32,
    slots: Vec<Option<(u64, Mode)>>,
}

impl Slots {
    pub fn new(baseline: u32) -> Self {
        Slots { baseline, slots: Vec::new() }
    }

    /// Monitors the driver should create.
    pub fn count(&self) -> u32 {
        self.baseline + self.slots.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    pub fn modes(&self) -> Vec<Mode> {
        self.slots.iter().flatten().map(|(_, m)| *m).collect()
    }

    /// (monitor id, connector, mode) of every held slot.
    pub fn entries(&self) -> Vec<(u64, u32, Mode)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|(id, m)| (id, self.baseline + i as u32, m)))
            .collect()
    }

    /// Takes the first free slot (a detached monitor a phone left behind) or a
    /// new one, and returns its connector.
    pub fn take(&mut self, id: u64, mode: Mode) -> u32 {
        let i = match self.slots.iter().position(Option::is_none) {
            Some(i) => {
                self.slots[i] = Some((id, mode));
                i
            }
            None => {
                self.slots.push(Some((id, mode)));
                self.slots.len() - 1
            }
        };
        self.baseline + i as u32
    }

    /// Frees `id`'s slot; trailing free slots are dropped (the driver can only
    /// remove its last monitors). Returns the freed connector.
    pub fn free(&mut self, id: u64) -> Option<u32> {
        let i = self.slots.iter().position(|s| s.is_some_and(|(sid, _)| sid == id))?;
        self.slots[i] = None;
        while self.slots.last().is_some_and(Option::is_none) {
            self.slots.pop();
        }
        Some(self.baseline + i as u32)
    }

    /// The settings file for the current slots: the user's `original` with our
    /// count and modes.
    pub fn settings(&self, original: &str) -> String {
        let mut xml = set_count(original, self.count());
        for m in self.modes() {
            xml = add_mode(&xml, m);
        }
        xml
    }
}

// ---------------------------------------------------------------------------
// System access (Windows in `windows_vdd_sys`, a fake in tests)
// ---------------------------------------------------------------------------

/// What happened to a pipe command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeResult {
    /// The driver took the command; its reply text (possibly empty).
    Sent(String),
    /// There is no pipe (driver stopped, or too old to have one).
    NoPipe,
    Failed(String),
}

pub trait VddSystem: Send + Sync + 'static {
    /// The installed driver, `None` when no `Root\MttVDD` device exists.
    fn driver(&self) -> Option<DriverInfo>;
    /// The settings directory (`VDDPATH`, else [`DEFAULT_DIR`]).
    fn settings_dir(&self) -> PathBuf;
    fn read(&self, path: &std::path::Path) -> std::io::Result<String>;
    /// Replaces the file (atomically where the platform allows).
    fn write(&self, path: &std::path::Path, text: &str) -> std::io::Result<()>;
    fn remove(&self, path: &std::path::Path);
    /// Whether `path` can be opened for writing (without changing it).
    fn writable(&self, path: &std::path::Path) -> bool;
    /// Whether the driver's pipe server is up, without connecting (a
    /// connection would use up the one instance the driver serves at a time).
    fn pipe_present(&self) -> bool;
    /// Sends one command on [`PIPE_NAME`] and reads the answer until the
    /// driver disconnects (a disconnect after the write counts as sent).
    fn pipe(&self, command: &str) -> PipeResult;
    /// Disables and re-enables the device (needs administrator rights).
    fn restart_device(&self, instance_id: &str) -> Result<(), String>;
    fn targets(&self) -> Vec<DisplayTarget>;
    /// Makes an inactive target part of the desktop.
    fn attach(&self, target_id: u32) -> Result<(), String>;
    /// Sets the mode (and position, when given) of an attached monitor.
    fn set_mode(&self, gdi_name: &str, mode: Mode, position: Option<(i32, i32)>) -> Result<(), String>;
    /// Takes an attached monitor off the desktop (it stays plugged in).
    fn detach(&self, gdi_name: &str) -> Result<(), String>;
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

struct State {
    /// The user's file as it was before DisplaySwarm's first change.
    original: Option<String>,
    slots: Slots,
    /// The file as the driver last loaded it (after our last reload).
    loaded: Option<String>,
    next_id: u64,
    last_reload: Option<Instant>,
    /// Target ids by monitor id, re-resolved on every add.
    targets: Vec<(u64, u32)>,
}

struct Inner<S: VddSystem> {
    sys: S,
    state: Mutex<State>,
    allow_any_version: bool,
    /// Tests release synchronously; the real host hands it to a thread (a
    /// release reloads the driver, which must not block a capture thread's
    /// join or the async runtime).
    release_inline: bool,
}

/// The Virtual Display Driver as a [`VirtualMonitorProvider`].
pub struct VddProvider<S: VddSystem> {
    inner: Arc<Inner<S>>,
}

impl<S: VddSystem> Clone for VddProvider<S> {
    fn clone(&self) -> Self {
        VddProvider { inner: self.inner.clone() }
    }
}

fn io_err(context: &str, detail: impl std::fmt::Display) -> DisplayError {
    DisplayError::Io { context: context.into(), detail: detail.to_string() }
}

impl<S: VddSystem> VddProvider<S> {
    pub fn new(sys: S, allow_any_version: bool) -> Self {
        Self::build(sys, allow_any_version, false)
    }

    fn build(sys: S, allow_any_version: bool, release_inline: bool) -> Self {
        VddProvider {
            inner: Arc::new(Inner {
                sys,
                state: Mutex::new(State {
                    original: None,
                    slots: Slots::default(),
                    loaded: None,
                    next_id: 0,
                    last_reload: None,
                    targets: Vec::new(),
                }),
                allow_any_version,
                release_inline,
            }),
        }
    }

    pub fn settings_path(&self) -> PathBuf {
        self.inner.sys.settings_dir().join(SETTINGS_FILE)
    }

    fn backup_path(&self) -> PathBuf {
        self.inner.sys.settings_dir().join(format!("{SETTINGS_FILE}{BACKUP_SUFFIX}"))
    }

    /// Startup: a previous run crashed while it had monitors, so the user's file
    /// is still in the backup. Put it back and set the count it had.
    pub fn recover(&self) {
        let sys = &self.inner.sys;
        let Ok(original) = sys.read(&self.backup_path()) else { return };
        log::warn!("Restoring the Virtual Display Driver settings a previous DisplaySwarm run left changed");
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = self.put_back(&mut st, &original) {
            log::warn!("Could not restore the Virtual Display Driver settings: {}", e.summary());
        }
    }

    /// Writes the user's file back, sets its count, and writes it once more:
    /// `SETDISPLAYCOUNT` makes the driver rewrite the file, which is then
    /// replaced by the original bytes.
    /// A failed reload is reported but does not stop the file from being put
    /// back: the driver then picks the user's settings up at its next start.
    fn put_back(&self, st: &mut State, original: &str) -> Result<(), DisplayError> {
        let sys = &self.inner.sys;
        let path = self.settings_path();
        sys.write(&path, original).map_err(|e| io_err("restore the Virtual Display Driver settings", e))?;
        let reloaded = self.reload(st, effective_count(original));
        sys.write(&path, original).map_err(|e| io_err("restore the Virtual Display Driver settings", e))?;
        sys.remove(&self.backup_path());
        st.original = None;
        st.loaded = Some(original.to_string());
        st.slots = Slots::default();
        st.targets.clear();
        reloaded
    }

    /// `SETDISPLAYCOUNT count`, spaced [`RELOAD_COOLDOWN`] after the previous
    /// one, then waits for the device to be running again.
    fn reload(&self, st: &mut State, count: u32) -> Result<(), DisplayError> {
        let sys = &self.inner.sys;
        if let Some(last) = st.last_reload {
            let since = sys.now().saturating_duration_since(last);
            if since < RELOAD_COOLDOWN {
                sys.sleep(RELOAD_COOLDOWN - since);
            }
        }
        let result = sys.pipe(&format!("SETDISPLAYCOUNT {count}"));
        st.last_reload = Some(sys.now());
        match result {
            PipeResult::Sent(reply) => log::debug!("VDD SETDISPLAYCOUNT {count}: {:?}", reply.trim()),
            PipeResult::NoPipe => {
                return Err(io_err(
                    "reach the Virtual Display Driver",
                    format!("its control pipe {PIPE_NAME} is not there; is the driver running?"),
                ))
            }
            PipeResult::Failed(e) => return Err(io_err("send a command to the Virtual Display Driver", e)),
        }
        // A reload can restart the driver host; give it the cooldown to come back.
        let deadline = sys.now() + RELOAD_COOLDOWN + APPEAR_TIMEOUT;
        loop {
            match sys.driver() {
                Some(d) if d.running => return Ok(()),
                Some(d) if sys.now() >= deadline => {
                    return Err(io_err(
                        "reload the Virtual Display Driver",
                        format!("the driver did not come back after the reload (problem code {})", d.problem),
                    ))
                }
                None if sys.now() >= deadline => {
                    return Err(io_err("reload the Virtual Display Driver", "the driver's device disappeared"))
                }
                _ => sys.sleep(POLL),
            }
        }
    }

    fn add_monitor(&self, mode: Mode) -> Result<VddMonitor<S>, DisplayError> {
        let sys = &self.inner.sys;
        if let Some(why) = self.availability().reason() {
            return Err(DisplayError::Unsupported { what: why.to_string() });
        }
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.settings_path();
        // First monitor: remember the user's file (on disk too, for a crash).
        if st.original.is_none() {
            let text = sys.read(&path).map_err(|e| io_err("read the Virtual Display Driver settings", e))?;
            sys.write(&self.backup_path(), &text)
                .map_err(|e| io_err("back up the Virtual Display Driver settings", e))?;
            st.slots = Slots::new(effective_count(&text));
            st.loaded.get_or_insert_with(|| text.clone());
            st.original = Some(text);
        }
        let original = st.original.clone().unwrap_or_default();
        let before = sys.targets();
        let count_before = st.slots.count();
        st.next_id += 1;
        let id = st.next_id;
        let connector = st.slots.take(id, mode);
        let needs_reload =
            st.slots.count() != count_before || !st.loaded.as_deref().is_some_and(|loaded| offers(loaded, mode));
        let settings = st.slots.settings(&original);
        let count = st.slots.count();
        let mut result = Ok(());
        if needs_reload {
            result = sys
                .write(&path, &settings)
                .map_err(|e| io_err("write the Virtual Display Driver settings", e))
                .and_then(|()| self.reload(&mut st, count));
            if result.is_ok() {
                st.loaded = Some(settings);
            }
        }
        let result = result.and_then(|()| self.wait_for(&before, connector, mode, needs_reload));
        match result {
            Ok(target) => {
                st.targets.retain(|(i, _)| *i != id);
                st.targets.push((id, target.target_id));
                if needs_reload {
                    self.reapply_others(&mut st, id);
                }
                log::info!(
                    "Virtual Display Driver monitor {} ({}) for connector {connector}",
                    target.gdi_name.as_deref().unwrap_or("?"),
                    mode_label(mode)
                );
                Ok(VddMonitor {
                    provider: self.clone(),
                    id,
                    connector,
                    target_id: AtomicU32::new(target.target_id),
                    mode,
                    released: false,
                })
            }
            Err(e) => {
                drop(st);
                self.release(id);
                Err(e)
            }
        }
    }

    /// After a reload re-created every VDD monitor: puts the other phones'
    /// monitors back at their modes (Windows may bring a re-created monitor
    /// back at the driver's first mode) and re-reads their target ids.
    fn reapply_others(&self, st: &mut State, except: u64) {
        let sys = &self.inner.sys;
        let all = sys.targets();
        for (id, connector, mode) in st.slots.entries() {
            if id == except {
                continue;
            }
            let known = st.targets.iter().find(|(i, _)| *i == id).map(|(_, t)| *t).unwrap_or(u32::MAX);
            let Some(t) = relocate(&all, known, connector) else { continue };
            st.targets.retain(|(i, _)| *i != id);
            st.targets.push((id, t.target_id));
            if let Some(place) = t.place() {
                if place.region.width != mode.width || place.region.height != mode.height {
                    if let Err(e) = sys.set_mode(&place.gdi_name, mode, None) {
                        log::warn!("Could not put {} back to {}: {e}", place.gdi_name, mode_label(mode));
                    }
                }
            }
        }
    }

    /// Waits for the monitor at `connector` to be on the desktop at `mode`,
    /// attaching it and setting its mode when Windows did not.
    fn wait_for(
        &self,
        before: &[DisplayTarget],
        connector: u32,
        mode: Mode,
        reloaded: bool,
    ) -> Result<DisplayTarget, DisplayError> {
        let sys = &self.inner.sys;
        let start = sys.now();
        let mut attach_tried = false;
        loop {
            let after = sys.targets();
            // A reused slot is not new: find it by position.
            let added = if reloaded { find_added(before, &after, connector) } else { None };
            let found = added.or_else(|| vdd_targets(&after).get(connector as usize).map(|t| (*t).clone()));
            if let Some(t) = found {
                if let Some(place) = t.place() {
                    // Windows may have put it on top of another monitor.
                    let overlaps = after.iter().any(|o| {
                        o.active
                            && o.target_id != t.target_id
                            && o.region.is_some_and(|r| r.x == place.region.x && r.y == place.region.y)
                    });
                    let pos = (attach_tried || overlaps).then(|| right_of_desktop(&after, t.target_id));
                    let wrong_size = place.region.width != mode.width || place.region.height != mode.height;
                    if wrong_size || pos.is_some() {
                        if let Err(e) = sys.set_mode(&place.gdi_name, mode, pos) {
                            log::warn!("Could not set {} on {}: {e}", mode_label(mode), place.gdi_name);
                        }
                        let now = sys.targets();
                        return Ok(relocate(&now, t.target_id, connector).filter(|n| n.place().is_some()).unwrap_or(t));
                    }
                    return Ok(t);
                }
                if !attach_tried && sys.now().saturating_duration_since(start) >= ATTACH_AFTER {
                    attach_tried = true;
                    if let Err(e) = sys.attach(t.target_id) {
                        log::warn!("Could not add the new virtual monitor to the desktop: {e}");
                    }
                }
            }
            if sys.now().saturating_duration_since(start) >= APPEAR_TIMEOUT {
                // Last resort, once: re-add the device so it re-reads its
                // settings. Needs administrator rights; harmless when it fails.
                if let Some(d) = sys.driver() {
                    match sys.restart_device(&d.instance_id) {
                        Ok(()) => {
                            log::info!("Restarted the Virtual Display Driver device to load its settings");
                            return self.wait_for_after_restart(before, connector, mode);
                        }
                        Err(e) => log::info!("Could not restart the Virtual Display Driver device: {e}"),
                    }
                }
                return Err(io_err(
                    "add a virtual monitor",
                    format!(
                        "no new Virtual Display Driver monitor appeared within {} s. Restarting the driver in Device \
                         Manager (or running DisplaySwarm once as administrator) usually fixes this; if the monitor exists \
                         but is off, choose Extend with Win+P.",
                        APPEAR_TIMEOUT.as_secs()
                    ),
                ));
            }
            sys.sleep(POLL);
        }
    }

    fn wait_for_after_restart(
        &self,
        before: &[DisplayTarget],
        connector: u32,
        mode: Mode,
    ) -> Result<DisplayTarget, DisplayError> {
        let sys = &self.inner.sys;
        let start = sys.now();
        loop {
            let after = sys.targets();
            if let Some(t) = find_added(before, &after, connector).filter(|t| t.place().is_some()) {
                let place = t.place().expect("filtered");
                if place.region.width != mode.width || place.region.height != mode.height {
                    let _ = sys.set_mode(&place.gdi_name, mode, None);
                }
                return Ok(relocate(&sys.targets(), t.target_id, connector).unwrap_or(t));
            }
            if sys.now().saturating_duration_since(start) >= APPEAR_TIMEOUT {
                return Err(io_err("add a virtual monitor", "the monitor did not appear after restarting the driver"));
            }
            sys.sleep(POLL);
        }
    }

    /// Gives monitor `id` back: the count drops when it was the last one, the
    /// monitor is detached when a later phone still holds a higher connector,
    /// and the user's file comes back when nothing is left.
    fn release(&self, id: u64) {
        let sys = &self.inner.sys;
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let target = st.targets.iter().find(|(i, _)| *i == id).map(|(_, t)| *t);
        st.targets.retain(|(i, _)| *i != id);
        let count_before = st.slots.count();
        let Some(connector) = st.slots.free(id) else { return };
        let Some(original) = st.original.clone() else { return };
        if st.slots.is_empty() {
            if let Err(e) = self.put_back(&mut st, &original) {
                log::warn!("Virtual monitor released, but the driver settings were not restored: {}", e.summary());
            }
            return;
        }
        if st.slots.count() != count_before {
            let settings = st.slots.settings(&original);
            let count = st.slots.count();
            let result = sys
                .write(&self.settings_path(), &settings)
                .map_err(|e| io_err("write the Virtual Display Driver settings", e))
                .and_then(|()| self.reload(&mut st, count));
            match result {
                Ok(()) => st.loaded = Some(settings),
                Err(e) => log::warn!("Virtual monitor released, but the driver kept it: {}", e.summary()),
            }
        } else {
            // A later phone holds a higher connector: keep the monitor, off the desktop.
            let all = sys.targets();
            if let Some(place) = target.and_then(|t| relocate(&all, t, connector)).and_then(|t| t.place()) {
                if let Err(e) = sys.detach(&place.gdi_name) {
                    log::warn!("Could not take {} off the desktop: {e}", place.gdi_name);
                }
            }
        }
    }
}

impl<S: VddSystem> VirtualMonitorProvider for VddProvider<S> {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Vdd
    }

    fn availability(&self) -> Availability {
        let sys = &self.inner.sys;
        let driver = sys.driver();
        let verdict = check_driver(driver.as_ref(), self.inner.allow_any_version);
        if !verdict.is_yes() {
            return verdict;
        }
        if !sys.pipe_present() {
            return Availability::No(format!(
                "The Virtual Display Driver is running but its control pipe ({PIPE_NAME}) is missing, so DisplaySwarm \
                 cannot add monitors. Restart the driver in Device Manager or reinstall it."
            ));
        }
        let path = self.settings_path();
        if sys.read(&path).is_err() {
            return Availability::No(format!(
                "The Virtual Display Driver is installed but its settings file {} cannot be read. Reinstall the driver.",
                path.display()
            ));
        }
        if !sys.writable(&path) {
            return Availability::No(format!(
                "DisplaySwarm cannot change {} to add the phone's resolution. Give your Windows user write access to that \
                 folder, or start DisplaySwarm once as administrator.",
                path.display()
            ));
        }
        Availability::Yes
    }

    fn add(&self, mode: Mode) -> Result<Box<dyn VirtualMonitor>, DisplayError> {
        Ok(Box::new(self.add_monitor(mode)?))
    }
}

/// A monitor added for one session.
pub struct VddMonitor<S: VddSystem> {
    provider: VddProvider<S>,
    id: u64,
    connector: u32,
    target_id: AtomicU32,
    mode: Mode,
    released: bool,
}

impl<S: VddSystem> VirtualMonitor for VddMonitor<S> {
    fn locate(&self) -> Option<MonitorPlace> {
        let all = self.provider.inner.sys.targets();
        let t = relocate(&all, self.target_id.load(Ordering::Relaxed), self.connector)?;
        self.target_id.store(t.target_id, Ordering::Relaxed);
        t.place()
    }

    fn describe(&self) -> String {
        format!("Virtual Display Driver monitor #{} ({})", self.connector + 1, mode_label(self.mode))
    }
}

impl<S: VddSystem> Drop for VddMonitor<S> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let (provider, id) = (self.provider.clone(), self.id);
        if provider.inner.release_inline {
            provider.release(id);
            return;
        }
        let spawned = std::thread::Builder::new().name("displayswarm-vdd-release".into()).spawn(move || provider.release(id));
        if let Err(e) = spawned {
            log::warn!("Could not start the virtual monitor release thread: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Fake system for tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    /// A scriptable Windows: one settings file, a driver that re-creates
    /// `count` monitors on `SETDISPLAYCOUNT` (reading the file like the real
    /// one), a clock that advances on sleep.
    pub struct FakeVdd {
        pub state: Mutex<FakeState>,
        base: Instant,
    }

    pub struct FakeState {
        pub files: HashMap<PathBuf, String>,
        pub driver: Option<DriverInfo>,
        pub writable: bool,
        pub pipe_up: bool,
        pub commands: Vec<String>,
        /// Monitors the driver currently exposes (from the file at the last reload).
        pub vdd_count: u32,
        /// The file as the driver last read it.
        pub loaded_xml: String,
        /// Next target id handed out; every reload renumbers when `renumber`.
        pub next_target: u32,
        pub renumber: bool,
        /// Target ids of the VDD monitors, in connector order.
        pub vdd_ids: Vec<u32>,
        /// Detached monitors by target id.
        pub detached: Vec<u32>,
        /// New monitors arrive detached (Windows did not extend onto them).
        pub arrive_detached: bool,
        pub attach_calls: u32,
        pub set_modes: Vec<(String, Mode, Option<(i32, i32)>)>,
        pub modes: HashMap<u32, Mode>,
        /// A reload leaves the driver without monitors (it "crashed").
        pub reload_breaks: bool,
        pub restarts: u32,
        pub elapsed: Duration,
    }

    pub const DIR: &str = "C:/VirtualDisplayDriver";

    impl FakeVdd {
        pub fn new(xml: &str) -> Self {
            let mut files = HashMap::new();
            files.insert(Path::new(DIR).join(SETTINGS_FILE), xml.to_string());
            let count = effective_count(xml);
            let fake = FakeVdd {
                state: Mutex::new(FakeState {
                    files,
                    driver: Some(DriverInfo {
                        instance_id: r"ROOT\DISPLAY\0000".into(),
                        version: "22.50.22.79".into(),
                        date: "8-14-2025".into(),
                        running: true,
                        problem: 0,
                    }),
                    writable: true,
                    pipe_up: true,
                    commands: Vec::new(),
                    vdd_count: 0,
                    loaded_xml: xml.to_string(),
                    next_target: 100,
                    renumber: false,
                    vdd_ids: Vec::new(),
                    detached: Vec::new(),
                    arrive_detached: false,
                    attach_calls: 0,
                    set_modes: Vec::new(),
                    modes: HashMap::new(),
                    reload_breaks: false,
                    restarts: 0,
                    elapsed: Duration::ZERO,
                }),
                base: Instant::now(),
            };
            fake.lock().recreate(count, xml);
            fake
        }

        pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().unwrap()
        }

        pub fn settings(&self) -> String {
            self.lock().files[&Path::new(DIR).join(SETTINGS_FILE)].clone()
        }

        pub fn has_backup(&self) -> bool {
            self.lock().files.contains_key(&Path::new(DIR).join(format!("{SETTINGS_FILE}{BACKUP_SUFFIX}")))
        }
    }

    impl FakeState {
        fn recreate(&mut self, count: u32, xml: &str) {
            let keep = self.vdd_ids.len().min(count as usize);
            if self.renumber {
                self.vdd_ids.clear();
            } else {
                self.vdd_ids.truncate(keep);
            }
            let new_from = self.vdd_ids.len();
            while self.vdd_ids.len() < count as usize {
                self.vdd_ids.push(self.next_target);
                self.next_target += 1;
            }
            if self.arrive_detached {
                let fresh: Vec<u32> = self.vdd_ids[new_from.min(keep)..].to_vec();
                for id in fresh {
                    if !self.detached.contains(&id) {
                        self.detached.push(id);
                    }
                }
            }
            self.vdd_count = count;
            self.loaded_xml = xml.to_string();
        }

        fn gdi(id: u32) -> String {
            format!(r"\\.\DISPLAY{id}")
        }
    }

    impl VddSystem for FakeVdd {
        fn driver(&self) -> Option<DriverInfo> {
            self.lock().driver.clone()
        }
        fn settings_dir(&self) -> PathBuf {
            PathBuf::from(DIR)
        }
        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.lock().files.get(path).cloned().ok_or_else(|| std::io::ErrorKind::NotFound.into())
        }
        fn write(&self, path: &Path, text: &str) -> std::io::Result<()> {
            let mut s = self.lock();
            if !s.writable {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            s.files.insert(path.to_path_buf(), text.to_string());
            Ok(())
        }
        fn remove(&self, path: &Path) {
            self.lock().files.remove(path);
        }
        fn writable(&self, _path: &Path) -> bool {
            self.lock().writable
        }
        fn pipe_present(&self) -> bool {
            self.lock().pipe_up
        }
        fn pipe(&self, command: &str) -> PipeResult {
            let mut s = self.lock();
            if !s.pipe_up {
                return PipeResult::NoPipe;
            }
            s.commands.push(command.to_string());
            if let Some(n) = command.strip_prefix("SETDISPLAYCOUNT ") {
                let n: u32 = n.trim().parse().unwrap_or(1);
                let path = Path::new(DIR).join(SETTINGS_FILE);
                // The driver rewrites <count> itself, then reloads.
                let xml = set_count(&s.files[&path], n);
                s.files.insert(path, xml.clone());
                if s.reload_breaks {
                    s.vdd_ids.clear();
                    s.vdd_count = 0;
                } else {
                    s.recreate(n.max(1), &xml);
                }
            }
            PipeResult::Sent(String::new())
        }
        fn restart_device(&self, _instance_id: &str) -> Result<(), String> {
            let mut s = self.lock();
            s.restarts += 1;
            let xml = s.files[&Path::new(DIR).join(SETTINGS_FILE)].clone();
            s.reload_breaks = false;
            s.recreate(effective_count(&xml), &xml);
            Ok(())
        }
        fn targets(&self) -> Vec<DisplayTarget> {
            let s = self.lock();
            let mut out = vec![DisplayTarget {
                vdd: false,
                target_id: 1,
                active: true,
                gdi_name: Some(FakeState::gdi(1)),
                region: Some(OutputRegion { x: 0, y: 0, width: 1920, height: 1080 }),
            }];
            let mut x = 1920;
            for id in &s.vdd_ids {
                let active = !s.detached.contains(id);
                let mode = s.modes.get(id).copied().unwrap_or(Mode { width: 1920, height: 1080, refresh_mhz: 60_000 });
                out.push(DisplayTarget {
                    vdd: true,
                    target_id: *id,
                    active,
                    gdi_name: active.then(|| FakeState::gdi(*id)),
                    region: active.then(|| OutputRegion { x, y: 0, width: mode.width, height: mode.height }),
                });
                if active {
                    x += mode.width as i32;
                }
            }
            out
        }
        fn attach(&self, target_id: u32) -> Result<(), String> {
            let mut s = self.lock();
            s.attach_calls += 1;
            s.detached.retain(|t| *t != target_id);
            Ok(())
        }
        fn set_mode(&self, gdi_name: &str, mode: Mode, position: Option<(i32, i32)>) -> Result<(), String> {
            let mut s = self.lock();
            let id: u32 = gdi_name.trim_start_matches(r"\\.\DISPLAY").parse().map_err(|_| "bad name".to_string())?;
            if !offers(&s.loaded_xml, mode) {
                return Err(format!("{} is not offered", mode_label(mode)));
            }
            s.modes.insert(id, mode);
            s.set_modes.push((gdi_name.to_string(), mode, position));
            Ok(())
        }
        fn detach(&self, gdi_name: &str) -> Result<(), String> {
            let mut s = self.lock();
            let id: u32 = gdi_name.trim_start_matches(r"\\.\DISPLAY").parse().map_err(|_| "bad name".to_string())?;
            s.detached.push(id);
            Ok(())
        }
        fn now(&self) -> Instant {
            self.base + self.lock().elapsed
        }
        fn sleep(&self, d: Duration) {
            self.lock().elapsed += d;
        }
    }

    pub fn provider(fake: FakeVdd) -> VddProvider<FakeVdd> {
        VddProvider::build(fake, false, true)
    }

    pub fn sys(p: &VddProvider<FakeVdd>) -> &FakeVdd {
        &p.inner.sys
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{provider, sys, FakeVdd};
    use super::*;

    /// Trimmed from the upstream `vdd_settings.xml` (MIT, VirtualDrivers), with
    /// its comment style: a commented-out count and resolution must be ignored.
    const XML: &str = "<?xml version='1.0' encoding='utf-8'?>\n<vdd_settings>\n\n    <!-- === BASIC DRIVER CONFIGURATION === -->\n    <monitors>\n        <count>1</count>\n    </monitors>\n    <!-- <monitors><count>9</count></monitors> -->\n\n    <gpu>\n        <friendlyname>default</friendlyname>\n    </gpu>\n\n    <global>\n        <!-- Global refresh rates - applied to all resolutions -->\n        <g_refresh_rate>60</g_refresh_rate>\n        <g_refresh_rate>90</g_refresh_rate>\n        <g_refresh_rate>120</g_refresh_rate>\n    </global>\n\n    <resolutions>\n        <resolution>\n            <width>1920</width>\n            <height>1080</height>\n            <refresh_rate>60</refresh_rate>\n        </resolution>\n        <!-- <resolution><width>1600</width><height>720</height><refresh_rate>60</refresh_rate></resolution> -->\n        <resolution>\n            <width>2560</width>\n            <height>1440</height>\n            <refresh_rate>59.94</refresh_rate>\n        </resolution>\n    </resolutions>\n\n    <logging>\n        <logging>false</logging>\n    </logging>\n</vdd_settings>\n";

    const PHONE: Mode = Mode { width: 1600, height: 720, refresh_mhz: 90_000 };
    const TABLET: Mode = Mode { width: 2000, height: 1200, refresh_mhz: 60_000 };

    #[test]
    fn count_is_read_and_replaced_ignoring_comments() {
        assert_eq!(effective_count(XML), 1);
        let three = set_count(XML, 3);
        assert_eq!(effective_count(&three), 3);
        assert_eq!(three.replacen("<count>3</count>", "<count>1</count>", 1), XML, "nothing else changes");
        assert!(three.contains("<count>9</count>"), "the comment is untouched");
        assert_eq!(effective_count("<vdd_settings></vdd_settings>"), 1, "absent means one monitor");
        assert_eq!(effective_count("<vdd_settings><monitors><count>0</count></monitors></vdd_settings>"), 1);
        assert_eq!(effective_count(&set_count("<vdd_settings>\n</vdd_settings>\n", 4)), 4);
        assert_eq!(effective_count(&set_count("<vdd_settings><monitors></monitors></vdd_settings>", 2)), 2);
    }

    #[test]
    fn modes_come_from_resolutions_and_global_rates() {
        let listed = listed_modes(XML);
        assert_eq!(listed.len(), 2, "the commented-out resolution is not listed: {listed:?}");
        assert_eq!(listed[1], ListedMode { width: 2560, height: 1440, refresh_mhz: Some(59_940) });
        assert_eq!(global_rates_hz(XML), vec![60, 90, 120]);
        assert!(offers(XML, Mode { width: 1920, height: 1080, refresh_mhz: 60_000 }));
        assert!(offers(XML, Mode { width: 1920, height: 1080, refresh_mhz: 120_000 }), "global rate");
        assert!(offers(XML, Mode { width: 2560, height: 1440, refresh_mhz: 59_940 }), "fractional");
        assert!(!offers(XML, Mode { width: 2560, height: 1440, refresh_mhz: 59_000 }));
        assert!(!offers(XML, PHONE), "size not listed");
    }

    #[test]
    fn adding_a_mode_appends_one_entry_the_driver_can_parse() {
        let with = add_mode(XML, PHONE);
        assert!(offers(&with, PHONE));
        assert_eq!(listed_modes(&with).len(), 3);
        assert_eq!(add_mode(&with, PHONE), with, "idempotent");
        // width, height, refresh_rate in that order (the driver pairs them by order).
        let entry = &with[with.find("<width>1600").unwrap()..];
        assert!(entry.find("<height>720").unwrap() < entry.find("<refresh_rate>90<").unwrap());
        // Indented like its neighbours, inside <resolutions>.
        assert!(with.contains("        <resolution>\n            <width>1600</width>"), "{with}");
        assert!(with.find("<width>1600").unwrap() < with.find("</resolutions>").unwrap());
        // 59.94 Hz is written as a decimal.
        let ntsc = add_mode(XML, Mode { width: 1600, height: 720, refresh_mhz: 59_940 });
        assert!(ntsc.contains("<refresh_rate>59.94</refresh_rate>"));
        // A file without a resolutions section gets one.
        let bare = add_mode("<vdd_settings>\n</vdd_settings>\n", PHONE);
        assert_eq!(listed_modes(&bare), vec![ListedMode { width: 1600, height: 720, refresh_mhz: Some(90_000) }]);
    }

    #[test]
    fn driver_versions_are_pinned_by_build_date() {
        let d = |date: &str| DriverInfo {
            instance_id: "x".into(),
            version: "22.50.22.79".into(),
            date: date.into(),
            running: true,
            problem: 0,
        };
        assert_eq!(parse_driver_date("8-14-2025"), Some((2025, 8, 14)));
        assert_eq!(parse_driver_date("08/14/2025"), Some((2025, 8, 14)));
        assert_eq!(parse_driver_date("2025-08-14"), None);
        assert!(check_driver(Some(&d("8-14-2025")), false).is_yes());
        let old = check_driver(Some(&d("12-24-2024")), false);
        assert!(old.reason().unwrap().contains("older") && old.reason().unwrap().contains("25.7.23"), "{old:?}");
        assert!(check_driver(Some(&d("3-1-2026")), false).reason().unwrap().contains("newer"));
        assert!(check_driver(Some(&d("3-1-2026")), true).is_yes(), "the override skips the pin");
        assert!(check_driver(Some(&d("garbage")), false).reason().unwrap().contains(ANY_VERSION_ENV));
        let stopped = DriverInfo { running: false, problem: 22, ..d("8-14-2025") };
        assert!(check_driver(Some(&stopped), true).reason().unwrap().contains("problem code 22"));
        assert!(check_driver(None, false).reason().unwrap().starts_with(NOT_INSTALLED_PREFIX));
    }

    #[test]
    fn slots_shrink_only_from_the_end() {
        let mut s = Slots::new(1);
        assert_eq!(s.take(1, PHONE), 1);
        assert_eq!(s.take(2, TABLET), 2);
        assert_eq!(s.count(), 3);
        assert_eq!(s.free(1), Some(1));
        assert_eq!(s.count(), 3, "phone 2 still holds connector 2");
        assert_eq!(s.take(3, PHONE), 1, "the free slot is reused");
        assert_eq!(s.free(2), Some(2));
        assert_eq!(s.count(), 2);
        assert_eq!(s.free(3), Some(1));
        assert!(s.is_empty());
        assert_eq!(s.count(), 1, "back to the user's own monitors");
        assert_eq!(s.free(9), None);
    }

    #[test]
    fn settings_carry_the_count_and_every_live_mode() {
        let mut s = Slots::new(effective_count(XML));
        assert_eq!(s.settings(XML), set_count(XML, 1));
        s.take(1, PHONE);
        s.take(2, TABLET);
        let xml = s.settings(XML);
        assert_eq!(effective_count(&xml), 3);
        assert!(offers(&xml, PHONE) && offers(&xml, TABLET));
        s.free(2);
        assert!(!offers(&s.settings(XML), TABLET), "a released mode is not kept");
    }

    #[test]
    fn a_new_monitor_is_found_by_diff_or_by_position() {
        let t = |id: u32, vdd: bool| DisplayTarget { vdd, target_id: id, active: true, gdi_name: None, region: None };
        let before = vec![t(1, false), t(100, true)];
        let after = vec![t(1, false), t(100, true), t(101, true)];
        assert_eq!(find_added(&before, &after, 1).unwrap().target_id, 101);
        // A reload that renumbered every VDD target: pick connector order.
        let renumbered = vec![t(1, false), t(205, true), t(204, true)];
        assert_eq!(find_added(&before, &renumbered, 1).unwrap().target_id, 205);
        assert!(find_added(&before, &before, 1).is_none());
        assert_eq!(relocate(&renumbered, 101, 0).unwrap().target_id, 204, "gone by id: by position");
        assert_eq!(relocate(&after, 101, 0).unwrap().target_id, 101);
    }

    #[test]
    fn adding_writes_the_mode_raises_the_count_and_finds_the_monitor() {
        let p = provider(FakeVdd::new(XML));
        assert!(p.availability().is_yes());
        let m = p.add(PHONE).unwrap();
        let fake = sys(&p);
        assert_eq!(fake.lock().commands, vec!["SETDISPLAYCOUNT 2"]);
        assert!(!fake.lock().commands.iter().any(|c| c.contains("RELOAD_DRIVER")));
        let xml = fake.settings();
        assert_eq!(effective_count(&xml), 2);
        assert!(offers(&xml, PHONE));
        assert!(fake.has_backup());
        let place = m.locate().unwrap();
        assert_eq!((place.region.width, place.region.height), (1600, 720));
        assert_eq!(place.region.x, 3840, "right of the panel and the user's own VDD monitor");
        assert!(m.describe().contains("#2"));
        drop(m);
        assert_eq!(fake.settings(), XML, "the user's file is back byte for byte");
        assert!(!fake.has_backup());
        assert_eq!(fake.lock().commands.last().unwrap(), "SETDISPLAYCOUNT 1");
        assert_eq!(fake.lock().vdd_ids.len(), 1);
    }

    #[test]
    fn reloads_are_spaced_by_the_cooldown() {
        let p = provider(FakeVdd::new(XML));
        let a = p.add(PHONE).unwrap();
        let t0 = sys(&p).lock().elapsed;
        let b = p.add(TABLET).unwrap();
        assert!(sys(&p).lock().elapsed - t0 >= RELOAD_COOLDOWN);
        assert_eq!(sys(&p).lock().commands, vec!["SETDISPLAYCOUNT 2", "SETDISPLAYCOUNT 3"]);
        drop(b);
        drop(a);
    }

    #[test]
    fn an_earlier_phone_leaving_detaches_its_monitor_and_keeps_the_later_one() {
        let p = provider(FakeVdd::new(XML));
        let first = p.add(PHONE).unwrap();
        let second = p.add(TABLET).unwrap();
        let second_before = second.locate().unwrap();
        let reloads = sys(&p).lock().commands.len();
        drop(first);
        assert_eq!(sys(&p).lock().commands.len(), reloads, "no reload: the count stays");
        assert_eq!(sys(&p).lock().detached.len(), 1);
        assert_eq!(second.locate().unwrap().gdi_name, second_before.gdi_name);
        // A third phone reuses the detached slot.
        let third = p.add(PHONE).unwrap();
        assert!(third.locate().is_some());
        assert_eq!(effective_count(&sys(&p).settings()), 3);
        drop(second);
        assert_eq!(effective_count(&sys(&p).settings()), 2, "the trailing slot went");
        drop(third);
        assert_eq!(sys(&p).settings(), XML);
    }

    #[test]
    fn a_monitor_that_arrives_detached_is_attached_and_placed() {
        let fake = FakeVdd::new(XML);
        fake.lock().arrive_detached = true;
        let p = provider(fake);
        let m = p.add(PHONE).unwrap();
        assert_eq!(sys(&p).lock().attach_calls, 1);
        let (_, mode, pos) = sys(&p).lock().set_modes.last().cloned().unwrap();
        assert_eq!(mode, PHONE);
        assert!(pos.is_some(), "an attached-by-hand monitor is placed right of the desktop");
        assert!(m.locate().is_some());
    }

    #[test]
    fn a_renumbering_reload_still_finds_each_phone() {
        let fake = FakeVdd::new(XML);
        fake.lock().renumber = true;
        let p = provider(fake);
        let first = p.add(PHONE).unwrap();
        let _second = p.add(TABLET).unwrap();
        // The first phone's target id changed in the second reload; it is found by position.
        let place = first.locate().unwrap();
        assert_eq!((place.region.width, place.region.height), (1600, 720));
    }

    #[test]
    fn a_driver_that_does_not_come_back_is_restarted_once() {
        let fake = FakeVdd::new(XML);
        fake.lock().reload_breaks = true;
        let p = provider(fake);
        let m = p.add(PHONE).unwrap();
        assert_eq!(sys(&p).lock().restarts, 1);
        assert!(m.locate().is_some());
    }

    #[test]
    fn refusals_name_the_fix() {
        let fake = FakeVdd::new(XML);
        fake.lock().writable = false;
        let p = provider(fake);
        let why = p.availability();
        assert!(why.reason().unwrap().contains("write access"), "{why:?}");
        assert!(p.add(PHONE).is_err());
        assert_eq!(sys(&p).settings(), XML, "nothing was changed");

        let fake = FakeVdd::new(XML);
        fake.lock().driver = None;
        assert!(provider(fake).availability().reason().unwrap().starts_with(NOT_INSTALLED_PREFIX));

        let fake = FakeVdd::new(XML);
        fake.lock().driver.as_mut().unwrap().date = "10-27-2024".into();
        assert!(provider(fake).availability().reason().unwrap().contains("older"));
    }

    #[test]
    fn a_missing_pipe_is_refused_up_front() {
        let fake = FakeVdd::new(XML);
        fake.lock().pipe_up = false;
        let p = provider(fake);
        assert!(p.availability().reason().unwrap().contains("control pipe"));
        let e = p.add(PHONE).err().unwrap();
        assert!(e.summary().contains("pipe"), "{}", e.summary());
        assert_eq!(sys(&p).settings(), XML);
        assert!(!sys(&p).has_backup(), "refused before anything was touched");
    }

    #[test]
    fn a_pipe_that_goes_away_before_release_still_restores_the_file() {
        let p = provider(FakeVdd::new(XML));
        let first = p.add(PHONE).unwrap();
        // Present when the monitor was added, gone when it is released.
        sys(&p).lock().pipe_up = false;
        drop(first);
        assert_eq!(sys(&p).settings(), XML, "the user's file is back even without a reload");
        assert!(!sys(&p).has_backup());
    }

    #[test]
    fn a_crash_backup_is_restored_at_start() {
        let fake = FakeVdd::new(XML);
        {
            let mut s = fake.lock();
            let dir = std::path::Path::new(fake::DIR);
            s.files.insert(dir.join(SETTINGS_FILE), set_count(&add_mode(XML, PHONE), 2));
            s.files.insert(dir.join(format!("{SETTINGS_FILE}{BACKUP_SUFFIX}")), XML.to_string());
        }
        let p = provider(fake);
        p.recover();
        assert_eq!(sys(&p).settings(), XML);
        assert!(!sys(&p).has_backup());
        assert_eq!(sys(&p).lock().commands, vec!["SETDISPLAYCOUNT 1"]);
        // Nothing to do the second time.
        p.recover();
        assert_eq!(sys(&p).lock().commands.len(), 1);
    }
}
