//! KDE Plasma (KWin) monitor layout through `kscreen-doctor`: read the layout,
//! give a virtual output the phone's mode, plan and apply "phone as main
//! screen", and put the old layout back.
//!
//! Everything that decides anything is a pure function over [`State`] (unit
//! tested with captured `kscreen-doctor -j` output); running the tool is a thin
//! wrapper at the bottom.
//!
//! Outputs are always addressed by their numeric id in `kscreen-doctor`
//! arguments, never by name: the portal names its virtual outputs after the
//! requesting app (`Virtual-virtual-xdp-kde-org.kde.konsole`), and the tool
//! splits `output.<name>.<setting>` on dots. Ids are only stable while the
//! output exists, so a saved layout stores names and resolves them again.
//!
//! Differences from GNOME that shape this module:
//! - The KDE portal creates every VIRTUAL monitor at 1920x1080 and ignores the
//!   size the PipeWire stream offers. The monitor is resized afterwards with a
//!   custom mode ([`set_output_mode`]); KWin then renegotiates the stream.
//! - KWin has no "temporary" apply: every change is stored in
//!   `~/.config/kwinoutputconfig.json`, keyed by the set of connected outputs.
//!   A layout applied while the phone's output exists is therefore only
//!   remembered for that set, and the journal in [`super::primary`] covers the
//!   rest.

use std::process::Command;

use serde_json::Value;

use super::{DisplayError, OutputRegion};

/// `KScreen::Output::Type::Panel`, the built-in laptop panel.
const TYPE_PANEL: u64 = 7;

/// KScreen rotations that swap width and height (`Left`, `Right`).
const ROTATION_LEFT: u32 = 2;
const ROTATION_RIGHT: u32 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub id: String,
    pub width: u32,
    pub height: u32,
    /// Hz, as KScreen reports it (for example 59.95).
    pub refresh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub id: u32,
    pub name: String,
    pub enabled: bool,
    pub builtin: bool,
    pub x: i32,
    pub y: i32,
    /// Current mode size in device pixels.
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub rotation: u32,
    /// 1 is the primary output; 0 for a disabled one.
    pub priority: u32,
    pub modes: Vec<Mode>,
    pub current_mode: Option<String>,
    /// Id of the output this one mirrors, 0 for none.
    pub replication_source: u32,
}

