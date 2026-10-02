//! X11 sessions (Cinnamon, XFCE, MATE, and KDE or GNOME on X11): the RandR
//! layout at tool level, the way [`super::kscreen`] is for KDE Wayland.
//!
//! * Reading: [`query`] talks RandR 1.3 directly through `x11rb`
//!   (`GetScreenResourcesCurrent`, which does not re-probe the hardware, so it is
//!   cheap enough to run on every change event). [`parse_verbose`] reads the
//!   same facts from `xrandr --verbose`; it is what the fixture tests (and
//!   user-contributed fixtures) go through, and the fallback when the direct
//!   connection fails but the tool works.
//! * Change events: [`RandrEvents`] selects RandR notifications on the root
//!   window and waits on the connection's socket, so a watcher can be stopped
//!   at any tick (no thread parked in a blocking read).
//! * Changing: the `xrandr` tool ([`apply`]). It grows and shrinks the
//!   framebuffer around a change by itself, which a hand-written
//!   `SetCrtcConfig` sequence would have to redo.
//!
//! Everything that decides (layout conversion, argument planning, the CVT
//! modeline) is pure and tested without an X server.

use super::backend::{LayoutEvent, LayoutEvents};
use super::model::{Layout, LayoutOp, Mode, Output};
use super::{DisplayError, OutputRegion};
use std::process::Command;
use std::time::{Duration, Instant};

/// Every mode the host creates is named with this prefix, so a crashed run's
/// modes can be found and removed even without a journal.
pub const MODE_PREFIX: &str = "displayswarm-";

fn io_err(context: &str, detail: impl std::fmt::Display) -> DisplayError {
    DisplayError::Io { context: context.into(), detail: detail.to_string() }
}

/// One mode the server knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandrMode {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// One RandR output (connector), connected or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandrOutput {
    pub name: String,
    pub connected: bool,
    pub primary: bool,
    /// Index of the CRTC driving it (`xrandr --verbose` numbers CRTCs by index).
    pub crtc: Option<usize>,
    /// Indices of the CRTCs that can drive it.
    pub possible_crtcs: Vec<usize>,
    /// Where it scans out, rotation included; `None` while off.
    pub region: Option<OutputRegion>,
    /// Id of the current mode.
    pub current: Option<u32>,
    /// Ids of the modes it offers.
    pub modes: Vec<u32>,
}

/// The RandR state of screen 0.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RandrState {
    /// The root window (framebuffer) size now.
    pub screen: (u32, u32),
    /// The largest framebuffer the server allows.
    pub max_screen: (u32, u32),
    pub crtc_count: usize,
    pub outputs: Vec<RandrOutput>,
    /// Every mode the server knows, attached to an output or not.
    pub modes: Vec<RandrMode>,
}

impl RandrState {
    pub fn by_name(&self, name: &str) -> Option<&RandrOutput> {
        self.outputs.iter().find(|o| o.name == name)
    }

    pub fn mode(&self, id: u32) -> Option<&RandrMode> {
        self.modes.iter().find(|m| m.id == id)
    }

    pub fn mode_named(&self, name: &str) -> Option<&RandrMode> {
        self.modes.iter().find(|m| m.name == name)
    }

    pub fn crtc_in_use(&self, index: usize) -> bool {
        self.outputs.iter().any(|o| o.crtc == Some(index))
    }

    /// Whether any output shows the mode called `name` right now.
    pub fn mode_in_use(&self, name: &str) -> bool {
        self.mode_named(name).is_some_and(|m| self.outputs.iter().any(|o| o.current == Some(m.id)))
    }

    /// Right edge of the active outputs other than `except`: where a new or
    /// un-mirrored output goes.
    pub fn right_edge(&self, except: Option<&str>) -> i32 {
        self.outputs
            .iter()
            .filter(|o| Some(o.name.as_str()) != except)
            .filter_map(|o| o.region)
            .map(|r| r.x + r.width as i32)
            .max()
            .unwrap_or(0)
    }
}

/// The laptop's own panel, by connector name (`eDP-1`, `eDP1`, `LVDS-1`, `DSI-1`).
pub fn is_builtin(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["edp", "lvds", "dsi"].iter().any(|p| n.starts_with(p))
}

fn refresh_mhz(dot_clock_hz: u64, htotal: u32, vtotal: u32) -> u32 {
    let total = htotal as u64 * vtotal as u64;
    if total == 0 {
        return 0;
    }
    ((dot_clock_hz * 1000 + total / 2) / total) as u32
}

