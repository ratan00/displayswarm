//! Translates protocol v2 input messages into the injector event model.
//!
//! The platform injectors still consume the per-pointer [`InputEvent`] and
//! [`KeyEvent`] they were written for; this is the one place that knows how a
//! v2 `Touch`, `Pen`, `Mouse`, `Scroll`, `Key` or `Text` message maps onto
//! them. The phone's touchpad mode (relative `Mouse`, phased `Scroll`,
//! `Pinch`) goes through [`InputInjector::inject_relative`] and
//! [`InputInjector::inject_zoom`] instead, since it must not warp the pointer.

use super::{InputInjector, RelativeEvent};
use crate::protocol::wire::{self, Message};
use crate::protocol::{
    InputEvent, KeyEvent, ACTION_CANCEL, ACTION_DOWN, ACTION_HOVER, ACTION_MOVE, ACTION_UP,
    INPUT_TYPE_MOUSE, INPUT_TYPE_STYLUS, INPUT_TYPE_TOUCH, KEY_EVENT_TYPE_KEY,
    POINTER_FLAG_BTN_PRIMARY, POINTER_FLAG_BTN_SECONDARY, POINTER_FLAG_BTN_TERTIARY,
    POINTER_FLAG_ERASER, POINTER_FLAG_SCROLL_DOWN, POINTER_FLAG_SCROLL_LEFT,
    POINTER_FLAG_SCROLL_RIGHT, POINTER_FLAG_SCROLL_UP,
};

/// Per-session input translation state.
#[derive(Default)]
pub struct InputRouter {
    vertical: ScrollAccumulator,
    horizontal: ScrollAccumulator,
    /// Buttons the phone's touchpad holds (`POINTER_FLAG_BTN_*`), carried on
    /// its scroll steps so a scroll never releases a drag.
    relative_buttons: u8,
    /// Pinch scale at which the last zoom notch was emitted.
    pinch_base: f32,
    warned_relative: bool,
}

/// Pinch scale change that makes one zoom notch.
const ZOOM_STEP: f32 = 1.2;

