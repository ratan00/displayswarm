//! KDE Plasma's layout through `kscreen-doctor`, behind [`LayoutBackend`].
//!
//! [`super::super::kscreen`] stays the tool-level code (parsing, argument
//! planning, the processes); this adapter only translates: the parsed
//! [`kscreen::State`] into a [`Layout`] keyed by output name, and a batch of
//! [`LayoutOp`]s into the argument list of one atomic `kscreen-doctor` call.
//! KWin has no output-change signal, so [`LayoutBackend::events`] polls.

use crate::display::backend::{LayoutBackend, LayoutEvents, PollEvents};
use crate::display::kscreen::{self, State};
use crate::display::model::{Layout, LayoutCaps, LayoutOp, Mode, Output};
use crate::display::DisplayError;
use std::time::Duration;

/// How often the layout is read for change events.
pub const POLL: Duration = Duration::from_millis(500);

/// The name KWin gives the portal's virtual output for a device
/// (`Virtual-virtual-xdp-kde-io.github.displayswarm.Host.d<first 8 of the id>`).
pub fn is_ours(name: &str, device_id: &str) -> bool {
    let id8: String = device_id.chars().filter(|c| *c != '-').take(8).collect();
    !id8.is_empty() && name.contains("displayswarm") && name.ends_with(&format!(".d{id8}"))
}

pub struct KdeLayout;

/// The neutral layout for a parsed `kscreen-doctor -j`.
pub fn layout_from_state(state: &State) -> Layout {
    let outputs = state
        .outputs
        .iter()
        .map(|o| Output {
            name: o.name.clone(),
            region: o.region(),
            enabled: o.enabled,
            mirror_of: (o.replication_source != 0)
                .then(|| state.outputs.iter().find(|m| m.id == o.replication_source).map(|m| m.name.clone()))
                .flatten(),
            primary: o.enabled && o.priority == 1,
            builtin: o.builtin,
            mode: o.current_mode.as_ref().and_then(|id| o.modes.iter().find(|m| &m.id == id)).map(|m| Mode {
                width: m.width,
                height: m.height,
                refresh_mhz: (m.refresh * 1000.0).round() as u32,
            }),
        })
        .collect();
    Layout { outputs }
}

/// What applying `ops` takes: the `kscreen-doctor` arguments for one atomic
/// change, and the mode changes, which need several calls (a custom mode may
/// have to be added first) and so run afterwards through
/// [`kscreen::set_output_mode`].
pub fn plan_ops(state: &State, ops: &[LayoutOp]) -> Result<(Vec<String>, Vec<(String, Mode)>), String> {
    let id_of = |name: &str| state.by_name(name).map(|o| o.id).ok_or_else(|| format!("{name} is not in the layout"));
    let mut args = Vec::new();
    let mut modes = Vec::new();
    for op in ops {
        match op {
            LayoutOp::Enable { name } => args.push(format!("output.{}.enable", id_of(name)?)),
            LayoutOp::MirrorNone { name } => args.push(format!("output.{}.mirror.none", id_of(name)?)),
            LayoutOp::SetPrimary { name } => args.push(format!("output.{}.priority.1", id_of(name)?)),
            LayoutOp::Move { name, x, y } => args.push(format!("output.{}.position.{x},{y}", id_of(name)?)),
            LayoutOp::SetMode { name, mode } => {
                id_of(name)?;
                modes.push((name.clone(), *mode));
            }
        }
    }
    Ok((args, modes))
}

