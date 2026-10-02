//! The pure policy that keeps a phone session consistent with what the desktop's
//! own display switcher does (KDE's F4 / Meta+P popup, GNOME's Display settings,
//! `kscreen-doctor`, `gnome-randr`).
//!
//! To the compositor the phone's virtual monitor is an ordinary external output,
//! so the user can move it, mirror it or turn the laptop panel off. [`reconcile`]
//! takes the current [`Layout`] and says which [`LayoutOp`]s restore the
//! invariants below and where the phone's monitor now is. It knows nothing about
//! any desktop; [`super::watch`] runs it against a [`LayoutBackend`](super::backend::LayoutBackend).
//!
//! Invariants, by [`Intent`]:
//! * I1 (both): the phone's output does not mirror another output, and no other
//!   output mirrors it (either would change what the stream shows).
//! * I2 (Extend): at least one output other than the phone's stays enabled.
//! * I3 (Extend): the phone's output is not the primary screen (that is the
//!   "Phone as main screen" role); a compositor sometimes gives a new output
//!   primary status from a saved setup.
//!
//! Moving the output is accepted: the new region is published so touch and pen
//! input follow it.

use super::model::{Layout, LayoutOp};
use super::OutputRegion;

/// What the session is for, which decides which invariants apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// The phone is an extra screen; the laptop keeps the main one.
    Extend,
    /// The phone is the main screen, possibly with the panel off on purpose, so
    /// only I1 applies.
    PhonePrimary,
}

/// What to do about the current layout.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    /// One atomic change; empty when nothing is wrong.
    pub ops: Vec<LayoutOp>,
    /// Human-readable reasons for `ops`, for the log.
    pub reasons: Vec<String>,
    /// The phone's monitor is not in the layout any more.
    pub gone: bool,
    /// Where the phone's monitor is now.
    pub region: Option<OutputRegion>,
}