/// The neutral layout. Only outputs that are connected or lit are listed: a
/// dark connector with nothing plugged in is not a monitor, and listing it
/// would let the reconcile policy try to "re-enable" it. The host's own
/// virtual output is disconnected but lit, so it is listed while it exists.
///
/// RandR has no mirror relation: a mirror is two outputs scanning out the same
/// rectangle. The source of such a group is the primary, else the built-in
/// panel, else the first listed; the others are its mirrors.
pub fn layout_from_state(state: &RandrState) -> Layout {
    let listed: Vec<&RandrOutput> = state.outputs.iter().filter(|o| o.connected || o.crtc.is_some()).collect();
    let source_of = |region: OutputRegion| -> Option<&str> {
        let group: Vec<&&RandrOutput> = listed.iter().filter(|o| o.region == Some(region)).collect();
        if group.len() < 2 {
            return None;
        }
        group
            .iter()
            .find(|o| o.primary)
            .or_else(|| group.iter().find(|o| is_builtin(&o.name)))
            .or_else(|| group.first())
            .map(|o| o.name.as_str())
    };
    let outputs = listed
        .iter()
        .map(|o| Output {
            name: o.name.clone(),
            region: o.region,
            enabled: o.region.is_some(),
            mirror_of: o.region.and_then(source_of).filter(|s| *s != o.name).map(str::to_string),
            primary: o.primary && o.region.is_some(),
            builtin: is_builtin(&o.name),
            mode: o.current.and_then(|id| state.mode(id)).map(|m| Mode {
                width: m.width,
                height: m.height,
                refresh_mhz: m.refresh_mhz,
            }),
        })
        .collect();
    Layout { outputs }
}

/// The lit output a Mirror capture shows: `want` when it names a lit output,
/// else the primary, else the first lit one.
pub fn pick_capture_output(state: &RandrState, want: Option<&str>) -> Option<(String, OutputRegion)> {
    let lit = || state.outputs.iter().filter_map(|o| Some((o, o.region?)));
    want.and_then(|w| lit().find(|(o, _)| o.name == w))
        .or_else(|| lit().find(|(o, _)| o.primary))
        .or_else(|| lit().next())
        .map(|(o, r)| (o.name.clone(), r))
}

/// A mode with `mode`'s size among `output`'s modes, the closest refresh rate
/// first, within 1 Hz.
pub fn find_mode<'a>(state: &'a RandrState, output: &RandrOutput, mode: Mode) -> Option<&'a RandrMode> {
    output
        .modes
        .iter()
        .filter_map(|id| state.mode(*id))
        .filter(|m| m.width == mode.width && m.height == mode.height)
        .filter(|m| m.refresh_mhz.abs_diff(mode.refresh_mhz) <= 1000)
        .min_by_key(|m| m.refresh_mhz.abs_diff(mode.refresh_mhz))
}

/// What applying a batch of [`LayoutOp`]s takes: the arguments of one `xrandr`
/// call, and the mode changes that need a new mode created first (those run
/// afterwards through [`set_output_mode`]).
pub fn plan_ops(state: &RandrState, ops: &[LayoutOp]) -> Result<(Vec<String>, Vec<(String, Mode)>), String> {
    // xrandr takes one `--output NAME ...` group per output; ops on the same
    // output are merged into its group.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut push = |name: &str, args: &[String]| {
        match groups.iter_mut().find(|(n, _)| n == name) {
            Some((_, a)) => a.extend_from_slice(args),
            None => groups.push((name.to_string(), args.to_vec())),
        }
    };
    let mut modes = Vec::new();
    for op in ops {
        let name = match op {
            LayoutOp::Enable { name }
            | LayoutOp::MirrorNone { name }
            | LayoutOp::SetPrimary { name }
            | LayoutOp::Move { name, .. }
            | LayoutOp::SetMode { name, .. } => name,
        };
        let out = state.by_name(name).ok_or_else(|| format!("{name} is not in the layout"))?;
        match op {
            LayoutOp::Enable { .. } => {
                // `--auto` alone would put it at 0,0, on top of (mirroring) the
                // first monitor; beside the others is what "enable" means here.
                let x = state.right_edge(Some(name));
                push(name, &["--auto".into(), "--pos".into(), format!("{x}x0")]);
            }
            LayoutOp::MirrorNone { .. } => {
                let x = state.right_edge(Some(name));
                push(name, &["--pos".into(), format!("{x}x0")]);
            }
            LayoutOp::SetPrimary { .. } => push(name, &["--primary".into()]),
            LayoutOp::Move { x, y, .. } => {
                if *x < 0 || *y < 0 {
                    return Err(format!("X11 screen positions cannot be negative ({x},{y} for {name})"));
                }
                push(name, &["--pos".into(), format!("{x}x{y}")]);
            }
            LayoutOp::SetMode { mode, .. } => match find_mode(state, out, *mode) {
                Some(m) => push(name, &["--mode".into(), format!("0x{:x}", m.id)]),
                None => modes.push((name.clone(), *mode)),
            },
        }
    }
    let mut args = Vec::new();
    for (name, a) in groups {
        args.push("--output".into());
        args.push(name);
        args.extend(a);
    }
    Ok((args, modes))
}

