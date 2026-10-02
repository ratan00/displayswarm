//! Keeps a phone session consistent with what the desktop's own display switcher
//! does (KDE's F4 / Meta+P popup, System Settings, GNOME's Display settings).
//!
//! [`reconcile`](super::reconcile) is the pure policy. This module runs it: it
//! listens to a [`LayoutBackend`]'s change events while the session runs,
//! applies the corrections, and publishes the phone's monitor region so touch and
//! pen input follow it when the user moves the monitor. Desktop-specific parts
//! are the backend (how to read and change the layout, what a change event is)
//! and [`Ownership`] (which output is the phone's).
//!
//! How it behaves, in order:
//! * A change is acted on only after the layout has been quiet for
//!   [`Timing::debounce`]: a switcher change arrives as several intermediate
//!   layouts.
//! * Corrections are rate limited ([`Timing::min_gap`], [`Timing::max_fixes`]) so
//!   the host never fights the user or the compositor.
//! * After the host's own correction, events are ignored for [`Timing::guard`]
//!   (its own change echoes back as an event) and the layout is checked once more.
//! * With a backend that cannot apply changes, the watcher only observes: it
//!   still tracks the region and notices the monitor vanishing.

use super::backend::{LayoutBackend, LayoutEvent, LayoutEvents};
use super::model::{Layout, LayoutOp};
use super::reconcile::{reconcile, Intent};
use super::OutputRegion;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The timing knobs of the watcher; tests use short ones.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long the layout must be quiet before it is acted on.
    pub debounce: Duration,
    /// Events are ignored this long after the host's own correction.
    pub guard: Duration,
    /// Minimum time between two corrections.
    pub min_gap: Duration,
    pub max_fixes: u32,
    /// How often the run flag is checked while nothing happens.
    pub tick: Duration,
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        debounce: Duration::from_millis(600),
        guard: Duration::from_millis(700),
        min_gap: Duration::from_secs(2),
        max_fixes: 6,
        tick: Duration::from_millis(250),
    };
}

/// A layout that keeps changing must not keep the watcher from ever acting.
const MAX_SETTLE_EVENTS: u32 = 20;

/// Which output in a layout is the phone's.
pub enum Ownership {
    /// Recognised by name (KDE names the portal's output after the app and
    /// device).
    Named(Box<dyn Fn(&str) -> bool + Send>),
    /// The output that was not in `known` when the session started (GNOME gives
    /// a virtual monitor no name we can recognise). Once one is picked it is
    /// kept by name; if several are new at once nothing is claimed, so the
    /// watcher does not touch a monitor that may belong to someone else.
    NewSince { known: Vec<String>, ours: Option<String> },
}

/// What [`Ownership::resolve`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Ours(String),
    /// Not in the layout (yet, or any more).
    Absent,
    /// Several outputs qualify and nothing tells them apart.
    Ambiguous,
}

impl Ownership {
    pub fn named(is_ours: impl Fn(&str) -> bool + Send + 'static) -> Self {
        Ownership::Named(Box::new(is_ours))
    }

    /// The output that is new compared with `baseline`.
    pub fn new_since(baseline: &Layout) -> Self {
        Ownership::NewSince { known: baseline.outputs.iter().map(|o| o.name.clone()).collect(), ours: None }
    }

    pub fn resolve(&mut self, layout: &Layout) -> Resolved {
        match self {
            Ownership::Named(is_ours) => match layout.outputs.iter().find(|o| is_ours(&o.name)) {
                Some(o) => Resolved::Ours(o.name.clone()),
                None => Resolved::Absent,
            },
            Ownership::NewSince { known, ours } => {
                if let Some(name) = ours {
                    return if layout.output(name).is_some() { Resolved::Ours(name.clone()) } else { Resolved::Absent };
                }
                let mut fresh = layout.outputs.iter().filter(|o| !known.contains(&o.name));
                match (fresh.next(), fresh.next()) {
                    (None, _) => Resolved::Absent,
                    (Some(o), None) => {
                        *ours = Some(o.name.clone());
                        Resolved::Ours(o.name.clone())
                    }
                    _ => Resolved::Ambiguous,
                }
            }
        }
    }
}

