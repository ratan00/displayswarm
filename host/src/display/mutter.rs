//! `org.gnome.Mutter.DisplayConfig` client: read the monitor layout, plan a new
//! one, apply it.
//!
//! Everything that decides anything is a pure function over [`State`] (unit
//! tested without D-Bus); the D-Bus part is a thin wrapper at the bottom.
//!
//! Wire shapes (`GetCurrentState`):
//!
//! ```text
//! (u serial,
//!  a((ssss) a(siiddada{sv}) a{sv})          monitors: (connector, vendor, product, serial), modes, props
//!  a(iiduba(ssss)a{sv})                     logical monitors: x, y, scale, transform, primary, monitors, props
//!  a{sv})                                   layout-mode etc.
//! ```
//!
//! `ApplyMonitorsConfig(u serial, u method, a(iiduba(ssa{sv})) logical, a{sv} props)`.
//! `method`: 0 = verify only, 1 = temporary, 2 = persistent.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

use super::{DisplayError, OutputRegion};

const BUS_NAME: &str = "org.gnome.Mutter.DisplayConfig";
const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";

/// `ApplyMonitorsConfig` method: applied now, never written to `monitors.xml`.
pub const METHOD_TEMPORARY: u32 = 1;

type RawId = (String, String, String, String);
type RawMode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type RawMonitor = (RawId, Vec<RawMode>, HashMap<String, OwnedValue>);
type RawLogical = (i32, i32, f64, u32, bool, Vec<RawId>, HashMap<String, OwnedValue>);
/// The `GetCurrentState` reply as zbus deserialises it.
pub type RawState = (u32, Vec<RawMonitor>, Vec<RawLogical>, HashMap<String, OwnedValue>);

#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub current: bool,
    pub preferred: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub connector: String,
    /// EDID identity from `GetCurrentState`; Mutter fills in placeholders for
    /// virtual monitors. GNOME's tablet mapping names an output by these.
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub builtin: bool,
    pub modes: Vec<Mode>,
}

impl Monitor {
    /// The mode in use, else the preferred one, else the first.
    pub fn best_mode(&self) -> Option<&Mode> {
        self.modes
            .iter()
            .find(|m| m.current)
            .or_else(|| self.modes.iter().find(|m| m.preferred))
            .or_else(|| self.modes.first())
    }
}

/// One logical monitor as configured, in the form `ApplyMonitorsConfig` takes.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    /// `(connector, mode id)`.
    pub monitors: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub serial: u32,
    pub monitors: Vec<Monitor>,
    pub logicals: Vec<LogicalConfig>,
    /// `layout-mode`: 1 = logical, 2 = physical.
    pub layout_mode: Option<u32>,
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| {
            let v: &Value<'_> = v;
            bool::try_from(v).ok()
        })
        .unwrap_or(false)
}

/// Turns the raw reply into [`State`].
pub fn parse_state(raw: RawState) -> State {
    let (serial, monitors, logicals, props) = raw;
    let monitors: Vec<Monitor> = monitors
        .into_iter()
        .map(|((connector, vendor, product, serial), modes, mprops)| {
            let builtin = prop_bool(&mprops, "is-builtin") || looks_builtin(&connector);
            let modes = modes
                .into_iter()
                .map(|(id, width, height, _refresh, _scale, _scales, mp)| Mode {
                    id,
                    width,
                    height,
                    current: prop_bool(&mp, "is-current"),
                    preferred: prop_bool(&mp, "is-preferred"),
                })
                .collect();
            Monitor { connector, vendor, product, serial, builtin, modes }
        })
        .collect();
    let logicals = logicals
        .into_iter()
        .map(|(x, y, scale, transform, primary, mons, _p)| LogicalConfig {
            x,
            y,
            scale,
            transform,
            primary,
            monitors: mons
                .into_iter()
                .map(|(connector, ..)| {
                    let mode = monitors
                        .iter()
                        .find(|m| m.connector == connector)
                        .and_then(|m| m.best_mode())
                        .map(|m| m.id.clone())
                        .unwrap_or_default();
                    (connector, mode)
                })
                .collect(),
        })
        .collect();
    let layout_mode = props.get("layout-mode").and_then(|v| {
        let v: &Value<'_> = v;
        u32::try_from(v).ok()
    });
    State { serial, monitors, logicals, layout_mode }
}

