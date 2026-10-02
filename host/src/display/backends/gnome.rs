//! GNOME's layout through Mutter's `org.gnome.Mutter.DisplayConfig`, behind
//! [`LayoutBackend`].
//!
//! Mutter's model differs from the neutral one: a *logical monitor* has a
//! position, a scale and one or more physical monitors (several means they are
//! mirrored), and a monitor that is in no logical monitor is switched off. The
//! functions here translate both ways; [`super::super::mutter`] keeps the wire
//! types and the D-Bus calls. Change events are Mutter's `MonitorsChanged`
//! signal, listened to on a thread of its own.

use crate::display::backend::{LayoutBackend, LayoutEvent, LayoutEvents};
use crate::display::model::{Layout, LayoutCaps, LayoutOp, Mode, Output};
use crate::display::mutter::{self, DisplayConfig, LogicalConfig, State};
use crate::display::{DisplayError, OutputRegion};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

pub struct GnomeLayout;

/// A refresh rate in millihertz from a Mutter mode id such as `1920x1080@59.997`.
fn refresh_mhz_of(mode_id: &str) -> u32 {
    mode_id.split('@').nth(1).and_then(|r| r.parse::<f64>().ok()).map_or(0, |hz| (hz * 1000.0).round() as u32)
}

/// The neutral layout for a Mutter state. Every physical monitor is an output;
/// one in no logical monitor is disabled. Monitors sharing a logical monitor are
/// mirrors: all but the first report the first as `mirror_of`.
pub fn layout_from_state(state: &State) -> Layout {
    let outputs = state
        .monitors
        .iter()
        .map(|m| {
            let logical = state.logicals.iter().find(|l| l.monitors.iter().any(|(c, _)| c == &m.connector));
            let mirror_of = logical.and_then(|l| {
                let first = &l.monitors.first()?.0;
                (first != &m.connector).then(|| first.clone())
            });
            let mode_id = logical.and_then(|l| l.monitors.iter().find(|(c, _)| c == &m.connector)).map(|(_, id)| id);
            let mode = mode_id
                .and_then(|id| m.modes.iter().find(|x| &x.id == id))
                .or_else(|| m.modes.iter().find(|x| x.current))
                .map(|x| Mode { width: x.width as u32, height: x.height as u32, refresh_mhz: refresh_mhz_of(&x.id) });
            Output {
                name: m.connector.clone(),
                region: logical.and_then(|l| {
                    let (width, height) = mutter::logical_size(state, l)?;
                    Some(OutputRegion { x: l.x, y: l.y, width, height })
                }),
                enabled: logical.is_some(),
                mirror_of,
                primary: logical.is_some_and(|l| l.primary),
                builtin: m.builtin,
                mode,
            }
        })
        .collect();
    Layout { outputs }
}

/// Mutter rejects a layout whose top-left is not (0, 0); shifts it to satisfy that.
fn normalise(cfgs: &mut [LogicalConfig]) {
    let dx = cfgs.iter().map(|c| c.x).min().unwrap_or(0);
    let dy = cfgs.iter().map(|c| c.y).min().unwrap_or(0);
    for c in cfgs.iter_mut() {
        c.x -= dx;
        c.y -= dy;
    }
}

/// The x that puts a new logical monitor just right of everything else.
fn right_edge(state: &State, cfgs: &[LogicalConfig]) -> i32 {
    cfgs.iter()
        .filter_map(|l| mutter::logical_size(state, l).map(|(w, _)| l.x + w as i32))
        .max()
        .unwrap_or(0)
}

/// The logical monitors after applying `ops` to `state`, ready for
/// `ApplyMonitorsConfig`. Nothing has changed when it errs.
pub fn plan_ops(state: &State, ops: &[LayoutOp]) -> Result<Vec<LogicalConfig>, String> {
    let mut cfgs = state.logicals.clone();
    let monitor = |name: &str| state.monitors.iter().find(|m| m.connector == name).ok_or(format!("{name} is not in the layout"));
    let position_of = |cfgs: &[LogicalConfig], name: &str| cfgs.iter().position(|l| l.monitors.iter().any(|(c, _)| c == name));
    for op in ops {
        match op {
            LayoutOp::Enable { name } => {
                let m = monitor(name)?;
                if position_of(&cfgs, name).is_none() {
                    let mode = m.best_mode().ok_or(format!("{name} has no modes"))?.id.clone();
                    let x = right_edge(state, &cfgs);
                    let primary = !cfgs.iter().any(|l| l.primary);
                    cfgs.push(LogicalConfig { x, y: 0, scale: 1.0, transform: 0, primary, monitors: vec![(name.clone(), mode)] });
                }
            }
            LayoutOp::MirrorNone { name } => {
                monitor(name)?;
                if let Some(i) = position_of(&cfgs, name) {
                    if cfgs[i].monitors.len() > 1 {
                        let at = cfgs[i].monitors.iter().position(|(c, _)| c == name).unwrap_or(0);
                        let own = cfgs[i].monitors.remove(at);
                        let x = right_edge(state, &cfgs);
                        cfgs.push(LogicalConfig { x, y: 0, scale: cfgs[i].scale, transform: 0, primary: false, monitors: vec![own] });
                    }
                }
            }
            LayoutOp::SetPrimary { name } => {
                monitor(name)?;
                let i = position_of(&cfgs, name).ok_or(format!("{name} is switched off"))?;
                for (j, l) in cfgs.iter_mut().enumerate() {
                    l.primary = j == i;
                }
            }
            LayoutOp::Move { name, x, y } => {
                monitor(name)?;
                let i = position_of(&cfgs, name).ok_or(format!("{name} is switched off"))?;
                cfgs[i].x = *x;
                cfgs[i].y = *y;
            }
            LayoutOp::SetMode { name, mode } => {
                let m = monitor(name)?;
                // Mutter's modes carry no separate refresh rate here, so the size decides.
                let id = m
                    .modes
                    .iter()
                    .filter(|x| x.width as u32 == mode.width && x.height as u32 == mode.height)
                    .min_by_key(|x| refresh_mhz_of(&x.id).abs_diff(mode.refresh_mhz))
                    .ok_or(format!("{name} has no {}x{} mode", mode.width, mode.height))?
                    .id
                    .clone();
                let i = position_of(&cfgs, name).ok_or(format!("{name} is switched off"))?;
                for (c, mid) in cfgs[i].monitors.iter_mut() {
                    if c == name {
                        *mid = id.clone();
                    }
                }
            }
        }
    }
    normalise(&mut cfgs);
    Ok(cfgs)
}