/// Whether recognising the phone's output on the backend called `kind` needs
/// the layout from before the monitor was created (see [`ownership_for`]).
pub fn needs_baseline(kind: &str) -> bool {
    kind == "gnome"
}

/// How to recognise the phone's output on the backend called `kind`, or `None`
/// where it cannot be told apart from the others (no watcher then). KDE names
/// it after the device, X11 remembers the output it lit; GNOME needs `baseline`, the layout taken just before the
/// session created its monitor.
pub fn ownership_for(kind: &str, device_id: &str, baseline: Option<&Layout>) -> Option<Ownership> {
    match kind {
        #[cfg(target_os = "linux")]
        "kde" => {
            let device_id = device_id.to_string();
            Some(Ownership::named(move |name| super::backends::kde::is_ours(name, &device_id)))
        }
        "gnome" => baseline.map(Ownership::new_since),
        // X11: the host lit the output itself and remembers for which device.
        #[cfg(target_os = "linux")]
        "x11" => {
            let device_id = device_id.to_string();
            Some(Ownership::named(move |name| super::x11_virtual::is_owned_by(name, &device_id)))
        }
        _ => {
            let _ = device_id;
            None
        }
    }
}

/// Watches the layout until `run` is cleared, correcting it and publishing the
/// phone's monitor region on `region_tx`. Does nothing when the backend cannot
/// produce events.
pub fn spawn(
    backend: Arc<dyn LayoutBackend>,
    ownership: Ownership,
    intent: Intent,
    run: Arc<AtomicBool>,
    region_tx: Arc<watch::Sender<Option<OutputRegion>>>,
    on_lost: Box<dyn Fn() + Send>,
) {
    let Some(events) = backend.events() else { return };
    let spawned = std::thread::Builder::new().name("displayswarm-layout-watch".into()).spawn(move || {
        run_watch(&*backend, events, ownership, intent, &run, &region_tx, &*on_lost, &Timing::DEFAULT);
    });
    if let Err(e) = spawned {
        log::warn!("could not start the layout watcher: {e}");
    }
}