impl InputRouter {
    /// Injects `msg` if it is an input message. Returns how many injector
    /// events succeeded (0 for anything that is not input).
    pub fn route(&mut self, msg: &Message, injector: &mut dyn InputInjector) -> u64 {
        if !matches!(msg, Message::Scroll(_)) {
            // Any other input ends a scroll gesture, so a trackpad's residual
            // cannot leak into the next one.
            self.vertical.reset();
            self.horizontal.reset();
        }
        let mut injected = 0u64;
        let mut pointer = |e: InputEvent| {
            if injector.inject(&e).is_ok() {
                injected += 1;
            }
        };
        match msg {
            Message::Touch(t) => {
                let all = matches!(t.action, wire::ACTION_MOVE | wire::ACTION_CANCEL);
                // A lift whose finger is missing from the list (the sender may
                // list only the fingers still down) must still release it, or
                // the injector's slot stays held.
                let lift = matches!(t.action, wire::ACTION_UP | wire::ACTION_CANCEL);
                if lift && !t.points.iter().any(|p| p.id == t.action_id) {
                    pointer(InputEvent {
                        event_type: INPUT_TYPE_TOUCH,
                        pointer_id: t.action_id,
                        action: t.action,
                        flags: 0,
                        x_norm: 0.0,
                        y_norm: 0.0,
                        pressure: 0.0,
                        tilt_x: 0.0,
                        tilt_y: 0.0,
                    });
                }
                for p in t.points.iter().filter(|p| all || p.id == t.action_id) {
                    pointer(InputEvent {
                        event_type: INPUT_TYPE_TOUCH,
                        pointer_id: p.id,
                        action: t.action.min(ACTION_CANCEL),
                        flags: 0,
                        x_norm: p.x,
                        y_norm: p.y,
                        pressure: p.pressure,
                        tilt_x: 0.0,
                        tilt_y: 0.0,
                    });
                }
            }
            Message::Pen(p) => {
                let mut flags = 0;
                if p.tool == wire::PEN_TOOL_ERASER {
                    flags |= POINTER_FLAG_ERASER;
                }
                if p.buttons & wire::PEN_BUTTON_PRIMARY != 0 {
                    flags |= POINTER_FLAG_BTN_PRIMARY;
                }
                if p.buttons & wire::PEN_BUTTON_SECONDARY != 0 {
                    flags |= POINTER_FLAG_BTN_SECONDARY;
                }
                let hovering = matches!(p.action, wire::ACTION_HOVER_MOVE | wire::ACTION_HOVER_EXIT);
                let last = p.samples.len().saturating_sub(1);
                for (i, s) in p.samples.iter().enumerate() {
                    let action = if i < last {
                        if hovering { ACTION_HOVER } else { ACTION_MOVE }
                    } else {
                        match p.action {
                            wire::ACTION_HOVER_MOVE => ACTION_HOVER,
                            // Leaving proximity is the injector's "up".
                            wire::ACTION_HOVER_EXIT => ACTION_UP,
                            a => a.min(ACTION_CANCEL),
                        }
                    };
                    let e = InputEvent {
                        event_type: INPUT_TYPE_STYLUS,
                        pointer_id: 0,
                        action,
                        flags,
                        x_norm: s.x,
                        y_norm: s.y,
                        pressure: if action == ACTION_UP { 0.0 } else { s.pressure },
                        tilt_x: s.tilt_x,
                        tilt_y: s.tilt_y,
                    };
                    if injector.inject_pen_sample(&e, s.age_us).is_ok() {
                        injected += 1;
                    }
                }
            }
            Message::Mouse(m) if m.relative => {
                if !(m.x.is_finite() && m.y.is_finite()) {
                    return 0;
                }
                self.relative_buttons = mouse_button_flags(m.buttons);
                let e = RelativeEvent {
                    dx: m.x.clamp(-10_000.0, 10_000.0),
                    dy: m.y.clamp(-10_000.0, 10_000.0),
                    buttons: self.relative_buttons,
                    ..Default::default()
                };
                return self.relative(injector, &e);
            }
            Message::Mouse(m) => {
                let action = match m.action {
                    wire::ACTION_HOVER_MOVE => ACTION_HOVER,
                    wire::ACTION_HOVER_EXIT => return 0,
                    a => a.min(ACTION_CANCEL),
                };
                pointer(InputEvent {
                    event_type: INPUT_TYPE_MOUSE,
                    pointer_id: 0,
                    action,
                    flags: mouse_button_flags(m.buttons),
                    x_norm: m.x,
                    y_norm: m.y,
                    pressure: 0.0,
                    tilt_x: 0.0,
                    tilt_y: 0.0,
                });
            }
            // A phased scroll is the touchpad's two-finger scroll: its position
            // is only the phone's guess of the cursor, so it must not move it.
            Message::Scroll(s) if s.phase != wire::PHASE_NONE => {
                let v = self.vertical.consume(s.dy);
                let h = self.horizontal.consume(s.dx);
                if s.phase == wire::PHASE_END {
                    self.vertical.reset();
                    self.horizontal.reset();
                }
                if v == 0 && h == 0 {
                    return 0;
                }
                // Positive dx scrolls left (see below), so it is a negative HWHEEL.
                let e = RelativeEvent { buttons: self.relative_buttons, wheel: v, hwheel: -h, ..Default::default() };
                return self.relative(injector, &e);
            }
            Message::Scroll(s) => {
                let v = self.vertical.consume(s.dy);
                let h = self.horizontal.consume(s.dx);
                if s.phase == wire::PHASE_END {
                    self.vertical.reset();
                    self.horizontal.reset();
                }
                // One event per notch; a diagonal step carries both flags.
                // Positive AXIS_HSCROLL has always been sent as SCROLL_LEFT.
                for i in 0..v.unsigned_abs().max(h.unsigned_abs()) {
                    let mut flags = 0;
                    if i < v.unsigned_abs() {
                        flags |= if v > 0 { POINTER_FLAG_SCROLL_UP } else { POINTER_FLAG_SCROLL_DOWN };
                    }
                    if i < h.unsigned_abs() {
                        flags |= if h > 0 { POINTER_FLAG_SCROLL_LEFT } else { POINTER_FLAG_SCROLL_RIGHT };
                    }
                    pointer(InputEvent {
                        event_type: INPUT_TYPE_MOUSE,
                        pointer_id: 0,
                        action: ACTION_HOVER,
                        flags,
                        x_norm: s.x,
                        y_norm: s.y,
                        pressure: 0.0,
                        tilt_x: 0.0,
                        tilt_y: 0.0,
                    });
                }
            }
            Message::Key(k) => {
                let e = KeyEvent {
                    event_type: KEY_EVENT_TYPE_KEY,
                    action: k.action.min(ACTION_CANCEL),
                    key_code: k.key_code,
                    scan_code: k.scan_code,
                    flags: k.meta,
                    text: k.text.clone(),
                };
                if injector.inject_key(&e).is_ok() {
                    injected += 1;
                }
            }
            Message::Text(text) => {
                // No key code: the injector types each character through its
                // text fallback. Both halves carry the character so the release
                // resolves to the same key as the press.
                for c in text.chars() {
                    let s = c.to_string();
                    for action in [ACTION_DOWN, ACTION_UP] {
                        if injector.inject_key(&KeyEvent::new(action, 0, &s)).is_ok() {
                            injected += 1;
                        }
                    }
                }
            }
            Message::Pinch(p) => {
                if p.phase == wire::PHASE_BEGIN {
                    self.pinch_base = 1.0;
                }
                if !(p.scale.is_finite() && p.scale > 0.0) {
                    return 0;
                }
                if self.pinch_base <= 0.0 {
                    self.pinch_base = 1.0;
                }
                let steps = ((p.scale / self.pinch_base).ln() / ZOOM_STEP.ln()).trunc().clamp(-20.0, 20.0) as i32;
                if steps != 0 {
                    self.pinch_base *= ZOOM_STEP.powi(steps);
                    if injector.inject_zoom(steps).is_ok() {
                        injected += 1;
                    }
                }
            }
            _ => {}
        }
        injected
    }