/// A modeline: pixel clock and the horizontal and vertical timings
/// (display, sync start, sync end, total).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modeline {
    pub clock_khz: u32,
    pub h: [u32; 4],
    pub v: [u32; 4],
}

impl Modeline {
    pub fn refresh_mhz(&self) -> u32 {
        refresh_mhz(self.clock_khz as u64 * 1000, self.h[3], self.v[3])
    }
}

/// VESA CVT reduced-blanking (v1) timings for `width`x`height` at
/// `refresh_mhz`, the same numbers `cvt -r` prints. Reduced blanking keeps the
/// pixel clock low, which matters to nothing real here (the output is virtual)
/// but is what every driver accepts.
pub fn cvt_rb(width: u32, height: u32, refresh_mhz: u32) -> Modeline {
    const MIN_V_BLANK_US: f64 = 460.0;
    const H_BLANK: u32 = 160;
    const H_FRONT: u32 = 48;
    const H_SYNC: u32 = 32;
    const V_FRONT: u32 = 3;
    const MIN_V_BACK: u32 = 6;
    const CLOCK_STEP_KHZ: f64 = 250.0;
    let (w, h) = (width, height);
    let v_sync = if h % 3 == 0 && h * 4 / 3 == w {
        4
    } else if h % 9 == 0 && h * 16 / 9 == w {
        5
    } else if h % 10 == 0 && h * 16 / 10 == w {
        6
    } else if (h % 4 == 0 && h * 5 / 4 == w) || (h % 9 == 0 && h * 15 / 9 == w) {
        7
    } else {
        10
    };
    let rate = (refresh_mhz.max(1000)) as f64 / 1000.0;
    let h_period_us = (1_000_000.0 / rate - MIN_V_BLANK_US) / h as f64;
    let vbi = ((MIN_V_BLANK_US / h_period_us).floor() as u32 + 1).max(V_FRONT + v_sync + MIN_V_BACK);
    let vtotal = h + vbi;
    let htotal = w + H_BLANK;
    let clock_khz = (rate * vtotal as f64 * htotal as f64 / 1000.0 / CLOCK_STEP_KHZ).floor() * CLOCK_STEP_KHZ;
    Modeline {
        clock_khz: clock_khz as u32,
        h: [w, w + H_FRONT, w + H_FRONT + H_SYNC, htotal],
        v: [h, h + V_FRONT, h + V_FRONT + v_sync, vtotal],
    }
}

/// `xrandr --newmode` arguments for `m` (reduced blanking: +hsync -vsync).
pub fn newmode_args(name: &str, m: &Modeline) -> Vec<String> {
    let mut a = vec!["--newmode".to_string(), name.to_string(), format!("{}.{:03}", m.clock_khz / 1000, m.clock_khz % 1000)];
    a.extend(m.h.iter().chain(m.v.iter()).map(|n| n.to_string()));
    a.push("+hsync".into());
    a.push("-vsync".into());
    a
}