fn looks_builtin(connector: &str) -> bool {
    let c = connector.to_ascii_lowercase();
    c.starts_with("edp") || c.starts_with("lvds") || c.starts_with("dsi")
}

/// Size of a logical monitor in logical pixels.
pub fn logical_size(state: &State, l: &LogicalConfig) -> Option<(u32, u32)> {
    let (conn, mode_id) = l.monitors.first()?;
    let mode = state
        .monitors
        .iter()
        .find(|m| &m.connector == conn)?
        .modes
        .iter()
        .find(|m| &m.id == mode_id)?;
    let (mut w, mut h) = (mode.width as f64, mode.height as f64);
    if l.transform & 1 == 1 {
        std::mem::swap(&mut w, &mut h);
    }
    if state.layout_mode.unwrap_or(1) == 1 && l.scale > 0.0 {
        w /= l.scale;
        h /= l.scale;
    }
    Some((w.round() as u32, h.round() as u32))
}

fn region_of(state: &State, l: &LogicalConfig) -> Option<OutputRegion> {
    let (width, height) = logical_size(state, l)?;
    Some(OutputRegion { x: l.x, y: l.y, width, height })
}

/// The index of the logical monitor at `region`: an exact match, else the same
/// origin with a size within 2 px (rounding under fractional scaling).
pub fn find_by_region(state: &State, region: OutputRegion) -> Option<usize> {
    let regions: Vec<Option<OutputRegion>> = state.logicals.iter().map(|l| region_of(state, l)).collect();
    regions
        .iter()
        .position(|r| *r == Some(region))
        .or_else(|| {
            regions.iter().position(|r| {
                r.is_some_and(|r| {
                    r.x == region.x
                        && r.y == region.y
                        && r.width.abs_diff(region.width) <= 2
                        && r.height.abs_diff(region.height) <= 2
                })
            })
        })
}

/// The monitor shown at `region`, when exactly one physical monitor is (a
/// mirrored logical monitor has several and no single identity).
pub fn monitor_at_region(state: &State, region: OutputRegion) -> Option<&Monitor> {
    let l = &state.logicals[find_by_region(state, region)?];
    match l.monitors.as_slice() {
        [(connector, _)] => state.monitors.iter().find(|m| &m.connector == connector),
        _ => None,
    }
}

/// The region of a logical monitor present in `after` but not in `before`
/// (matched by connector), preferring one of `want` size (logical pixels).
/// This is how a portal VIRTUAL monitor is found when the portal's Start
/// response carries no position/size (GNOME does not report them).
pub fn find_new_monitor(before: &State, after: &State, want: Option<(u32, u32)>) -> Option<OutputRegion> {
    let known = |conn: &str| before.logicals.iter().any(|l| l.monitors.iter().any(|(c, _)| c == conn));
    let new: Vec<OutputRegion> = after
        .logicals
        .iter()
        .filter(|l| l.monitors.iter().all(|(c, _)| !known(c)))
        .filter_map(|l| region_of(after, l))
        .collect();
    want.and_then(|(w, h)| {
        new.iter()
            .copied()
            .find(|r| r.width.abs_diff(w) <= 2 && r.height.abs_diff(h) <= 2)
    })
    .or_else(|| if new.len() == 1 { new.first().copied() } else { None })
}

