//! The interface every desktop's layout adapter implements, plus the two
//! backends that need no desktop: [`NoLayout`] (a session whose layout we can
//! neither read nor change) and [`FakeLayout`] (a scriptable in-memory layout
//! for the reconcile and watcher tests).

use super::model::{Layout, LayoutCaps, LayoutOp};
use super::{DisplayError, OutputRegion};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a [`LayoutEvents::wait`] call returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutEvent {
    /// The layout may have changed; read it again.
    Changed,
    /// Nothing happened within the timeout.
    Timeout,
    /// The event source is gone; stop watching.
    Closed,
}

/// A source of "the layout changed" notifications (a D-Bus signal, a poll).
pub trait LayoutEvents: Send {
    fn wait(&mut self, timeout: Duration) -> LayoutEvent;
}

pub trait LayoutBackend: Send + Sync {
    /// Short name for logs ("kde", "gnome", "none", "fake").
    fn kind(&self) -> &'static str;
    fn caps(&self) -> LayoutCaps;
    fn snapshot(&self) -> Result<Layout, DisplayError>;
    /// Applies the ops as one change.
    fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError>;
    /// A fresh event stream, or `None` when the backend cannot watch (the
    /// caller then does not watch).
    fn events(&self) -> Option<Box<dyn LayoutEvents>>;

    /// Bounding box of the enabled outputs, if the layout can be read.
    fn desktop_bounds(&self) -> Option<OutputRegion> {
        bounds_of(&self.snapshot().ok()?)
    }

    /// The primary output's region, if the layout can be read and one is marked.
    fn primary_region(&self) -> Option<OutputRegion> {
        let layout = self.snapshot().ok()?;
        layout.outputs.iter().find(|o| o.enabled && o.primary).and_then(|o| o.region)
    }
}

/// Change events for a desktop that has no signal: reads the layout every
/// `poll` and reports [`LayoutEvent::Changed`] when it differs from the last
/// read. The first read happens at construction, so a change between the
/// caller's own snapshot and the first `wait` is not lost.
pub struct PollEvents {
    read: Box<dyn FnMut() -> Option<Layout> + Send>,
    poll: Duration,
    last: Option<Layout>,
}

impl PollEvents {
    pub fn new(mut read: Box<dyn FnMut() -> Option<Layout> + Send>, poll: Duration) -> Self {
        let last = read();
        PollEvents { read, poll, last }
    }
}

impl LayoutEvents for PollEvents {
    fn wait(&mut self, timeout: Duration) -> LayoutEvent {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return LayoutEvent::Timeout;
            }
            std::thread::sleep(self.poll.min(left));
            // A failed read (the tool hiccuped) is not a change.
            if let Some(now) = (self.read)() {
                if self.last.as_ref() != Some(&now) {
                    self.last = Some(now);
                    return LayoutEvent::Changed;
                }
            }
        }
    }
}

/// An enabled output in `after` that is not in `before` (by name), preferring
/// one whose size is `want` (within 2 px, for rounding under fractional scaling).
/// With no size match, the only new output. This is how a portal VIRTUAL monitor
/// is found when the portal's Start response carries no position or size.
pub fn find_new_output<'a>(
    before: &Layout,
    after: &'a Layout,
    want: Option<(u32, u32)>,
) -> Option<&'a super::model::Output> {
    let new: Vec<&super::model::Output> = after
        .outputs
        .iter()
        .filter(|o| o.enabled && o.region.is_some() && before.output(&o.name).is_none())
        .collect();
    want.and_then(|(w, h)| {
        new.iter().copied().find(|o| o.region.is_some_and(|r| r.width.abs_diff(w) <= 2 && r.height.abs_diff(h) <= 2))
    })
    .or_else(|| if new.len() == 1 { new.first().copied() } else { None })
}