impl Output {
    /// Rectangle in the global layout, in logical pixels. `None` if disabled.
    pub fn region(&self) -> Option<OutputRegion> {
        if !self.enabled || self.width == 0 || self.height == 0 {
            return None;
        }
        let (mut w, mut h) = (self.width as f64, self.height as f64);
        if self.rotation == ROTATION_LEFT || self.rotation == ROTATION_RIGHT {
            std::mem::swap(&mut w, &mut h);
        }
        if self.scale > 0.0 {
            w /= self.scale;
            h /= self.scale;
        }
        Some(OutputRegion { x: self.x, y: self.y, width: w.round() as u32, height: h.round() as u32 })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub outputs: Vec<Output>,
}

impl State {
    pub fn by_name(&self, name: &str) -> Option<&Output> {
        self.outputs.iter().find(|o| o.name == name)
    }
}

fn io_err(context: &str, detail: impl std::fmt::Display) -> DisplayError {
    DisplayError::Io { context: context.into(), detail: detail.to_string() }
}

fn looks_builtin(name: &str) -> bool {
    let c = name.to_ascii_lowercase();
    c.starts_with("edp") || c.starts_with("lvds") || c.starts_with("dsi")
}

/// Parses `kscreen-doctor -j`. Disconnected outputs are skipped.
pub fn parse_state(json: &str) -> Result<State, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("kscreen-doctor -j: {e}"))?;
    let list = v.get("outputs").and_then(Value::as_array).ok_or("kscreen-doctor -j: no \"outputs\" array")?;
    let int = |o: &Value, a: &str, b: &str| o.get(a).and_then(|p| p.get(b)).and_then(Value::as_i64).unwrap_or(0);
    let mut outputs = Vec::new();
    for o in list {
        if o.get("connected").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let name = o.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let Some(id) = o.get("id").and_then(Value::as_u64) else { continue };
        let modes = o
            .get("modes")
            .and_then(Value::as_array)
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| {
                        Some(Mode {
                            id: m.get("id")?.as_str()?.to_string(),
                            width: int(m, "size", "width") as u32,
                            height: int(m, "size", "height") as u32,
                            refresh: m.get("refreshRate").and_then(Value::as_f64).unwrap_or(0.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        outputs.push(Output {
            id: id as u32,
            enabled: o.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            builtin: o.get("type").and_then(Value::as_u64) == Some(TYPE_PANEL) || looks_builtin(&name),
            x: int(o, "pos", "x") as i32,
            y: int(o, "pos", "y") as i32,
            width: int(o, "size", "width") as u32,
            height: int(o, "size", "height") as u32,
            scale: o.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
            rotation: o.get("rotation").and_then(Value::as_u64).unwrap_or(1) as u32,
            priority: o.get("priority").and_then(Value::as_u64).unwrap_or(0) as u32,
            modes,
            current_mode: o.get("currentModeId").and_then(Value::as_str).map(str::to_string),
            replication_source: o.get("replicationSource").and_then(Value::as_u64).unwrap_or(0) as u32,
            name,
        });
    }
    Ok(State { outputs })
}

/// The enabled output at `region`: exact, else same origin and size within
/// 2 px (fractional scaling rounds).
pub fn find_by_region(state: &State, region: OutputRegion) -> Option<&Output> {
    state.outputs.iter().find(|o| o.region() == Some(region)).or_else(|| {
        state.outputs.iter().find(|o| {
            o.region().is_some_and(|r| {
                r.x == region.x
                    && r.y == region.y
                    && r.width.abs_diff(region.width) <= 2
                    && r.height.abs_diff(region.height) <= 2
            })
        })
    })
}

/// An enabled output present in `after` but not `before` (by name), preferring
/// one whose *mode* is `want` (device pixels). With no size match, the only
/// new output. This is how the portal's VIRTUAL monitor is found: the KDE
/// portal reports no position for it.
pub fn find_new_output<'a>(before: &State, after: &'a State, want: Option<(u32, u32)>) -> Option<&'a Output> {
    let new: Vec<&Output> =
        after.outputs.iter().filter(|o| o.enabled && before.by_name(&o.name).is_none()).collect();
    want.and_then(|(w, h)| new.iter().copied().find(|o| o.width == w && o.height == h))
        .or_else(|| if new.len() == 1 { new.first().copied() } else { None })
}

/// Outputs present in `after` but not `before` that mirror another output.
pub fn new_mirrored_outputs<'a>(before: &State, after: &'a State) -> Vec<&'a Output> {
    after
        .outputs
        .iter()
        .filter(|o| o.replication_source != 0 && before.by_name(&o.name).is_none())
        .collect()
}