/// Bounding box of every logical monitor.
pub fn bounds_of(state: &State) -> Option<OutputRegion> {
    let mut it = state.logicals.iter().filter_map(|l| region_of(state, l));
    let first = it.next()?;
    let (mut x0, mut y0) = (first.x as i64, first.y as i64);
    let (mut x1, mut y1) = (x0 + first.width as i64, y0 + first.height as i64);
    for r in it {
        x0 = x0.min(r.x as i64);
        y0 = y0.min(r.y as i64);
        x1 = x1.max(r.x as i64 + r.width as i64);
        y1 = y1.max(r.y as i64 + r.height as i64);
    }
    Some(OutputRegion { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
}

/// Mutter rejects a layout whose top-left is not (0, 0); shift to satisfy it.
fn normalise(cfgs: &mut [LogicalConfig]) -> (i32, i32) {
    let dx = cfgs.iter().map(|c| c.x).min().unwrap_or(0);
    let dy = cfgs.iter().map(|c| c.y).min().unwrap_or(0);
    for c in cfgs.iter_mut() {
        c.x -= dx;
        c.y -= dy;
    }
    (dx, dy)
}

/// A new layout that makes the monitor at `region` primary and, with
/// `panel_off`, drops the built-in panel. Returns it with the region the
/// target monitor will have afterwards (it moves when the layout is shifted to
/// keep the origin at 0,0). Errors describe why it cannot be done; nothing has
/// been changed at that point.
pub fn plan_primary(
    state: &State,
    region: OutputRegion,
    panel_off: bool,
) -> Result<(Vec<LogicalConfig>, OutputRegion), String> {
    let target = find_by_region(state, region).ok_or_else(|| {
        format!(
            "no logical monitor at {}x{}+{}+{} (have: {})",
            region.width,
            region.height,
            region.x,
            region.y,
            state
                .logicals
                .iter()
                .filter_map(|l| region_of(state, l))
                .map(|r| format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let builtin = |c: &str| state.monitors.iter().any(|m| m.builtin && m.connector == c);
    let mut cfgs = state.logicals.clone();
    if panel_off && cfgs[target].monitors.iter().all(|(c, _)| builtin(c)) {
        return Err("the target monitor is the built-in panel itself".into());
    }
    for (i, c) in cfgs.iter_mut().enumerate() {
        c.primary = i == target;
    }
    let mut target_id = target;
    if panel_off {
        let mut kept = Vec::new();
        for (i, mut c) in cfgs.into_iter().enumerate() {
            c.monitors.retain(|(conn, _)| !builtin(conn));
            if !c.monitors.is_empty() {
                if i == target {
                    target_id = kept.len();
                }
                kept.push(c);
            }
        }
        cfgs = kept;
    }
    normalise(&mut cfgs);
    let t = &cfgs[target_id];
    let mut new_region = region;
    new_region.x = t.x;
    new_region.y = t.y;
    // Size comes from the live state (it can differ from `region` by rounding).
    if let Some((w, h)) = logical_size(state, t) {
        new_region.width = w;
        new_region.height = h;
    }
    Ok((cfgs, new_region))
}

/// A layout that puts `saved` back on whatever hardware exists now: monitors
/// that vanished (the virtual one) are dropped, a mode that no longer exists
/// falls back to the monitor's best, exactly one primary is kept. `None` if
/// nothing is left to configure.
pub fn plan_restore(state: &State, saved: &[LogicalConfig]) -> Option<Vec<LogicalConfig>> {
    let mut out: Vec<LogicalConfig> = Vec::new();
    for l in saved {
        let monitors: Vec<(String, String)> = l
            .monitors
            .iter()
            .filter_map(|(conn, mode)| {
                let m = state.monitors.iter().find(|m| &m.connector == conn)?;
                let mode = if m.modes.iter().any(|x| &x.id == mode) {
                    mode.clone()
                } else {
                    m.best_mode()?.id.clone()
                };
                Some((conn.clone(), mode))
            })
            .collect();
        if !monitors.is_empty() {
            out.push(LogicalConfig { monitors, ..l.clone() });
        }
    }
    if out.is_empty() {
        return None;
    }
    let first_primary = out.iter().position(|c| c.primary).unwrap_or(0);
    for (i, c) in out.iter_mut().enumerate() {
        c.primary = i == first_primary;
    }
    normalise(&mut out);
    Some(out)
}

// ---------------------------------------------------------------------------
// D-Bus
// ---------------------------------------------------------------------------

fn io_err(context: &str, detail: impl std::fmt::Display) -> DisplayError {
    DisplayError::Io { context: context.into(), detail: detail.to_string() }
}

/// An open connection to Mutter's DisplayConfig service.
pub struct DisplayConfig {
    proxy: Proxy<'static>,
}

impl DisplayConfig {
    pub fn connect() -> Result<Self, DisplayError> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let conn = Connection::session()
            .map_err(|e| io_err("connect to the session bus", format!("{e} (not a graphical session?)")))?;
        let proxy = Proxy::new_owned(conn, BUS_NAME, OBJECT_PATH, INTERFACE)
            .map_err(|e| io_err("open org.gnome.Mutter.DisplayConfig", e))?;
        // Probe now so a non-GNOME desktop fails here with a clear message.
        if let Err(e) = proxy.call::<_, _, RawState>("GetCurrentState", &()) {
            let upper = desktop.to_ascii_uppercase();
            let hint = if upper.contains("KDE") {
                "this is KDE; the KScreen backend should have been used"
            } else if upper.contains("GNOME") {
                "the GNOME display service did not answer"
            } else {
                "only GNOME/Mutter and KDE/KScreen are supported for the monitor layout (wlroots is not implemented yet)"
            };
            return Err(io_err("phone as main screen", format!("{hint} (desktop {desktop:?}): {e}")));
        }
        Ok(Self { proxy })
    }

    pub fn state(&self) -> Result<State, DisplayError> {
        let raw: RawState = self
            .proxy
            .call("GetCurrentState", &())
            .map_err(|e| io_err("GetCurrentState", e))?;
        Ok(parse_state(raw))
    }

    /// `ApplyMonitorsConfig`. `serial` must be the one from the [`State`] the
    /// config was planned against; Mutter rejects a stale one.
    pub fn apply(&self, state: &State, cfgs: &[LogicalConfig], method: u32) -> Result<(), DisplayError> {
        let logical: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, HashMap<String, Value<'_>>)>)> = cfgs
            .iter()
            .map(|c| {
                (
                    c.x,
                    c.y,
                    c.scale,
                    c.transform,
                    c.primary,
                    c.monitors
                        .iter()
                        .map(|(conn, mode)| (conn.clone(), mode.clone(), HashMap::new()))
                        .collect(),
                )
            })
            .collect();
        let mut props: HashMap<String, Value<'_>> = HashMap::new();
        if let Some(lm) = state.layout_mode {
            props.insert("layout-mode".into(), Value::from(lm));
        }
        self.proxy
            .call::<_, _, ()>("ApplyMonitorsConfig", &(state.serial, method, logical, props))
            .map_err(|e| io_err("ApplyMonitorsConfig", e))
    }
}

/// Bounding box of the desktop right now, in logical pixels.
pub fn desktop_bounds() -> Result<OutputRegion, DisplayError> {
    let state = DisplayConfig::connect()?.state()?;
    bounds_of(&state).ok_or_else(|| io_err("desktop bounds", "no logical monitors"))
}

#[cfg(test)]
mod new_monitor_tests {
    use super::*;

    fn state(logicals: &[(&str, i32, i32, i32, i32)]) -> State {
        State {
            serial: 1,
            monitors: logicals
                .iter()
                .map(|&(c, _, _, w, h)| Monitor {
                    connector: c.into(),
                    vendor: String::new(),
                    product: String::new(),
                    serial: String::new(),
                    builtin: false,
                    modes: vec![Mode { id: "m".into(), width: w, height: h, current: true, preferred: true }],
                })
                .collect(),
            logicals: logicals
                .iter()
                .map(|&(c, x, y, _, _)| LogicalConfig {
                    x,
                    y,
                    scale: 1.0,
                    transform: 0,
                    primary: false,
                    monitors: vec![(c.into(), "m".into())],
                })
                .collect(),
            layout_mode: Some(1),
        }
    }

    #[test]
    fn finds_the_monitor_that_appeared() {
        let before = state(&[("eDP-1", 0, 0, 1920, 1080)]);
        let after = state(&[("eDP-1", 0, 0, 1920, 1080), ("Meta-0", 1920, 0, 1600, 720)]);
        let r = find_new_monitor(&before, &after, Some((1600, 720))).unwrap();
        assert_eq!(r, OutputRegion { x: 1920, y: 0, width: 1600, height: 720 });
        // Without a size hint, a single new monitor is still accepted.
        assert_eq!(find_new_monitor(&before, &after, None), Some(r));
    }

    #[test]
    fn nothing_new_or_ambiguous_is_none() {
        let before = state(&[("eDP-1", 0, 0, 1920, 1080)]);
        assert_eq!(find_new_monitor(&before, &before, Some((1600, 720))), None);
        let two = state(&[("eDP-1", 0, 0, 1920, 1080), ("Meta-0", 1920, 0, 800, 600), ("Meta-1", 2720, 0, 640, 480)]);
        assert_eq!(find_new_monitor(&before, &two, None), None);
        assert!(find_new_monitor(&before, &two, Some((640, 480))).is_some());
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    fn mon(conn: &str, builtin: bool, w: i32, h: i32) -> Monitor {
        Monitor {
            connector: conn.into(),
            vendor: if builtin { "AUO".into() } else { "MetaVendor".into() },
            product: if builtin { "0x1234".into() } else { "Virtual remote monitor".into() },
            serial: if builtin { "0x00000001".into() } else { "0x0000002".into() },
            builtin,
            modes: vec![
                Mode { id: format!("{w}x{h}@60"), width: w, height: h, current: true, preferred: true },
                Mode { id: "800x600@60".into(), width: 800, height: 600, current: false, preferred: false },
            ],
        }
    }

    fn lm(x: i32, y: i32, primary: bool, conn: &str, w: i32, h: i32) -> LogicalConfig {
        LogicalConfig { x, y, scale: 1.0, transform: 0, primary, monitors: vec![(conn.into(), format!("{w}x{h}@60"))] }
    }

    /// Panel 1920x1080 at 0,0 (primary) and a phone-sized virtual monitor to its right.
    pub fn panel_plus_virtual() -> State {
        State {
            serial: 7,
            monitors: vec![mon("eDP-1", true, 1920, 1080), mon("Meta-0", false, 1080, 2400)],
            logicals: vec![lm(0, 0, true, "eDP-1", 1920, 1080), lm(1920, 0, false, "Meta-0", 1080, 2400)],
            layout_mode: Some(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const PHONE: OutputRegion = OutputRegion { x: 1920, y: 0, width: 1080, height: 2400 };

    #[test]
    fn monitor_identity_comes_from_the_region() {
        let s = panel_plus_virtual();
        let m = monitor_at_region(&s, OutputRegion { x: 1920, y: 0, width: 1080, height: 2400 }).unwrap();
        assert_eq!((m.connector.as_str(), m.vendor.as_str()), ("Meta-0", "MetaVendor"));
        assert!(monitor_at_region(&s, OutputRegion { x: 5, y: 5, width: 10, height: 10 }).is_none());
    }

    #[test]
    fn finds_the_virtual_monitor_by_region() {
        let s = panel_plus_virtual();
        assert_eq!(find_by_region(&s, PHONE), Some(1));
        assert_eq!(find_by_region(&s, OutputRegion { width: 1081, ..PHONE }), Some(1), "rounding tolerance");
        assert_eq!(find_by_region(&s, OutputRegion { x: 5, ..PHONE }), None);
    }

    #[test]
    fn size_honours_scale_and_transform() {
        let mut s = panel_plus_virtual();
        s.logicals[0].scale = 2.0;
        assert_eq!(logical_size(&s, &s.logicals[0]), Some((960, 540)));
        s.layout_mode = Some(2);
        assert_eq!(logical_size(&s, &s.logicals[0]), Some((1920, 1080)));
        s.logicals[0].transform = 1;
        assert_eq!(logical_size(&s, &s.logicals[0]), Some((1080, 1920)));
    }

    #[test]
    fn bounds_cover_all_monitors() {
        let s = panel_plus_virtual();
        assert_eq!(bounds_of(&s), Some(OutputRegion { x: 0, y: 0, width: 3000, height: 2400 }));
    }

    #[test]
    fn make_primary_keeps_panel_and_positions() {
        let s = panel_plus_virtual();
        let (cfgs, region) = plan_primary(&s, PHONE, false).unwrap();
        assert_eq!(cfgs.len(), 2);
        assert!(!cfgs[0].primary && cfgs[1].primary);
        assert_eq!(region, PHONE);
        assert_eq!((cfgs[1].x, cfgs[1].y), (1920, 0));
    }

    #[test]
    fn panel_off_removes_it_and_shifts_to_origin() {
        let s = panel_plus_virtual();
        let (cfgs, region) = plan_primary(&s, PHONE, true).unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].monitors[0].0, "Meta-0");
        assert!(cfgs[0].primary);
        assert_eq!((cfgs[0].x, cfgs[0].y), (0, 0));
        assert_eq!(region, OutputRegion { x: 0, y: 0, width: 1080, height: 2400 });
    }

    #[test]
    fn refuses_unknown_region_and_panel_target() {
        let s = panel_plus_virtual();
        assert!(plan_primary(&s, OutputRegion { x: 9, y: 9, width: 9, height: 9 }, false).is_err());
        let panel = OutputRegion { x: 0, y: 0, width: 1920, height: 1080 };
        assert!(plan_primary(&s, panel, true).is_err());
        assert!(plan_primary(&s, panel, false).is_ok());
    }

    #[test]
    fn restore_drops_vanished_monitors_and_keeps_one_primary() {
        let before = panel_plus_virtual();
        let mut now = before.clone();
        now.monitors.retain(|m| m.connector != "Meta-0");
        now.logicals.truncate(1);
        let mut saved = before.logicals.clone();
        saved[1].primary = true; // two primaries in the journal, defensively
        let plan = plan_restore(&now, &saved).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].monitors[0].0, "eDP-1");
        assert!(plan[0].primary);
    }

    #[test]
    fn restore_falls_back_when_mode_is_gone_and_none_when_empty() {
        let now = panel_plus_virtual();
        let mut saved = now.logicals.clone();
        saved[0].monitors[0].1 = "9999x9999@1".into();
        let plan = plan_restore(&now, &saved).unwrap();
        assert_eq!(plan[0].monitors[0].1, "1920x1080@60");
        assert!(plan_restore(&now, &[lm_gone()]).is_none());
    }

    fn lm_gone() -> LogicalConfig {
        LogicalConfig { x: 0, y: 0, scale: 1.0, transform: 0, primary: true, monitors: vec![("HDMI-9".into(), "x".into())] }
    }

    #[test]
    fn builtin_detection_by_connector_name() {
        assert!(looks_builtin("eDP-1") && looks_builtin("LVDS-1") && !looks_builtin("HDMI-A-1"));
    }

    /// Read-only: parses the live session's `GetCurrentState`.
    #[test]
    #[ignore]
    fn live_get_current_state() {
        let cfg = DisplayConfig::connect().expect("Mutter DisplayConfig");
        let s = cfg.state().unwrap();
        println!("{s:#?}\nbounds: {:?}", bounds_of(&s));
        assert!(!s.logicals.is_empty());
        assert!(bounds_of(&s).is_some());
    }
}
