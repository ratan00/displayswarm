//! The platform-independent half of the Windows injector: everything that
//! decides *what* to inject (coordinates, pen and touch flags, multi-touch
//! contact bookkeeping, mouse button transitions, wheel deltas). It builds plain
//! integers; `input::windows` turns them into `POINTER_*_INFO` / `INPUT`. Kept
//! out of the `cfg(windows)` module so it is unit-tested on Linux too.
//!
//! The flag values below are the Windows SDK's (`winuser.h`); `input::windows`
//! asserts them against the `windows` crate's constants in a test that runs on
//! Windows.

use crate::display::OutputRegion;
use crate::protocol::{
    InputEvent, ACTION_CANCEL, ACTION_DOWN, ACTION_HOVER, ACTION_MOVE, ACTION_UP, POINTER_FLAG_BTN_PRIMARY,
    POINTER_FLAG_BTN_SECONDARY, POINTER_FLAG_BTN_TERTIARY, POINTER_FLAG_ERASER, POINTER_FLAG_SCROLL_DOWN,
    POINTER_FLAG_SCROLL_LEFT, POINTER_FLAG_SCROLL_RIGHT, POINTER_FLAG_SCROLL_UP,
};
use std::collections::BTreeMap;

// POINTER_FLAGS
pub const PF_NEW: u32 = 0x0000_0001;
pub const PF_INRANGE: u32 = 0x0000_0002;
pub const PF_INCONTACT: u32 = 0x0000_0004;
pub const PF_FIRSTBUTTON: u32 = 0x0000_0010;
pub const PF_SECONDBUTTON: u32 = 0x0000_0020;
pub const PF_PRIMARY: u32 = 0x0000_2000;
pub const PF_CONFIDENCE: u32 = 0x0000_4000;
pub const PF_CANCELED: u32 = 0x0000_8000;
pub const PF_DOWN: u32 = 0x0001_0000;
pub const PF_UPDATE: u32 = 0x0002_0000;
pub const PF_UP: u32 = 0x0004_0000;
// PEN_FLAGS / PEN_MASK
pub const PEN_BARREL: u32 = 0x1;
pub const PEN_INVERTED: u32 = 0x2;
pub const PEN_ERASER: u32 = 0x4;
pub const PEN_MASK_PRESSURE: u32 = 0x1;
pub const PEN_MASK_TILT_X: u32 = 0x4;
pub const PEN_MASK_TILT_Y: u32 = 0x8;
// TOUCH_MASK
pub const TOUCH_MASK_PRESSURE: u32 = 0x4;

pub const MAX_PRESSURE: u32 = 1024;
pub const WHEEL_DELTA: i32 = 120;
/// Contacts the synthetic touch device is created for.
pub const MAX_TOUCH_CONTACTS: u32 = 10;

const TILT_LIMIT_DEG: f32 = 90.0;

/// Tilt from radians (what the phone sends) to the whole degrees
/// `POINTER_PEN_INFO` wants, in -90..=90. NaN is 0: one bad field would make
/// `InjectSyntheticPointerInput` reject the whole packet.
pub fn tilt_radians_to_degrees(radians: f32) -> i32 {
    if radians.is_nan() {
        return 0;
    }
    (radians.to_degrees()).clamp(-TILT_LIMIT_DEG, TILT_LIMIT_DEG).round() as i32
}

/// 0..1 to 0..=1024 (the range pen and touch pressure use); NaN is 0.
pub fn pressure_to_1024(p: f32) -> u32 {
    if p.is_nan() {
        return 0;
    }
    (p.clamp(0.0, 1.0) * MAX_PRESSURE as f32).round() as u32
}

/// Where a normalised position lands on the virtual screen: on `region` when
/// one is set, else on `fallback` (the whole virtual screen).
pub fn map_to_screen(x_norm: f32, y_norm: f32, region: Option<OutputRegion>, fallback: OutputRegion) -> (i32, i32) {
    let (x, y) = super::region_point(x_norm, y_norm, region.unwrap_or(fallback));
    (x.round() as i32, y.round() as i32)
}