/// Parses `xrandr --verbose` (screen 0 only).
pub fn parse_verbose(text: &str) -> Result<RandrState, String> {
    let mut st = RandrState::default();
    let mut seen_screen = false;
    let mut crtc_max: Option<usize> = None;
    // The mode being read: its index in `st.modes`, and h/v totals once known.
    let mut cur_mode: Option<(usize, u64, u32)> = None;
    let finish_mode = |st: &mut RandrState, cur: &mut Option<(usize, u64, u32)>, vtotal: Option<u32>| {
        if let (Some((i, clock_hz, htotal)), Some(vt)) = (*cur, vtotal) {
            st.modes[i].refresh_mhz = refresh_mhz(clock_hz, htotal, vt);
        }
        if vtotal.is_some() {
            *cur = None;
        }
    };
    let num = |s: &str| s.parse::<u32>().map_err(|_| format!("bad number {s:?}"));
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Screen ") {
            if seen_screen {
                break; // screen 1 and later: not ours
            }
            seen_screen = true;
            let words: Vec<&str> = rest.split(|c: char| c == ' ' || c == ',').filter(|w| !w.is_empty()).collect();
            let after = |key: &str| -> Option<(u32, u32)> {
                let i = words.iter().position(|w| *w == key)?;
                Some((words.get(i + 1)?.parse().ok()?, words.get(i + 3)?.parse().ok()?))
            };
            st.screen = after("current").ok_or("no current screen size")?;
            st.max_screen = after("maximum").unwrap_or(st.screen);
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let first = line.as_bytes()[0];
        if first != b' ' && first != b'\t' {
            // An output header: NAME STATUS [primary] [WxH+X+Y] [(0xID)] ...
            let words: Vec<&str> = line.split_whitespace().collect();
            let name = words.first().ok_or("empty output line")?.to_string();
            let connected = words.get(1) == Some(&"connected");
            let primary = words.contains(&"primary");
            let region = words.iter().find_map(|w| parse_geometry(w));
            let current = region
                .and_then(|_| words.iter().find(|w| w.starts_with("(0x")))
                .and_then(|w| u32::from_str_radix(w.trim_start_matches("(0x").trim_end_matches(')'), 16).ok());
            st.outputs.push(RandrOutput {
                name,
                connected,
                primary,
                crtc: None,
                possible_crtcs: Vec::new(),
                region,
                current,
                modes: Vec::new(),
            });
            cur_mode = None;
            continue;
        }
        let Some(out) = st.outputs.last_mut() else { continue };
        if let Some(v) = line.strip_prefix("\tCRTCs:") {
            out.possible_crtcs = v.split_whitespace().filter_map(|w| w.parse().ok()).collect();
            crtc_max = out.possible_crtcs.iter().copied().max().max(crtc_max);
        } else if let Some(v) = line.strip_prefix("\tCRTC:") {
            out.crtc = v.trim().parse().ok();
            crtc_max = out.crtc.max(crtc_max);
        } else if line.starts_with("  ") && !line.starts_with("   ") {
            // A mode: NAME (0xID) CLOCKMHz flags [*current] [+preferred]
            let words: Vec<&str> = line.split_whitespace().collect();
            let (Some(name), Some(id)) = (words.first(), words.get(1)) else { continue };
            let Ok(id) = u32::from_str_radix(id.trim_start_matches("(0x").trim_end_matches(')'), 16) else {
                continue;
            };
            let clock_hz = words
                .iter()
                .find_map(|w| w.strip_suffix("MHz"))
                .and_then(|c| c.parse::<f64>().ok())
                .map_or(0, |mhz| (mhz * 1_000_000.0).round() as u64);
            if words.contains(&"*current") && out.region.is_some() {
                out.current = Some(id);
            }
            out.modes.push(id);
            let i = match st.modes.iter().position(|m| m.id == id) {
                Some(i) => i,
                None => {
                    st.modes.push(RandrMode { id, name: name.to_string(), width: 0, height: 0, refresh_mhz: 0 });
                    st.modes.len() - 1
                }
            };
            cur_mode = Some((i, clock_hz, 0));
        } else if let Some(v) = line.trim_start().strip_prefix("h: width") {
            let words: Vec<&str> = v.split_whitespace().collect();
            if let Some((i, clock, _)) = cur_mode {
                st.modes[i].width = num(words.first().copied().unwrap_or(""))?;
                let htotal = words.iter().position(|w| *w == "total").and_then(|p| words.get(p + 1)).map(|w| num(w));
                cur_mode = Some((i, clock, htotal.transpose()?.unwrap_or(0)));
            }
        } else if let Some(v) = line.trim_start().strip_prefix("v: height") {
            let words: Vec<&str> = v.split_whitespace().collect();
            if let Some((i, _, _)) = cur_mode {
                st.modes[i].height = num(words.first().copied().unwrap_or(""))?;
                let vtotal = words.iter().position(|w| *w == "total").and_then(|p| words.get(p + 1)).map(|w| num(w));
                finish_mode(&mut st, &mut cur_mode, vtotal.transpose()?);
            }
        }
    }
    if !seen_screen {
        return Err("no \"Screen 0\" line".into());
    }
    st.crtc_count = crtc_max.map_or(0, |m| m + 1);
    Ok(st)
}

/// `WxH+X+Y` as a region.
fn parse_geometry(word: &str) -> Option<OutputRegion> {
    let (size, pos) = word.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let (x, y) = pos.split_once('+')?;
    Some(OutputRegion { x: x.parse().ok()?, y: y.parse().ok()?, width: w.parse().ok()?, height: h.parse().ok()? })
}

/// Reads the RandR state of the default screen through a fresh connection.
pub fn query() -> Result<RandrState, DisplayError> {
    use x11rb::connection::Connection as _;
    let (conn, screen) = x11rb::connect(None).map_err(|e| io_err("connect to the X server", e))?;
    let root = conn.setup().roots.get(screen).ok_or_else(|| io_err("read the X screen", "no such screen"))?.root;
    query_on(&conn, root).map_err(|e| io_err("read the RandR layout", e))
}