impl LayoutBackend for GnomeLayout {
    fn kind(&self) -> &'static str {
        "gnome"
    }

    fn caps(&self) -> LayoutCaps {
        LayoutCaps::ALL
    }

    fn snapshot(&self) -> Result<Layout, DisplayError> {
        Ok(layout_from_state(&DisplayConfig::connect()?.state()?))
    }

    fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError> {
        let dc = DisplayConfig::connect()?;
        let state = dc.state()?;
        let cfgs = plan_ops(&state, ops)
            .map_err(|detail| DisplayError::Io { context: "change the GNOME monitor layout".into(), detail })?;
        dc.apply(&state, &cfgs, mutter::METHOD_TEMPORARY)
    }

    fn events(&self) -> Option<Box<dyn LayoutEvents>> {
        MonitorsChanged::subscribe().map(|e| Box::new(e) as Box<dyn LayoutEvents>)
    }
}

/// Mutter's `MonitorsChanged` signal as [`LayoutEvents`]. A thread reads the
/// blocking signal iterator and forwards to a channel; it ends at the first
/// signal after this is dropped (tearing a session down removes the phone's
/// monitor, which is one).
pub struct MonitorsChanged {
    rx: mpsc::Receiver<()>,
    alive: Arc<AtomicBool>,
}

impl MonitorsChanged {
    fn subscribe() -> Option<Self> {
        let conn = zbus::blocking::Connection::session().ok()?;
        let proxy = zbus::blocking::Proxy::new_owned(
            conn,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
        )
        .ok()?;
        let signals = proxy.receive_signal("MonitorsChanged").ok()?;
        // The iterator borrows the proxy, so both live on the thread.
        let (tx, rx) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        let spawned = std::thread::Builder::new().name("displayswarm-mutter-signal".into()).spawn(move || {
            let _proxy = proxy;
            for _ in signals {
                if !flag.load(Ordering::Acquire) || tx.send(()).is_err() {
                    return;
                }
            }
        });
        spawned.ok()?;
        Some(MonitorsChanged { rx, alive })
    }
}