/// Makes every output that appeared since `before` stand on its own.
///
/// KWin restores a saved setup when an output appears, and a setup can make
/// the portal's virtual monitor a *mirror* of the panel. KWin then resolves
/// the screencast of that virtual output to the panel it mirrors, so the
/// phone's "extended" display streams the laptop screen. Returns the names of
/// the outputs changed; KWin saves the change, so the next virtual output of
/// that name comes up unmirrored.
pub fn unmirror_new_outputs(before: &State) -> Result<Vec<String>, DisplayError> {
    // KWin creates the output before answering the portal, but give it a
    // moment to show up in the layout.
    let mut after = state()?;
    for _ in 0..10 {
        if after.outputs.iter().any(|o| before.by_name(&o.name).is_none()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        after = state()?;
    }
    let mirrored = new_mirrored_outputs(before, &after);
    if mirrored.is_empty() {
        return Ok(Vec::new());
    }
    let args: Vec<String> = mirrored.iter().map(|o| format!("output.{}.mirror.none", o.id)).collect();
    apply(&args)?;
    Ok(mirrored.iter().map(|o| o.name.clone()).collect())
}

/// Bounding box of every enabled output.
pub fn bounds_of(state: &State) -> Option<OutputRegion> {
    let mut it = state.outputs.iter().filter_map(Output::region);
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

/// The mode of `output` with this size whose refresh is closest to `refresh_hz`.
pub fn find_mode(output: &Output, width: u32, height: u32, refresh_hz: f64) -> Option<&Mode> {
    output
        .modes
        .iter()
        .filter(|m| m.width == width && m.height == height)
        .min_by(|a, b| (a.refresh - refresh_hz).abs().total_cmp(&(b.refresh - refresh_hz).abs()))
}

/// One output's settings that "phone as main screen" changes, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedOutput {
    pub name: String,
    pub enabled: bool,
    pub x: i32,
    pub y: i32,
    pub priority: u32,
}

pub fn save(state: &State) -> Vec<SavedOutput> {
    state
        .outputs
        .iter()
        .map(|o| SavedOutput { name: o.name.clone(), enabled: o.enabled, x: o.x, y: o.y, priority: o.priority })
        .collect()
}

/// `kscreen-doctor` arguments that make the output at `region` primary and,
/// with `panel_off`, disable the built-in panel (the rest is shifted so the
/// layout starts at 0,0 again). Returns them with the region the phone's
/// output will have afterwards.
pub fn plan_primary(state: &State, region: OutputRegion, panel_off: bool) -> Result<(Vec<String>, OutputRegion), String> {
    let target = find_by_region(state, region).ok_or_else(|| {
        format!(
            "no output at {}x{}+{}+{} (have: {})",
            region.width,
            region.height,
            region.x,
            region.y,
            state
                .outputs
                .iter()
                .filter_map(|o| o.region().map(|r| format!("{} {}x{}+{}+{}", o.name, r.width, r.height, r.x, r.y)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if panel_off && target.builtin {
        return Err("the target output is the built-in panel itself".into());
    }
    let mut args = vec![format!("output.{}.priority.1", target.id)];
    let off = |o: &Output| panel_off && o.builtin && o.enabled;
    let kept: Vec<&Output> = state.outputs.iter().filter(|o| o.enabled && !off(o)).collect();
    let dx = kept.iter().map(|o| o.x).min().unwrap_or(0);
    let dy = kept.iter().map(|o| o.y).min().unwrap_or(0);
    for o in state.outputs.iter().filter(|o| off(o)) {
        args.push(format!("output.{}.disable", o.id));
    }
    if dx != 0 || dy != 0 {
        for o in &kept {
            args.push(format!("output.{}.position.{},{}", o.id, o.x - dx, o.y - dy));
        }
    }
    let mut new_region = target.region().unwrap_or(region);
    new_region.x -= dx;
    new_region.y -= dy;
    Ok((args, new_region))
}

/// Arguments that put `saved` back on the outputs that still exist (a vanished
/// virtual output is skipped). `None` if none of them exist.
pub fn plan_restore(state: &State, saved: &[SavedOutput]) -> Option<Vec<String>> {
    let present: Vec<(&SavedOutput, &Output)> =
        saved.iter().filter_map(|s| state.by_name(&s.name).map(|o| (s, o))).collect();
    if present.is_empty() {
        return None;
    }
    let mut args = Vec::new();
    for (s, o) in &present {
        if s.enabled {
            if !o.enabled {
                args.push(format!("output.{}.enable", o.id));
            }
            args.push(format!("output.{}.position.{},{}", o.id, s.x, s.y));
        } else if o.enabled {
            args.push(format!("output.{}.disable", o.id));
        }
    }
    // The saved primary, else the first saved enabled output.
    let primary = present
        .iter()
        .filter(|(s, _)| s.enabled)
        .min_by_key(|(s, _)| if s.priority == 0 { u32::MAX } else { s.priority });
    if let Some((_, o)) = primary {
        args.push(format!("output.{}.priority.1", o.id));
    }
    Some(args)
}

// ---------------------------------------------------------------------------
// kscreen-doctor
// ---------------------------------------------------------------------------

/// Whether `kscreen-doctor` can be run.
pub fn available() -> bool {
    Command::new("kscreen-doctor").arg("--help").output().is_ok_and(|o| o.status.success())
}

pub fn state() -> Result<State, DisplayError> {
    let out = Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .map_err(|e| io_err("run kscreen-doctor", format!("{e} (install kscreen / libkscreen)")))?;
    if !out.status.success() {
        return Err(io_err("kscreen-doctor -j", String::from_utf8_lossy(&out.stderr).trim()));
    }
    parse_state(&String::from_utf8_lossy(&out.stdout)).map_err(|e| io_err("read the KDE monitor layout", e))
}

/// Runs `kscreen-doctor` with `args` (one config change, applied atomically).
pub fn apply(args: &[String]) -> Result<(), DisplayError> {
    if args.is_empty() {
        return Ok(());
    }
    log::info!("kscreen-doctor {}", args.join(" "));
    let out = Command::new("kscreen-doctor")
        .args(args)
        .output()
        .map_err(|e| io_err("run kscreen-doctor", e))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(io_err("kscreen-doctor", format!("{} ({})", msg.trim(), out.status)));
    }
    Ok(())
}

/// Gives the output `name` a `width`x`height` mode at about `refresh_mhz`,
/// adding a custom mode when it has none of that size, and returns its region
/// afterwards. This is how a portal virtual monitor gets the phone's size.
pub fn set_output_mode(name: &str, width: u32, height: u32, refresh_mhz: u32) -> Result<OutputRegion, DisplayError> {
    let hz = refresh_mhz as f64 / 1000.0;
    let find = |s: &State| -> Result<(u32, Option<String>, bool), DisplayError> {
        let o = s.by_name(name).ok_or_else(|| io_err("resize the virtual monitor", format!("{name} is gone")))?;
        let m = find_mode(o, width, height, hz).map(|m| m.id.clone());
        let current = m.is_some() && m == o.current_mode;
        Ok((o.id, m, current))
    };
    let (id, mut mode, current) = find(&state()?)?;
    if current {
        return region_of_named(name);
    }
    if mode.is_none() {
        apply(&[format!("output.{id}.addCustomMode.{width}.{height}.{refresh_mhz}.full")])?;
        mode = find(&state()?)?.1;
    }
    let mode = mode.ok_or_else(|| {
        io_err("resize the virtual monitor", format!("KWin did not accept a {width}x{height} mode for {name}"))
    })?;
    apply(&[format!("output.{id}.mode.{mode}")])?;
    // KWin applies asynchronously; wait until the layout shows the new size.
    for _ in 0..20 {
        if let Some(o) = state()?.by_name(name) {
            if o.width == width && o.height == height {
                return o.region().ok_or_else(|| io_err("resize the virtual monitor", "output is disabled"));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(io_err("resize the virtual monitor", format!("{name} did not change to {width}x{height}")))
}

fn region_of_named(name: &str) -> Result<OutputRegion, DisplayError> {
    state()?
        .by_name(name)
        .and_then(Output::region)
        .ok_or_else(|| io_err("find the virtual monitor", format!("{name} is not in the layout")))
}

/// Bounding box of the desktop right now, in logical pixels.
pub fn desktop_bounds() -> Result<OutputRegion, DisplayError> {
    bounds_of(&state()?).ok_or_else(|| io_err("desktop bounds", "no enabled outputs"))
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// Trimmed `kscreen-doctor -j` from Plasma 6.7: panel, a vkms output, and
    /// the portal's virtual output (resized to 1280x800), all enabled.
    pub const THREE_OUTPUTS: &str = r#"{"features":255,"outputs":[
      {"connected":true,"currentModeId":"12","enabled":true,"id":1,"name":"Virtual-1",
       "modes":[{"id":"12","name":"1920x1080@60","refreshRate":60,"size":{"height":1080,"width":1920}}],
       "pos":{"x":1920,"y":0},"priority":2,"rotation":1,"scale":1,"size":{"height":1080,"width":1920},"type":0},
      {"connected":true,"currentModeId":"1","enabled":true,"id":2,"name":"eDP-1",
       "modes":[{"id":"1","name":"1920x1080@60","refreshRate":60.096,"size":{"height":1080,"width":1920}}],
       "pos":{"x":0,"y":0},"priority":1,"rotation":1,"scale":1,"size":{"height":1080,"width":1920},"type":7},
      {"connected":true,"currentModeId":"3","enabled":true,"id":3,"name":"Virtual-virtual-xdp-kde-org.kde.konsole",
       "modes":[{"id":"2","name":"1920x1080@60","refreshRate":60,"size":{"height":1080,"width":1920}},
                {"id":"3","name":"1280x800@60","refreshRate":59.81,"size":{"height":800,"width":1280}}],
       "pos":{"x":3840,"y":0},"priority":3,"rotation":1,"scale":1,"size":{"height":800,"width":1280},"type":0},
      {"connected":false,"enabled":false,"id":9,"name":"HDMI-A-1","modes":[],"pos":{"x":0,"y":0},"size":{"height":0,"width":0}}
    ]}"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::THREE_OUTPUTS;
    use super::*;

    const PHONE: OutputRegion = OutputRegion { x: 3840, y: 0, width: 1280, height: 800 };

    fn three() -> State {
        parse_state(THREE_OUTPUTS).unwrap()
    }

    #[test]
    fn parses_outputs_and_skips_disconnected() {
        let s = three();
        assert_eq!(s.outputs.len(), 3);
        let panel = s.by_name("eDP-1").unwrap();
        assert!(panel.builtin && panel.enabled);
        assert_eq!(panel.priority, 1);
        let v = s.by_name("Virtual-virtual-xdp-kde-org.kde.konsole").unwrap();
        assert!(!v.builtin);
        assert_eq!(v.region(), Some(PHONE));
        assert_eq!(v.current_mode.as_deref(), Some("3"));
    }

    #[test]
    fn a_new_output_mirroring_the_panel_is_found() {
        let virt = "Virtual-virtual-xdp-kde-org.kde.konsole";
        let mut before = three();
        before.outputs.retain(|o| o.name != virt);
        let mut after = three();
        assert!(new_mirrored_outputs(&before, &after).is_empty());
        let json = THREE_OUTPUTS.replace(r#""id":3,"name""#, r#""replicationSource":2,"id":3,"name""#);
        after = parse_state(&json).unwrap();
        assert_eq!(after.by_name(virt).unwrap().replication_source, 2);
        let found: Vec<_> = new_mirrored_outputs(&before, &after).iter().map(|o| o.id).collect();
        assert_eq!(found, [3]);
        // An output that already existed is not touched, mirrored or not.
        assert!(new_mirrored_outputs(&after, &after).is_empty());
    }

    #[test]
    fn region_accounts_for_scale_and_rotation() {
        let mut o = three().by_name("eDP-1").unwrap().clone();
        o.scale = 1.25;
        assert_eq!(o.region(), Some(OutputRegion { x: 0, y: 0, width: 1536, height: 864 }));
        o.scale = 1.0;
        o.rotation = ROTATION_LEFT;
        assert_eq!(o.region(), Some(OutputRegion { x: 0, y: 0, width: 1080, height: 1920 }));
        o.enabled = false;
        assert_eq!(o.region(), None);
    }

    #[test]
    fn finds_new_output_by_name() {
        let after = three();
        let mut before = after.clone();
        before.outputs.retain(|o| !o.name.contains("xdp"));
        let n = find_new_output(&before, &after, Some((1920, 1080))).unwrap();
        assert!(n.name.contains("xdp"), "only candidate is taken even with another size");
        assert!(find_new_output(&after, &after, None).is_none());
    }

    #[test]
    fn bounds_cover_all_enabled() {
        assert_eq!(bounds_of(&three()), Some(OutputRegion { x: 0, y: 0, width: 5120, height: 1080 }));
    }

    #[test]
    fn find_mode_prefers_closest_refresh() {
        let mut o = three().by_name("Virtual-1").unwrap().clone();
        o.modes.push(Mode { id: "40".into(), width: 1920, height: 1080, refresh: 40.0 });
        assert_eq!(find_mode(&o, 1920, 1080, 59.9).unwrap().id, "12");
        assert_eq!(find_mode(&o, 1920, 1080, 41.0).unwrap().id, "40");
        assert!(find_mode(&o, 1280, 800, 60.0).is_none());
    }

    #[test]
    fn primary_uses_ids_and_keeps_panel() {
        let (args, r) = plan_primary(&three(), PHONE, false).unwrap();
        assert_eq!(args, vec!["output.3.priority.1"]);
        assert_eq!(r, PHONE);
    }

    #[test]
    fn primary_with_panel_off_shifts_to_origin() {
        let (args, r) = plan_primary(&three(), PHONE, true).unwrap();
        assert_eq!(
            args,
            vec!["output.3.priority.1", "output.2.disable", "output.1.position.0,0", "output.3.position.1920,0"]
        );
        assert_eq!(r, OutputRegion { x: 1920, y: 0, width: 1280, height: 800 });
    }

    #[test]
    fn primary_errors_without_touching_anything() {
        let panel = OutputRegion { x: 0, y: 0, width: 1920, height: 1080 };
        assert!(plan_primary(&three(), panel, true).unwrap_err().contains("built-in"));
        let nowhere = OutputRegion { x: 9, y: 9, width: 10, height: 10 };
        assert!(plan_primary(&three(), nowhere, false).unwrap_err().contains("no output"));
    }

    #[test]
    fn restore_reenables_panel_and_skips_vanished_output() {
        let before = three();
        let saved = save(&before);
        // Panel off, phone primary, then the phone's output went away.
        let mut now = before.clone();
        now.outputs.retain(|o| !o.name.contains("xdp"));
        let panel = now.outputs.iter_mut().find(|o| o.builtin).unwrap();
        panel.enabled = false;
        panel.priority = 0;
        now.outputs[0].x = 0;
        let args = plan_restore(&now, &saved).unwrap();
        assert_eq!(
            args,
            vec!["output.1.position.1920,0", "output.2.enable", "output.2.position.0,0", "output.2.priority.1"]
        );
        assert!(plan_restore(&State::default(), &saved).is_none());
    }
}