/// Bounding box of the enabled outputs that have a region.
pub fn bounds_of(layout: &Layout) -> Option<OutputRegion> {
    let regions: Vec<OutputRegion> = layout.outputs.iter().filter(|o| o.enabled).filter_map(|o| o.region).collect();
    let first = regions.first()?;
    let (mut x0, mut y0) = (first.x, first.y);
    let (mut x1, mut y1) = (first.x + first.width as i32, first.y + first.height as i32);
    for r in &regions {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.width as i32);
        y1 = y1.max(r.y + r.height as i32);
    }
    Some(OutputRegion { x: x0, y: y0, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
}

/// The backend for a session whose layout is out of reach. Everything says
/// "unsupported"; callers degrade quietly (Mirror still works).
pub struct NoLayout;

impl LayoutBackend for NoLayout {
    fn kind(&self) -> &'static str {
        "none"
    }
    fn caps(&self) -> LayoutCaps {
        LayoutCaps::NONE
    }
    fn snapshot(&self) -> Result<Layout, DisplayError> {
        Err(DisplayError::Unsupported { what: "reading the monitor layout".into() })
    }
    fn apply(&self, _ops: &[LayoutOp]) -> Result<(), DisplayError> {
        Err(DisplayError::Unsupported { what: "changing the monitor layout".into() })
    }
    fn events(&self) -> Option<Box<dyn LayoutEvents>> {
        None
    }
}

struct FakeState {
    layout: Layout,
    /// Bumped on every change; event streams compare against it.
    generation: u64,
    applied: Vec<Vec<LayoutOp>>,
    fail_apply: bool,
    closed: bool,
}

struct FakeShared {
    state: Mutex<FakeState>,
    cv: Condvar,
}

/// A layout held in memory. `apply` edits it the way a real compositor would,
/// `set_layout` plays the part of the user in the desktop's own switcher, and
/// both wake the event streams.
#[derive(Clone)]
pub struct FakeLayout {
    shared: Arc<FakeShared>,
    caps: LayoutCaps,
}

impl FakeLayout {
    pub fn new(layout: Layout) -> Self {
        FakeLayout {
            shared: Arc::new(FakeShared {
                state: Mutex::new(FakeState { layout, generation: 0, applied: Vec::new(), fail_apply: false, closed: false }),
                cv: Condvar::new(),
            }),
            caps: LayoutCaps::ALL,
        }
    }

    pub fn with_caps(mut self, caps: LayoutCaps) -> Self {
        self.caps = caps;
        self
    }

    /// Replaces the layout, as if the user changed it elsewhere.
    pub fn set_layout(&self, layout: Layout) {
        let mut s = self.shared.state.lock().unwrap();
        s.layout = layout;
        s.generation += 1;
        self.shared.cv.notify_all();
    }

    /// Edits the layout in place, as if the user changed it elsewhere.
    pub fn edit(&self, f: impl FnOnce(&mut Layout)) {
        let mut s = self.shared.state.lock().unwrap();
        f(&mut s.layout);
        s.generation += 1;
        self.shared.cv.notify_all();
    }

    pub fn current(&self) -> Layout {
        self.shared.state.lock().unwrap().layout.clone()
    }

    /// Every batch `apply` received, in order.
    pub fn applied(&self) -> Vec<Vec<LayoutOp>> {
        self.shared.state.lock().unwrap().applied.clone()
    }

    /// Makes the next applies fail.
    pub fn set_fail_apply(&self, fail: bool) {
        self.shared.state.lock().unwrap().fail_apply = fail;
    }

    /// Ends every event stream with [`LayoutEvent::Closed`].
    pub fn close_events(&self) {
        let mut s = self.shared.state.lock().unwrap();
        s.closed = true;
        self.shared.cv.notify_all();
    }
}

