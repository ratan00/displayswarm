//! Phone <-> host protocol.
//!
//! * [`wire`] is the on-the-wire format (protocol v2): framing, channels and
//!   every message type. It is the only part the phone sees.
//! * This module keeps the host-internal event model that the input injectors
//!   consume ([`InputEvent`], [`KeyEvent`] and their constants). It predates v2,
//!   when those structs *were* the wire format; now `input::router` translates
//!   v2 input messages into them, so the platform injectors did not have to
//!   change with the protocol.
//! * [`host_monotonic_us`] is the clock every timestamp on the wire uses.

pub mod wire;

// Video frame types, shared by the encoders and `wire::VideoFrame`.
pub const FRAME_TYPE_CONFIG: u8 = 1; // SPS / PPS
pub const FRAME_TYPE_KEY: u8 = 2; // IDR
pub const FRAME_TYPE_DELTA: u8 = 3; // P-slice

// ---------------------------------------------------------------------------
// Injector event model
// ---------------------------------------------------------------------------

pub const INPUT_TYPE_TOUCH: u8 = 1;
pub const INPUT_TYPE_STYLUS: u8 = 2;
pub const INPUT_TYPE_MOUSE: u8 = 3;

// Actions. The v2 wire uses the same numbering (see `wire::ACTION_*`).
pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;
pub const ACTION_CANCEL: u8 = 3;
pub const ACTION_HOVER: u8 = 4;

/// `flags` bit 0x01: the pointer is an eraser.
pub const POINTER_FLAG_ERASER: u8 = 0x01;
/// `flags` bit 0x02: primary button held (pen: first barrel button).
pub const POINTER_FLAG_BTN_PRIMARY: u8 = 0x02;
/// `flags` bit 0x04: secondary button held (pen: second barrel button).
pub const POINTER_FLAG_BTN_SECONDARY: u8 = 0x04;
/// `flags` bit 0x08: tertiary (middle) button held.
pub const POINTER_FLAG_BTN_TERTIARY: u8 = 0x08;
/// `flags` bit 0x10: one wheel notch, content moving towards the top of the
/// document (Linux `REL_WHEEL > 0`).
pub const POINTER_FLAG_SCROLL_UP: u8 = 0x10;
/// `flags` bit 0x20: one wheel notch towards the bottom (`REL_WHEEL < 0`).
pub const POINTER_FLAG_SCROLL_DOWN: u8 = 0x20;
/// `flags` bit 0x40: one wheel notch towards the left (`REL_HWHEEL < 0`).
pub const POINTER_FLAG_SCROLL_LEFT: u8 = 0x40;
/// `flags` bit 0x80: one wheel notch towards the right (`REL_HWHEEL > 0`).
pub const POINTER_FLAG_SCROLL_RIGHT: u8 = 0x80;

/// One pointer sample for an injector: pen, finger or mouse.
#[derive(Debug, Clone, PartialEq)]
pub struct InputEvent {
    /// `INPUT_TYPE_*`.
    pub event_type: u8,
    pub pointer_id: u8,
    /// `ACTION_*`.
    pub action: u8,
    /// `POINTER_FLAG_*` bits.
    pub flags: u8,
    /// Position, normalised to 0..1 across the streamed picture.
    pub x_norm: f32,
    pub y_norm: f32,
    /// 0..1.
    pub pressure: f32,
    /// Per-axis pen lean, radians in (-PI/2, PI/2).
    pub tilt_x: f32,
    pub tilt_y: f32,
}

pub const KEY_EVENT_TYPE_KEY: u8 = 1;

/// `KeyEvent::flags` bit 0x01: Shift was held.
pub const KEY_FLAG_SHIFT: u8 = 0x01;
/// `flags` bit 0x02: Ctrl was held.
pub const KEY_FLAG_CTRL: u8 = 0x02;
/// `flags` bit 0x04: Alt was held.
pub const KEY_FLAG_ALT: u8 = 0x04;
/// `flags` bit 0x08: Meta was held.
pub const KEY_FLAG_META: u8 = 0x08;
/// `flags` bit 0x10: Caps Lock was on.
pub const KEY_FLAG_CAPS_LOCK: u8 = 0x10;
/// `flags` bit 0x20: Num Lock was on.
pub const KEY_FLAG_NUM_LOCK: u8 = 0x20;
/// `flags` bit 0x40: an auto-repeat, not the initial press.
pub const KEY_FLAG_REPEAT: u8 = 0x40;

/// One key press, release or cancel for an injector.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEvent {
    /// Always [`KEY_EVENT_TYPE_KEY`].
    pub event_type: u8,
    /// `ACTION_DOWN` / `ACTION_UP` / `ACTION_CANCEL` (treated as up).
    pub action: u8,
    /// `android.view.KeyEvent.KEYCODE_*`; each injector maps it to its platform.
    pub key_code: u16,
    /// `KeyEvent.getScanCode()` when known, else 0. A hint only.
    pub scan_code: u16,
    /// `KEY_FLAG_*` bits.
    pub flags: u8,
    /// Characters the key produced; empty for non-text keys and releases.
    pub text: String,
}

impl KeyEvent {
    /// A key event with no scan code and no modifiers.
    pub fn new(action: u8, key_code: u16, text: &str) -> Self {
        Self {
            event_type: KEY_EVENT_TYPE_KEY,
            action,
            key_code,
            scan_code: 0,
            flags: 0,
            text: text.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Microseconds on the host's monotonic clock (CLOCK_MONOTONIC on Linux).
///
/// Used for every timestamp that crosses the wire, so the phone can relate
/// them to its own clock through the ping/pong offset. Unlike `SystemTime` it
/// never jumps when NTP adjusts the wall clock.
pub fn host_monotonic_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_clock_advances() {
        let a = host_monotonic_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(host_monotonic_us() > a);
    }

    /// The pen branch of the Linux injector masks 0x01/0x02/0x04 individually,
    /// and the scroll bits must not overlap them.
    #[test]
    fn pointer_flag_bits_are_distinct() {
        let flags = [
            POINTER_FLAG_ERASER,
            POINTER_FLAG_BTN_PRIMARY,
            POINTER_FLAG_BTN_SECONDARY,
            POINTER_FLAG_BTN_TERTIARY,
            POINTER_FLAG_SCROLL_UP,
            POINTER_FLAG_SCROLL_DOWN,
            POINTER_FLAG_SCROLL_LEFT,
            POINTER_FLAG_SCROLL_RIGHT,
        ];
        assert_eq!(flags.iter().fold(0u8, |acc, f| {
            assert_eq!(acc & f, 0);
            acc | f
        }), 0xFF);
    }
}