    fn relative(&mut self, injector: &mut dyn InputInjector, e: &RelativeEvent) -> u64 {
        match injector.inject_relative(e) {
            Ok(()) => 1,
            Err(err) => {
                if !self.warned_relative {
                    self.warned_relative = true;
                    log::info!("Touchpad input is not supported by this host: {err}");
                }
                0
            }
        }
    }
}

fn mouse_button_flags(buttons: u8) -> u8 {
    let mut flags = 0;
    if buttons & wire::MOUSE_BUTTON_PRIMARY != 0 {
        flags |= POINTER_FLAG_BTN_PRIMARY;
    }
    if buttons & wire::MOUSE_BUTTON_SECONDARY != 0 {
        flags |= POINTER_FLAG_BTN_SECONDARY;
    }
    if buttons & wire::MOUSE_BUTTON_TERTIARY != 0 {
        flags |= POINTER_FLAG_BTN_TERTIARY;
    }
    flags
}

/// Residual (sub-notch) scroll motion.
///
/// A mouse wheel reports whole detents; a trackpad reports a stream of
/// sub-notch deltas, and a high-resolution wheel fractions of a detent. The
/// injector only knows whole notches, so fractions are carried until they add
/// up. A delta of a whole notch or more replaces the residual instead of adding
/// to it: some devices report a running total, and adding the earlier
/// fractions on top of it would scroll too far.
#[derive(Default)]
struct ScrollAccumulator {
    residual: f32,
}

impl ScrollAccumulator {
    /// Signed whole notches to emit for `delta`.
    fn consume(&mut self, delta: f32) -> i32 {
        if !delta.is_finite() {
            return 0;
        }
        let total = if delta.abs() >= 1.0 { delta } else { self.residual + delta };
        let notches = total.abs().floor().min(100.0);
        // Only the sub-notch part is carried: a huge delta (capped above) must
        // not leave a backlog that keeps scrolling on later small deltas.
        self.residual = total.fract();
        (notches as i32) * if total > 0.0 { 1 } else { -1 }
    }