fn apply_op(layout: &mut Layout, op: &LayoutOp) {
    let find = |layout: &mut Layout, name: &str| layout.outputs.iter().position(|o| o.name == name);
    match op {
        LayoutOp::Enable { name } => {
            if let Some(i) = find(layout, name) {
                let o = &mut layout.outputs[i];
                o.enabled = true;
                if o.region.is_none() {
                    o.region = Some(OutputRegion { x: 0, y: 0, width: 1920, height: 1080 });
                }
            }
        }
        LayoutOp::MirrorNone { name } => {
            if let Some(i) = find(layout, name) {
                layout.outputs[i].mirror_of = None;
            }
        }
        LayoutOp::SetPrimary { name } => {
            if find(layout, name).is_some() {
                for o in &mut layout.outputs {
                    o.primary = o.name == *name;
                }
            }
        }
        LayoutOp::Move { name, x, y } => {
            if let Some(i) = find(layout, name) {
                if let Some(r) = &mut layout.outputs[i].region {
                    r.x = *x;
                    r.y = *y;
                }
            }
        }
        LayoutOp::SetMode { name, mode } => {
            if let Some(i) = find(layout, name) {
                let o = &mut layout.outputs[i];
                o.mode = Some(*mode);
                if let Some(r) = &mut o.region {
                    r.width = mode.width;
                    r.height = mode.height;
                }
            }
        }
    }
}

impl LayoutBackend for FakeLayout {
    fn kind(&self) -> &'static str {
        "fake"
    }
    fn caps(&self) -> LayoutCaps {
        self.caps
    }
    fn snapshot(&self) -> Result<Layout, DisplayError> {
        if !self.caps.query {
            return Err(DisplayError::Unsupported { what: "reading the monitor layout".into() });
        }
        Ok(self.current())
    }
    fn apply(&self, ops: &[LayoutOp]) -> Result<(), DisplayError> {
        if !self.caps.apply {
            return Err(DisplayError::Unsupported { what: "changing the monitor layout".into() });
        }
        let mut s = self.shared.state.lock().unwrap();
        s.applied.push(ops.to_vec());
        if s.fail_apply {
            return Err(DisplayError::Io { context: "fake apply".into(), detail: "scripted failure".into() });
        }
        for op in ops {
            apply_op(&mut s.layout, op);
        }
        s.generation += 1;
        self.shared.cv.notify_all();
        Ok(())
    }
    fn events(&self) -> Option<Box<dyn LayoutEvents>> {
        if !self.caps.events {
            return None;
        }
        let seen = self.shared.state.lock().unwrap().generation;
        Some(Box::new(FakeEvents { shared: self.shared.clone(), seen }))
    }
}

struct FakeEvents {
    shared: Arc<FakeShared>,
    seen: u64,
}