impl Drop for MonitorsChanged {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

impl LayoutEvents for MonitorsChanged {
    fn wait(&mut self, timeout: Duration) -> LayoutEvent {
        match self.rx.recv_timeout(timeout) {
            Ok(()) => LayoutEvent::Changed,
            Err(RecvTimeoutError::Timeout) => LayoutEvent::Timeout,
            Err(RecvTimeoutError::Disconnected) => LayoutEvent::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::mutter::fixtures::panel_plus_virtual;

    fn mode(w: u32, h: u32) -> Mode {
        Mode { width: w, height: h, refresh_mhz: 60000 }
    }

    #[test]
    fn converts_monitors_to_outputs_by_connector() {
        let l = layout_from_state(&panel_plus_virtual());
        let panel = l.output("eDP-1").unwrap();
        assert!(panel.builtin && panel.enabled && panel.primary);
        assert_eq!(panel.region, Some(OutputRegion { x: 0, y: 0, width: 1920, height: 1080 }));
        assert_eq!(panel.mode, Some(mode(1920, 1080)));
        let v = l.output("Meta-0").unwrap();
        assert!(!v.builtin && !v.primary && v.mirror_of.is_none());
        assert_eq!(v.region, Some(OutputRegion { x: 1920, y: 0, width: 1080, height: 2400 }));
    }

    #[test]
    fn a_monitor_in_no_logical_monitor_is_disabled() {
        let mut s = panel_plus_virtual();
        s.logicals.remove(0);
        let l = layout_from_state(&s);
        let panel = l.output("eDP-1").unwrap();
        assert!(!panel.enabled && panel.region.is_none() && !panel.primary);
    }

    #[test]
    fn monitors_sharing_a_logical_monitor_are_mirrors() {
        let mut s = panel_plus_virtual();
        let second = s.logicals.remove(1);
        s.logicals[0].monitors.push(second.monitors[0].clone());
        let l = layout_from_state(&s);
        assert_eq!(l.output("eDP-1").unwrap().mirror_of, None);
        assert_eq!(l.output("Meta-0").unwrap().mirror_of.as_deref(), Some("eDP-1"));
    }

    #[test]
    fn switching_the_panel_off_and_on_again() {
        let mut s = panel_plus_virtual();
        s.logicals.remove(0);
        s.logicals[0].primary = true;
        // Only the phone's monitor is left, at x=1920.
        let cfgs = plan_ops(&s, &[LayoutOp::Enable { name: "eDP-1".into() }]).unwrap();
        assert_eq!(cfgs.len(), 2);
        let panel = cfgs.iter().find(|c| c.monitors[0].0 == "eDP-1").unwrap();
        assert_eq!(panel.monitors[0].1, "1920x1080@60");
        assert!(!panel.primary, "the phone's logical monitor kept the primary flag");
        assert_eq!(cfgs.iter().map(|c| c.x).min(), Some(0), "the layout starts at the origin again");
    }

    #[test]
    fn enabling_into_a_layout_with_no_primary_takes_it() {
        let mut s = panel_plus_virtual();
        s.logicals.remove(0);
        s.logicals[0].primary = false;
        let cfgs = plan_ops(&s, &[LayoutOp::Enable { name: "eDP-1".into() }]).unwrap();
        assert!(cfgs.iter().find(|c| c.monitors[0].0 == "eDP-1").unwrap().primary);
    }

    #[test]
    fn unmirroring_splits_the_monitor_out_to_the_right() {
        let mut s = panel_plus_virtual();
        let second = s.logicals.remove(1);
        s.logicals[0].monitors.push(second.monitors[0].clone());
        let cfgs = plan_ops(&s, &[LayoutOp::MirrorNone { name: "Meta-0".into() }]).unwrap();
        assert_eq!(cfgs.len(), 2);
        assert_eq!(cfgs[0].monitors, vec![("eDP-1".to_string(), "1920x1080@60".to_string())]);
        assert_eq!(cfgs[1].monitors[0].0, "Meta-0");
        assert_eq!(cfgs[1].x, 1920);
        assert!(cfgs[0].primary && !cfgs[1].primary);
    }

    #[test]
    fn set_primary_and_move() {
        let s = panel_plus_virtual();
        let cfgs = plan_ops(
            &s,
            &[LayoutOp::SetPrimary { name: "Meta-0".into() }, LayoutOp::Move { name: "Meta-0".into(), x: -1080, y: 0 }],
        )
        .unwrap();
        assert!(!cfgs[0].primary && cfgs[1].primary);
        // Normalised: the phone's monitor is now at the origin and the panel right of it.
        assert_eq!((cfgs[1].x, cfgs[0].x), (0, 1080));
    }

    #[test]
    fn set_mode_picks_the_mode_of_that_size() {
        let s = panel_plus_virtual();
        let cfgs = plan_ops(&s, &[LayoutOp::SetMode { name: "eDP-1".into(), mode: mode(800, 600) }]).unwrap();
        assert_eq!(cfgs[0].monitors[0].1, "800x600@60");
        assert!(plan_ops(&s, &[LayoutOp::SetMode { name: "eDP-1".into(), mode: mode(1, 1) }]).is_err());
    }

    #[test]
    fn unknown_monitors_and_switched_off_ones_are_errors() {
        let mut s = panel_plus_virtual();
        assert!(plan_ops(&s, &[LayoutOp::SetPrimary { name: "DP-9".into() }]).unwrap_err().contains("DP-9"));
        s.logicals.remove(0);
        assert!(plan_ops(&s, &[LayoutOp::SetPrimary { name: "eDP-1".into() }]).unwrap_err().contains("switched off"));
    }

    #[test]
    fn refresh_comes_from_the_mode_id() {
        assert_eq!(refresh_mhz_of("1920x1080@59.997"), 59997);
        assert_eq!(refresh_mhz_of("1920x1080"), 0);
    }

    #[test]
    fn the_reconcile_fixes_translate_into_a_valid_mutter_config() {
        use crate::display::reconcile::{reconcile, Intent};
        // "Switch to external screen": the panel is off, the phone's monitor is alone.
        let mut s = panel_plus_virtual();
        s.logicals.remove(0);
        s.logicals[0].primary = true;
        let out = reconcile(&layout_from_state(&s), "Meta-0", Intent::Extend);
        let cfgs = plan_ops(&s, &out.ops).unwrap();
        let l = layout_from_state(&State { logicals: cfgs, ..s });
        assert!(l.output("eDP-1").unwrap().enabled);
        assert!(reconcile(&l, "Meta-0", Intent::Extend).ops.len() <= 1, "at most the primary flag is left to fix");
    }
}