    fn reset(&mut self) {
        self.residual = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{Key, Mouse, Pen, PenSample, Scroll, Touch, TouchPoint};

    #[derive(Default)]
    struct Rec {
        pointers: Vec<InputEvent>,
        keys: Vec<KeyEvent>,
        ages: Vec<u32>,
        relative: Vec<RelativeEvent>,
        zoom: Vec<i32>,
    }
    impl InputInjector for Rec {
        fn inject_relative(&mut self, e: &RelativeEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.relative.push(*e);
            Ok(())
        }
        fn inject_zoom(&mut self, steps: i32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.zoom.push(steps);
            Ok(())
        }
        fn inject_pen_sample(&mut self, e: &InputEvent, age_us: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.ages.push(age_us);
            self.inject(e)
        }
        fn inject(&mut self, e: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.pointers.push(e.clone());
            Ok(())
        }
        fn inject_key(&mut self, e: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.keys.push(e.clone());
            Ok(())
        }
    }

    fn pt(id: u8, x: f32) -> TouchPoint {
        TouchPoint { id, x, y: 0.5, pressure: 1.0 }
    }

    #[test]
    fn touch_down_up_address_one_finger_and_move_all() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let down = Message::Touch(Touch { action: wire::ACTION_DOWN, action_id: 1, points: vec![pt(0, 0.1), pt(1, 0.9)] });
        let mv = Message::Touch(Touch { action: wire::ACTION_MOVE, action_id: 0, points: vec![pt(0, 0.2), pt(1, 0.8)] });
        assert_eq!(r.route(&down, &mut inj), 1);
        assert_eq!(r.route(&mv, &mut inj), 2);
        assert_eq!(inj.pointers[0].pointer_id, 1);
        assert_eq!(inj.pointers[0].action, ACTION_DOWN);
        assert!(inj.pointers[1..].iter().all(|e| e.action == ACTION_MOVE));
    }