impl LayoutEvents for FakeEvents {
    fn wait(&mut self, timeout: Duration) -> LayoutEvent {
        let deadline = Instant::now() + timeout;
        let mut s = self.shared.state.lock().unwrap();
        loop {
            if s.generation != self.seen {
                self.seen = s.generation;
                return LayoutEvent::Changed;
            }
            if s.closed {
                return LayoutEvent::Closed;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return LayoutEvent::Timeout;
            }
            s = self.shared.cv.wait_timeout(s, left).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::model::Output;

    fn out(name: &str, x: i32, w: u32, primary: bool) -> Output {
        Output {
            name: name.into(),
            region: Some(OutputRegion { x, y: 0, width: w, height: 1080 }),
            enabled: true,
            mirror_of: None,
            primary,
            builtin: false,
            mode: None,
        }
    }

    #[test]
    fn no_layout_is_unsupported_everywhere() {
        let b = NoLayout;
        assert_eq!(b.caps(), LayoutCaps::NONE);
        assert!(matches!(b.snapshot(), Err(DisplayError::Unsupported { .. })));
        assert!(matches!(b.apply(&[]), Err(DisplayError::Unsupported { .. })));
        assert!(b.events().is_none());
        assert!(b.desktop_bounds().is_none());
        assert!(b.primary_region().is_none());
        assert_eq!(b.kind(), "none");
    }

    #[test]
    fn bounds_cover_every_enabled_output() {
        let mut l = Layout { outputs: vec![out("a", 0, 1920, true), out("b", 1920, 1280, false)] };
        assert_eq!(bounds_of(&l), Some(OutputRegion { x: 0, y: 0, width: 3200, height: 1080 }));
        l.outputs[1].enabled = false;
        assert_eq!(bounds_of(&l), Some(OutputRegion { x: 0, y: 0, width: 1920, height: 1080 }));
        assert_eq!(bounds_of(&Layout::default()), None);
    }

    #[test]
    fn a_new_output_is_found_by_name_and_preferably_by_size() {
        let before = Layout { outputs: vec![out("a", 0, 1920, true)] };
        let mut after = before.clone();
        after.outputs.push(out("b", 1920, 1280, false));
        assert_eq!(find_new_output(&before, &after, None).unwrap().name, "b");
        assert_eq!(find_new_output(&before, &after, Some((1281, 1080))).unwrap().name, "b", "2 px tolerance");
        after.outputs.push(out("c", 3200, 1600, false));
        assert!(find_new_output(&before, &after, None).is_none(), "two candidates and no size: no guess");
        assert_eq!(find_new_output(&before, &after, Some((1600, 1080))).unwrap().name, "c");
        after.outputs[1].enabled = false;
        after.outputs.remove(2);
        assert!(find_new_output(&before, &after, None).is_none(), "a disabled output does not count");
    }

    #[test]
    fn fake_applies_ops_and_wakes_events() {
        let fake = FakeLayout::new(Layout { outputs: vec![out("a", 0, 1920, true), out("b", 1920, 1280, false)] });
        let mut ev = fake.events().unwrap();
        assert_eq!(ev.wait(Duration::from_millis(5)), LayoutEvent::Timeout);
        fake.apply(&[LayoutOp::SetPrimary { name: "b".into() }, LayoutOp::Move { name: "b".into(), x: -1280, y: 0 }]).unwrap();
        assert_eq!(ev.wait(Duration::from_millis(5)), LayoutEvent::Changed);
        assert_eq!(ev.wait(Duration::from_millis(5)), LayoutEvent::Timeout);
        let l = fake.current();
        assert!(l.output("b").unwrap().primary && !l.output("a").unwrap().primary);
        assert_eq!(l.output("b").unwrap().region.unwrap().x, -1280);
        assert_eq!(fake.applied().len(), 1);
        fake.close_events();
        assert_eq!(ev.wait(Duration::from_millis(5)), LayoutEvent::Closed);
    }

    #[test]
    fn poll_events_report_changes_and_ignore_failed_reads() {
        let script = Arc::new(Mutex::new(Some(Layout { outputs: vec![out("a", 0, 1920, true)] })));
        let s2 = script.clone();
        let mut ev = PollEvents::new(Box::new(move || s2.lock().unwrap().clone()), Duration::from_millis(2));
        assert_eq!(ev.wait(Duration::from_millis(20)), LayoutEvent::Timeout);
        *script.lock().unwrap() = None;
        assert_eq!(ev.wait(Duration::from_millis(20)), LayoutEvent::Timeout, "a failed read is not a change");
        *script.lock().unwrap() = Some(Layout { outputs: vec![out("a", 0, 1280, true)] });
        assert_eq!(ev.wait(Duration::from_millis(50)), LayoutEvent::Changed);
        assert_eq!(ev.wait(Duration::from_millis(20)), LayoutEvent::Timeout);
    }

    #[test]
    fn fake_failure_is_recorded_but_changes_nothing() {
        let fake = FakeLayout::new(Layout { outputs: vec![out("a", 0, 1920, true)] });
        fake.set_fail_apply(true);
        assert!(fake.apply(&[LayoutOp::MirrorNone { name: "a".into() }]).is_err());
        assert_eq!(fake.applied().len(), 1);
        assert_eq!(fake.current().outputs[0].mirror_of, None);
    }
}