/// [`query`] on an existing connection.
pub fn query_on(conn: &impl x11rb::connection::Connection, root: u32) -> Result<RandrState, String> {
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::ConnectionExt as _;
    let e = |e: &dyn std::fmt::Display| e.to_string();
    let ver = conn.randr_query_version(1, 3).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
    if (ver.major_version, ver.minor_version) < (1, 3) {
        return Err(format!("RandR {}.{} is too old (1.3 needed)", ver.major_version, ver.minor_version));
    }
    let res = conn.randr_get_screen_resources_current(root).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
    let primary = conn.randr_get_output_primary(root).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?.output;
    let range = conn.randr_get_screen_size_range(root).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
    let geo = conn.get_geometry(root).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;

    let mut names = res.names.as_slice();
    let modes: Vec<RandrMode> = res
        .modes
        .iter()
        .map(|m| {
            let (name, rest) = names.split_at((m.name_len as usize).min(names.len()));
            names = rest;
            RandrMode {
                id: m.id,
                name: String::from_utf8_lossy(name).into_owned(),
                width: m.width as u32,
                height: m.height as u32,
                refresh_mhz: refresh_mhz(m.dot_clock as u64, m.htotal as u32, m.vtotal as u32),
            }
        })
        .collect();
    let crtc_index = |c: u32| res.crtcs.iter().position(|x| *x == c);
    let mut crtcs = Vec::new();
    for c in &res.crtcs {
        crtcs.push(conn.randr_get_crtc_info(*c, res.config_timestamp).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?);
    }
    let mut outputs = Vec::new();
    for o in &res.outputs {
        let info = conn.randr_get_output_info(*o, res.config_timestamp).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
        let crtc = (info.crtc != 0).then(|| crtc_index(info.crtc)).flatten();
        let lit = crtc.map(|i| &crtcs[i]).filter(|c| c.mode != 0);
        outputs.push(RandrOutput {
            name: String::from_utf8_lossy(&info.name).into_owned(),
            connected: info.connection == randr::Connection::CONNECTED,
            primary: *o == primary,
            crtc: lit.and(crtc),
            possible_crtcs: info.crtcs.iter().filter_map(|c| crtc_index(*c)).collect(),
            region: lit.map(|c| OutputRegion { x: c.x as i32, y: c.y as i32, width: c.width as u32, height: c.height as u32 }),
            current: lit.map(|c| c.mode),
            modes: info.modes.clone(),
        });
    }
    Ok(RandrState {
        screen: (geo.width as u32, geo.height as u32),
        max_screen: (range.max_width as u32, range.max_height as u32),
        crtc_count: res.crtcs.len(),
        outputs,
        modes,
    })
}

/// [`query`], falling back to parsing `xrandr --verbose` when the direct
/// connection fails.
pub fn state() -> Result<RandrState, DisplayError> {
    match query() {
        Ok(s) => Ok(s),
        Err(direct) => {
            let out = Command::new("xrandr").arg("--verbose").output().map_err(|_| direct.clone())?;
            if !out.status.success() {
                return Err(direct);
            }
            parse_verbose(&String::from_utf8_lossy(&out.stdout)).map_err(|e| io_err("read xrandr --verbose", e))
        }
    }
}

/// Whether the `xrandr` tool is installed (checked once).
pub fn xrandr_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| Command::new("xrandr").arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// Runs `xrandr` with `args` (one change).
pub fn apply(args: &[String]) -> Result<(), DisplayError> {
    if args.is_empty() {
        return Ok(());
    }
    log::info!("xrandr {}", args.join(" "));
    let out = Command::new("xrandr").args(args).output().map_err(|e| io_err("run xrandr", format!("{e} (install xrandr)")))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(io_err("xrandr", format!("{} ({})", msg.trim(), out.status)));
    }
    Ok(())
}

/// The name of a mode the host creates for `width`x`height` at `refresh_mhz`
/// on an existing output.
pub fn mode_name(tag: &str, width: u32, height: u32, refresh_mhz: u32) -> String {
    format!("{MODE_PREFIX}{tag}-{width}x{height}-{}", (refresh_mhz + 500) / 1000)
}

/// Gives `output` a `mode`, creating a CVT mode when it has none of that size.
pub fn set_output_mode(output: &str, mode: Mode) -> Result<(), DisplayError> {
    let st = state()?;
    let out = st.by_name(output).ok_or_else(|| io_err("change the mode", format!("{output} is gone")))?;
    if let Some(m) = find_mode(&st, out, mode) {
        return apply(&["--output".into(), output.into(), "--mode".into(), format!("0x{:x}", m.id)]);
    }
    let name = mode_name("m", mode.width, mode.height, mode.refresh_mhz);
    if st.mode_named(&name).is_none() {
        apply(&newmode_args(&name, &cvt_rb(mode.width, mode.height, mode.refresh_mhz)))?;
    }
    apply(&["--addmode".into(), output.into(), name.clone()])?;
    apply(&["--output".into(), output.into(), "--mode".into(), name])
}

/// RandR change notifications on the root window, waited for on the
/// connection's socket.
pub struct RandrEvents {
    conn: x11rb::rust_connection::RustConnection,
}