/// The watch loop, blocking until `run` is cleared, the event source closes or
/// the phone's monitor is lost.
#[allow(clippy::too_many_arguments)]
pub fn run_watch(
    backend: &dyn LayoutBackend,
    mut events: Box<dyn LayoutEvents>,
    mut ownership: Ownership,
    intent: Intent,
    run: &AtomicBool,
    region_tx: &watch::Sender<Option<OutputRegion>>,
    on_lost: &dyn Fn(),
    timing: &Timing,
) {
    let observe_only = !backend.caps().apply;
    let mut last_fix: Option<Instant> = None;
    let mut fixes = 0u32;
    let mut seen = false;
    let mut warned_gone = false;
    let mut warned_ambiguous = false;
    // The first look happens once the layout is quiet, like any other.
    let mut pending = true;
    while run.load(Ordering::Acquire) {
        if !pending {
            match events.wait(timing.tick) {
                LayoutEvent::Changed => {}
                LayoutEvent::Timeout => continue,
                LayoutEvent::Closed => return,
            }
        }
        // Wait for the layout to settle.
        let mut settle_events = 0;
        loop {
            match events.wait(timing.debounce) {
                LayoutEvent::Changed if settle_events < MAX_SETTLE_EVENTS => settle_events += 1,
                LayoutEvent::Changed | LayoutEvent::Timeout => break,
                LayoutEvent::Closed => return,
            }
        }
        if !run.load(Ordering::Acquire) {
            return;
        }
        pending = false;

        let layout = match backend.snapshot() {
            Ok(l) => l,
            Err(e) => {
                log::debug!("layout watch: cannot read the layout: {}", e.summary());
                continue;
            }
        };
        let phone = match ownership.resolve(&layout) {
            Resolved::Ours(name) => name,
            Resolved::Ambiguous => {
                if !warned_ambiguous {
                    warned_ambiguous = true;
                    log::warn!("layout watch: cannot tell which new monitor is the phone's; leaving the layout alone");
                }
                continue;
            }
            Resolved::Absent => {
                if seen {
                    // The output existed and is gone. KWin drops it when it is made a
                    // mirror ("Unify outputs" in its switcher), and the stream dies with it.
                    log::warn!("layout watch: the phone's monitor disappeared; treating it as a request to mirror");
                    on_lost();
                    return;
                }
                // Not created yet: nothing to correct.
                if !warned_gone {
                    warned_gone = true;
                    log::debug!("layout watch: the phone's monitor is not in the layout yet");
                }
                continue;
            }
        };
        warned_gone = false;
        seen = true;
        let outcome = reconcile(&layout, &phone, intent);
        if !outcome.ops.is_empty() && observe_only {
            log::info!(
                "layout watch: not correcting the layout, this desktop only allows watching ({})",
                outcome.reasons.join("; ")
            );
        } else if !outcome.ops.is_empty() {
            let reasons = outcome.reasons.join("; ");
            let due = last_fix.map_or(true, |t| t.elapsed() >= timing.min_gap);
            if fixes < timing.max_fixes && due {
                fixes += 1;
                last_fix = Some(Instant::now());
                log::warn!("layout watch: {reasons}; correcting the layout");
                apply_and_settle(backend, &outcome.ops, &mut *events, timing.guard);
                // Look again once the correction has echoed, whatever the result.
                pending = true;
            } else {
                log::warn!("layout watch: leaving the layout as it is ({reasons})");
            }
            continue;
        }
        if let Some(region) = outcome.region {
            if *region_tx.borrow() != Some(region) {
                log::info!("layout watch: the phone's monitor is now at {region:?}");
                region_tx.send_replace(Some(region));
            }
        }
    }
}