impl LayoutBackend for KdeLayout {
    fn kind(&self) -> &'static str {
        "kde"
    }

    fn caps(&self) -> LayoutCaps {
        LayoutCaps::ALL
    }

    fn snapshot(&self) -> Result<Layout, DisplayError> {
        Ok(layout_from_state(&kscreen::state()?))
    }

    fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError> {
        let (args, modes) = plan_ops(&kscreen::state()?, ops)
            .map_err(|detail| DisplayError::Io { context: "change the KDE monitor layout".into(), detail })?;
        kscreen::apply(&args)?;
        for (name, m) in modes {
            kscreen::set_output_mode(&name, m.width, m.height, m.refresh_mhz)?;
        }
        Ok(())
    }

    fn events(&self) -> Option<Box<dyn LayoutEvents>> {
        Some(Box::new(PollEvents::new(Box::new(|| kscreen::state().ok().map(|s| layout_from_state(&s))), POLL)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::kscreen::fixtures::THREE_OUTPUTS;
    use crate::display::OutputRegion;

    const VIRT: &str = "Virtual-virtual-xdp-kde-org.kde.konsole";

    fn three() -> State {
        kscreen::parse_state(THREE_OUTPUTS).unwrap()
    }

    #[test]
    fn converts_the_fixture_by_name() {
        let l = layout_from_state(&three());
        assert_eq!(l.outputs.len(), 3);
        let panel = l.output("eDP-1").unwrap();
        assert!(panel.builtin && panel.enabled && panel.primary);
        assert_eq!(panel.mirror_of, None);
        assert_eq!(panel.mode, Some(Mode { width: 1920, height: 1080, refresh_mhz: 60096 }));
        let v = l.output(VIRT).unwrap();
        assert!(!v.builtin && !v.primary);
        assert_eq!(v.region, Some(OutputRegion { x: 3840, y: 0, width: 1280, height: 800 }));
        assert_eq!(v.mode, Some(Mode { width: 1280, height: 800, refresh_mhz: 59810 }));
    }

    #[test]
    fn mirror_source_ids_become_names() {
        let json = THREE_OUTPUTS.replace(r#""id":3,"name""#, r#""replicationSource":2,"id":3,"name""#);
        let l = layout_from_state(&kscreen::parse_state(&json).unwrap());
        assert_eq!(l.output(VIRT).unwrap().mirror_of.as_deref(), Some("eDP-1"));
        assert_eq!(l.output("eDP-1").unwrap().mirror_of, None);
    }

    #[test]
    fn a_disabled_output_has_no_region_and_is_not_primary() {
        let mut s = three();
        s.outputs[1].enabled = false;
        let l = layout_from_state(&s);
        let panel = l.output("eDP-1").unwrap();
        assert!(!panel.enabled && panel.region.is_none() && !panel.primary);
    }

    #[test]
    fn ops_become_one_kscreen_doctor_call() {
        let ops = [
            LayoutOp::MirrorNone { name: VIRT.into() },
            LayoutOp::Enable { name: "eDP-1".into() },
            LayoutOp::SetPrimary { name: "eDP-1".into() },
            LayoutOp::Move { name: VIRT.into(), x: -1280, y: 0 },
        ];
        let (args, modes) = plan_ops(&three(), &ops).unwrap();
        assert_eq!(args, ["output.3.mirror.none", "output.2.enable", "output.2.priority.1", "output.3.position.-1280,0"]);
        assert!(modes.is_empty());
    }

    #[test]
    fn mode_changes_are_split_from_the_atomic_call() {
        let mode = Mode { width: 1600, height: 720, refresh_mhz: 60000 };
        let (args, modes) = plan_ops(&three(), &[LayoutOp::SetMode { name: VIRT.into(), mode }]).unwrap();
        assert!(args.is_empty());
        assert_eq!(modes, vec![(VIRT.to_string(), mode)]);
    }

    #[test]
    fn an_unknown_output_is_an_error_not_a_guess() {
        let err = plan_ops(&three(), &[LayoutOp::Enable { name: "HDMI-A-9".into() }]).unwrap_err();
        assert!(err.contains("HDMI-A-9"));
    }

    #[test]
    fn recognises_only_this_devices_output() {
        const DEV: &str = "e41699b8-ee27-4f36-9239-95ffd2f6cd31";
        const NAME: &str = "Virtual-virtual-xdp-kde-io.github.displayswarm.Host.de41699b8";
        assert!(is_ours(NAME, DEV));
        assert!(!is_ours(NAME, "aaaaaaaa-0000-0000-0000-000000000000"));
        assert!(!is_ours("eDP-1", DEV));
        assert!(!is_ours("Virtual-virtual-xdp-kde-org.kde.konsole.de41699b8", DEV));
    }
}