impl RandrEvents {
    pub fn open() -> Result<Self, DisplayError> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
        let (conn, screen) = x11rb::connect(None).map_err(|e| io_err("connect to the X server", e))?;
        let root = conn.setup().roots.get(screen).ok_or_else(|| io_err("read the X screen", "no such screen"))?.root;
        conn.randr_select_input(root, NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE)
            .map_err(|e| io_err("watch RandR changes", e))?;
        conn.flush().map_err(|e| io_err("watch RandR changes", e))?;
        Ok(RandrEvents { conn })
    }

    /// Takes every buffered event; `Some(true)` if one was a RandR change,
    /// `None` when the connection is gone.
    fn drain(&self) -> Option<bool> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::Event;
        let mut changed = false;
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_))) => changed = true,
                Ok(Some(_)) => {}
                Ok(None) => return Some(changed),
                Err(_) => return None,
            }
        }
    }
}

impl LayoutEvents for RandrEvents {
    fn wait(&mut self, timeout: Duration) -> LayoutEvent {
        use std::os::fd::AsRawFd;
        let deadline = Instant::now() + timeout;
        loop {
            match self.drain() {
                None => return LayoutEvent::Closed,
                Some(true) => return LayoutEvent::Changed,
                Some(false) => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return LayoutEvent::Timeout;
            }
            let mut fd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
            let n = unsafe { libc::poll(&mut fd, 1, ms) };
            if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return LayoutEvent::Closed;
            }
            if fd.revents & (libc::POLLERR | libc::POLLHUP) != 0 && fd.revents & libc::POLLIN == 0 {
                return LayoutEvent::Closed;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// Hand-written in the shape of `xrandr --verbose` (xrandr 1.5, an intel
    /// DDX-style setup): a laptop panel, a disconnected HDMI port and the
    /// driver's spare `VIRTUAL1` output, trimmed to the lines the parser reads plus a few it
    /// must skip (EDID, properties). Not captured from a real session: xrandr is
    /// not installed on the machine this was written on.
    pub const PANEL_AND_SPARE: &str = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
eDP-1 connected primary 1920x1080+0+0 (0x46) normal (normal left inverted right x axis y axis) 344mm x 194mm
\tIdentifier: 0x42
\tTimestamp:  5316
\tSubpixel:   unknown
\tGamma:      1.0:1.0:1.0
\tBrightness: 1.0
\tClones:
\tCRTC:       0
\tCRTCs:      0 1 2
\tTransform:  1.000000 0.000000 0.000000
\t            0.000000 1.000000 0.000000
\t            0.000000 0.000000 1.000000
\t           filter:
\tEDID:
\t\t00ffffffffffff0030e4d8040000000000
\tscaling mode: Full aspect
\t\tsupported: Full, Center, Full aspect
  1920x1080 (0x46) 138.500MHz +HSync -VSync *current +preferred
        h: width  1920 start 1968 end 2000 total 2080 skew    0 clock  66.59KHz
        v: height 1080 start 1083 end 1088 total 1111           clock  59.93Hz
  1680x1050 (0x47) 146.250MHz -HSync +VSync
        h: width  1680 start 1784 end 1960 total 2240 skew    0 clock  65.29KHz
        v: height 1050 start 1053 end 1059 total 1089           clock  59.95Hz
HDMI-1 disconnected (normal left inverted right x axis y axis)
\tIdentifier: 0x43
\tTimestamp:  5316
\tClones:
\tCRTCs:      0 1 2
VIRTUAL1 disconnected (normal left inverted right x axis y axis)
\tIdentifier: 0x49
\tTimestamp:  5316
\tClones:
\tCRTCs:      3
";

    /// [`PANEL_AND_SPARE`] with the host's virtual output lit at 1600x720
    /// right of the panel, and an external monitor mirroring the panel.
    pub const WITH_VIRTUAL_AND_MIRROR: &str = "\
Screen 0: minimum 8 x 8, current 3520 x 1080, maximum 32767 x 32767
eDP-1 connected primary 1920x1080+0+0 (0x46) normal (normal left inverted right x axis y axis) 344mm x 194mm
\tCRTC:       0
\tCRTCs:      0 1 2
  1920x1080 (0x46) 138.500MHz +HSync -VSync *current +preferred
        h: width  1920 start 1968 end 2000 total 2080 skew    0 clock  66.59KHz
        v: height 1080 start 1083 end 1088 total 1111           clock  59.93Hz
HDMI-1 connected 1920x1080+0+0 (0x46) normal (normal left inverted right x axis y axis) 527mm x 296mm
\tCRTC:       1
\tCRTCs:      0 1 2
  1920x1080 (0x46) 138.500MHz +HSync -VSync *current
        h: width  1920 start 1968 end 2000 total 2080 skew    0 clock  66.59KHz
        v: height 1080 start 1083 end 1088 total 1111           clock  59.93Hz
VIRTUAL1 disconnected 1600x720+1920+0 (0x4a) normal (normal left inverted right x axis y axis) 0mm x 0mm
\tCRTC:       3
\tCRTCs:      3
  displayswarm-e41699b8-1600x720 (0x4a) 74.500MHz +HSync -VSync *current
        h: width  1600 start 1648 end 1680 total 1760 skew    0 clock  42.33KHz
        v: height  720 start  723 end  733 total  741           clock  57.13Hz
";
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn parses_outputs_crtcs_and_modes() {
        let s = parse_verbose(PANEL_AND_SPARE).unwrap();
        assert_eq!(s.screen, (1920, 1080));
        assert_eq!(s.max_screen, (32767, 32767));
        assert_eq!(s.crtc_count, 4);
        assert_eq!(s.outputs.len(), 3);
        let panel = s.by_name("eDP-1").unwrap();
        assert!(panel.connected && panel.primary);
        assert_eq!(panel.crtc, Some(0));
        assert_eq!(panel.region, Some(OutputRegion { x: 0, y: 0, width: 1920, height: 1080 }));
        assert_eq!(panel.current, Some(0x46));
        assert_eq!(panel.modes, vec![0x46, 0x47]);
        let m = s.mode(0x46).unwrap();
        assert_eq!((m.width, m.height), (1920, 1080));
        assert_eq!(m.refresh_mhz, 59_934, "from the clock and totals, not the rounded Hz column");
        let v = s.by_name("VIRTUAL1").unwrap();
        assert!(!v.connected && v.crtc.is_none() && v.region.is_none());
        assert_eq!(v.possible_crtcs, vec![3]);
        assert!(!s.crtc_in_use(3) && s.crtc_in_use(0));
    }

    #[test]
    fn a_non_verbose_or_empty_text_is_an_error() {
        assert!(parse_verbose("").is_err());
        assert!(parse_verbose("eDP-1 connected").is_err());
    }

    #[test]
    fn layout_lists_connected_or_lit_outputs_only() {
        let l = layout_from_state(&parse_verbose(PANEL_AND_SPARE).unwrap());
        let names: Vec<&str> = l.outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["eDP-1"], "dark, unplugged connectors are not monitors");
        let p = &l.outputs[0];
        assert!(p.enabled && p.primary && p.builtin && p.mirror_of.is_none());
        assert_eq!(p.mode, Some(Mode { width: 1920, height: 1080, refresh_mhz: 59_934 }));
    }

    #[test]
    fn same_rectangle_is_a_mirror_of_the_primary() {
        let l = layout_from_state(&parse_verbose(WITH_VIRTUAL_AND_MIRROR).unwrap());
        assert_eq!(l.outputs.len(), 3);
        assert_eq!(l.output("HDMI-1").unwrap().mirror_of.as_deref(), Some("eDP-1"));
        assert_eq!(l.output("eDP-1").unwrap().mirror_of, None);
        let v = l.output("VIRTUAL1").unwrap();
        assert!(v.enabled && !v.builtin && v.mirror_of.is_none());
        assert_eq!(v.region, Some(OutputRegion { x: 1920, y: 0, width: 1600, height: 720 }));
    }

    #[test]
    fn without_a_primary_the_panel_is_the_mirror_source() {
        let text = WITH_VIRTUAL_AND_MIRROR.replace("connected primary", "connected");
        let l = layout_from_state(&parse_verbose(&text).unwrap());
        assert_eq!(l.output("HDMI-1").unwrap().mirror_of.as_deref(), Some("eDP-1"));
        assert!(!l.output("eDP-1").unwrap().primary);
    }

    #[test]
    fn ops_become_one_xrandr_call_grouped_by_output() {
        let s = parse_verbose(WITH_VIRTUAL_AND_MIRROR).unwrap();
        let ops = [
            LayoutOp::MirrorNone { name: "HDMI-1".into() },
            LayoutOp::SetPrimary { name: "eDP-1".into() },
            LayoutOp::Move { name: "VIRTUAL1".into(), x: 0, y: 1080 },
            LayoutOp::SetMode { name: "HDMI-1".into(), mode: Mode { width: 1920, height: 1080, refresh_mhz: 60_000 } },
        ];
        let (args, modes) = plan_ops(&s, &ops).unwrap();
        assert_eq!(
            args,
            [
                "--output", "HDMI-1", "--pos", "3520x0", "--mode", "0x46", //
                "--output", "eDP-1", "--primary", //
                "--output", "VIRTUAL1", "--pos", "0x1080",
            ]
        );
        assert!(modes.is_empty());
    }

    #[test]
    fn enable_goes_beside_the_others_and_unknown_modes_are_split_off() {
        let mut s = parse_verbose(PANEL_AND_SPARE).unwrap();
        s.outputs[0].region = None;
        s.outputs[0].crtc = None;
        let want = Mode { width: 1280, height: 800, refresh_mhz: 60_000 };
        let (args, modes) =
            plan_ops(&s, &[LayoutOp::Enable { name: "eDP-1".into() }, LayoutOp::SetMode { name: "eDP-1".into(), mode: want }])
                .unwrap();
        assert_eq!(args, ["--output", "eDP-1", "--auto", "--pos", "0x0"]);
        assert_eq!(modes, vec![("eDP-1".to_string(), want)]);
    }

    #[test]
    fn negative_positions_and_unknown_outputs_are_refused() {
        let s = parse_verbose(PANEL_AND_SPARE).unwrap();
        assert!(plan_ops(&s, &[LayoutOp::Move { name: "eDP-1".into(), x: -1280, y: 0 }]).unwrap_err().contains("negative"));
        assert!(plan_ops(&s, &[LayoutOp::Enable { name: "DP-9".into() }]).unwrap_err().contains("DP-9"));
    }

    #[test]
    fn cvt_reduced_blanking_matches_the_cvt_tool() {
        // `cvt -r 1920 1080`: 138.50  1920 1968 2000 2080  1080 1083 1088 1111 +hsync -vsync
        let m = cvt_rb(1920, 1080, 60_000);
        assert_eq!(m, Modeline { clock_khz: 138_500, h: [1920, 1968, 2000, 2080], v: [1080, 1083, 1088, 1111] });
        assert_eq!(
            newmode_args("x", &m),
            ["--newmode", "x", "138.500", "1920", "1968", "2000", "2080", "1080", "1083", "1088", "1111", "+hsync", "-vsync"]
        );
        // `cvt -r 1280 800`: 71.00  1280 1328 1360 1440  800 803 809 823
        assert_eq!(cvt_rb(1280, 800, 60_000), Modeline { clock_khz: 71_000, h: [1280, 1328, 1360, 1440], v: [800, 803, 809, 823] });
        // A phone's odd aspect gets the 10-line vsync.
        let p = cvt_rb(2400, 1088, 60_000);
        assert_eq!(p.v[2] - p.v[1], 10);
        assert!(p.refresh_mhz().abs_diff(60_000) < 300, "{}", p.refresh_mhz());
    }

    #[test]
    fn mirror_captures_the_named_output_else_the_primary() {
        let s = parse_verbose(WITH_VIRTUAL_AND_MIRROR).unwrap();
        assert_eq!(pick_capture_output(&s, None).unwrap().0, "eDP-1");
        let (name, r) = pick_capture_output(&s, Some("VIRTUAL1")).unwrap();
        assert_eq!((name.as_str(), r.x, r.width), ("VIRTUAL1", 1920, 1600));
        assert_eq!(pick_capture_output(&s, Some("DP-9")).unwrap().0, "eDP-1", "an unknown name falls back");
        let no_primary = parse_verbose(&WITH_VIRTUAL_AND_MIRROR.replace("connected primary", "connected")).unwrap();
        assert_eq!(pick_capture_output(&no_primary, None).unwrap().0, "eDP-1", "first lit");
        assert!(pick_capture_output(&RandrState::default(), None).is_none());
    }

    #[test]
    fn builtin_panels_are_recognised_by_connector_name() {
        for n in ["eDP-1", "eDP1", "LVDS-1", "DSI-1"] {
            assert!(is_builtin(n), "{n}");
        }
        for n in ["HDMI-1", "DP-2", "VIRTUAL1"] {
            assert!(!is_builtin(n), "{n}");
        }
    }

    #[test]
    fn mode_names_carry_the_prefix() {
        assert_eq!(mode_name("e41699b8", 1600, 720, 59_940), "displayswarm-e41699b8-1600x720-60");
        assert!(mode_name("m", 1, 1, 0).starts_with(MODE_PREFIX));
    }

    /// Read-only: reads RandR on whatever X server `DISPLAY` names (Xwayland on
    /// a Wayland desktop) and waits briefly for change events. Changes nothing.
    /// Run with `cargo test --lib -- --ignored --nocapture live_randr`.
    #[test]
    #[ignore]
    fn live_randr_snapshot_is_read_only() {
        let s = query().expect("RandR query");
        println!("screen {:?} (max {:?}), {} CRTCs", s.screen, s.max_screen, s.crtc_count);
        for o in &s.outputs {
            let mode = o.current.and_then(|id| s.mode(id));
            println!(
                "{:<12} connected={} primary={} crtc={:?} possible={:?} region={:?} mode={:?}",
                o.name, o.connected, o.primary, o.crtc, o.possible_crtcs, o.region, mode
            );
        }
        let layout = layout_from_state(&s);
        println!("layout: {layout:#?}");
        println!("spare output: {:?}", crate::display::x11_virtual::spare_output(&s, false).map(|o| &o.name));
        assert!(s.screen.0 > 0 && s.screen.1 > 0);
        assert!(layout.outputs.iter().any(|o| o.enabled), "at least one lit output");
        let mut ev = RandrEvents::open().expect("select RandR events");
        let got = ev.wait(Duration::from_millis(300));
        println!("events after 300 ms: {got:?}");
        assert_ne!(got, LayoutEvent::Closed);
    }
}