pub fn reconcile(layout: &Layout, phone: &str, intent: Intent) -> Outcome {
    let mut out = Outcome::default();
    let Some(me) = layout.output(phone) else {
        out.gone = true;
        return out;
    };
    if me.mirror_of.is_some() {
        out.ops.push(LayoutOp::MirrorNone { name: me.name.clone() });
        out.reasons.push(format!("{} was made a mirror, so it would stream another screen", me.name));
    }
    for o in layout.outputs.iter().filter(|o| o.name != me.name && o.mirror_of.as_deref() == Some(phone)) {
        out.ops.push(LayoutOp::MirrorNone { name: o.name.clone() });
        out.reasons.push(format!("{} was made a mirror of the phone's display", o.name));
    }
    if intent == Intent::Extend {
        let others_on = layout.outputs.iter().any(|o| o.name != me.name && o.enabled);
        if !others_on {
            // Prefer the built-in panel; otherwise any other output.
            if let Some(o) = layout.outputs.iter().filter(|o| o.name != me.name).min_by_key(|o| !o.builtin) {
                out.ops.push(LayoutOp::Enable { name: o.name.clone() });
                out.reasons.push("the phone's display was left as the only screen".into());
            }
        }
        if me.enabled && me.primary {
            if let Some(panel) = layout.outputs.iter().find(|o| o.name != me.name && o.enabled && o.builtin) {
                out.ops.push(LayoutOp::SetPrimary { name: panel.name.clone() });
                out.reasons.push("the phone's display had become the primary screen".into());
            }
        }
    }
    out.region = me.region;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::model::Output;

    const PHONE: &str = "Virtual-virtual-xdp-kde-io.github.displayswarm.Host.de41699b8";

    fn out(name: &str, x: i32, w: u32, h: u32, builtin: bool) -> Output {
        Output {
            name: name.into(),
            region: Some(OutputRegion { x, y: 0, width: w, height: h }),
            enabled: true,
            mirror_of: None,
            primary: builtin,
            builtin,
            mode: None,
        }
    }

    fn layout() -> Layout {
        Layout { outputs: vec![out("eDP-1", 0, 1920, 1080, true), out(PHONE, 1920, 1600, 720, false)] }
    }

    fn run(l: &Layout) -> Outcome {
        reconcile(l, PHONE, Intent::Extend)
    }

    #[test]
    fn a_normal_layout_needs_no_fix_and_reports_the_region() {
        let o = run(&layout());
        assert!(o.ops.is_empty() && !o.gone);
        assert_eq!(o.region, Some(OutputRegion { x: 1920, y: 0, width: 1600, height: 720 }));
    }

    #[test]
    fn extend_left_only_moves_the_region() {
        let mut l = layout();
        l.outputs[1].region.as_mut().unwrap().x = -1600;
        let o = run(&l);
        assert!(o.ops.is_empty());
        assert_eq!(o.region.unwrap().x, -1600);
    }

    #[test]
    fn unify_outputs_is_undone_in_either_direction() {
        let mut l = layout();
        l.outputs[1].mirror_of = Some("eDP-1".into()); // the phone mirrors the panel
        assert_eq!(run(&l).ops, vec![LayoutOp::MirrorNone { name: PHONE.into() }]);
        let mut l = layout();
        l.outputs[0].mirror_of = Some(PHONE.into()); // the panel mirrors the phone
        assert_eq!(run(&l).ops, vec![LayoutOp::MirrorNone { name: "eDP-1".into() }]);
    }

    #[test]
    fn switch_to_external_screen_brings_the_panel_back() {
        let mut l = layout();
        l.outputs[0].enabled = false;
        l.outputs[0].primary = false;
        l.outputs[1].primary = true;
        let o = run(&l);
        assert_eq!(o.ops, vec![LayoutOp::Enable { name: "eDP-1".into() }]);
    }

    #[test]
    fn the_phone_does_not_stay_primary_in_extend() {
        let mut l = layout();
        l.outputs[0].primary = false;
        l.outputs[1].primary = true;
        assert_eq!(run(&l).ops, vec![LayoutOp::SetPrimary { name: "eDP-1".into() }]);
    }

    #[test]
    fn a_missing_output_is_reported_not_fixed() {
        let l = Layout { outputs: vec![out("eDP-1", 0, 1920, 1080, true)] };
        let o = run(&l);
        assert!(o.gone && o.ops.is_empty());
    }

    #[test]
    fn the_panel_is_preferred_over_another_external_output() {
        let mut l = layout();
        l.outputs.push(out("HDMI-A-1", 3520, 1920, 1080, false));
        l.outputs[0].enabled = false;
        l.outputs[2].enabled = false;
        assert_eq!(run(&l).ops, vec![LayoutOp::Enable { name: "eDP-1".into() }]);
        l.outputs.remove(0);
        assert_eq!(run(&l).ops, vec![LayoutOp::Enable { name: "HDMI-A-1".into() }]);
    }

    #[test]
    fn phone_as_main_may_keep_the_panel_off_and_stay_primary() {
        let mut l = layout();
        l.outputs[0].enabled = false;
        l.outputs[0].primary = false;
        l.outputs[1].primary = true;
        assert!(reconcile(&l, PHONE, Intent::PhonePrimary).ops.is_empty());
        // Mirroring is still undone.
        l.outputs[1].mirror_of = Some("eDP-1".into());
        assert_eq!(
            reconcile(&l, PHONE, Intent::PhonePrimary).ops,
            vec![LayoutOp::MirrorNone { name: PHONE.into() }]
        );
    }

    #[test]
    fn every_fix_comes_with_a_reason() {
        let mut l = layout();
        l.outputs[0].enabled = false;
        l.outputs[1].mirror_of = Some("eDP-1".into());
        let o = run(&l);
        assert_eq!(o.ops.len(), o.reasons.len());
        assert_eq!(o.ops.len(), 2);
    }

    #[test]
    fn a_fix_applied_to_the_fake_backend_converges() {
        use crate::display::backend::{FakeLayout, LayoutBackend};
        let mut l = layout();
        l.outputs[0].enabled = false;
        l.outputs[0].primary = false;
        l.outputs[1].primary = true;
        l.outputs[1].mirror_of = Some("eDP-1".into());
        let fake = FakeLayout::new(l);
        // The panel has to be back before it can take the primary status, so it
        // takes a second pass, as it does live.
        for _ in 0..2 {
            fake.apply(&run(&fake.current()).ops).unwrap();
        }
        let after = run(&fake.current());
        assert!(after.ops.is_empty(), "still wrong: {:?}", after.reasons);
        assert!(fake.current().output("eDP-1").unwrap().primary);
    }
}