/// Applies `ops`, then swallows the events the change itself causes for `guard`.
fn apply_and_settle(backend: &dyn LayoutBackend, ops: &[LayoutOp], events: &mut dyn LayoutEvents, guard: Duration) {
    if let Err(e) = backend.apply(ops) {
        log::warn!("layout watch: could not correct the layout: {}", e.summary());
    }
    let until = Instant::now() + guard;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        if events.wait(left) == LayoutEvent::Closed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::backend::FakeLayout;
    use crate::display::model::{LayoutCaps, Output};
    use std::sync::Mutex;

    const PHONE: &str = "Virtual-virtual-xdp-kde-io.github.displayswarm.Host.de41699b8";

    const FAST: Timing = Timing {
        debounce: Duration::from_millis(20),
        guard: Duration::from_millis(20),
        min_gap: Duration::from_millis(30),
        max_fixes: 6,
        tick: Duration::from_millis(10),
    };

    fn out(name: &str, x: i32, w: u32, builtin: bool) -> Output {
        Output {
            name: name.into(),
            region: Some(OutputRegion { x, y: 0, width: w, height: 1080 }),
            enabled: true,
            mirror_of: None,
            primary: builtin,
            builtin,
            mode: None,
        }
    }

    fn normal() -> Layout {
        Layout { outputs: vec![out("eDP-1", 0, 1920, true), out(PHONE, 1920, 1600, false)] }
    }

    struct Watcher {
        fake: FakeLayout,
        run: Arc<AtomicBool>,
        region: Arc<watch::Sender<Option<OutputRegion>>>,
        lost: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        fn start(fake: FakeLayout) -> Watcher {
            Self::start_with(fake, FAST)
        }

        fn start_with(fake: FakeLayout, timing: Timing) -> Watcher {
            let run = Arc::new(AtomicBool::new(true));
            let region = Arc::new(watch::channel(None).0);
            let lost = Arc::new(AtomicBool::new(false));
            let (f, r, rg, l) = (fake.clone(), run.clone(), region.clone(), lost.clone());
            let events = fake.events().unwrap();
            let thread = std::thread::spawn(move || {
                run_watch(
                    &f,
                    events,
                    Ownership::named(|n| n == PHONE),
                    Intent::Extend,
                    &r,
                    &rg,
                    &move || l.store(true, Ordering::SeqCst),
                    &timing,
                );
            });
            Watcher { fake, run, region, lost, thread: Some(thread) }
        }

        fn region(&self) -> Option<OutputRegion> {
            *self.region.borrow()
        }

        fn stop(&mut self) {
            self.run.store(false, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn publishes_the_region_and_moves_with_it() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        wait_until("the first region", || w.region().is_some());
        assert_eq!(w.region().unwrap().x, 1920);
        // "Extend left" in the switcher: the phone is now on the left.
        w.fake.edit(|l| {
            l.outputs[1].region.as_mut().unwrap().x = -1600;
            l.outputs[0].region.as_mut().unwrap().x = 0;
        });
        wait_until("the moved region", || w.region().map(|r| r.x) == Some(-1600));
        assert!(w.fake.applied().is_empty(), "moving is accepted, never reverted");
        w.stop();
    }

    #[test]
    fn switch_to_external_screen_brings_the_panel_back() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        wait_until("the first region", || w.region().is_some());
        w.fake.edit(|l| {
            l.outputs[0].enabled = false;
            l.outputs[0].primary = false;
            l.outputs[1].primary = true;
        });
        wait_until("the panel to be re-enabled", || w.fake.current().output("eDP-1").unwrap().enabled);
        wait_until("the panel to be primary again", || w.fake.current().output("eDP-1").unwrap().primary);
        assert!(!w.lost.load(Ordering::SeqCst));
        w.stop();
    }

    #[test]
    fn unify_outputs_is_undone() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        wait_until("the first region", || w.region().is_some());
        w.fake.edit(|l| l.outputs[1].mirror_of = Some("eDP-1".into()));
        wait_until("the mirror to be undone", || w.fake.current().output(PHONE).unwrap().mirror_of.is_none());
        w.stop();
    }

    #[test]
    fn our_own_correction_does_not_trigger_another_one() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        wait_until("the first region", || w.region().is_some());
        w.fake.edit(|l| l.outputs[1].mirror_of = Some("eDP-1".into()));
        wait_until("the fix", || w.fake.applied().len() == 1);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(w.fake.applied().len(), 1, "the echo of our own change was acted on");
        w.stop();
    }

    #[test]
    fn the_phone_vanishing_ends_the_watch_and_reports_it() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        wait_until("the first region", || w.region().is_some());
        w.fake.edit(|l| l.outputs.retain(|o| o.name != PHONE));
        wait_until("the loss report", || w.lost.load(Ordering::SeqCst));
        w.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn a_monitor_that_was_never_created_is_not_a_loss() {
        let fake = FakeLayout::new(Layout { outputs: vec![out("eDP-1", 0, 1920, true)] });
        let mut w = Watcher::start(fake);
        std::thread::sleep(Duration::from_millis(150));
        assert!(!w.lost.load(Ordering::SeqCst));
        w.fake.edit(|l| l.outputs.push(out(PHONE, 1920, 1600, false)));
        wait_until("the region once it appears", || w.region().is_some());
        w.stop();
    }

    #[test]
    fn corrections_are_capped_when_they_do_not_stick() {
        let fake = FakeLayout::new(normal());
        fake.set_fail_apply(true);
        fake.edit(|l| l.outputs[1].mirror_of = Some("eDP-1".into()));
        let mut w = Watcher::start(fake);
        wait_until("the cap", || w.fake.applied().len() >= 6);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(w.fake.applied().len(), 6);
        w.stop();
    }

    #[test]
    fn a_backend_that_cannot_apply_is_only_observed() {
        let caps = LayoutCaps { apply: false, ..LayoutCaps::ALL };
        let fake = FakeLayout::new(normal()).with_caps(caps);
        fake.edit(|l| l.outputs[1].mirror_of = Some("eDP-1".into()));
        let mut w = Watcher::start(fake);
        wait_until("the region", || w.region().is_some());
        std::thread::sleep(Duration::from_millis(100));
        assert!(w.fake.applied().is_empty());
        w.stop();
    }

    #[test]
    fn new_since_claims_the_single_new_output_and_keeps_it() {
        let mut o = Ownership::new_since(&Layout { outputs: vec![out("eDP-1", 0, 1920, true)] });
        assert_eq!(o.resolve(&Layout { outputs: vec![out("eDP-1", 0, 1920, true)] }), Resolved::Absent);
        let with = normal();
        assert_eq!(o.resolve(&with), Resolved::Ours(PHONE.into()));
        // A second new output later does not steal the claim.
        let mut more = normal();
        more.outputs.push(out("HDMI-A-1", 3520, 1920, false));
        assert_eq!(o.resolve(&more), Resolved::Ours(PHONE.into()));
        // Once the claimed output is gone it stays gone.
        assert_eq!(o.resolve(&Layout { outputs: vec![out("eDP-1", 0, 1920, true), out("HDMI-A-1", 0, 1, false)] }), Resolved::Absent);
    }

    #[test]
    fn new_since_refuses_to_guess_between_two_new_outputs() {
        let mut o = Ownership::new_since(&Layout { outputs: vec![out("eDP-1", 0, 1920, true)] });
        let mut two = normal();
        two.outputs.push(out("Meta-1", 3520, 1080, false));
        assert_eq!(o.resolve(&two), Resolved::Ambiguous);
        // It can still resolve later, once only one of them is new... but never by guessing.
        assert_eq!(o.resolve(&two), Resolved::Ambiguous);
    }

    #[test]
    fn an_ambiguous_watch_changes_nothing_and_never_reports_a_loss() {
        let fake = FakeLayout::new(Layout { outputs: vec![out("eDP-1", 0, 1920, true), out("A", 1920, 1080, false), out("B", 3000, 1080, false)] });
        fake.edit(|l| l.outputs[1].mirror_of = Some("eDP-1".into()));
        let run = Arc::new(AtomicBool::new(true));
        let region = Arc::new(watch::channel(None).0);
        let events = fake.events().unwrap();
        let (f, r) = (fake.clone(), run.clone());
        let t = std::thread::spawn(move || {
            let baseline = Layout { outputs: vec![out("eDP-1", 0, 1920, true)] };
            run_watch(&f, events, Ownership::new_since(&baseline), Intent::Extend, &r, &region, &|| {}, &FAST);
        });
        std::thread::sleep(Duration::from_millis(150));
        run.store(false, Ordering::SeqCst);
        t.join().unwrap();
        assert!(fake.applied().is_empty());
    }

    #[test]
    fn ownership_needs_a_baseline_only_where_names_do_not_identify() {
        assert!(needs_baseline("gnome") && !needs_baseline("kde"));
        assert!(ownership_for("gnome", "d", None).is_none());
        assert!(ownership_for("gnome", "d", Some(&Layout::default())).is_some());
        assert!(ownership_for("none", "d", None).is_none());
        assert!(ownership_for("x11", "d", None).is_some() && !needs_baseline("x11"));
    }

    #[test]
    fn a_closed_event_source_ends_the_watch() {
        let mut w = Watcher::start(FakeLayout::new(normal()));
        w.fake.close_events();
        w.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn a_layout_that_never_settles_is_still_acted_on() {
        let mut w = Watcher::start_with(FakeLayout::new(normal()), Timing { debounce: Duration::from_millis(40), ..FAST });
        let fake = w.fake.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let jitter = std::thread::spawn(move || {
            let n = Mutex::new(0);
            while !s2.load(Ordering::SeqCst) {
                let mut n = n.lock().unwrap();
                *n += 1;
                let v = *n;
                fake.edit(|l| l.outputs[0].region.as_mut().unwrap().height = 1000 + (v % 2) as u32);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        wait_until("a region despite the churn", || w.region().is_some());
        stop.store(true, Ordering::SeqCst);
        jitter.join().unwrap();
        w.stop();
    }
}