/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` coordinates (0..=65535
/// across the virtual screen) for a screen point.
pub fn to_absolute(x: i32, y: i32, virtual_screen: OutputRegion) -> (i32, i32) {
    let scale = |v: i32, origin: i32, len: u32| -> i32 {
        let span = len.saturating_sub(1).max(1) as f64;
        (((v - origin) as f64 / span) * 65535.0).round().clamp(0.0, 65535.0) as i32
    };
    (scale(x, virtual_screen.x, virtual_screen.width), scale(y, virtual_screen.y, virtual_screen.height))
}

/// The `ptPixelLocation` for a screen point. Per Microsoft's
/// `InjectSyntheticPointerInput` page it is relative to the virtual screen's
/// top-left corner, so a monitor left of or above the primary one (negative
/// screen coordinates) still maps to non-negative injected positions.
/// UNVERIFIED on a real multi-monitor machine; with the primary monitor at the
/// top-left of the desktop both readings are the same.
pub fn injection_point(x: i32, y: i32, virtual_screen: OutputRegion) -> (i32, i32) {
    (x - virtual_screen.x, y - virtual_screen.y)
}

/// Whole pixels to move the cursor for a relative step of `(dx, dy)`, carrying
/// the fractions in `frac` so slow movement is not lost to truncation.
pub fn take_relative(frac: &mut (f32, f32), dx: f32, dy: f32) -> (i32, i32) {
    let fx = frac.0 + if dx.is_finite() { dx } else { 0.0 };
    let fy = frac.1 + if dy.is_finite() { dy } else { 0.0 };
    let (ix, iy) = (fx.trunc() as i32, fy.trunc() as i32);
    *frac = (fx - ix as f32, fy - iy as f32);
    (ix, iy)
}

// ---------------------------------------------------------------------------
// Pen
// ---------------------------------------------------------------------------

/// One `POINTER_PEN_INFO` worth of decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PenFrame {
    pub pointer_flags: u32,
    pub pen_flags: u32,
    pub pen_mask: u32,
    pub pressure: u32,
    pub tilt_x: i32,
    pub tilt_y: i32,
}

/// Pen contact threshold: below this a MOVE is a hover.
const HOVER_PRESSURE: f32 = 0.01;

/// Tracks whether the pen is currently in range, so the first sample of a
/// hover or touch gets `POINTER_FLAG_NEW` and leaving range is reported.
#[derive(Debug, Default)]
pub struct PenState {
    in_range: bool,
}

impl PenState {
    pub fn frame(&mut self, e: &InputEvent) -> PenFrame {
        let eraser = e.flags & POINTER_FLAG_ERASER != 0;
        let barrel = e.flags & POINTER_FLAG_BTN_SECONDARY != 0 || e.flags & POINTER_FLAG_BTN_PRIMARY != 0;
        let touching = match e.action {
            ACTION_DOWN => true,
            ACTION_MOVE => e.pressure > HOVER_PRESSURE,
            _ => false,
        };
        let mut f = PF_CONFIDENCE | PF_PRIMARY;
        match e.action {
            ACTION_DOWN => f |= PF_INRANGE | PF_INCONTACT | PF_FIRSTBUTTON | PF_DOWN,
            ACTION_MOVE if touching => f |= PF_INRANGE | PF_INCONTACT | PF_FIRSTBUTTON | PF_UPDATE,
            ACTION_MOVE | ACTION_HOVER => f |= PF_INRANGE | PF_UPDATE,
            ACTION_UP => f |= PF_INRANGE | PF_UP, // lifted, still hovering
            ACTION_CANCEL => f |= PF_UPDATE,     // left range
            _ => f |= PF_INRANGE | PF_UPDATE,
        }
        if barrel && e.action != ACTION_CANCEL {
            f |= PF_SECONDBUTTON;
        }
        if !self.in_range && f & PF_INRANGE != 0 {
            f |= PF_NEW;
        }
        self.in_range = f & PF_INRANGE != 0;
        let mut pen_flags = 0;
        if barrel {
            pen_flags |= PEN_BARREL;
        }
        if eraser {
            pen_flags |= PEN_INVERTED;
            if touching {
                pen_flags |= PEN_ERASER;
            }
        }
        PenFrame {
            pointer_flags: f,
            pen_flags,
            pen_mask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: if touching { pressure_to_1024(e.pressure) } else { 0 },
            tilt_x: tilt_radians_to_degrees(e.tilt_x),
            tilt_y: tilt_radians_to_degrees(e.tilt_y),
        }
    }
}

// ---------------------------------------------------------------------------
// Touch
// ---------------------------------------------------------------------------

/// One finger in a touch frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchContact {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub pressure: u32,
    pub flags: u32,
}

/// Multi-touch bookkeeping. Windows wants every live contact in each injected
/// frame, so the tracker remembers the last position of all fingers down and
/// emits the whole set whenever one of them changes.
#[derive(Debug, Default)]
pub struct TouchTracker {
    down: BTreeMap<u8, (i32, i32, u32)>,
}

impl TouchTracker {
    /// The frame for `e` at screen position `(x, y)`, or `None` for an event
    /// that changes nothing (a hover, or an UP for a finger never seen down).
    pub fn frame(&mut self, e: &InputEvent, x: i32, y: i32) -> Option<Vec<TouchContact>> {
        let id = e.pointer_id;
        let pressure = pressure_to_1024(if e.pressure > 0.0 { e.pressure } else { 0.5 });
        let mut flags_for: BTreeMap<u8, u32> = BTreeMap::new();
        let mut gone: Option<u8> = None;
        match e.action {
            ACTION_DOWN => {
                self.down.insert(id, (x, y, pressure));
                flags_for.insert(id, PF_NEW | PF_INRANGE | PF_INCONTACT | PF_DOWN);
            }
            ACTION_MOVE => {
                if !self.down.contains_key(&id) {
                    // A MOVE with no DOWN (dropped packet): start the contact.
                    self.down.insert(id, (x, y, pressure));
                    flags_for.insert(id, PF_NEW | PF_INRANGE | PF_INCONTACT | PF_DOWN);
                } else {
                    self.down.insert(id, (x, y, pressure));
                    flags_for.insert(id, PF_INRANGE | PF_INCONTACT | PF_UPDATE);
                }
            }
            ACTION_UP | ACTION_CANCEL => {
                self.down.get(&id)?; // an UP for a finger never seen down changes nothing
                self.down.insert(id, (x, y, 0));
                flags_for.insert(id, if e.action == ACTION_CANCEL { PF_UP | PF_CANCELED } else { PF_UP });
                gone = Some(id);
            }
            _ => return None,
        }
        let first = *self.down.keys().next()?;
        let out = self
            .down
            .iter()
            .map(|(pid, (px, py, pr))| {
                let flags = flags_for.get(pid).copied().unwrap_or(PF_INRANGE | PF_INCONTACT | PF_UPDATE);
                TouchContact {
                    id: *pid as u32,
                    x: *px,
                    y: *py,
                    pressure: *pr,
                    flags: flags | PF_CONFIDENCE | if *pid == first { PF_PRIMARY } else { 0 },
                }
            })
            .collect();
        if let Some(g) = gone {
            self.down.remove(&g);
        }
        Some(out)
    }

    pub fn active(&self) -> usize {
        self.down.len()
    }
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Button presses and releases needed to go from held-mask `prev` to `now`
/// (`POINTER_FLAG_BTN_*` bits). Releases come first so a swap never has two
/// buttons down at once.
pub fn mouse_transitions(prev: u8, now: u8) -> Vec<(MouseButton, bool)> {
    let buttons = [
        (POINTER_FLAG_BTN_PRIMARY, MouseButton::Left),
        (POINTER_FLAG_BTN_SECONDARY, MouseButton::Right),
        (POINTER_FLAG_BTN_TERTIARY, MouseButton::Middle),
    ];
    let mut out = Vec::new();
    for (bit, b) in buttons {
        if prev & bit != 0 && now & bit == 0 {
            out.push((b, false));
        }
    }
    for (bit, b) in buttons {
        if prev & bit == 0 && now & bit != 0 {
            out.push((b, true));
        }
    }
    out
}

/// `(vertical, horizontal)` wheel movement for one event's scroll bits, in
/// `WHEEL_DELTA` units: up and right are positive, as `SendInput` wants.
pub fn scroll_deltas(flags: u8) -> (i32, i32) {
    let mut v = 0;
    let mut h = 0;
    if flags & POINTER_FLAG_SCROLL_UP != 0 {
        v += WHEEL_DELTA;
    }
    if flags & POINTER_FLAG_SCROLL_DOWN != 0 {
        v -= WHEEL_DELTA;
    }
    if flags & POINTER_FLAG_SCROLL_RIGHT != 0 {
        h += WHEEL_DELTA;
    }
    if flags & POINTER_FLAG_SCROLL_LEFT != 0 {
        h -= WHEEL_DELTA;
    }
    (v, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{INPUT_TYPE_STYLUS, INPUT_TYPE_TOUCH};

    fn ev(kind: u8, id: u8, action: u8, pressure: f32) -> InputEvent {
        InputEvent {
            event_type: kind,
            pointer_id: id,
            action,
            flags: 0,
            x_norm: 0.5,
            y_norm: 0.5,
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
        }
    }

    #[test]
    fn tilt_and_pressure_are_clamped_and_never_nan() {
        assert_eq!(tilt_radians_to_degrees(std::f32::consts::FRAC_PI_4), 45);
        assert_eq!(tilt_radians_to_degrees(-9.0), -90);
        assert_eq!(tilt_radians_to_degrees(f32::INFINITY), 90);
        assert_eq!(tilt_radians_to_degrees(f32::NAN), 0);
        assert_eq!(pressure_to_1024(1.0), 1024);
        assert_eq!(pressure_to_1024(0.5), 512);
        assert_eq!(pressure_to_1024(7.0), 1024);
        assert_eq!(pressure_to_1024(-1.0), 0);
        assert_eq!(pressure_to_1024(f32::NAN), 0);
    }

    #[test]
    fn coordinates_map_onto_the_region_or_the_virtual_screen() {
        let vs = OutputRegion { x: -1920, y: 0, width: 4920, height: 2400 };
        let phone = OutputRegion { x: 1920, y: 0, width: 1080, height: 2400 };
        assert_eq!(map_to_screen(0.0, 0.0, Some(phone), vs), (1920, 0));
        assert_eq!(map_to_screen(1.0, 1.0, Some(phone), vs), (2999, 2399));
        assert_eq!(map_to_screen(0.0, 0.0, None, vs), (-1920, 0));
        assert_eq!(map_to_screen(9.0, -3.0, Some(phone), vs), (2999, 0), "clamped to the phone");
    }

    #[test]
    fn absolute_mouse_coordinates_span_the_virtual_desktop() {
        let vs = OutputRegion { x: -1920, y: 0, width: 4920, height: 2400 };
        assert_eq!(to_absolute(-1920, 0, vs), (0, 0));
        assert_eq!(to_absolute(2999, 2399, vs), (65535, 65535));
        let (x, _) = to_absolute(540, 0, vs);
        assert!((x - 32768).abs() <= 10, "{x}");
        assert_eq!(to_absolute(99999, -5, vs), (65535, 0));
    }

    #[test]
    fn injection_points_are_relative_to_the_virtual_screen_origin() {
        let vs = OutputRegion { x: -1920, y: -100, width: 4920, height: 2400 };
        assert_eq!(injection_point(-1920, -100, vs), (0, 0));
        assert_eq!(injection_point(1920, 0, vs), (3840, 100));
        let primary_first = OutputRegion { x: 0, y: 0, width: 1920, height: 1080 };
        assert_eq!(injection_point(5, 7, primary_first), (5, 7));
    }

    #[test]
    fn relative_motion_carries_fractions_and_ignores_garbage() {
        let mut frac = (0.0, 0.0);
        assert_eq!(take_relative(&mut frac, 0.4, -0.4), (0, 0));
        assert_eq!(take_relative(&mut frac, 0.4, -0.4), (0, 0));
        assert_eq!(take_relative(&mut frac, 0.4, -0.4), (1, -1), "the carried 1.2 and -1.2 surface");
        assert!(frac.0.abs() < 0.25 && frac.1.abs() < 0.25);
        assert_eq!(take_relative(&mut frac, f32::NAN, f32::INFINITY), (0, 0));
        assert_eq!(take_relative(&mut (0.0, 0.0), 3.7, 2.2), (3, 2));
    }

    #[test]
    fn pen_lifecycle_hover_touch_lift_leave() {
        let mut p = PenState::default();
        let hover = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_HOVER, 0.0));
        assert_eq!(hover.pointer_flags & (PF_INRANGE | PF_INCONTACT | PF_NEW), PF_INRANGE | PF_NEW);
        let down = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_DOWN, 0.6));
        assert_eq!(down.pointer_flags & PF_NEW, 0, "already in range");
        assert!(down.pointer_flags & PF_DOWN != 0 && down.pointer_flags & PF_INCONTACT != 0);
        assert_eq!(down.pressure, 614);
        let drag = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_MOVE, 0.3));
        assert!(drag.pointer_flags & PF_UPDATE != 0 && drag.pointer_flags & PF_INCONTACT != 0);
        let soft = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_MOVE, 0.0));
        assert_eq!(soft.pointer_flags & PF_INCONTACT, 0, "zero pressure move is a hover");
        assert_eq!(soft.pressure, 0);
        let up = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_UP, 0.0));
        assert!(up.pointer_flags & PF_UP != 0 && up.pointer_flags & PF_INRANGE != 0);
        let leave = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_CANCEL, 0.0));
        assert_eq!(leave.pointer_flags & PF_INRANGE, 0);
        let back = p.frame(&ev(INPUT_TYPE_STYLUS, 0, ACTION_HOVER, 0.0));
        assert!(back.pointer_flags & PF_NEW != 0, "re-entering range is new again");
    }

    #[test]
    fn pen_barrel_and_eraser_flags() {
        let mut p = PenState::default();
        let mut e = ev(INPUT_TYPE_STYLUS, 0, ACTION_DOWN, 0.5);
        e.flags = POINTER_FLAG_BTN_SECONDARY;
        let f = p.frame(&e);
        assert!(f.pen_flags & PEN_BARREL != 0 && f.pointer_flags & PF_SECONDBUTTON != 0);
        e.flags = POINTER_FLAG_ERASER;
        let f = p.frame(&e);
        assert_eq!(f.pen_flags, PEN_INVERTED | PEN_ERASER);
        let mut hover = ev(INPUT_TYPE_STYLUS, 0, ACTION_HOVER, 0.0);
        hover.flags = POINTER_FLAG_ERASER;
        assert_eq!(p.frame(&hover).pen_flags, PEN_INVERTED, "eraser end hovering: inverted, not erasing");
    }

    #[test]
    fn touch_frames_carry_every_live_finger() {
        let mut t = TouchTracker::default();
        let a = t.frame(&ev(INPUT_TYPE_TOUCH, 0, ACTION_DOWN, 0.5), 10, 10).unwrap();
        assert_eq!(a.len(), 1);
        assert!(a[0].flags & PF_DOWN != 0 && a[0].flags & PF_PRIMARY != 0);
        let b = t.frame(&ev(INPUT_TYPE_TOUCH, 1, ACTION_DOWN, 0.5), 50, 60).unwrap();
        assert_eq!(b.len(), 2);
        assert!(b[0].flags & PF_UPDATE != 0 && b[0].flags & PF_DOWN == 0, "finger 0 just updates");
        assert!(b[1].flags & PF_DOWN != 0 && b[1].flags & PF_PRIMARY == 0);
        let m = t.frame(&ev(INPUT_TYPE_TOUCH, 0, ACTION_MOVE, 0.5), 12, 11).unwrap();
        assert_eq!((m[0].x, m[0].y), (12, 11));
        assert_eq!((m[1].x, m[1].y), (50, 60), "the other finger keeps its place");
        let u = t.frame(&ev(INPUT_TYPE_TOUCH, 0, ACTION_UP, 0.0), 12, 11).unwrap();
        assert!(u[0].flags & PF_UP != 0 && u[0].flags & PF_INCONTACT == 0);
        assert_eq!(t.active(), 1);
        let after = t.frame(&ev(INPUT_TYPE_TOUCH, 1, ACTION_MOVE, 0.5), 51, 61).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].flags & PF_PRIMARY != 0, "the remaining finger becomes primary");
        assert!(t.frame(&ev(INPUT_TYPE_TOUCH, 7, ACTION_UP, 0.0), 0, 0).is_none());
        assert!(t.frame(&ev(INPUT_TYPE_TOUCH, 1, ACTION_HOVER, 0.0), 0, 0).is_none());
        let c = t.frame(&ev(INPUT_TYPE_TOUCH, 1, ACTION_CANCEL, 0.0), 51, 61).unwrap();
        assert!(c[0].flags & PF_CANCELED != 0);
        assert_eq!(t.active(), 0);
    }

    #[test]
    fn mouse_buttons_release_before_press() {
        let l = POINTER_FLAG_BTN_PRIMARY;
        let r = POINTER_FLAG_BTN_SECONDARY;
        assert_eq!(mouse_transitions(0, l), vec![(MouseButton::Left, true)]);
        assert_eq!(mouse_transitions(l, l), vec![]);
        assert_eq!(mouse_transitions(l, r), vec![(MouseButton::Left, false), (MouseButton::Right, true)]);
        assert_eq!(mouse_transitions(l | r, 0).len(), 2);
    }

    #[test]
    fn scroll_notches_become_wheel_deltas() {
        assert_eq!(scroll_deltas(POINTER_FLAG_SCROLL_UP), (120, 0));
        assert_eq!(scroll_deltas(POINTER_FLAG_SCROLL_DOWN), (-120, 0));
        assert_eq!(scroll_deltas(POINTER_FLAG_SCROLL_LEFT), (0, -120));
        assert_eq!(scroll_deltas(POINTER_FLAG_SCROLL_RIGHT | POINTER_FLAG_SCROLL_UP), (120, 120));
        assert_eq!(scroll_deltas(0), (0, 0));
    }
}