    #[test]
    fn pen_history_becomes_moves_then_the_final_action() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let s = |x| PenSample { age_us: 0, x, y: 0.5, pressure: 0.5, tilt_x: 0.1, tilt_y: 0.0 };
        let msg = Message::Pen(Pen {
            action: wire::ACTION_UP,
            tool: wire::PEN_TOOL_ERASER,
            buttons: wire::PEN_BUTTON_SECONDARY,
            samples: vec![s(0.1), s(0.2), s(0.3)],
        });
        assert_eq!(r.route(&msg, &mut inj), 3);
        let actions: Vec<u8> = inj.pointers.iter().map(|e| e.action).collect();
        assert_eq!(actions, [ACTION_MOVE, ACTION_MOVE, ACTION_UP]);
        assert!(inj.pointers.iter().all(|e| e.event_type == INPUT_TYPE_STYLUS
            && e.flags == POINTER_FLAG_ERASER | POINTER_FLAG_BTN_SECONDARY));
        assert_eq!(inj.pointers[2].pressure, 0.0);
    }

    #[test]
    fn pen_history_samples_carry_their_age() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let s = |age_us| PenSample { age_us, x: 0.5, y: 0.5, pressure: 0.5, tilt_x: 0.0, tilt_y: 0.0 };
        let msg = Message::Pen(Pen { action: wire::ACTION_MOVE, tool: 0, buttons: 0, samples: vec![s(8000), s(4000), s(0)] });
        assert_eq!(r.route(&msg, &mut inj), 3);
        assert_eq!(inj.ages, [8000, 4000, 0]);
    }

    #[test]
    fn pen_hover_exit_leaves_proximity() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let s = PenSample { age_us: 0, x: 0.5, y: 0.5, pressure: 0.0, tilt_x: 0.0, tilt_y: 0.0 };
        for action in [wire::ACTION_HOVER_MOVE, wire::ACTION_HOVER_EXIT] {
            r.route(&Message::Pen(Pen { action, tool: 0, buttons: 0, samples: vec![s] }), &mut inj);
        }
        assert_eq!(inj.pointers[0].action, ACTION_HOVER);
        assert_eq!(inj.pointers[1].action, ACTION_UP);
    }

    #[test]
    fn mouse_buttons_map_to_pointer_flags() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let m = Mouse { action: wire::ACTION_DOWN, buttons: wire::MOUSE_BUTTON_SECONDARY, relative: false, x: 0.5, y: 0.5 };
        r.route(&Message::Mouse(m), &mut inj);
        assert_eq!(inj.pointers[0].flags, POINTER_FLAG_BTN_SECONDARY);
        assert_eq!(inj.pointers[0].event_type, INPUT_TYPE_MOUSE);
    }

    #[test]
    fn relative_mouse_moves_without_warping_and_keeps_buttons() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let m = |action, buttons, x, y| Message::Mouse(Mouse { action, buttons, relative: true, x, y });
        assert_eq!(r.route(&m(wire::ACTION_HOVER_MOVE, 0, 3.5, -2.0), &mut inj), 1);
        r.route(&m(wire::ACTION_DOWN, wire::MOUSE_BUTTON_PRIMARY, 0.0, 0.0), &mut inj);
        r.route(&m(wire::ACTION_MOVE, wire::MOUSE_BUTTON_PRIMARY, 1.0, 0.0), &mut inj);
        r.route(&m(wire::ACTION_UP, 0, 0.0, 0.0), &mut inj);
        assert!(inj.pointers.is_empty(), "no absolute events");
        assert_eq!(inj.relative[0], RelativeEvent { dx: 3.5, dy: -2.0, ..Default::default() });
        assert_eq!(inj.relative[1].buttons, POINTER_FLAG_BTN_PRIMARY);
        assert_eq!(inj.relative[2].buttons, POINTER_FLAG_BTN_PRIMARY);
        assert_eq!(inj.relative[3].buttons, 0);
        assert_eq!(r.route(&m(wire::ACTION_MOVE, 0, f32::NAN, 0.0), &mut inj), 0);
    }

    #[test]
    fn touchpad_scroll_is_wheel_only() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let step = |phase, dx, dy| Message::Scroll(Scroll { phase, x: 0.9, y: 0.9, dx, dy });
        assert_eq!(r.route(&step(wire::PHASE_BEGIN, 0.0, 0.6), &mut inj), 0);
        assert_eq!(r.route(&step(wire::PHASE_UPDATE, 1.0, 0.6), &mut inj), 1);
        assert!(inj.pointers.is_empty());
        assert_eq!(inj.relative[0], RelativeEvent { wheel: 1, hwheel: -1, ..Default::default() });
        // A discrete wheel step stays positional.
        r.route(&step(wire::PHASE_NONE, 0.0, -1.0), &mut inj);
        assert_eq!(inj.pointers.len(), 1);
    }

    #[test]
    fn pinch_becomes_zoom_notches() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let p = |phase, scale| Message::Pinch(wire::Pinch { phase, cx: 0.5, cy: 0.5, scale, rotation: 0.0 });
        r.route(&p(wire::PHASE_BEGIN, 1.0), &mut inj);
        r.route(&p(wire::PHASE_UPDATE, 1.1), &mut inj);
        r.route(&p(wire::PHASE_UPDATE, 1.5), &mut inj); // 1.5 / 1.2^2 = 1.04
        r.route(&p(wire::PHASE_UPDATE, 0.9), &mut inj); // 0.9 / 1.44 = 1.2^-2.6
        r.route(&p(wire::PHASE_UPDATE, f32::INFINITY), &mut inj);
        r.route(&p(wire::PHASE_END, 1.0), &mut inj);
        assert_eq!(inj.zoom, [2, -2]);
        r.route(&p(wire::PHASE_BEGIN, 1.0), &mut inj);
        r.route(&p(wire::PHASE_UPDATE, 0.5), &mut inj);
        assert_eq!(inj.zoom, [2, -2, -3]);
    }

    #[test]
    fn scroll_accumulates_fractions_into_notches() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let step = |dy| Message::Scroll(Scroll { phase: wire::PHASE_NONE, x: 0.5, y: 0.5, dx: 0.0, dy });
        assert_eq!(r.route(&step(0.4), &mut inj), 0);
        assert_eq!(r.route(&step(0.4), &mut inj), 0);
        assert_eq!(r.route(&step(0.4), &mut inj), 1, "1.2 accumulated is one notch");
        assert_eq!(inj.pointers[0].flags, POINTER_FLAG_SCROLL_UP);
        assert_eq!(r.route(&step(-3.0), &mut inj), 3);
        assert!(inj.pointers[1..].iter().all(|e| e.flags == POINTER_FLAG_SCROLL_DOWN));
    }

    #[test]
    fn huge_scroll_delta_leaves_no_backlog() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let step = |dy| Message::Scroll(Scroll { phase: wire::PHASE_NONE, x: 0.0, y: 0.0, dx: 0.0, dy });
        assert_eq!(r.route(&step(1000.0), &mut inj), 100, "capped");
        assert_eq!(r.route(&step(0.1), &mut inj), 0, "no leftover from the capped delta");
    }

    #[test]
    fn touch_lift_of_a_finger_missing_from_the_list_still_lifts() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let up = Message::Touch(Touch { action: wire::ACTION_UP, action_id: 1, points: vec![pt(0, 0.1)] });
        assert_eq!(r.route(&up, &mut inj), 1);
        assert_eq!((inj.pointers[0].pointer_id, inj.pointers[0].action), (1, ACTION_UP));
        let cancel = Message::Touch(Touch { action: wire::ACTION_CANCEL, action_id: 0, points: vec![] });
        assert_eq!(r.route(&cancel, &mut inj), 1);
    }

    #[test]
    fn diagonal_scroll_shares_events() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let msg = Message::Scroll(Scroll { phase: wire::PHASE_NONE, x: 0.0, y: 0.0, dx: -1.0, dy: 2.0 });
        assert_eq!(r.route(&msg, &mut inj), 2);
        assert_eq!(inj.pointers[0].flags, POINTER_FLAG_SCROLL_UP | POINTER_FLAG_SCROLL_RIGHT);
        assert_eq!(inj.pointers[1].flags, POINTER_FLAG_SCROLL_UP);
    }

    #[test]
    fn other_input_resets_the_scroll_residual() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let step = |dy| Message::Scroll(Scroll { phase: wire::PHASE_UPDATE, x: 0.0, y: 0.0, dx: 0.0, dy });
        r.route(&step(0.9), &mut inj);
        r.route(&Message::Touch(Touch { action: wire::ACTION_DOWN, action_id: 0, points: vec![pt(0, 0.5)] }), &mut inj);
        assert_eq!(r.route(&step(0.2), &mut inj), 0);
    }

    #[test]
    fn key_and_text_reach_inject_key() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        let k = Key { action: wire::ACTION_DOWN, key_code: 29, scan_code: 30, meta: 1, text: "A".into() };
        assert_eq!(r.route(&Message::Key(k), &mut inj), 1);
        assert_eq!(inj.keys[0].flags, 1);
        assert_eq!(inj.keys[0].text, "A");
        assert_eq!(r.route(&Message::Text("hé".into()), &mut inj), 4);
        assert_eq!(inj.keys[1..].iter().map(|k| (k.action, k.text.as_str())).collect::<Vec<_>>(),
            [(ACTION_DOWN, "h"), (ACTION_UP, "h"), (ACTION_DOWN, "é"), (ACTION_UP, "é")]);
    }

    #[test]
    fn non_input_messages_inject_nothing() {
        let (mut r, mut inj) = (InputRouter::default(), Rec::default());
        assert_eq!(r.route(&Message::Heartbeat, &mut inj), 0);
        assert!(inj.pointers.is_empty() && inj.keys.is_empty());
    }
}
