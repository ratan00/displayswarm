use super::InputInjector;
use crate::protocol::{
    InputEvent, KeyEvent, ACTION_CANCEL, ACTION_DOWN, ACTION_HOVER, ACTION_MOVE, ACTION_UP,
    INPUT_TYPE_MOUSE, INPUT_TYPE_STYLUS, KEY_FLAG_ALT, KEY_FLAG_CTRL, KEY_FLAG_META,
    KEY_FLAG_SHIFT, POINTER_FLAG_BTN_PRIMARY, POINTER_FLAG_ERASER, POINTER_FLAG_BTN_SECONDARY,
    POINTER_FLAG_BTN_TERTIARY, POINTER_FLAG_SCROLL_DOWN, POINTER_FLAG_SCROLL_LEFT,
    POINTER_FLAG_SCROLL_RIGHT, POINTER_FLAG_SCROLL_UP,
};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;

// Linux input event codes
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0x00;

// Relative axes used by the virtual mouse
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;

// Mouse buttons
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

// Tablet & Stylus keys
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;
const BTN_STYLUS2: u16 = 0x14c;

// Absolute axes
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;

// Multi-touch axes
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

// uinput ioctl codes
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c5504;

const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_SET_RELBIT: libc::c_ulong = 0x40045566;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004556e;

const INPUT_PROP_DIRECT: u16 = 0x01;

/// `INPUT_PROP_POINTING_STICK` - the device has no acceleration and does not
/// scroll naturally.
///
/// The virtual mouse reports absolute positions translated into per-event
/// deltas, which only lands the pointer exactly where the client asked if the
/// input stack does *not* re-scale them through a pointer-acceleration profile.
/// Declaring this property is how a uinput device says so, and it is what
/// `xdotool type`-style virtual devices do. Without it, scroll deltas also get
/// multiplied by the scroll-acceleration curve, so fast scrolls overshoot.
const INPUT_PROP_POINTING_STICK: u16 = 0x02;

/// Tilt range declared on `ABS_TILT_X` / `ABS_TILT_Y` in `setup_pen_device`,
/// and the range the Linux input core documents for pen tilt.
const TILT_MIN_DEG: i32 = -90;
const TILT_MAX_DEG: i32 = 90;

/// Pen axis units per streamed pixel. The phone's digitizer reports
/// sub-pixel positions; whole pixels would put steps into slow strokes.
const PEN_SUBPIXEL: i32 = 8;
/// About 300 dpi at [`PEN_SUBPIXEL`] units per pixel.
const PEN_UNITS_PER_MM: i32 = PEN_SUBPIXEL * 12;
/// Wacom's USB vendor id, kept on the pen and touchscreen on purpose (see
/// `setup_pen_device`).
pub const WACOM_VENDOR_ID: u16 = 0x056a;
/// A Wacom product id no libwacom entry or libinput quirk matches (see
/// `setup_pen_device`).
pub const PEN_PRODUCT_ID: u16 = 0xf0f0;
pub const TOUCH_PRODUCT_ID: u16 = 0x0301;
/// Declared maximum of `ABS_PRESSURE` and `ABS_MT_PRESSURE`; every pressure
/// written is scaled to and clamped into it.
const PRESSURE_MAX: i32 = 4096;

/// Pen contact starts above this pressure and ends below the lower one. One
/// threshold makes a light stroke, whose pressure hovers around it, flip
/// `BTN_TOUCH` on and off (a dotted line in the app).
const PRESSURE_TOUCH_ON: f32 = 0.015;
const PRESSURE_TOUCH_OFF: f32 = 0.006;

/// `pressure` (0..1, anything the wire delivered) as an axis value.
fn pressure_axis(pressure: f32) -> i32 {
    if pressure.is_nan() {
        return 0;
    }
    (pressure.clamp(0.0, 1.0) * PRESSURE_MAX as f32).round() as i32
}

/// Largest pen axis value for a picture `pixels` wide (or high).
fn pen_axis_max(pixels: u32) -> i32 {
    (pixels.max(1) as i32).saturating_mul(PEN_SUBPIXEL) - 1
}

/// Pen axis value for a normalised coordinate.
fn pen_axis_value(norm: f32, pixels: u32) -> i32 {
    let max = pen_axis_max(pixels);
    ((norm.clamp(0.0, 1.0) as f64) * max as f64).round() as i32
}

/// Converts a tilt angle from radians (what the Android client sends, see
/// `InputManager.kt`) into the whole degrees `ABS_TILT_X` / `ABS_TILT_Y` are
/// declared in.
///
/// The scale factor is 180/PI. The clamp is not cosmetic: a single-axis tilt
/// angle is mathematically confined to (-PI/2, PI/2) *if* it is derived from
/// the pen's polar report (altitude/azimuth), which the client now does, but
/// `InputEvent::decode` will happily hand us whatever f32 a peer on the wire
/// sent. uinput clamps out-of-range values to the axis min/max, but Windows'
/// synthetic pointer path rejects the whole injection when tilt falls outside
/// -90..90, so validating here keeps one bad packet from silently dropping
/// every subsequent pen event.
///
/// NaN needs an explicit early return: `f32::clamp` propagates NaN and only the
/// saturating `as i32` cast would then quietly turn it into 0. Infinities are
/// left to `clamp`, which maps them onto the axis limits - an infinite tilt is a
/// client bug, but "as tilted as the axis allows" is a far better answer than
/// claiming the pen is perfectly upright.
fn tilt_radians_to_degrees(radians: f32) -> i32 {
    const RAD_TO_DEG: f32 = 57.2958; // 180 / PI
    if radians.is_nan() {
        return 0;
    }
    (radians * RAD_TO_DEG).clamp(TILT_MIN_DEG as f32, TILT_MAX_DEG as f32) as i32
}

// ===========================================================================
// KEYBOARD: Android KEYCODE_* -> Linux KEY_*
// ===========================================================================
//
// THE ONE PLACE A SILENT MISTAKE COSTS THE USER EVERY CHARACTER
//
// The wire format is Android's keycode numbering (`KeyEvent.KEYCODE_*`), because
// the packet is produced by an Android client. The kernel speaks its own
// completely different numbering (`linux/input-event-codes.h`, `KEY_*`). There is
// no relationship between the two, and - this is the part that makes it
// dangerous - they are *not* even a constant offset:
//
//     Android KEYCODE_A  = 29    Linux KEY_A  = 30   -> +1
//     Android KEYCODE_B  = 30    Linux KEY_B  = 48   -> +18
//     Android KEYCODE_Q  = 45    Linux KEY_Q  = 16   -> -29
//     Android KEYCODE_Z  = 54    Linux KEY_Z  = 44   -> -10
//     Android KEYCODE_0  =  7    Linux KEY_0  = 11   -> +4
//     Android KEYCODE_9  = 16    Linux KEY_9  = 10   -> -6
//     Android KEYCODE_SPACE = 62 Linux KEY_SPACE = 57
//
// Android's alphanumeric range is dense and alphabetical (0-9 then A-Z); the
// kernel's is a QWERTY *scan order*, so `KEY_Q`..`KEY_P` (16-25) sits below
// `KEY_A`..`KEY_L` (30-38) which sits below `KEY_Z`..`KEY_M` (44-50). An offset
// therefore produces a plausible-looking stream of *wrong letters* with no error
// anywhere, which is exactly the failure mode that survives code review and
// reaches the user as "the keyboard types gibberish".
//
// So this is an explicit `match`, not arithmetic. A `match` also makes an
// unmapped keycode a compile-time-visible `None` rather than a silent `+1`,
// and the compiler checks for unreachable arms and duplicate values.
//
// There is a second trap here, and it is why the KEY_FLAG_* modifier bits are
// handled by `modifier_key_for_flag` rather than being ignored: Android's
// `getUnicodeChar()` bakes the *layout* into the `text` field, so the client
// sends key_code 29 with text "a" for an unshifted press and the SAME key_code
// 29 with text "A" for a shifted one. The keycode alone therefore cannot say
// which character the user meant - the modifier flags can, and the host has to
// press the real `KEY_LEFTSHIFT` to make the kernel's layout produce it. That
// is also why `text` is used only for the non-ASCII fallback and never as the
// primary signal: the host's own layout is the one the user is looking at.

/// Android `KeyEvent.KEYCODE_*` -> Linux `KEY_*`.
///
/// Returns `None` for a keycode the host deliberately does not support. Note
/// that a *missing* keycode is not necessarily a missing character: the
/// characters Android sends as a shifted variant of a supported key (a
/// non-US layout's accented letters, a Cyrillic or Greek layout) share the
/// base key's keycode, and are produced by the host's own layout instead. See
/// `text_requires_fallback`.
fn android_keycode_to_linux_keycode(key_code: u16) -> Option<u16> {
    Some(match key_code {
        // --- Letters. Android A..Z is 29..54 dense; the kernel's is QWERTY
        // scan order. Every value below is spelled out, not derived.
        29 => 30,  // KEYCODE_A            -> KEY_A
        30 => 48,  // KEYCODE_B            -> KEY_B
        31 => 46,  // KEYCODE_C            -> KEY_C
        32 => 32,  // KEYCODE_D            -> KEY_D
        33 => 18,  // KEYCODE_E            -> KEY_E
        34 => 33,  // KEYCODE_F            -> KEY_F
        35 => 34,  // KEYCODE_G            -> KEY_G
        36 => 35,  // KEYCODE_H            -> KEY_H
        37 => 23,  // KEYCODE_I            -> KEY_I
        38 => 36,  // KEYCODE_J            -> KEY_J
        39 => 37,  // KEYCODE_K            -> KEY_K
        40 => 38,  // KEYCODE_L            -> KEY_L
        41 => 50,  // KEYCODE_M            -> KEY_M
        42 => 49,  // KEYCODE_N            -> KEY_N
        43 => 24,  // KEYCODE_O            -> KEY_O
        44 => 25,  // KEYCODE_P            -> KEY_P
        45 => 16,  // KEYCODE_Q            -> KEY_Q
        46 => 19,  // KEYCODE_R            -> KEY_R
        47 => 31,  // KEYCODE_S            -> KEY_S
        48 => 20,  // KEYCODE_T            -> KEY_T
        49 => 22,  // KEYCODE_U            -> KEY_U
        50 => 47,  // KEYCODE_V            -> KEY_V
        51 => 17,  // KEYCODE_W            -> KEY_W
        52 => 45,  // KEYCODE_X            -> KEY_X
        53 => 21,  // KEYCODE_Y            -> KEY_Y
        54 => 44,  // KEYCODE_Z            -> KEY_Z

        // --- Digits. Android 0..9 is 7..16 with 0 FIRST; the kernel numbers
        // KEY_1..KEY_9 (2..10) then KEY_0 (11), because evdev key codes are
        // scancodes and the digit row starts above the number row's "1".
        7 => 11,   // KEYCODE_0            -> KEY_0
        8 => 2,    // KEYCODE_1            -> KEY_1
        9 => 3,    // KEYCODE_2            -> KEY_2
        10 => 4,   // KEYCODE_3            -> KEY_3
        11 => 5,   // KEYCODE_4            -> KEY_4
        12 => 6,   // KEYCODE_5            -> KEY_5
        13 => 7,   // KEYCODE_6            -> KEY_6
        14 => 8,   // KEYCODE_7            -> KEY_7
        15 => 9,   // KEYCODE_8            -> KEY_8
        16 => 10,  // KEYCODE_9            -> KEY_9

        // --- Editing / navigation
        61 => 15,  // KEYCODE_TAB          -> KEY_TAB
        66 => 28,  // KEYCODE_ENTER        -> KEY_ENTER
        67 => 14,  // KEYCODE_DEL          -> KEY_BACKSPACE (Android "DEL" IS backspace)
        112 => 111, // KEYCODE_FORWARD_DEL  -> KEY_DELETE (the real forward delete)
        62 => 57,  // KEYCODE_SPACE        -> KEY_SPACE
        111 => 1,  // KEYCODE_ESCAPE       -> KEY_ESC
        19 => 103, // KEYCODE_DPAD_UP      -> KEY_UP
        20 => 108, // KEYCODE_DPAD_DOWN    -> KEY_DOWN
        21 => 105, // KEYCODE_DPAD_LEFT    -> KEY_LEFT
        22 => 106, // KEYCODE_DPAD_RIGHT   -> KEY_RIGHT
        23 => 28,  // KEYCODE_DPAD_CENTER  -> KEY_ENTER (Android's centre key is Enter)
        92 => 104, // KEYCODE_PAGE_UP      -> KEY_PAGEUP
        93 => 109, // KEYCODE_PAGE_DOWN    -> KEY_PAGEDOWN
        122 => 102, // KEYCODE_MOVE_HOME   -> KEY_HOME
        123 => 107, // KEYCODE_MOVE_END    -> KEY_END
        124 => 110, // KEYCODE_INSERT      -> KEY_INSERT
        125 => 159, // KEYCODE_FORWARD     -> KEY_FORWARD (browser forward)
        4 => 158,  // KEYCODE_BACK         -> KEY_BACK (browser back)
        3 => 172,  // KEYCODE_HOME         -> KEY_HOMEPAGE

        // --- Modifiers. Left/right are kept distinct because they map to
        // distinct key codes, and xdotool-style layouts distinguish them.
        59 => 42,  // KEYCODE_SHIFT_LEFT   -> KEY_LEFTSHIFT
        60 => 54,  // KEYCODE_SHIFT_RIGHT  -> KEY_RIGHTSHIFT
        113 => 29, // KEYCODE_CTRL_LEFT    -> KEY_LEFTCTRL
        114 => 97, // KEYCODE_CTRL_RIGHT   -> KEY_RIGHTCTRL
        57 => 56,  // KEYCODE_ALT_LEFT     -> KEY_LEFTALT
        58 => 100, // KEYCODE_ALT_RIGHT    -> KEY_RIGHTALT
        117 => 125, // KEYCODE_META_LEFT   -> KEY_LEFTMETA
        118 => 126, // KEYCODE_META_RIGHT  -> KEY_RIGHTMETA
        115 => 58, // KEYCODE_CAPS_LOCK    -> KEY_CAPSLOCK
        116 => 70, // KEYCODE_SCROLL_LOCK  -> KEY_SCROLLLOCK
        143 => 69, // KEYCODE_NUM_LOCK     -> KEY_NUMLOCK

        // --- Function keys. F11/F12 are 87/88, NOT 69/70: the kernel spends
        // 69-70 on NUMLOCK/SCROLLLOCK and puts the numeric keypad in between.
        131 => 59, // KEYCODE_F1  -> KEY_F1
        132 => 60, // KEYCODE_F2  -> KEY_F2
        133 => 61, // KEYCODE_F3  -> KEY_F3
        134 => 62, // KEYCODE_F4  -> KEY_F4
        135 => 63, // KEYCODE_F5  -> KEY_F5
        136 => 64, // KEYCODE_F6  -> KEY_F6
        137 => 65, // KEYCODE_F7  -> KEY_F7
        138 => 66, // KEYCODE_F8  -> KEY_F8
        139 => 67, // KEYCODE_F9  -> KEY_F9
        140 => 68, // KEYCODE_F10 -> KEY_F10
        141 => 87, // KEYCODE_F11 -> KEY_F11
        142 => 88, // KEYCODE_F12 -> KEY_F12

        // --- Punctuation. Android's keycodes here are layout-specific names for
        // the US base-layout symbol on that physical position, which is what the
        // kernel's `KEY_*` names also mean, so these map 1:1.
        55 => 51,  // KEYCODE_COMMA        -> KEY_COMMA
        56 => 52,  // KEYCODE_PERIOD       -> KEY_DOT
        68 => 41,  // KEYCODE_GRAVE        -> KEY_GRAVE
        69 => 12,  // KEYCODE_MINUS        -> KEY_MINUS
        70 => 13,  // KEYCODE_EQUALS       -> KEY_EQUAL
        71 => 26,  // KEYCODE_LEFT_BRACKET -> KEY_LEFTBRACE
        72 => 27,  // KEYCODE_RIGHT_BRACKET-> KEY_RIGHTBRACE
        73 => 43,  // KEYCODE_BACKSLASH    -> KEY_BACKSLASH
        74 => 39,  // KEYCODE_SEMICOLON    -> KEY_SEMICOLON
        75 => 40,  // KEYCODE_APOSTROPHE   -> KEY_APOSTROPHE
        76 => 53,  // KEYCODE_SLASH        -> KEY_SLASH

        // --- Numpad. Android's NUMPAD_0..9 (144-153) map to the kernel's
        // KEY_KP0..KEY_KP9, which are NOT in numeric order: 82,79,80,81,75,76,77,71,72,73.
        144 => 82, // KEYCODE_NUMPAD_0      -> KEY_KP0
        145 => 79, // KEYCODE_NUMPAD_1      -> KEY_KP1
        146 => 80, // KEYCODE_NUMPAD_2      -> KEY_KP2
        147 => 81, // KEYCODE_NUMPAD_3      -> KEY_KP3
        148 => 75, // KEYCODE_NUMPAD_4      -> KEY_KP4
        149 => 76, // KEYCODE_NUMPAD_5      -> KEY_KP5
        150 => 77, // KEYCODE_NUMPAD_6      -> KEY_KP6
        151 => 71, // KEYCODE_NUMPAD_7      -> KEY_KP7
        152 => 72, // KEYCODE_NUMPAD_8      -> KEY_KP8
        153 => 73, // KEYCODE_NUMPAD_9      -> KEY_KP9
        154 => 98, // KEYCODE_NUMPAD_DIVIDE -> KEY_KPSLASH
        155 => 55, // KEYCODE_NUMPAD_MULTIPLY-> KEY_KPASTERISK
        156 => 74, // KEYCODE_NUMPAD_SUBTRACT-> KEY_KPMINUS
        157 => 78, // KEYCODE_NUMPAD_ADD    -> KEY_KPPLUS
        158 => 83, // KEYCODE_NUMPAD_DOT    -> KEY_KPDOT
        159 => 121, // KEYCODE_NUMPAD_COMMA -> KEY_KPCOMMA
        160 => 96, // KEYCODE_NUMPAD_ENTER  -> KEY_KPENTER
        161 => 117, // KEYCODE_NUMPAD_EQUALS-> KEY_KPEQUAL

        // --- Audio / media / system. A phone is far more likely to have these
        // keys than a desktop's own keyboard, and they are harmless to expose.
        24 => 115,  // KEYCODE_VOLUME_UP    -> KEY_VOLUMEUP
        25 => 114,  // KEYCODE_VOLUME_DOWN  -> KEY_VOLUMEDOWN
        164 => 113, // KEYCODE_VOLUME_MUTE  -> KEY_MUTE
        82 => 164,  // KEYCODE_MEDIA_PLAY_PAUSE -> KEY_PLAYPAUSE
        83 => 166,  // KEYCODE_MEDIA_STOP   -> KEY_STOPCD
        84 => 163,  // KEYCODE_MEDIA_NEXT   -> KEY_NEXTSONG
        85 => 165,  // KEYCODE_MEDIA_PREVIOUS -> KEY_PREVIOUSSONG
        86 => 168,  // KEYCODE_MEDIA_REWIND -> KEY_REWIND
        87 => 208,  // KEYCODE_MEDIA_FAST_FORWARD -> KEY_FASTFORWARD
        88 => 113,  // KEYCODE_MUTE         -> KEY_MUTE (duplicate of VOLUME_MUTE)
        81 => 217,  // KEYCODE_SEARCH       -> KEY_SEARCH
        120 => 99,  // KEYCODE_SYSRQ        -> KEY_SYSRQ
        26 => 116,  // KEYCODE_POWER        -> KEY_POWER

        // Everything else - gamepad buttons, IME keys, TV remote keys, and
        // anything a newer Android release adds - is unsupported. Returning
        // `None` is a silent no-op on the host, which is the right outcome: an
        // unmapped key must never be coerced onto some other key, because that
        // would type a character the user did not ask for.
        _ => return None,
    })
}

/// The Linux key code each modifier bit is injected as, or `None` if the bit is
/// not a modifier.
///
/// Caps Lock and Num Lock are deliberately absent. They are *latch* state, not
/// held keys: the client reports them on every event as a level, and pressing
/// `KEY_CAPSLOCK` on a host that does not know the host's current lock state
/// would toggle it the wrong way. Since the client already sends the real
/// `KEYCODE_CAPS_LOCK` press/release when the user actually toggles it, the
/// host tracks nothing and the kernel keeps the authoritative state.
fn modifier_key_for_flag(flag: u8) -> Option<u16> {
    Some(match flag {
        KEY_FLAG_SHIFT => 42,  // KEY_LEFTSHIFT
        KEY_FLAG_CTRL => 29,   // KEY_LEFTCTRL
        KEY_FLAG_ALT => 56,    // KEY_LEFTALT
        KEY_FLAG_META => 125,  // KEY_LEFTMETA
        // `KEY_FLAG_CAPS_LOCK` and `KEY_FLAG_NUM_LOCK` deliberately fall through
        // to `None` here, and are not imported: they are levels, not keys.
        _ => return None,
    })
}

/// Every key code the host can produce, i.e. the union of the mapping above and
/// the modifier codes.
///
/// This is the capability list the virtual keyboard device has to *declare*.
/// The pen and touchscreen devices declare only `BTN_TOOL_*`/`BTN_TOUCH`, and
/// libinput will not route a keystroke to a device that never claimed the
/// corresponding key code, so a keyboard device that did not declare this set
/// would be created successfully and then silently receive nothing.
fn supported_linux_keycodes() -> Vec<u16> {
    // Android keycodes in use stay far below this, but the range is walked
    // rather than hardcoded so a future Android release cannot add a code the
    // device fails to declare.
    const ANDROID_KEYCODE_SCAN_MAX: u16 = 0x02FF;

    let mut codes: Vec<u16> = (0..=ANDROID_KEYCODE_SCAN_MAX)
        .filter_map(android_keycode_to_linux_keycode)
        .collect();

    // Every key `us_layout_key_for_char` can name, so the text-derived fallback
    // in `resolve_key_for_event` can never land on a key the device did not
    // declare. All of these also appear in the table above, and the `dedup`
    // below makes the overlap harmless - the point is that the two lists cannot
    // drift apart silently.
    for c in '\u{8}'..='\u{7F}' {
        if let Some((code, _)) = us_layout_key_for_char(c) {
            codes.push(code);
        }
    }

    // Modifiers are injected on their own behalf (see `inject_key`), so they
    // must be declared even though no keycode maps *to* them.
    for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
        if let Some(code) = modifier_key_for_flag(flag) {
            codes.push(code);
        }
    }

    codes.sort_unstable();
    codes.dedup();
    codes
}

/// A key on a US (QWERTY) layout that produces `c`, and whether Shift is needed.
type LayoutKey = (u16, bool);

/// The key on a **US layout** that types `c`, plus whether Shift is required.
///
/// This is the last-resort fallback for a character the `key_code` path cannot
/// reach, and it is deliberately restricted to what a US keyboard can actually
/// produce, because the *host's* loaded layout is what will interpret whatever
/// key code is emitted. Two things it therefore will not do:
///
/// * It will not invent a key code for a non-ASCII character. There is no evdev
///   mechanism for injecting an arbitrary codepoint, and guessing a key and
///   hoping the layout turns it into the right glyph types a wrong character -
///   strictly worse than dropping the key, which at least leaves the screen
///   unchanged. `None` means "the host cannot type this".
/// * It will not try to model a non-US layout. Android hands out key *codes* by
///   physical position, so a QWERTZ client pressing its `Z` sends
///   `KEYCODE_Y` (`53`) with text `"z"`, and the host's QWERTY layout will type
///   `y`. That mismatch is a real limitation of passing key codes rather than
///   characters, and it is the reason the primary path prefers the key code:
///   the two sides agree on physical position, which is the most that can be
///   recovered without shipping a layout-switching mechanism.
///
/// Control characters are included because Android's `getUnicodeChar()` does
/// report them: `KEYCODE_ENTER` arrives with text `"\n"` and `KEYCODE_DEL`
/// with `"\u{8}"`.
///
/// Pure, and pinned character by character in the tests below - this table is
/// the second place a silent mistake can hide, since a wrong entry types a wrong
/// character rather than failing.
fn us_layout_key_for_char(c: char) -> Option<LayoutKey> {
    Some(match c {
        // Control characters. None of these need a modifier.
        '\n' | '\r' => (28, false),  // KEY_ENTER
        '\t' => (15, false),         // KEY_TAB
        '\u{8}' => (14, false),      // KEY_BACKSPACE
        '\u{1B}' => (1, false),      // KEY_ESC
        '\u{7F}' => (111, false),    // KEY_DELETE

        // Space and the unshifted digit row. The kernel numbers KEY_1..KEY_9
        // before KEY_0, which is why this is a table and not an offset.
        ' ' => (57, false),  // KEY_SPACE
        '1' => (2, false),   // KEY_1
        '2' => (3, false),   // KEY_2
        '3' => (4, false),   // KEY_3
        '4' => (5, false),   // KEY_4
        '5' => (6, false),   // KEY_5
        '6' => (7, false),   // KEY_6
        '7' => (8, false),   // KEY_7
        '8' => (9, false),   // KEY_8
        '9' => (10, false),  // KEY_9
        '0' => (11, false),  // KEY_0

        // Unshifted punctuation.
        '-' => (12, false),  // KEY_MINUS
        '=' => (13, false),  // KEY_EQUAL
        '[' => (26, false),  // KEY_LEFTBRACE
        ']' => (27, false),  // KEY_RIGHTBRACE
        '\\' => (43, false), // KEY_BACKSLASH
        ';' => (39, false),  // KEY_SEMICOLON
        '\'' => (40, false), // KEY_APOSTROPHE
        '`' => (41, false),  // KEY_GRAVE
        ',' => (51, false),  // KEY_COMMA
        '.' => (52, false),  // KEY_DOT
        '/' => (53, false),  // KEY_SLASH

        // Shifted digits.
        '!' => (2, true),   // Shift + KEY_1
        '@' => (3, true),   // Shift + KEY_2
        '#' => (4, true),   // Shift + KEY_3
        '$' => (5, true),   // Shift + KEY_4
        '%' => (6, true),   // Shift + KEY_5
        '^' => (7, true),   // Shift + KEY_6
        '&' => (8, true),   // Shift + KEY_7
        '*' => (9, true),   // Shift + KEY_8
        '(' => (10, true),  // Shift + KEY_9
        ')' => (11, true),  // Shift + KEY_0

        // Shifted punctuation.
        '_' => (12, true),  // Shift + KEY_MINUS
        '+' => (13, true),  // Shift + KEY_EQUAL
        '{' => (26, true),  // Shift + KEY_LEFTBRACE
        '}' => (27, true),  // Shift + KEY_RIGHTBRACE
        '|' => (43, true),  // Shift + KEY_BACKSLASH
        ':' => (39, true),  // Shift + KEY_SEMICOLON
        '"' => (40, true),  // Shift + KEY_APOSTROPHE
        '~' => (41, true),  // Shift + KEY_GRAVE
        '<' => (51, true),  // Shift + KEY_COMMA
        '>' => (52, true),  // Shift + KEY_DOT
        '?' => (53, true),  // Shift + KEY_SLASH

        // Letters. The lowercase/uppercase split is a match guard rather than
        // `to_ascii_uppercase()` because the return value has to be the *same*
        // key code either way, with only the shift flag differing.
        'a'..='z' => (lowercase_letter_to_key(c), false),
        'A'..='Z' => (lowercase_letter_to_key(c.to_ascii_lowercase()), true),

        // Anything else - a non-ASCII codepoint, or a control character with no
        // key on this layout. Dropped; see the doc comment.
        _ => return None,
    })
}

/// The Linux key code for a lowercase ASCII letter.
///
/// Split out of [`us_layout_key_for_char`] because it is the single most
/// error-prone table in the file, and it is *not* a formula.
///
/// The evdev key codes for letters are laid out by **keyboard row**, and the
/// numeric row is interleaved into the first letter row, so each row is one
/// ascending run starting 16 apart from the last:
///
/// ```text
///   Q=16 W=17 E=18 R=19 T=20 Y=21 U=22 I=23 O=24 P=25   <- top letter row
///  [1= 2 ... 9=10 0=11]                                <- number row, same run
///   A=30 S=31 D=32 F=33 G=34 H=35 J=36 K=37 L=38       <- home row
///   Z=44 X=45 C=46 V=47 B=48 N=49 M=50                 <- bottom row
/// ```
///
/// So "alphabetical index plus a base" is wrong for 15 of the 26 letters -
/// `KEY_B` is 48, not 31 - and there is no arithmetic that reproduces this
/// without first un-permuting the alphabet into QWERTY order. Every value is
/// therefore listed, and the test module checks it against the row structure
/// above, independently derived.
///
/// Total on purpose: the `_` arm keeps the function usable directly from a test
/// and can only be reached with a non-letter, which `us_layout_key_for_char`
/// never passes.
fn lowercase_letter_to_key(c: char) -> u16 {
    match c {
        'q' => 16, // KEY_Q
        'w' => 17, // KEY_W
        'e' => 18, // KEY_E
        'r' => 19, // KEY_R
        't' => 20, // KEY_T
        'y' => 21, // KEY_Y
        'u' => 22, // KEY_U
        'i' => 23, // KEY_I
        'o' => 24, // KEY_O
        'p' => 25, // KEY_P
        'a' => 30, // KEY_A
        's' => 31, // KEY_S
        'd' => 32, // KEY_D
        'f' => 33, // KEY_F
        'g' => 34, // KEY_G
        'h' => 35, // KEY_H
        'j' => 36, // KEY_J
        'k' => 37, // KEY_K
        'l' => 38, // KEY_L
        'z' => 44, // KEY_Z
        'x' => 45, // KEY_X
        'c' => 46, // KEY_C
        'v' => 47, // KEY_V
        'b' => 48, // KEY_B
        'n' => 49, // KEY_N
        'm' => 50, // KEY_M
        // Unreachable from `us_layout_key_for_char`, which only calls this for
        // 'a'..='z'. KEY_A keeps it total.
        _ => 30,
    }
}

/// Resolves the Linux key code to emit for `event`, and whether Shift is
/// additionally required because the resolved character is a shifted one.
///
/// Two-stage, because the two inputs disagree in different ways:
///
/// 1. `key_code` is authoritative. The two sides agree on *physical key
///    position*, which is layout-independent, so a host whose layout differs
///    from the client's still presses the key the user actually pressed. Its
///    `text` is discarded at this stage even when it is present and even when it
///    looks more specific, because `text` is the *client's* layout's output and
///    preferring it would relabel a QWERTZ `Z` as a QWERTY `Z`.
/// 2. Only if there is no key code at all - an IME key, a gamepad button, a
///    keycode from a newer Android release - does `text` get a say, via
///    [`us_layout_key_for_char`]. Here the host has nothing better, and one
///    character is better than none. `text` may be several characters long (an
///    IME commit), and only the first is representable as a single key, so the
///    rest is dropped and logged.
///
/// Returns `None` when the host genuinely cannot type the key, which is a
/// silent no-op rather than an error: the caller counts a dropped key as a
/// handled packet, and a screen that did not change is the correct outcome for
/// a character the host has no key for.
fn resolve_key_for_event(event: &KeyEvent) -> Option<LayoutKey> {
    if let Some(code) = android_keycode_to_linux_keycode(event.key_code) {
        return Some((code, false));
    }

    let first = event.text.chars().next()?;
    let resolved = us_layout_key_for_char(first);
    if resolved.is_some() && event.text.chars().count() > 1 {
        log::debug!(
            "Android keycode {} produced {:?}, which is longer than the single key that can be \
             injected; typing {:?} and dropping the rest",
            event.key_code, event.text, first
        );
    }
    resolved
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [libc::c_char; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
struct TimeVal {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
}

#[repr(C)]
struct LinuxInputEvent {
    time: TimeVal,
    r#type: u16,
    code: u16,
    value: i32,
}

// ===========================================================================
// PEN STATE MACHINE
// ===========================================================================

/// One evdev event: `(type, code, value)`.
type Ev = (u16, u16, i32);

/// One pen sample, already in axis units.
struct PenSample {
    action: u8,
    eraser: bool,
    barrel1: bool,
    barrel2: bool,
    x: i32,
    y: i32,
    pressure: f32,
    tilt_x: i32,
    tilt_y: i32,
}

/// What the host has told the kernel about the pen so far. Turns samples into
/// the event frames a real tablet would emit, so libinput sees proper
/// proximity and contact transitions; pure, so it is tested without uinput.
#[derive(Default)]
struct PenTracker {
    /// `BTN_TOOL_PEN` or `BTN_TOOL_RUBBER` while in proximity.
    tool: Option<u16>,
    /// `BTN_TOUCH` as last written (after hysteresis).
    touching: bool,
    /// The last sample was a contact sample (DOWN/MOVE), whatever its
    /// pressure. This is what tells a contact lift from leaving the range:
    /// both arrive as `ACTION_UP`.
    contact: bool,
    barrel1: bool,
    barrel2: bool,
}

impl PenTracker {
    /// Frames (each ends in a `SYN_REPORT`) for one sample.
    ///
    /// * Switching pen and eraser takes the old tool out of proximity in a
    ///   frame of its own first: libinput does not accept two tools at once
    ///   and would drop or misattribute the change.
    /// * `ACTION_UP` after contact lifts the tip but stays in proximity (the
    ///   phone follows a stroke with hover events, and leaving here would make
    ///   the tool flicker out and in between strokes); `ACTION_UP` without
    ///   contact is the pen leaving the range, as is `ACTION_CANCEL`. A digitizer
    ///   that reports no hover therefore keeps the pen "in range" at its last
    ///   position after a stroke.
    /// * A hover is never touching, and contact starts and ends with
    ///   hysteresis on the pressure.
    fn frames(&mut self, s: &PenSample) -> Vec<Vec<Ev>> {
        let was_contact = self.contact;
        let (in_range, contact) = match s.action {
            ACTION_UP => (was_contact, false),
            ACTION_CANCEL => (false, false),
            ACTION_DOWN | ACTION_MOVE => (true, true),
            _ => (true, false),
        };
        let want_tool = in_range.then_some(if s.eraser { BTN_TOOL_RUBBER } else { BTN_TOOL_PEN });
        let mut frames = Vec::new();

        if let (Some(old), Some(new)) = (self.tool, want_tool) {
            if old != new {
                frames.push(self.release_all(old));
            }
        }

        let touching = contact
            && if self.touching { s.pressure >= PRESSURE_TOUCH_OFF } else { s.pressure >= PRESSURE_TOUCH_ON };
        let (b1, b2) = (in_range && s.barrel1, in_range && s.barrel2);
        let mut f: Vec<Ev> = Vec::new();
        if want_tool.is_some() && self.tool != want_tool {
            f.push((EV_KEY, want_tool.unwrap(), 1));
        }
        f.push((EV_ABS, ABS_X, s.x));
        f.push((EV_ABS, ABS_Y, s.y));
        f.push((EV_ABS, ABS_PRESSURE, if touching { pressure_axis(s.pressure) } else { 0 }));
        f.push((EV_ABS, ABS_TILT_X, s.tilt_x));
        f.push((EV_ABS, ABS_TILT_Y, s.tilt_y));
        // Contact and buttons go out only when they change; leaving range
        // releases them before the tool goes.
        if touching != self.touching {
            f.push((EV_KEY, BTN_TOUCH, i32::from(touching)));
        }
        if b1 != self.barrel1 {
            f.push((EV_KEY, BTN_STYLUS, i32::from(b1)));
        }
        if b2 != self.barrel2 {
            f.push((EV_KEY, BTN_STYLUS2, i32::from(b2)));
        }
        if want_tool.is_none() {
            if let Some(old) = self.tool {
                f.push((EV_KEY, old, 0));
            }
        }
        frames.push(f);

        self.tool = want_tool;
        self.touching = touching;
        self.contact = contact;
        self.barrel1 = b1;
        self.barrel2 = b2;
        frames
    }

    /// A frame that lifts the tip, releases the buttons and takes `tool` out
    /// of proximity.
    fn release_all(&mut self, tool: u16) -> Vec<Ev> {
        let mut f = Vec::new();
        if self.touching {
            f.push((EV_ABS, ABS_PRESSURE, 0));
            f.push((EV_KEY, BTN_TOUCH, 0));
        }
        if self.barrel1 {
            f.push((EV_KEY, BTN_STYLUS, 0));
        }
        if self.barrel2 {
            f.push((EV_KEY, BTN_STYLUS2, 0));
        }
        f.push((EV_KEY, tool, 0));
        self.tool = None;
        self.touching = false;
        self.contact = false;
        self.barrel1 = false;
        self.barrel2 = false;
        f
    }
}

/// Timestamp (microseconds, `CLOCK_MONOTONIC`) for a history sample that was
/// taken `age_us` before `now_us`: never before the previous stamp (the events
/// of one device must not run backwards) and never after `now_us`.
fn history_stamp(now_us: i64, age_us: u32, last_us: i64) -> i64 {
    (now_us - i64::from(age_us)).max(last_us).min(now_us)
}

fn monotonic_us() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000 + ts.tv_nsec as i64 / 1000
}

pub struct LinuxUinputInjector {
    display_width: u32,
    display_height: u32,
    /// Monitor that absolute input maps onto, with the desktop bounding box it
    /// was computed against. `None` maps onto the whole desktop.
    region: Option<(crate::display::OutputRegion, crate::display::OutputRegion)>,
    /// Separate virtual devices for the pen and the finger, so libinput
    /// classifies them correctly (tablet vs touchscreen). See `setup_pen_device`.
    pen_file: Option<File>,
    touch_file: Option<File>,
    pen: PenTracker,
    /// Timestamp of the last pen event written, in `CLOCK_MONOTONIC` microseconds.
    pen_stamp_us: i64,
    /// The pen / touch device has an `output` mapping on GNOME, so its
    /// coordinates are the phone's own (see `set_output_region`).
    pen_mapped: bool,
    touch_mapped: bool,
    /// The GNOME `output` keys are set and must be reset again.
    gnome_keys_set: bool,
    /// Virtual keyboard, a *third* device with its own fd. It has to be a
    /// separate device, not extra keys on the pen: libinput classifies a device
    /// by its declared axes, and a tablet with letter keys in its `EV_KEY` set
    /// is still just a tablet - the keystrokes would be accepted by the kernel
    /// and never routed as keyboard input.
    keyboard_file: Option<File>,
    /// Virtual mouse, a *fourth* device. Same reasoning as the keyboard: a
    /// touchscreen device has no `EV_REL` axes, so a `REL_WHEEL` event written
    /// to it is dropped by the input core.
    ///
    /// `INPUT_TYPE_MOUSE` is routed here rather than to the touchscreen, which
    /// is what the pre-existing code did. The touchscreen has no way to express
    /// "relative movement" or "wheel", so the old route could only ever produce
    /// a finger tap at an absolute position. See `inject`.
    mouse_file: Option<File>,
    /// Contact slots that are currently down on the touchscreen, so we only emit
    /// `ABS_MT_TRACKING_ID` when a slot is actually acquired.
    active_slots: [bool; MAX_SLOTS],
    /// Last position written to the virtual mouse, so a normalised position from
    /// the client can be turned into the `REL_X`/`REL_Y` delta a relative device
    /// needs. `None` until the first mouse event; the first one only seeds the
    /// position instead of jumping the cursor across the screen.
    mouse_pos: Option<(i32, i32)>,
    /// Which mouse buttons the host currently believes are held, so a press and
    /// its release are paired and a dropped packet cannot leave a button stuck.
    mouse_buttons: u8,
    /// Sub-pixel remainder of relative (touchpad) motion, carried to the next event.
    rel_frac: (f32, f32),
    /// A KWin output binding that failed and is retried on later events.
    kwin_pending: Option<(Option<crate::display::OutputRegion>, std::time::Instant)>,
    /// Whether the touch-down emulating a mouse click (the fallback used when no
    /// virtual mouse could be created) is currently held. Kept apart from
    /// `active_slots` because a mouse pointer_id is 0, the same slot a first
    /// finger would occupy.
    mouse_touch_active: bool,
    /// The modifier state the host currently believes is held, as a mask of
    /// `KEY_FLAG_*`. The client re-reports the *state* of all four modifiers on
    /// every key event rather than sending modifier transitions, so without this
    /// the host would re-press Shift for every character and never release it.
    pressed_modifiers: u8,
}

const MAX_SLOTS: usize = 10;

/// USB vendor/product ids for the keyboard and mouse virtual devices.
///
/// The pen and touchscreen borrow Wacom's (0x056a) because they are pretending
/// to be a tablet, and libinput's heuristics and udev hwdb are keyed partly on
/// it. A keyboard and a mouse want no such thing: giving them ids that belong to
/// a real hardware vendor would let hwdb rules for that vendor's keyboards apply
/// to them. `0x1209` is pid.codes' open-hardware vendor id, which is the
/// conventional choice for exactly this case, and `0x0001`-style product ids
/// that belong to nobody.
const DISPLAYSWARM_VENDOR_ID: u16 = 0x1209;
const DISPLAYSWARM_KEYBOARD_PRODUCT_ID: u16 = 0x0001;
const DISPLAYSWARM_MOUSE_PRODUCT_ID: u16 = 0x0002;

impl LinuxUinputInjector {
    pub fn new(display_width: u32, display_height: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // One /dev/uinput file descriptor per virtual device. `UI_DEV_SETUP` is only
        // legal before `UI_DEV_CREATE` on a given fd, and after a device is created
        // the fd is bound to it, so a second device needs its own descriptor.
        let open_uinput = || -> std::io::Result<File> {
            OpenOptions::new().read(true).write(true).open("/dev/uinput")
        };

        let pen_file = open_uinput().and_then(|f| {
            Self::setup_pen_device(&f, display_width, display_height)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(f)
        });
        let touch_file = open_uinput().and_then(|f| {
            Self::setup_touch_device(&f, display_width, display_height)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(f)
        });
        // Keyboard and mouse take no geometry: the keyboard has no axes at all,
        // and the mouse is relative, so unlike the digitizer devices they have
        // nothing to scale to the display size.
        let keyboard_file = open_uinput().and_then(|f| {
            Self::setup_keyboard_device(&f).map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(f)
        });
        let mouse_file = open_uinput().and_then(|f| {
            Self::setup_mouse_device(&f).map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(f)
        });

        let report = |label: &str, r: &std::io::Result<File>| match r {
            Ok(_) => log::info!("Created virtual device: {}", label),
            Err(e) => log::warn!("Failed to create virtual device {}: {}", label, e),
        };
        report("DisplaySwarm Virtual Pen (libinput tablet)", &pen_file);
        report("DisplaySwarm Virtual Touchscreen", &touch_file);
        report("DisplaySwarm Virtual Keyboard", &keyboard_file);
        report("DisplaySwarm Virtual Mouse", &mouse_file);

        let pen_file = pen_file.ok();
        let touch_file = touch_file.ok();
        let keyboard_file = keyboard_file.ok();
        let mouse_file = mouse_file.ok();

        if pen_file.is_none() && touch_file.is_none() {
            log::warn!(
                "No uinput device could be created. Input injection is disabled. \
                 Add your user to the 'input' group or run: sudo chmod 666 /dev/uinput"
            );
        }
        if keyboard_file.is_none() {
            log::warn!(
                "No virtual keyboard could be created. Key events from the client will be \
                 dropped. Pen and touch input are unaffected."
            );
        }
        if mouse_file.is_none() {
            log::warn!(
                "No virtual mouse could be created. Scroll and mouse buttons from the client \
                 will be dropped. Pen and touch input are unaffected."
            );
        }

        Ok(Self {
            display_width,
            display_height,
            region: None,
            pen_file,
            touch_file,
            pen: PenTracker::default(),
            pen_stamp_us: 0,
            pen_mapped: false,
            touch_mapped: false,
            gnome_keys_set: false,
            keyboard_file,
            mouse_file,
            active_slots: [false; MAX_SLOTS],
            mouse_pos: None,
            mouse_buttons: 0,
            rel_frac: (0.0, 0.0),
            kwin_pending: None,
            mouse_touch_active: false,
            pressed_modifiers: 0,
        })
    }

    /// Sets up the pen/eraser half of the digitizer: a true libinput *tablet*.
    ///
    /// Deliberately declares NO `ABS_MT_*` axes. libinput tests `has_touch`
    /// (ABS_MT_POSITION_X/Y or ABS_TOUCH, combined with INPUT_PROP_DIRECT)
    /// *before* `has_pen`/`has_tilt`, so a device carrying both MT and pen axes
    /// is classified as a touchscreen and the pen branch is never reached. Pen
    /// and touch are therefore exposed as two separate virtual devices.
    fn setup_pen_device(file: &File, width: u32, height: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let fd = file.as_raw_fd();

        unsafe {
            // Enable Event Types
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int);
            libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT as libc::c_int);

            // Stylus & Tablet keys. BTN_TOOL_PEN/BTN_TOOL_RUBBER are what make
            // libinput recognise this as a tablet rather than a mouse.
            let keys = [
                BTN_TOOL_PEN,
                BTN_TOOL_RUBBER,
                BTN_TOUCH,
                BTN_STYLUS,
                BTN_STYLUS2,
            ];
            for k in keys {
                libc::ioctl(fd, UI_SET_KEYBIT, k as libc::c_int);
            }

            let axes = [
                (ABS_X, 0, pen_axis_max(width)),
                (ABS_Y, 0, pen_axis_max(height)),
                (ABS_PRESSURE, 0, PRESSURE_MAX),
                // Tilt is declared in whole degrees because that is what
                // `tilt_radians_to_degrees` emits, and that is what the kernel
                // and libinput expect on these axes.
                (ABS_TILT_X, TILT_MIN_DEG, TILT_MAX_DEG),
                (ABS_TILT_Y, TILT_MIN_DEG, TILT_MAX_DEG),
            ];

            for (axis, min, max) in axes {
                // X/Y get a plausible physical size (about 300 dpi) instead of
                // 1 unit/mm, which would describe a tablet metres wide.
                let res = if axis == ABS_X || axis == ABS_Y { PEN_UNITS_PER_MM } else { 1 };
                Self::setup_axis_res(fd, axis, min, max, res);
            }

            let mut name = [0 as libc::c_char; 80];
            let dev_name = b"DisplaySwarm Virtual Pen";
            for (i, &b) in dev_name.iter().enumerate() {
                name[i] = b as libc::c_char;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03, // BUS_USB
                    // Wacom's vendor id keeps libinput from smoothing (and so
                    // delaying) the pen, which it does for every other vendor.
                    // The product id must not be a real model: libwacom would
                    // describe us as that tablet (0x0300 is a "Wacom One S",
                    // an external, non-screen tablet).
                    vendor: WACOM_VENDOR_ID,
                    product: PEN_PRODUCT_ID,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };

            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                return Err("UI_DEV_SETUP failed for pen device".into());
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("UI_DEV_CREATE failed for pen device".into());
            }
        }

        Ok(())
    }

    /// Sets up the finger half of the digitizer: a libinput *touchscreen*.
    fn setup_touch_device(file: &File, width: u32, height: u32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let fd = file.as_raw_fd();

        unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int);
            libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT as libc::c_int);
            libc::ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as libc::c_int);

            let axes = [
                (ABS_MT_SLOT, 0, 9),
                (ABS_MT_TRACKING_ID, 0, 65535),
                (ABS_MT_POSITION_X, 0, width as i32),
                (ABS_MT_POSITION_Y, 0, height as i32),
                (ABS_MT_PRESSURE, 0, PRESSURE_MAX),
            ];

            for (axis, min, max) in axes {
                Self::setup_axis(fd, axis, min, max);
            }

            let mut name = [0 as libc::c_char; 80];
            let dev_name = b"DisplaySwarm Virtual Touchscreen";
            for (i, &b) in dev_name.iter().enumerate() {
                name[i] = b as libc::c_char;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03,
                    vendor: WACOM_VENDOR_ID,
                    product: TOUCH_PRODUCT_ID,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };

            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                return Err("UI_DEV_SETUP failed for touch device".into());
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("UI_DEV_CREATE failed for touch device".into());
            }
        }

        Ok(())
    }

    /// Sets up the virtual keyboard.
    ///
    /// A third device, with its own `/dev/uinput` fd, for the reason spelled out
    /// on `LinuxUinputInjector::keyboard_file`. Two constraints shape what is
    /// declared here:
    ///
    /// * `EV_KEY` codes are **capabilities**. libinput routes an event to a
    ///   device according to the codes that device declared, so every key in
    ///   `supported_linux_keycodes()` has to be declared or the corresponding
    ///   keystroke is silently dropped no matter how it is written. The set is
    ///   derived from the mapping itself rather than being a second, hand-kept
    ///   list, because two lists would drift and a missing entry is invisible.
    /// * No `INPUT_PROP_DIRECT`. That property marks a device whose position is
    ///   an absolute screen coordinate (a tablet or a touchscreen). A keyboard
    ///   declaring it would be classified as a direct pointer device and
    ///   libinput would stop treating it as a keyboard.
    fn setup_keyboard_device(file: &File) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let fd = file.as_raw_fd();

        let keycodes = supported_linux_keycodes();
        log::debug!("Declaring virtual keyboard with {} key codes", keycodes.len());

        unsafe {
            // EV_SYN is implied by the kernel for uinput but declared explicitly
            // for consistency with the other three devices here.
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);

            for code in &keycodes {
                libc::ioctl(fd, UI_SET_KEYBIT, *code as libc::c_int);
            }

            let mut name = [0 as libc::c_char; 80];
            for (i, &b) in b"DisplaySwarm Virtual Keyboard".iter().enumerate() {
                name[i] = b as libc::c_char;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03, // BUS_USB
                    vendor: DISPLAYSWARM_VENDOR_ID,
                    product: DISPLAYSWARM_KEYBOARD_PRODUCT_ID,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };

            // NOTE: UI_DEV_SETUP is only legal before UI_DEV_CREATE, which is why
            // each virtual device gets a freshly opened fd rather than sharing one.
            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                return Err("UI_DEV_SETUP failed for keyboard device".into());
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("UI_DEV_CREATE failed for keyboard device".into());
            }
        }

        Ok(())
    }

    /// Sets up the virtual mouse that the client's scroll and button bits drive.
    ///
    /// Declared capabilities, and why each is needed:
    ///
    /// | capability                                | why                                        |
    /// |-------------------------------------------|--------------------------------------------|
    /// | `EV_REL` `REL_X`/`REL_Y`                  | motion. The client sends absolute normalised |
    /// |                                           | positions, converted to deltas in `inject`. |
    /// | `EV_REL` `REL_WHEEL`                      | `POINTER_FLAG_SCROLL_UP`/`DOWN`.           |
    /// | `EV_REL` `REL_HWHEEL`                     | `POINTER_FLAG_SCROLL_LEFT`/`RIGHT`.         |
    /// | `EV_KEY` `BTN_LEFT`/`RIGHT`/`MIDDLE`      | `POINTER_FLAG_BTN_PRIMARY`/`SECONDARY`/`TERTIARY`. |
    /// | `INPUT_PROP_POINTING_STICK`               | disables pointer and scroll acceleration,  |
    /// |                                           | so the computed deltas land where asked.  |
    /// | `EV_SYN`                                  | frame delimiter, as on the other devices.  |
    ///
    /// Deliberately **no** `EV_ABS` and **no** `INPUT_PROP_DIRECT`. An absolute
    /// axis on a device that also has `BTN_LEFT` is how libinput decides a thing
    /// is a pad, and a pad's `REL_WHEEL` gets its own acceleration and
    /// two-finger-scroll heuristics applied. The client already sends absolute
    /// *normalised* coordinates, and converting them to deltas host-side is both
    /// trivial and exactly reversible.
    fn setup_mouse_device(file: &File) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let fd = file.as_raw_fd();

        unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int);
            libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTING_STICK as libc::c_int);

            for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int);
            }
            for code in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                libc::ioctl(fd, UI_SET_RELBIT, code as libc::c_int);
            }

            let mut name = [0 as libc::c_char; 80];
            for (i, &b) in b"DisplaySwarm Virtual Mouse".iter().enumerate() {
                name[i] = b as libc::c_char;
            }

            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03, // BUS_USB
                    vendor: DISPLAYSWARM_VENDOR_ID,
                    product: DISPLAYSWARM_MOUSE_PRODUCT_ID,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };

            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                return Err("UI_DEV_SETUP failed for mouse device".into());
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("UI_DEV_CREATE failed for mouse device".into());
            }
        }

        Ok(())
    }

    fn setup_axis(fd: libc::c_int, axis: u16, min: i32, max: i32) {
        Self::setup_axis_res(fd, axis, min, max, 1)
    }

    fn setup_axis_res(fd: libc::c_int, axis: u16, min: i32, max: i32, resolution: i32) {
        unsafe {
            libc::ioctl(fd, UI_SET_ABSBIT, axis as libc::c_int);
            let abs_setup = UinputAbsSetup {
                code: axis,
                absinfo: AbsInfo {
                    value: 0,
                    minimum: min,
                    maximum: max,
                    fuzz: 0,
                    flat: 0,
                    resolution,
                },
            };
            libc::ioctl(fd, UI_ABS_SETUP, &abs_setup);
        }
    }

    fn write_event(file: &mut File, r#type: u16, code: u16, value: i32) {
        Self::write_event_at(file, 0, r#type, code, value);
    }

    /// Writes one event. A `stamp_us` of 0 leaves the time to the kernel; a
    /// `CLOCK_MONOTONIC` stamp within the last second is kept by uinput.
    fn write_event_at(file: &mut File, stamp_us: i64, r#type: u16, code: u16, value: i32) {
        let ev = LinuxInputEvent {
            time: TimeVal {
                tv_sec: (stamp_us / 1_000_000) as libc::time_t,
                tv_usec: (stamp_us % 1_000_000) as libc::suseconds_t,
            },
            r#type,
            code,
            value,
        };
        unsafe {
            let slice = std::slice::from_raw_parts(
                &ev as *const LinuxInputEvent as *const u8,
                std::mem::size_of::<LinuxInputEvent>(),
            );
            let _ = file.write_all(slice);
        }
    }

    fn syn_report(file: &mut File) {
        Self::write_event(file, EV_SYN, SYN_REPORT, 0);
    }

    /// Injects one `INPUT_TYPE_MOUSE` packet into the virtual mouse.
    ///
    /// This is the branch that makes the client's new scroll bits and mouse
    /// buttons do something. Before this existed, `INPUT_TYPE_MOUSE` fell
    /// through to the touchscreen branch above, which could only ever express an
    /// absolute finger tap: the touchscreen has no `EV_REL` axes, so the
    /// `POINTER_FLAG_SCROLL_*` bits were decoded nowhere and discarded.
    ///
    /// Absolute-to-relative conversion: the client sends normalised positions
    /// (it is a touchscreen client, so it only ever knows where a finger is),
    /// while the virtual mouse is a *relative* device, which is what a
    /// compositor expects a mouse to be - a pointer device with a warped
    /// pointer, not a screen coordinate. The target is therefore converted to
    /// pixels and emitted as the delta from the previous target. Two
    /// consequences worth stating:
    ///
    /// * The first mouse event only *records* the position. Without that, the
    ///   cursor would be yanked from wherever it happened to be to the first
    ///   touch point.
    /// * `INPUT_PROP_POINTING_STICK` is declared on the device, so no pointer
    ///   acceleration re-scales these deltas and the cursor lands where asked.
    fn inject_mouse(&mut self, event: &InputEvent, abs_x: i32, abs_y: i32) {
        let Some(file) = self.mouse_file.as_mut() else {
            // No mouse device (no /dev/uinput access, or creation failed). Fall
            // back to the *old* behaviour - emulate the click as a finger tap on
            // the touchscreen - so a mouse click still does something rather than
            // vanishing. Scroll still cannot be represented there, which is why
            // the creation failure is logged separately.
            //
            // The tap is tracked in its own `mouse_touch_active` flag rather than
            // in `active_slots`: a mouse pointer_id is 0, which is the same slot a
            // first finger would take, and sharing the array would let a click and
            // a touch cancel each other's contact.
            if let Some(touch) = self.touch_file.as_mut() {
                Self::write_event(touch, EV_ABS, ABS_MT_SLOT, 0);
                let down = event.action == ACTION_DOWN || event.action == ACTION_MOVE;
                if down {
                    if !self.mouse_touch_active {
                        Self::write_event(touch, EV_ABS, ABS_MT_TRACKING_ID, 1);
                        self.mouse_touch_active = true;
                    }
                    Self::write_event(touch, EV_ABS, ABS_MT_POSITION_X, abs_x);
                    Self::write_event(touch, EV_ABS, ABS_MT_POSITION_Y, abs_y);
                    Self::write_event(touch, EV_KEY, BTN_TOUCH, 1);
                } else if self.mouse_touch_active {
                    Self::write_event(touch, EV_ABS, ABS_MT_TRACKING_ID, -1);
                    Self::write_event(touch, EV_KEY, BTN_TOUCH, 0);
                    self.mouse_touch_active = false;
                }
                Self::syn_report(touch);
            }
            return;
        };

        // --- Motion -------------------------------------------------------
        match self.mouse_pos {
            Some((prev_x, prev_y)) => {
                let (dx, dy) = (abs_x - prev_x, abs_y - prev_y);
                if dx != 0 || dy != 0 {
                    Self::write_event(file, EV_REL, REL_X, dx);
                    Self::write_event(file, EV_REL, REL_Y, dy);
                }
            }
            // First event: seed only. See the doc comment.
            None => log::debug!("Seeding virtual mouse at ({}, {})", abs_x, abs_y),
        }
        self.mouse_pos = Some((abs_x, abs_y));

        // --- Buttons ------------------------------------------------------
        // The flags describe the *current* button state on every packet, so
        // comparing against what the host last wrote means only transitions are
        // emitted. A mouse that re-sends "button 1 down" on every move would
        // still work in most applications, but X11 counts clicks on the press
        // edge, so a repeated press edge is a double-click.
        let buttons = MouseButtons::from_flags(event.flags);
        let new_state = u8::from(buttons.primary)
            | (u8::from(buttons.secondary) << 1)
            | (u8::from(buttons.tertiary) << 2);

        for (index, code) in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE].into_iter().enumerate() {
            let was = (self.mouse_buttons >> index) & 1 != 0;
            let now = (new_state >> index) & 1 != 0;
            if was != now {
                Self::write_event(file, EV_KEY, code, i32::from(now));
            }
        }
        self.mouse_buttons = new_state;

        // --- Scroll -------------------------------------------------------
        // Independent of position and buttons: a scroll packet is a normal
        // pointer packet that additionally carries a notch, so all three are
        // emitted in one frame.
        let (wheel, hwheel) = wheel_deltas_from_flags(event.flags);
        if wheel != 0 {
            Self::write_event(file, EV_REL, REL_WHEEL, wheel);
        }
        if hwheel != 0 {
            Self::write_event(file, EV_REL, REL_HWHEEL, hwheel);
        }

        // An ACTION_UP / ACTION_CANCEL with no scroll and no button left set is
        // still a position update worth syncing, so the frame is always closed.
        Self::syn_report(file);
    }
}

/// The mouse's button state, derived from a pointer packet's `flags` byte.
///
/// A separate type rather than an inline bit test because the translation is
/// the part with a real chance of being wrong in a way nobody notices: getting
/// primary and secondary swapped still "clicks something", and getting the
/// wheel sign wrong scrolls the wrong way. Keeping it as data makes it directly
/// assertable - see the tests below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct MouseButtons {
    primary: bool,
    secondary: bool,
    tertiary: bool,
}

impl MouseButtons {
    /// Decodes the three button bits.
    ///
    /// `POINTER_FLAG_BTN_PRIMARY`/`SECONDARY`/`TERTIARY` are MotionEvent's
    /// `BUTTON_PRIMARY`/`BUTTON_SECONDARY`/`BUTTON_TERTIARY` in the order a
    /// mouse reports them, which is left, right, middle - so they map onto
    /// `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE` in that order. The pen branch in
    /// `inject` reads the *same* three bits as eraser/barrel1/barrel2, which is
    /// correct there because a pen has no left button; the two are
    /// distinguished by `event_type`, not by the bits.
    fn from_flags(flags: u8) -> Self {
        Self {
            primary: flags & POINTER_FLAG_BTN_PRIMARY != 0,
            secondary: flags & POINTER_FLAG_BTN_SECONDARY != 0,
            tertiary: flags & POINTER_FLAG_BTN_TERTIARY != 0,
        }
    }
}

/// The wheel deltas a pointer packet's `flags` byte asks for, in Linux
/// `REL_WHEEL` / `REL_HWHEEL` units (one wheel notch per event).
///
/// Sign convention, which is the whole difficulty of this function:
///
/// * `POINTER_FLAG_SCROLL_UP` means *content moves towards the top of the
///   document*, which is Linux `REL_WHEEL` **positive**.
/// * `POINTER_FLAG_SCROLL_LEFT` means *content moves towards the left*, which
///   is `REL_HWHEEL` **negative** - the horizontal axis points the opposite way
///   to the vertical one, because dragging a horizontal scrollbar to the left
///   brings leftward content into view.
///
/// The flags are named for the content direction precisely so that the client's
/// finger movement and this conversion cannot disagree: a finger swiping *up*
/// scrolls content up, and so sends `SCROLL_UP`. Inverting either mapping here
/// would produce a scroll that feels inverted, which is the classic symptom of
/// a sign error here.
fn wheel_deltas_from_flags(flags: u8) -> (i32, i32) {
    let vertical =
        i32::from(flags & POINTER_FLAG_SCROLL_UP != 0) - i32::from(flags & POINTER_FLAG_SCROLL_DOWN != 0);
    let horizontal = i32::from(flags & POINTER_FLAG_SCROLL_RIGHT != 0)
        - i32::from(flags & POINTER_FLAG_SCROLL_LEFT != 0);
    (vertical, horizontal)
}

/// The modifier set an event asks for. Shift required only to produce a
/// text-derived character applies to the press: counting it on the release too
/// would make the release look like "Shift is still wanted" and leave Shift
/// held, so the next character (typed "b") would come out shifted ("B").
fn effective_modifiers(reported: u8, extra_shift: bool, pressing: bool) -> u8 {
    reported | if extra_shift && pressing { KEY_FLAG_SHIFT } else { 0 }
}

impl LinuxUinputInjector {
    /// The event's normalised position after the region mapping. Devices GNOME
    /// maps to the monitor itself keep the phone's own coordinates.
    fn scaled(&self, event: &InputEvent) -> (f32, f32) {
        let mapped = match event.event_type {
            INPUT_TYPE_STYLUS => self.pen_mapped,
            INPUT_TYPE_MOUSE => false,
            _ => self.touch_mapped,
        };
        match self.region {
            Some((region, bounds)) if !mapped => {
                super::map_norm_into_region(event.x_norm, event.y_norm, region, bounds)
            }
            _ => (event.x_norm, event.y_norm),
        }
    }

    /// Writes a pen sample as the frames `PenTracker` derives. `age_us` is how
    /// long ago the digitizer took it: batched history samples get their real
    /// times (uinput keeps a `CLOCK_MONOTONIC` stamp from the last second), so
    /// apps see the stroke's true velocity instead of one burst. libinput's
    /// tablet code does not use `MSC_TIMESTAMP`, so it is not emitted.
    fn inject_pen(&mut self, event: &InputEvent, age_us: u32) {
        let (x_norm, y_norm) = self.scaled(event);
        let sample = PenSample {
            action: event.action,
            eraser: event.flags & POINTER_FLAG_ERASER != 0,
            barrel1: event.flags & POINTER_FLAG_BTN_PRIMARY != 0,
            barrel2: event.flags & POINTER_FLAG_BTN_SECONDARY != 0,
            x: pen_axis_value(x_norm, self.display_width),
            y: pen_axis_value(y_norm, self.display_height),
            pressure: event.pressure,
            tilt_x: tilt_radians_to_degrees(event.tilt_x),
            tilt_y: tilt_radians_to_degrees(event.tilt_y),
        };
        let frames = self.pen.frames(&sample);
        let Some(file) = self.pen_file.as_mut() else { return };
        let stamp = history_stamp(monotonic_us(), age_us, self.pen_stamp_us);
        self.pen_stamp_us = stamp;
        for frame in frames {
            for (t, c, v) in frame {
                Self::write_event_at(file, stamp, t, c, v);
            }
            Self::write_event_at(file, stamp, EV_SYN, SYN_REPORT, 0);
        }
    }

    /// GNOME: map the pen and touch devices onto the monitor at `region`
    /// through their `output` settings. Whatever cannot be mapped falls back to
    /// scaling into the desktop's bounding box.
    fn map_gnome(&mut self, region: Option<crate::display::OutputRegion>) {
        use super::gnome::{self, Kind, OutputId};
        use crate::display::mutter;
        self.unmap_gnome();
        self.region = None;
        let Some(r) = region else { return };
        let state = mutter::DisplayConfig::connect().and_then(|c| c.state());
        let (bounds, id) = match &state {
            Ok(st) => (mutter::bounds_of(st), mutter::monitor_at_region(st, r).map(OutputId::of)),
            Err(e) => {
                log::warn!("Cannot read the Mutter layout ({e}); input maps to the whole desktop");
                (None, None)
            }
        };
        if let Some(id) = &id {
            for (label, kind, pid, have, mapped) in [
                ("pen", Kind::Tablet, PEN_PRODUCT_ID, self.pen_file.is_some(), &mut self.pen_mapped),
                ("touch", Kind::Touchscreen, TOUCH_PRODUCT_ID, self.touch_file.is_some(), &mut self.touch_mapped),
            ] {
                if !have {
                    continue;
                }
                match gnome::map_output(kind, WACOM_VENDOR_ID, pid, id) {
                    Ok(()) => {
                        log::info!("GNOME: {label} mapped to {id:?}");
                        *mapped = true;
                        self.gnome_keys_set = true;
                    }
                    Err(e) => log::warn!("GNOME: could not map the {label} device: {e}"),
                }
            }
        } else if state.is_ok() {
            log::warn!("No single Mutter monitor at {r:?}; input is scaled into the desktop box");
        }
        let all_mapped = (self.pen_mapped || self.pen_file.is_none()) && (self.touch_mapped || self.touch_file.is_none());
        if !all_mapped {
            self.region = bounds.or_else(|| crate::display::layout::current().desktop_bounds()).map(|b| (r, b));
            if let Some((r, b)) = self.region {
                log::info!("Input scaled to {r:?} within desktop {b:?}");
            }
        }
    }

    /// Resets the GNOME `output` keys this injector set.
    fn unmap_gnome(&mut self) {
        use super::gnome::{self, Kind};
        if self.gnome_keys_set {
            for (kind, pid) in [(Kind::Tablet, PEN_PRODUCT_ID), (Kind::Touchscreen, TOUCH_PRODUCT_ID)] {
                if let Err(e) = gnome::clear_output(kind, WACOM_VENDOR_ID, pid) {
                    log::warn!("GNOME: could not reset the {kind:?} mapping: {e}");
                }
            }
        }
        self.gnome_keys_set = false;
        self.pen_mapped = false;
        self.touch_mapped = false;
    }

    /// `eventN` nodes of the touch and pen devices that exist.
    pub fn digitizer_event_nodes(&self) -> Vec<String> {
        [&self.touch_file, &self.pen_file].into_iter().flatten().filter_map(super::kwin::event_node).collect()
    }

    /// Binds the touch and pen devices to the KWin output at `region`.
    /// Binds the touch and pen devices to the output at `region` (all outputs for
    /// `None`). False when the output could not be found yet, so the caller retries.
    fn bind_kwin_output(&self, region: Option<crate::display::OutputRegion>) -> bool {
        let output = match region {
            None => String::new(),
            Some(r) => match crate::display::kscreen::state() {
                Ok(s) => match crate::display::kscreen::find_by_region(&s, r).or_else(|| {
                    // The region can be a moment behind the layout (a rotation or a move):
                    // fall back to the phone's output when there is exactly one.
                    let mut ours = s.outputs.iter().filter(|o| o.enabled && o.name.contains("displayswarm"));
                    match (ours.next(), ours.next()) {
                        (Some(o), None) => Some(o),
                        _ => None,
                    }
                }) {
                    Some(o) => o.name.clone(),
                    None => {
                        log::warn!("No KDE output at {r:?}; will retry, input stays on KWin's default output meanwhile");
                        return false;
                    }
                },
                Err(e) => {
                    log::warn!("Cannot read the KDE layout ({}); input stays on KWin's default output", e.summary());
                    return false;
                }
            },
        };
        for (label, file) in [("touch", &self.touch_file), ("pen", &self.pen_file)] {
            let Some(event) = file.as_ref().and_then(super::kwin::event_node) else { continue };
            match super::kwin::bind_to_output(&event, &output) {
                Ok(()) => log::info!("KWin: {label} device {event} bound to output {output:?}"),
                Err(e) => log::warn!("KWin: could not bind the {label} device to {output:?}: {e}"),
            }
            // The pen keeps its aspect ratio when the phone and the monitor
            // differ. With `inputArea` support only the matching part of the
            // phone is used (all of the monitor stays reachable); without it
            // (KWin 6.7 offers it for none of our uinput devices) the whole
            // phone covers a centred part of the monitor via `outputArea`.
            if label == "pen" {
                let (w, h) = (self.display_width, self.display_height);
                let full = super::kwin::FULL_AREA;
                let res = if super::kwin::supports_input_area(&event) {
                    let a = region.map_or(full, |r| super::kwin::keep_aspect_area(w, h, r.width, r.height));
                    super::kwin::set_input_area(&event, a)
                } else {
                    let a = region.map_or(full, |r| super::kwin::letterbox_output_area(w, h, r.width, r.height));
                    super::kwin::set_output_area(&event, a)
                };
                if let Err(e) = res {
                    log::warn!("KWin: could not set the pen area: {e}");
                }
            }
        }
        true
    }

    /// Retries a binding that failed because the output was not in the layout yet.
    fn retry_kwin_bind(&mut self) {
        let Some((region, at)) = self.kwin_pending else { return };
        if at.elapsed() < std::time::Duration::from_millis(700) {
            return;
        }
        self.kwin_pending = if self.bind_kwin_output(region) { None } else { Some((region, std::time::Instant::now())) };
    }
}

impl InputInjector for LinuxUinputInjector {
    /// GNOME: the pen and touch devices get an `output` setting naming the
    /// monitor (`map_gnome`), so their coordinates stay as they are. Whatever
    /// cannot be mapped is scaled into `region` within the desktop's bounding
    /// box, which is re-read from Mutter on every call. If the layout cannot
    /// be read the mapping is left off (whole desktop) rather than guessed.
    ///
    /// KDE: KWin maps each absolute device onto one output, so the touch and
    /// pen devices are bound to the output at `region` instead and coordinates
    /// stay as they are. `None` unbinds them (KWin's default output).
    ///
    /// X11 (any desktop): X maps each absolute device over the whole root
    /// window, so coordinates are scaled into `region` within the root window
    /// (the X11 layout backend's `desktop_bounds`), from RandR's geometry.
    fn set_output_region(&mut self, region: Option<crate::display::OutputRegion>) {
        use crate::display::model::{Desktop, SessionType};
        let desktop = match crate::display::env::current_session() {
            // KWin's and Mutter's device mapping are Wayland-only.
            SessionType::X11 => Desktop::X11Generic,
            _ => crate::display::env::current_desktop(),
        };
        match desktop {
            Desktop::Kde => {
                self.region = None;
                self.kwin_pending =
                    if self.bind_kwin_output(region) { None } else { Some((region, std::time::Instant::now())) };
            }
            Desktop::Gnome => self.map_gnome(region),
            // No compositor API to bind the devices to a monitor: scale into
            // the region within the desktop when the layout is known, else leave
            // the whole desktop.
            _ => {
                self.unmap_gnome();
                self.region = region.and_then(|r| crate::display::layout::current().desktop_bounds().map(|b| (r, b)));
            }
        }
    }

    fn inject_relative(&mut self, e: &super::RelativeEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(file) = self.mouse_file.as_mut() else {
            return Err("no virtual mouse (is /dev/uinput writable?)".into());
        };
        let fx = self.rel_frac.0 + e.dx;
        let fy = self.rel_frac.1 + e.dy;
        let (dx, dy) = (fx.trunc() as i32, fy.trunc() as i32);
        self.rel_frac = (fx - dx as f32, fy - dy as f32);
        if dx != 0 {
            Self::write_event(file, EV_REL, REL_X, dx);
        }
        if dy != 0 {
            Self::write_event(file, EV_REL, REL_Y, dy);
        }
        let b = MouseButtons::from_flags(e.buttons);
        let new_state = u8::from(b.primary) | (u8::from(b.secondary) << 1) | (u8::from(b.tertiary) << 2);
        for (index, code) in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE].into_iter().enumerate() {
            let was = (self.mouse_buttons >> index) & 1 != 0;
            let now = (new_state >> index) & 1 != 0;
            if was != now {
                Self::write_event(file, EV_KEY, code, i32::from(now));
            }
        }
        self.mouse_buttons = new_state;
        if e.wheel != 0 {
            Self::write_event(file, EV_REL, REL_WHEEL, e.wheel);
        }
        if e.hwheel != 0 {
            Self::write_event(file, EV_REL, REL_HWHEEL, e.hwheel);
        }
        Self::syn_report(file);
        Ok(())
    }

    fn inject_pen_sample(
        &mut self,
        event: &InputEvent,
        age_us: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.retry_kwin_bind();
        self.inject_pen(event, age_us);
        Ok(())
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.retry_kwin_bind();
        // A device GNOME maps to the monitor itself takes the phone's own
        // coordinates; the others are scaled into the monitor's part of the
        // desktop box.
        let (x_norm, y_norm) = self.scaled(event);
        let abs_x = (x_norm * self.display_width as f32) as i32;
        let abs_y = (y_norm * self.display_height as f32) as i32;
        let pressure_val = pressure_axis(event.pressure);

        if event.event_type == INPUT_TYPE_STYLUS {
            self.inject_pen(event, 0);
        } else if event.event_type == INPUT_TYPE_MOUSE {
            self.inject_mouse(event, abs_x, abs_y);
        } else {
            let Some(file) = self.touch_file.as_mut() else {
                return Ok(());
            };

            let slot = (event.pointer_id as usize).min(MAX_SLOTS - 1);
            Self::write_event(file, EV_ABS, ABS_MT_SLOT, slot as i32);

            match event.action {
                ACTION_DOWN | ACTION_MOVE => {
                    // ABS_MT_TRACKING_ID identifies a *new* contact. Re-sending it
                    // on every move makes the compositor see a lift-and-repress per
                    // frame, so it is only emitted when the slot is acquired.
                    if !self.active_slots[slot] {
                        Self::write_event(file, EV_ABS, ABS_MT_TRACKING_ID, event.pointer_id as i32);
                        self.active_slots[slot] = true;
                    }
                    Self::write_event(file, EV_ABS, ABS_MT_POSITION_X, abs_x);
                    Self::write_event(file, EV_ABS, ABS_MT_POSITION_Y, abs_y);
                    Self::write_event(file, EV_ABS, ABS_MT_PRESSURE, pressure_val);
                    Self::write_event(file, EV_KEY, BTN_TOUCH, 1);
                }
                ACTION_UP | ACTION_CANCEL => {
                    if self.active_slots[slot] {
                        Self::write_event(file, EV_ABS, ABS_MT_TRACKING_ID, -1);
                        self.active_slots[slot] = false;
                    }
                    // BTN_TOUCH reflects whether *any* contact is still down.
                    if !self.active_slots.iter().any(|s| *s) {
                        Self::write_event(file, EV_KEY, BTN_TOUCH, 0);
                    }
                }
                ACTION_HOVER => {}
                _ => {}
            }

            Self::syn_report(file);
        }

        Ok(())
    }

    /// Injects one key press, release or cancel.
    ///
    /// # Why modifiers are state-tracked rather than driven by the flags alone
    ///
    /// The client re-reports the *state* of all four modifiers on every single
    /// key event (`KeyEvent.isShiftPressed` and friends), not transitions. So
    /// "press every modifier whose flag is set, then release every modifier
    /// whose flag is clear" - the obvious reading - emits a Shift press *and* a
    /// Shift release for every character typed, and Shift ends up toggling once
    /// per keystroke. `pressed_modifiers` holds what the host believes is
    /// currently down and only the *difference* is emitted, which makes the
    /// injection idempotent: the same modifier state twice in a row is a no-op,
    /// whether it came from a modifier key event or from the flags.
    ///
    /// That also covers the IME case, where the client reports modifiers only in
    /// the flags and never sends a modifier key event at all.
    ///
    /// # Frame layout
    ///
    /// A press is one frame: newly-held modifiers, then the key, then
    /// `SYN_REPORT`. A release is two frames: the key up, then the modifiers
    /// that are no longer held, because the host only learns that a modifier
    /// was let go when it sees a subsequent event without the flag. The
    /// intermediate frame (key released, modifier still down) is inert - a
    /// modifier with no key produces no input - and one frame per event keeps
    /// the sequence identical to what a physical keyboard emits.
    fn inject_key(&mut self, event: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // No keyboard device (no /dev/uinput, or creation failed). The rest of
        // the host still works; the caller counts this as a failed event.
        if self.keyboard_file.is_none() {
            return Ok(());
        }

        let reported_modifier_bits =
            event.flags & (KEY_FLAG_SHIFT | KEY_FLAG_CTRL | KEY_FLAG_ALT | KEY_FLAG_META);

        let Some((key_code, extra_shift)) = resolve_key_for_event(event) else {
            // No key code and no representable text. Dropping is correct:
            // coercing the key onto some other code would type a character the
            // user never asked for, which is worse than nothing happening.
            log::debug!(
                "Android keycode {} (text {:?}) is not typeable by the host; dropping",
                event.key_code, event.text
            );
            return Ok(());
        };

        // The text-derived fallback may need Shift on top of the modifiers the
        // client reported, so the effective set is the union of the two.
        // ACTION_CANCEL is reserved by the protocol for a host releasing a key
        // it believes is held. Treated exactly like ACTION_UP.
        let pressing = event.action == ACTION_DOWN;
        let modifier_bits = effective_modifiers(reported_modifier_bits, extra_shift, pressing);
        let modifiers_changed = modifier_bits != self.pressed_modifiers;
        let releasing = event.action == ACTION_UP || event.action == ACTION_CANCEL;

        if !pressing && !releasing {
            log::debug!("Ignoring key event with unexpected action {}", event.action);
            return Ok(());
        }

        let file = self
            .keyboard_file
            .as_mut()
            .expect("keyboard_file presence checked above");

        if pressing && modifiers_changed {
            for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
                // Newly-held modifiers only; the ones already held stay down.
                if modifier_bits & flag != 0 && self.pressed_modifiers & flag == 0 {
                    if let Some(code) = modifier_key_for_flag(flag) {
                        Self::write_event(file, EV_KEY, code, 1);
                    }
                }
            }
        }

        Self::write_event(file, EV_KEY, key_code, i32::from(pressing));
        Self::syn_report(file);

        if releasing && modifiers_changed {
            let mut released_any = false;
            for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
                // Only the ones that were held and are now not. Releasing a
                // modifier the user is still holding would break the next
                // character in a chord.
                if modifier_bits & flag == 0 && self.pressed_modifiers & flag != 0 {
                    // Skip the case where the key being released *is* the
                    // modifier - its own release has already been written, and
                    // the synthetic one would be a duplicate. Harmless in
                    // evdev, but it would double-count in any tap.
                    if let Some(code) = modifier_key_for_flag(flag) {
                        if code != key_code {
                            Self::write_event(file, EV_KEY, code, 0);
                            released_any = true;
                        }
                    }
                }
            }
            if released_any {
                Self::syn_report(file);
            }
        }

        self.pressed_modifiers = modifier_bits;

        Ok(())
    }
}

impl Drop for LinuxUinputInjector {
    fn drop(&mut self) {
        self.unmap_gnome();
        for (file, label) in [
            (self.pen_file.take(), "DisplaySwarm Virtual Pen"),
            (self.touch_file.take(), "DisplaySwarm Virtual Touchscreen"),
            (self.keyboard_file.take(), "DisplaySwarm Virtual Keyboard"),
            (self.mouse_file.take(), "DisplaySwarm Virtual Mouse"),
        ] {
            if let Some(file) = file {
                unsafe {
                    libc::ioctl(file.as_raw_fd(), UI_DEV_DESTROY);
                }
                log::info!("Destroyed virtual device: {}", label);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_axes_are_sub_pixel_and_cover_the_range() {
        assert_eq!(pen_axis_max(1600), 12_799);
        assert_eq!(pen_axis_value(0.0, 1600), 0);
        assert_eq!(pen_axis_value(1.0, 1600), 12_799);
        // Two positions a fifth of a pixel apart are distinct.
        let a = pen_axis_value(0.5, 1600);
        let b = pen_axis_value(0.5 + 0.2 / 1600.0, 1600);
        assert!(b > a, "{a} vs {b}");
        assert_eq!(pen_axis_value(-1.0, 1600), 0);
        assert_eq!(pen_axis_value(2.0, 1600), 12_799);
    }

    // =======================================================================
    // Android KEYCODE_* -> Linux KEY_*
    //
    // These are the highest-value tests in the file. A wrong entry in that
    // table is not a crash and not a compile error: it types a plausible but
    // incorrect character, on every keystroke, forever. So the tests assert
    // against the kernel's own `linux/input-event-codes.h` values rather than
    // against the implementation, and include specific cases chosen to fail if
    // anyone replaces the table with arithmetic.
    // =======================================================================

    /// The entries most likely to be got wrong, asserted against the values in
    /// `linux/input-event-codes.h`.
    ///
    /// The chosen cases are deliberately the awkward ones:
    ///
    /// * `KEYCODE_A` (29) -> `KEY_A` (30) and `KEYCODE_0` (7) -> `KEY_0` (11)
    ///   are the only two where a "+1" happens to be right, which is exactly
    ///   why a wrong implementation can look correct on a smoke test.
    /// * `KEYCODE_Z` (54) -> `KEY_Z` (44) and `KEYCODE_9` (16) -> `KEY_9` (10)
    ///   are cases an offset gets backwards in *sign*.
    /// * `KEYCODE_Q` (45) -> `KEY_Q` (16) is the largest single jump, and sits
    ///   in a different ascending run from A-L, which is why the evdev
    ///   numbering is not linear in the alphabet.
    /// * `KEYCODE_DEL` -> `KEY_BACKSPACE` (not `KEY_DELETE`) and
    ///   `KEYCODE_FORWARD_DEL` -> `KEY_DELETE` is the naming trap: Android's
    ///   "DEL" is Backspace, and its forward-delete is the real Delete.
    /// * `KEYCODE_F11`/`F12` -> 87/88 rather than 69/70, because the kernel
    ///   spends 69/70 on Num/Scroll Lock.
    #[test]
    fn test_android_to_linux_keycode_known_values() {
        // (android key_code, expected Linux key code, label)
        let cases: &[(u16, u16, &str)] = &[
            // The two coincidences that make a "+1" bug survive a smoke test.
            (29, 30, "KEYCODE_A -> KEY_A"),
            (7, 11, "KEYCODE_0 -> KEY_0"),
            // Letters where the offset is wrong, in both directions.
            (30, 48, "KEYCODE_B -> KEY_B (+18, not +1)"),
            (45, 16, "KEYCODE_Q -> KEY_Q (-29, the largest jump)"),
            (54, 44, "KEYCODE_Z -> KEY_Z (-10)"),
            (31, 46, "KEYCODE_C -> KEY_C"),
            (47, 31, "KEYCODE_S -> KEY_S"),
            (51, 17, "KEYCODE_W -> KEY_W (-34, and 47 is S, not W)"),
            // Digits. Android numbers 0 first; the kernel numbers 1..9 first.
            (8, 2, "KEYCODE_1 -> KEY_1"),
            (16, 10, "KEYCODE_9 -> KEY_9"),
            (9, 3, "KEYCODE_2 -> KEY_2"),
            // Editing / navigation.
            (61, 15, "KEYCODE_TAB -> KEY_TAB"),
            (62, 57, "KEYCODE_SPACE -> KEY_SPACE"),
            (66, 28, "KEYCODE_ENTER -> KEY_ENTER"),
            (67, 14, "KEYCODE_DEL -> KEY_BACKSPACE"),
            (112, 111, "KEYCODE_FORWARD_DEL -> KEY_DELETE"),
            (111, 1, "KEYCODE_ESCAPE -> KEY_ESC"),
            (19, 103, "KEYCODE_DPAD_UP -> KEY_UP"),
            (20, 108, "KEYCODE_DPAD_DOWN -> KEY_DOWN"),
            (21, 105, "KEYCODE_DPAD_LEFT -> KEY_LEFT"),
            (22, 106, "KEYCODE_DPAD_RIGHT -> KEY_RIGHT"),
            (92, 104, "KEYCODE_PAGE_UP -> KEY_PAGEUP"),
            (93, 109, "KEYCODE_PAGE_DOWN -> KEY_PAGEDOWN"),
            (122, 102, "KEYCODE_MOVE_HOME -> KEY_HOME"),
            (123, 107, "KEYCODE_MOVE_END -> KEY_END"),
            (124, 110, "KEYCODE_INSERT -> KEY_INSERT"),
            // Modifiers, left and right kept distinct.
            (59, 42, "KEYCODE_SHIFT_LEFT -> KEY_LEFTSHIFT"),
            (60, 54, "KEYCODE_SHIFT_RIGHT -> KEY_RIGHTSHIFT"),
            (113, 29, "KEYCODE_CTRL_LEFT -> KEY_LEFTCTRL"),
            (114, 97, "KEYCODE_CTRL_RIGHT -> KEY_RIGHTCTRL"),
            (57, 56, "KEYCODE_ALT_LEFT -> KEY_LEFTALT"),
            (58, 100, "KEYCODE_ALT_RIGHT -> KEY_RIGHTALT"),
            (117, 125, "KEYCODE_META_LEFT -> KEY_LEFTMETA"),
            (118, 126, "KEYCODE_META_RIGHT -> KEY_RIGHTMETA"),
            (115, 58, "KEYCODE_CAPS_LOCK -> KEY_CAPSLOCK"),
            (143, 69, "KEYCODE_NUM_LOCK -> KEY_NUMLOCK"),
            // Function keys, including the F11/F12 gap.
            (131, 59, "KEYCODE_F1 -> KEY_F1"),
            (140, 68, "KEYCODE_F10 -> KEY_F10"),
            (141, 87, "KEYCODE_F11 -> KEY_F11 (not 69, that is NUMLOCK)"),
            (142, 88, "KEYCODE_F12 -> KEY_F12 (not 70, that is SCROLLLOCK)"),
            // Punctuation.
            (55, 51, "KEYCODE_COMMA -> KEY_COMMA"),
            (56, 52, "KEYCODE_PERIOD -> KEY_DOT"),
            (68, 41, "KEYCODE_GRAVE -> KEY_GRAVE"),
            (69, 12, "KEYCODE_MINUS -> KEY_MINUS"),
            (70, 13, "KEYCODE_EQUALS -> KEY_EQUAL"),
            (71, 26, "KEYCODE_LEFT_BRACKET -> KEY_LEFTBRACE"),
            (72, 27, "KEYCODE_RIGHT_BRACKET -> KEY_RIGHTBRACE"),
            (73, 43, "KEYCODE_BACKSLASH -> KEY_BACKSLASH"),
            (74, 39, "KEYCODE_SEMICOLON -> KEY_SEMICOLON"),
            (75, 40, "KEYCODE_APOSTROPHE -> KEY_APOSTROPHE"),
            (76, 53, "KEYCODE_SLASH -> KEY_SLASH"),
            // Numpad, whose key codes are in neither numeric nor scan order.
            (144, 82, "KEYCODE_NUMPAD_0 -> KEY_KP0"),
            (145, 79, "KEYCODE_NUMPAD_1 -> KEY_KP1"),
            (151, 71, "KEYCODE_NUMPAD_7 -> KEY_KP7"),
            (152, 72, "KEYCODE_NUMPAD_8 -> KEY_KP8"),
            (153, 73, "KEYCODE_NUMPAD_9 -> KEY_KP9"),
            (160, 96, "KEYCODE_NUMPAD_ENTER -> KEY_KPENTER"),
        ];

        for &(android, linux, label) in cases {
            assert_eq!(
                android_keycode_to_linux_keycode(android),
                Some(linux),
                "{label}: Android {android} must map to Linux {linux}"
            );
        }
    }

    /// The whole alphanumeric range, checked end to end against the kernel's
    /// own values. `test_android_to_linux_keycode_known_values` pins named
    /// anchors; this proves there is no gap or transposition in between, which
    /// is what a copy-paste slip in the table would produce.
    #[test]
    fn test_android_to_linux_keycode_full_alphanumeric() {
        // Linux key codes for a-z, straight out of input-event-codes.h.
        const LINUX_LOWERCASE: [u16; 26] = [
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, // a-m
            49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, // n-z
        ];
        // Linux key codes for 0-9, which are KEY_0 first then KEY_1..KEY_9.
        const LINUX_DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];

        // Android KEYCODE_0 is 7 and increments by one; KEYCODE_A is 29.
        for (i, &expected) in LINUX_DIGITS.iter().enumerate() {
            assert_eq!(
                android_keycode_to_linux_keycode(7 + i as u16),
                Some(expected),
                "digit {i} (Android keycode {})",
                7 + i as u16
            );
        }
        for (i, &expected) in LINUX_LOWERCASE.iter().enumerate() {
            assert_eq!(
                android_keycode_to_linux_keycode(29 + i as u16),
                Some(expected),
                "letter {} (Android keycode {})",
                (b'a' + i as u8) as char,
                29 + i as u16
            );
        }

        // And the property that makes the whole table necessary: no constant
        // offset relates the two numbering schemes. If this ever *did* hold, the
        // table could be replaced by arithmetic - and the anchors above would
        // still pass, which is precisely why this assertion exists. (Note that
        // `KEYCODE_D` -> `KEY_D` is an identity mapping, 32 -> 32, so "no
        // letter is identity-mapped" would be false; what matters is that they
        // are not *all* the same delta.)
        let deltas: std::collections::BTreeSet<i32> = (0..26)
            .map(|i| {
                android_keycode_to_linux_keycode(29 + i as u16).unwrap() as i32 - (29 + i as u16) as i32
            })
            .collect();
        assert!(
            deltas.len() > 1,
            "an offset-based mapping would have been simpler; the letter deltas are {deltas:?}"
        );
    }

    /// Unmapped keycodes must be `None`, never a guess.
    ///
    /// A `Some` here would mean the host types *something* for a key it does not
    /// understand, which is worse than dropping it. The list includes whole
    /// ranges the client legitimately sends (gamepad buttons, IME keys) plus the
    /// gaps inside the alphanumeric block, which are the most likely place for
    /// an off-by-one to leak a mapping.
    #[test]
    fn test_android_to_linux_keycode_rejects_unsupported() {
        for key_code in [
            0u16, 1, 2, 5, 6, 17, 18, 27, 28, 89, 90, 91, 94, 95, 96, 97, 200, 255, 0x0300, u16::MAX,
        ] {
            assert_eq!(
                android_keycode_to_linux_keycode(key_code),
                None,
                "Android keycode {key_code} is not supported and must not be mapped to a guess"
            );
        }

        // Every Linux key code produced must be a real one: within the range
        // evdev defines, and never one of the reserved or BTN_ codes, which
        // would collide with the pen and mouse devices' button space.
        for key_code in 0..=0x02FFu16 {
            if let Some(linux) = android_keycode_to_linux_keycode(key_code) {
                assert!(
                    linux <= 0x2FF,
                    "Android {key_code} -> Linux {linux}: out of the KEY_* range"
                );
                assert!(
                    !(0x100..=0x2FF).contains(&linux),
                    "Android {key_code} -> Linux {linux}: BTN_* codes belong to the pen/mouse"
                );
            }
        }
    }

    /// Every key code the host can produce must be *declared* on the virtual
    /// keyboard, or libinput will not route it and the keypress is lost with no
    /// error anywhere.
    ///
    /// This is the test that catches a capability list drifting away from the
    /// mapping table, which is the failure mode of a hand-kept second list.
    #[test]
    fn test_declared_keycodes_cover_every_producible_key() {
        let declared = supported_linux_keycodes();
        assert!(!declared.is_empty());

        // Sorted and deduplicated, so the ioctl loop cannot declare a code twice.
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(declared, sorted, "the declared key codes must be sorted and unique");

        for key_code in 0..=0x02FFu16 {
            if let Some(linux) = android_keycode_to_linux_keycode(key_code) {
                assert!(
                    declared.contains(&linux),
                    "Android keycode {key_code} maps to Linux {linux}, which the keyboard \
                     device does not declare"
                );
            }
        }

        // Modifiers are injected without a keycode ever mapping to them.
        for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
            let code = modifier_key_for_flag(flag).unwrap();
            assert!(
                declared.contains(&code),
                "modifier flag {flag:#04x} injects Linux {code}, which is not declared"
            );
        }

        // The whole US-layout fallback has to be declared too.
        for c in '\u{8}'..='\u{7F}' {
            if let Some((code, _)) = us_layout_key_for_char(c) {
                assert!(
                    declared.contains(&code),
                    "text fallback for {c:?} uses Linux {code}, which is not declared"
                );
            }
        }

        // Spot-check that the interesting ones really are in there. A keyboard
        // device missing letters would be created happily and then do nothing.
        for (linux, label) in [
            (30u16, "KEY_A"),
            (44, "KEY_Z"),
            (11, "KEY_0"),
            (57, "KEY_SPACE"),
            (42, "KEY_LEFTSHIFT"),
            (15, "KEY_TAB"),
        ] {
            assert!(declared.contains(&linux), "{label} ({linux}) must be declared");
        }
    }

    /// Only the four held modifiers get a key code injected.
    ///
    /// Caps Lock and Num Lock are *levels* on the wire. Injecting `KEY_CAPSLOCK`
    /// in response to them would toggle the host's lock on every key event - and
    /// in whichever direction happened to be wrong, because the host does not
    /// know the lock's current state.
    #[test]
    fn test_modifier_key_for_flag_covers_held_modifiers_only() {
        assert_eq!(modifier_key_for_flag(KEY_FLAG_SHIFT), Some(42)); // KEY_LEFTSHIFT
        assert_eq!(modifier_key_for_flag(KEY_FLAG_CTRL), Some(29)); // KEY_LEFTCTRL
        assert_eq!(modifier_key_for_flag(KEY_FLAG_ALT), Some(56)); // KEY_LEFTALT
        assert_eq!(modifier_key_for_flag(KEY_FLAG_META), Some(125)); // KEY_LEFTMETA

        for flag in [0u8, 0x10, 0x20, 0x40, 0x80, 0xFF] {
            assert_eq!(
                modifier_key_for_flag(flag),
                None,
                "flag {flag:#04x} is not a held modifier and must not inject a key"
            );
        }
    }

    // =======================================================================
    // US-layout text fallback
    // =======================================================================

    /// The fallback table, character by character, against the US layout.
    ///
    /// `(char, linux key code, needs shift)`. Every printable ASCII character
    /// the host claims it can type is listed, and a completeness sweep
    /// afterwards proves none was missed - the table is the only thing standing
    /// between an IME character and a dropped keystroke.
    #[test]
    fn test_us_layout_key_for_char() {
        let cases: &[(char, u16, bool)] = &[
            // Control characters Android actually reports.
            ('\n', 28, false),    // KEY_ENTER
            ('\r', 28, false),    // KEY_ENTER
            ('\t', 15, false),    // KEY_TAB
            ('\u{8}', 14, false), // KEY_BACKSPACE
            ('\u{1B}', 1, false), // KEY_ESC
            ('\u{7F}', 111, false), // KEY_DELETE
            // Space, digits.
            (' ', 57, false), // KEY_SPACE
            ('0', 11, false), ('1', 2, false), ('2', 3, false), ('3', 4, false),
            ('4', 5, false), ('5', 6, false), ('6', 7, false), ('7', 8, false),
            ('8', 9, false), ('9', 10, false),
            // Shifted digits.
            ('!', 2, true), ('@', 3, true), ('#', 4, true), ('$', 5, true),
            ('%', 6, true), ('^', 7, true), ('&', 8, true), ('*', 9, true),
            ('(', 10, true), (')', 11, true),
            // Unshifted punctuation.
            ('-', 12, false), ('=', 13, false), ('[', 26, false), (']', 27, false),
            ('\\', 43, false), (';', 39, false), ('\'', 40, false), ('`', 41, false),
            (',', 51, false), ('.', 52, false), ('/', 53, false),
            // Shifted punctuation.
            ('_', 12, true), ('+', 13, true), ('{', 26, true), ('}', 27, true),
            ('|', 43, true), (':', 39, true), ('"', 40, true), ('~', 41, true),
            ('<', 51, true), ('>', 52, true), ('?', 53, true),
        ];

        for &(c, code, shift) in cases {
            assert_eq!(
                us_layout_key_for_char(c),
                Some((code, shift)),
                "{c:?} should be Linux {code} with shift={shift}"
            );
        }

        // All 26 letters, both cases: same key code, differing only in Shift.
        const LETTERS: [(char, u16); 26] = [
            ('a', 30), ('b', 48), ('c', 46), ('d', 32), ('e', 18), ('f', 33),
            ('g', 34), ('h', 35), ('i', 23), ('j', 36), ('k', 37), ('l', 38),
            ('m', 50), ('n', 49), ('o', 24), ('p', 25), ('q', 16), ('r', 19),
            ('s', 31), ('t', 20), ('u', 22), ('v', 47), ('w', 17), ('x', 45),
            ('y', 21), ('z', 44),
        ];
        for (c, code) in LETTERS {
            assert_eq!(us_layout_key_for_char(c), Some((code, false)), "lowercase {c:?}");
            let upper = c.to_ascii_uppercase();
            assert_eq!(
                us_layout_key_for_char(upper),
                Some((code, true)),
                "uppercase {upper:?} is the same key as {c:?} plus Shift"
            );
        }

        // Completeness: every printable ASCII character must resolve, so nothing
        // falls through to `None` by accident.
        for b in 0x20u8..=0x7E {
            let c = b as char;
            assert!(
                us_layout_key_for_char(c).is_some(),
                "printable ASCII {c:?} ({b:#04x}) must be typeable"
            );
        }
    }

    /// The letter table, cross-checked against the *structure* of the evdev
    /// numbering rather than against itself.
    ///
    /// `lowercase_letter_to_key` is a 26-entry literal table precisely because
    /// there is no formula, but a literal table can still be transposed - `b`
    /// and `c` swapped still "looks like a mapping". This test therefore derives
    /// the expected values from the layout rule the kernel follows (rows, in
    /// scan order, with the number row sharing the first letter row's run) and
    /// compares, so a transposition fails.
    ///
    /// The derivation lives in the test rather than in the implementation on
    /// purpose: if it lived in the implementation, the implementation would be
    /// checking itself.
    #[test]
    fn test_lowercase_letter_keys_match_evdev_row_layout() {
        /// The evdev key codes are assigned in scan order: the top letter row,
        /// then the number row, then the home row, then the bottom row. Each
        /// starts 16 after the previous, because a row is 10-13 keys plus gaps.
        const ROWS: [(&str, u16); 4] = [
            ("qwertyuiop", 16), // Q=16 .. P=25
            ("1234567890", 2),  // KEY_1=2 .. KEY_0=11
            ("asdfghjkl", 30),  // A=30 .. L=38
            ("zxcvbnm", 44),    // Z=44 .. M=50
        ];

        let mut expected = std::collections::BTreeMap::new();
        for (row, base) in ROWS {
            for (i, c) in row.chars().enumerate() {
                expected.insert(c, base + i as u16);
            }
        }
        assert_eq!(expected.len(), 36, "all 26 letters and 10 digits must be covered");

        for (c, code) in expected.iter() {
            if c.is_ascii_alphabetic() {
                assert_eq!(
                    lowercase_letter_to_key(*c),
                    *code,
                    "{c:?} should be Linux {code} from its position in the QWERTY scan order"
                );
            }
        }

        // Sanity: the derived table has to disagree with the naive
        // "alphabetical index + a base" reading for most letters, otherwise this
        // cross-check is not testing anything.
        let by_alphabet = lowercase_letter_to_key('b') != 30 + 1;
        assert!(by_alphabet, "'b' must not be 31; KEY_B is 48");
    }

    /// Characters with no key on a US layout are dropped, not guessed.
    ///
    /// There is no evdev mechanism for injecting an arbitrary codepoint, so a
    /// `Some` here would mean picking some key and hoping the host's layout
    /// produced the right glyph - which types a wrong character. `None` means
    /// the screen simply does not change.
    #[test]
    fn test_us_layout_rejects_non_ascii() {
        for c in ['\u{e9}', '\u{20AC}', '\u{263A}', '\u{1F600}', '\u{4F60}', '\u{41F}'] {
            assert_eq!(
                us_layout_key_for_char(c),
                None,
                "{c:?} has no key on a US layout and must not be coerced onto one"
            );
        }
        // Control characters outside the handful Android actually reports.
        for c in ['\u{0}', '\u{1}', '\u{1A}', '\u{1C}'] {
            assert_eq!(us_layout_key_for_char(c), None, "{c:?} has no key");
        }
    }

    /// Key code first, text second - and a layout mismatch must not relabel a
    /// key.
    ///
    /// Android reports key codes by *physical position*, so a QWERTZ client
    /// pressing its `Z` sends `KEYCODE_Y` (53) with text `"z"`. Preferring the
    /// text would type `Z` on a QWERTY host; preferring the key code types `Y`,
    /// which is wrong too, but it is the same physical key the user pressed, and
    /// it is the only choice that is correct whenever the two layouts agree.
    #[test]
    fn test_resolve_key_prefers_keycode_over_text() {
        use crate::protocol::KeyEvent;

        // The common case: a supported keycode, with text agreeing.
        let event = KeyEvent::new(ACTION_DOWN, 29, "a");
        assert_eq!(resolve_key_for_event(&event), Some((30, false)), "KEY_A, no shift");

        // Text that disagrees with the keycode: the keycode wins.
        let mismatched = KeyEvent {
            event_type: 1,
            action: ACTION_DOWN,
            key_code: 53, // KEYCODE_Y, whose letter is "y" on a US layout
            scan_code: 0,
            flags: 0,
            // A QWERTZ client reports keycode 53 for its "z" key, so the text is
            // "z" while the keycode says Y. Trusting the text would silently
            // relabel the physical key.
            text: "z".to_string(),
        };
        assert_eq!(
            resolve_key_for_event(&mismatched),
            Some((21, false)),
            "KEYCODE_Y -> KEY_Y (21) regardless of the reported text"
        );

        // No keycode: the text decides, which is the whole point of the
        // fallback. KEYCODE_UNKNOWN (0) is what an IME commit reports.
        let ime = KeyEvent::new(ACTION_DOWN, 0, "Z");
        assert_eq!(
            resolve_key_for_event(&ime),
            Some((44, true)),
            "an unmapped keycode falls back to Shift + KEY_Z"
        );

        // Neither: dropped.
        assert_eq!(resolve_key_for_event(&KeyEvent::new(ACTION_DOWN, 0, "")), None);
        assert_eq!(resolve_key_for_event(&KeyEvent::new(ACTION_DOWN, 0, "\u{20AC}")), None);
    }

    // =======================================================================
    // Mouse: flags -> buttons and wheel deltas
    // =======================================================================

    /// The three button bits map left, right, middle, in that order.
    #[test]
    fn test_mouse_buttons_from_flags() {
        assert_eq!(
            MouseButtons::from_flags(POINTER_FLAG_BTN_PRIMARY),
            MouseButtons { primary: true, secondary: false, tertiary: false }
        );
        assert_eq!(
            MouseButtons::from_flags(POINTER_FLAG_BTN_SECONDARY),
            MouseButtons { primary: false, secondary: true, tertiary: false }
        );
        assert_eq!(
            MouseButtons::from_flags(POINTER_FLAG_BTN_TERTIARY),
            MouseButtons { primary: false, secondary: false, tertiary: true }
        );
        assert_eq!(
            MouseButtons::from_flags(
                POINTER_FLAG_BTN_PRIMARY | POINTER_FLAG_BTN_SECONDARY | POINTER_FLAG_BTN_TERTIARY
            ),
            MouseButtons { primary: true, secondary: true, tertiary: true }
        );
        assert_eq!(MouseButtons::from_flags(0), MouseButtons::default());

        // The pen's eraser bit (POINTER_FLAG_ERASER, 0x01) is a *different*
        // bit from BTN_PRIMARY (0x02), so it must not be read as a left click.
        // It only reaches here for a mouse event, which never sets it, but the
        // two decoders share the byte and are kept apart by `event_type`.
        assert_eq!(
            MouseButtons::from_flags(0x01),
            MouseButtons::default(),
            "the eraser bit is not a mouse button"
        );
    }

    /// The wheel sign convention, which is the part of the mouse work that can
    /// be backwards without anything looking broken.
    ///
    /// * `SCROLL_UP` -> `REL_WHEEL` **+1**: content moves towards the top.
    /// * `SCROLL_LEFT` -> `REL_HWHEEL` **-1**: the horizontal axis is inverted
    ///   relative to the vertical one.
    #[test]
    fn test_text_derived_shift_is_released_with_the_key() {
        assert_eq!(effective_modifiers(0, true, true), KEY_FLAG_SHIFT);
        assert_eq!(effective_modifiers(0, true, false), 0, "release drops the implied Shift");
        assert_eq!(effective_modifiers(KEY_FLAG_SHIFT, true, false), KEY_FLAG_SHIFT, "a held Shift stays");
        assert_eq!(effective_modifiers(KEY_FLAG_CTRL, false, true), KEY_FLAG_CTRL);
    }

    #[test]
    fn test_wheel_deltas_from_flags() {
        assert_eq!(wheel_deltas_from_flags(POINTER_FLAG_SCROLL_UP), (1, 0));
        assert_eq!(wheel_deltas_from_flags(POINTER_FLAG_SCROLL_DOWN), (-1, 0));
        assert_eq!(wheel_deltas_from_flags(POINTER_FLAG_SCROLL_LEFT), (0, -1));
        assert_eq!(wheel_deltas_from_flags(POINTER_FLAG_SCROLL_RIGHT), (0, 1));

        // No scroll bits at all -> no wheel motion, even with buttons set.
        assert_eq!(wheel_deltas_from_flags(POINTER_FLAG_BTN_PRIMARY), (0, 0));
        let all_non_scroll = !(POINTER_FLAG_SCROLL_UP
            | POINTER_FLAG_SCROLL_DOWN
            | POINTER_FLAG_SCROLL_LEFT
            | POINTER_FLAG_SCROLL_RIGHT);
        assert_eq!(wheel_deltas_from_flags(all_non_scroll), (0, 0));

        // A diagonal scroll is both axes at once, not one of them.
        assert_eq!(
            wheel_deltas_from_flags(POINTER_FLAG_SCROLL_UP | POINTER_FLAG_SCROLL_LEFT),
            (1, -1)
        );
        assert_eq!(
            wheel_deltas_from_flags(POINTER_FLAG_SCROLL_DOWN | POINTER_FLAG_SCROLL_RIGHT),
            (-1, 1)
        );

        // The two directions on an axis are exact negatives of each other. A
        // sign error anywhere in `wheel_deltas_from_flags` breaks this.
        for (positive, negative) in [
            (POINTER_FLAG_SCROLL_UP, POINTER_FLAG_SCROLL_DOWN),
            (POINTER_FLAG_SCROLL_RIGHT, POINTER_FLAG_SCROLL_LEFT),
        ] {
            let (vp, hp) = wheel_deltas_from_flags(positive);
            let (vn, hn) = wheel_deltas_from_flags(negative);
            assert_eq!(
                (vp, hp),
                (-vn, -hn),
                "{positive:#04x} must be the exact inverse of {negative:#04x}"
            );
        }
    }

    /// A scroll packet is an ordinary pointer packet that also carries a notch,
    /// so the two are independent and both survive on the same `flags` byte.
    #[test]
    fn test_scroll_and_buttons_are_independent_flag_bits() {
        let flags = POINTER_FLAG_BTN_PRIMARY | POINTER_FLAG_SCROLL_UP;
        assert!(MouseButtons::from_flags(flags).primary);
        assert_eq!(wheel_deltas_from_flags(flags), (1, 0));
    }

    /// The client-side conversion from the pen's *polar* tilt report to per-axis
    /// X/Y lean angles, mirrored here so it can be regression-tested.
    ///
    /// `MotionEvent` does not report X/Y tilt components. It reports:
    /// * `AXIS_TILT`        - **altitude**: angle between the pen and the
    ///   surface normal, in radians. 0 = held perfectly upright, PI/2 = flat on
    ///   the screen.
    /// * `AXIS_ORIENTATION` - **azimuth**: the direction the pen leans towards,
    ///   in radians, measured from +X towards +Y.
    ///
    /// The old client code forwarded altitude as `tiltX` and azimuth as
    /// `tiltY`, as if they were already the two components. They are not, and
    /// the result is nonsense in a way users notice immediately: holding the pen
    /// upright and simply rotating it in your hand (altitude 0, arbitrary
    /// azimuth) reported a 57-degree "lean" that flipped purely with the
    /// azimuth, while the direction the pen actually leans towards - the only
    /// thing a user perceives as tilt - went unreported.
    ///
    /// The correct conversion projects the lean onto the surface plane:
    ///
    ///     v = (cos(azimuth), sin(azimuth)) * tan(altitude)
    ///     tiltX = atan(cos(azimuth) * tan(altitude))
    ///     tiltY = atan(sin(azimuth) * tan(altitude))
    ///
    /// `atan` per component (rather than `altitude * cos/sin`) is what keeps
    /// each result inside (-PI/2, PI/2), so no per-axis value can ever escape
    /// the -90..90 degree range declared on `ABS_TILT_X`/`ABS_TILT_Y`.
    ///
    /// This is a *specification* of the Android-side maths, not a re-run of it:
    /// the client is Kotlin and cannot share this function. It exists so the
    /// formula and its known-good results are pinned here, and it must be kept
    /// in sync with `stylusTiltComponents` in
    /// `client-android/.../InputManager.kt`.
    fn altitude_azimuth_to_xy_tilt(altitude: f64, azimuth: f64) -> (f64, f64) {
        // `tan(PI/2)` is infinite, so the client clamps the altitude to just
        // under 90 degrees before this point. The same bound is applied here.
        const MAX_ALTITUDE: f64 = std::f64::consts::FRAC_PI_2 - 0.01;
        let a = altitude.clamp(-MAX_ALTITUDE, MAX_ALTITUDE);
        let tan_a = a.tan();
        (
            (azimuth.cos() * tan_a).atan(),
            (azimuth.sin() * tan_a).atan(),
        )
    }

    /// Known altitude/azimuth -> X/Y tilt cases, plus the degrees the host
    /// actually writes to `ABS_TILT_X` / `ABS_TILT_Y`.
    ///
    /// Cases are chosen so that a wrong formula cannot accidentally pass:
    ///   * upright pen at a non-zero azimuth must be exactly 0,0 (the old
    ///     altitude/azimuth passthrough reported ~57 degrees here)
    ///   * a pen leaning purely along an axis must report that axis at the full
    ///     altitude and the other at 0
    ///   * the diagonal must split the lean, not saturate either axis
    #[test]
    fn test_altitude_azimuth_convert_to_xy_tilt() {
        use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

        let cases: &[(f64, f64, f64, f64, &str)] = &[
            // (altitude, azimuth, expected tiltX deg, expected tiltY deg, label)
            (0.0, 1.234, 0.0, 0.0, "upright, any azimuth -> no tilt at all"),
            (0.0, -2.9, 0.0, 0.0, "upright, other azimuth -> no tilt at all"),
            (FRAC_PI_4, 0.0, 45.0, 0.0, "leaning towards +X"),
            (FRAC_PI_4, PI, -45.0, 0.0, "leaning towards -X"),
            (FRAC_PI_4, FRAC_PI_2, 0.0, 45.0, "leaning towards +Y"),
            (FRAC_PI_4, -FRAC_PI_2, 0.0, -45.0, "leaning towards -Y"),
            (
                FRAC_PI_4,
                FRAC_PI_4,
                35.264_389_682_754_654,
                35.264_389_682_754_654,
                "45 degree lean towards +X+Y splits to 35.26 on both axes",
            ),
        ];

        for &(altitude, azimuth, want_x_deg, want_y_deg, label) in cases {
            let (tilt_x, tilt_y) = altitude_azimuth_to_xy_tilt(altitude, azimuth);
            assert!(
                (tilt_x - want_x_deg.to_radians()).abs() < 1e-4,
                "{label}: tiltX was {} rad, expected {} deg",
                tilt_x,
                want_x_deg
            );
            assert!(
                (tilt_y - want_y_deg.to_radians()).abs() < 1e-4,
                "{label}: tiltY was {} rad, expected {} deg",
                tilt_y,
                want_y_deg
            );

            // And the values the uinput device ends up with after conversion.
            assert_eq!(
                tilt_radians_to_degrees(tilt_x as f32) as i32,
                want_x_deg.round() as i32,
                "{label}: ABS_TILT_X after radians->degrees"
            );
            assert_eq!(
                tilt_radians_to_degrees(tilt_y as f32) as i32,
                want_y_deg.round() as i32,
                "{label}: ABS_TILT_Y after radians->degrees"
            );
        }
    }

    /// The altitude clamp must keep a flat-on-the-screen pen from blowing up.
    /// At altitude = PI/2 `tan` is infinite, which would make the client emit
    /// NaN/inf tilt and the host would then write garbage to the axis.
    #[test]
    fn test_altitude_clamp_keeps_tilt_in_range() {
        use std::f64::consts::FRAC_PI_2;

        for altitude in [FRAC_PI_2, -FRAC_PI_2, 10.0, -10.0] {
            for azimuth in [0.0, 0.7, 2.0, 3.5, -1.0] {
                let (tilt_x, tilt_y) = altitude_azimuth_to_xy_tilt(altitude, azimuth);
                assert!(
                    tilt_x.is_finite() && tilt_y.is_finite(),
                    "altitude {altitude} azimuth {azimuth} produced non-finite tilt \
                     ({tilt_x}, {tilt_y})"
                );
                for (component, value) in [("tiltX", tilt_x), ("tiltY", tilt_y)] {
                    assert!(
                        (-89.0..89.0).contains(&value),
                        "{component} = {} deg escaped the +/-90 degree range \
                         (altitude {altitude}, azimuth {azimuth})",
                        value.to_degrees()
                    );
                }
            }
        }

        // A pen lying flat at the clamped altitude still reports a large but
        // legal tilt, and the host writes a legal degree value for it.
        let (tilt_x, _) = altitude_azimuth_to_xy_tilt(FRAC_PI_2, 0.0);
        assert!(tilt_x.to_degrees() < 90.0, "clamped lean must stay under 90 deg");
        let deg = tilt_radians_to_degrees(tilt_x as f32);
        assert!(
            (TILT_MIN_DEG..=TILT_MAX_DEG).contains(&deg),
            "host emitted {deg} deg, outside the declared axis range"
        );
    }

    /// Radians -> whole degrees for the `ABS_TILT_*` axes, including the clamp
    /// that keeps a hostile or buggy client inside the declared axis range.
    #[test]
    fn test_tilt_radians_to_degrees() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

        assert_eq!(tilt_radians_to_degrees(0.0), 0, "upright pen");
        assert_eq!(tilt_radians_to_degrees(FRAC_PI_4), 45, "+45 deg lean");
        assert_eq!(tilt_radians_to_degrees(-FRAC_PI_4), -45, "-45 deg lean");
        assert_eq!(tilt_radians_to_degrees(PI / 3.0), 60, "+60 deg lean");
        assert_eq!(tilt_radians_to_degrees(-PI / 3.0), -60, "-60 deg lean");
        assert_eq!(tilt_radians_to_degrees(FRAC_PI_2), 90, "clamped at the top");
        assert_eq!(tilt_radians_to_degrees(-FRAC_PI_2), -90, "clamped at the bottom");

        // Out-of-range and non-finite wire values must never escape the range:
        // uinput would clamp them anyway, but Windows rejects the whole
        // injection, and neither should be reachable from a single bad packet.
        for bad in [10.0f32, -10.0, 1e30, -1e30] {
            assert_eq!(
                tilt_radians_to_degrees(bad),
                if bad.is_sign_positive() { 90 } else { -90 },
                "{bad} must clamp to the axis limit"
            );
        }
        // An infinite tilt is a client bug, but clamping to the axis limit beats
        // reporting the pen as perfectly upright.
        assert_eq!(tilt_radians_to_degrees(f32::INFINITY), 90);
        assert_eq!(tilt_radians_to_degrees(f32::NEG_INFINITY), -90);
        // NaN is meaningless in any direction: report no tilt.
        assert_eq!(tilt_radians_to_degrees(f32::NAN), 0);
    }

    // ---- pen state machine ------------------------------------------------

    fn ps(action: u8, eraser: bool, pressure: f32) -> PenSample {
        PenSample { action, eraser, barrel1: false, barrel2: false, x: 10, y: 20, pressure, tilt_x: 0, tilt_y: 0 }
    }

    fn has(frame: &[Ev], code: u16, value: i32) -> bool {
        frame.iter().any(|&(_, c, v)| c == code && v == value)
    }

    fn keys(frame: &[Ev]) -> Vec<(u16, i32)> {
        frame.iter().filter(|e| e.0 == EV_KEY).map(|e| (e.1, e.2)).collect()
    }

    #[test]
    fn switching_tools_leaves_proximity_before_the_new_tool_enters() {
        let mut t = PenTracker::default();
        t.frames(&ps(ACTION_HOVER, false, 0.0));
        let f = t.frames(&ps(ACTION_HOVER, true, 0.0));
        assert_eq!(f.len(), 2, "one frame out, one frame in");
        assert_eq!(keys(&f[0]), [(BTN_TOOL_PEN, 0)]);
        assert!(!f[0].iter().any(|e| e.1 == BTN_TOOL_RUBBER));
        assert!(has(&f[1], BTN_TOOL_RUBBER, 1) && !has(&f[1], BTN_TOOL_PEN, 0));
    }

    #[test]
    fn switching_tools_mid_stroke_lifts_the_tip_and_buttons_first() {
        let mut t = PenTracker::default();
        let mut s = ps(ACTION_MOVE, false, 0.5);
        s.barrel1 = true;
        t.frames(&s);
        let f = t.frames(&ps(ACTION_MOVE, true, 0.5));
        assert!(has(&f[0], BTN_TOUCH, 0) && has(&f[0], BTN_STYLUS, 0) && has(&f[0], BTN_TOOL_PEN, 0));
        assert!(has(&f[1], BTN_TOOL_RUBBER, 1) && has(&f[1], BTN_TOUCH, 1));
    }

    #[test]
    fn hover_is_in_proximity_and_never_touching() {
        let mut t = PenTracker::default();
        t.frames(&ps(ACTION_MOVE, false, 0.6));
        let f = t.frames(&ps(ACTION_HOVER, false, 0.6));
        assert_eq!(f.len(), 1);
        assert!(has(&f[0], BTN_TOUCH, 0), "hover clears BTN_TOUCH");
        assert!(has(&f[0], ABS_PRESSURE, 0));
        assert!(!has(&f[0], BTN_TOOL_PEN, 0), "still in proximity");
        // First hover enters proximity without a touch.
        let mut t = PenTracker::default();
        let f = t.frames(&ps(ACTION_HOVER, false, 0.0));
        assert!(has(&f[0], BTN_TOOL_PEN, 1) && !f[0].iter().any(|e| e.1 == BTN_TOUCH));
    }

    #[test]
    fn contact_lift_stays_in_range_and_hover_exit_leaves() {
        let mut t = PenTracker::default();
        t.frames(&ps(ACTION_DOWN, false, 0.5));
        let up = t.frames(&ps(ACTION_UP, false, 0.0));
        assert!(has(&up[0], BTN_TOUCH, 0) && !has(&up[0], BTN_TOOL_PEN, 0));
        t.frames(&ps(ACTION_HOVER, false, 0.0));
        let exit = t.frames(&ps(ACTION_UP, false, 0.0));
        assert!(has(&exit[0], BTN_TOOL_PEN, 0));
        // Leaving twice writes nothing further.
        let again = t.frames(&ps(ACTION_UP, false, 0.0));
        assert!(!again[0].iter().any(|e| e.0 == EV_KEY));
    }

    #[test]
    fn cancel_releases_everything_and_leaves() {
        let mut t = PenTracker::default();
        let mut s = ps(ACTION_MOVE, false, 0.5);
        s.barrel2 = true;
        t.frames(&s);
        let f = t.frames(&ps(ACTION_CANCEL, false, 0.5));
        assert!(has(&f[0], BTN_TOUCH, 0) && has(&f[0], BTN_STYLUS2, 0) && has(&f[0], BTN_TOOL_PEN, 0));
        assert!(has(&f[0], ABS_PRESSURE, 0));
    }

    #[test]
    fn light_pressure_does_not_flicker_touch() {
        let mut t = PenTracker::default();
        let mut touches = Vec::new();
        // Pressure wobbling around the on-threshold.
        for p in [0.0, 0.02, 0.012, 0.016, 0.009, 0.014, 0.008, 0.003] {
            for f in t.frames(&ps(ACTION_MOVE, false, p)) {
                touches.extend(f.iter().filter(|e| e.1 == BTN_TOUCH).map(|e| e.2));
            }
        }
        assert_eq!(touches, [1, 0], "one press and one release");
    }

    #[test]
    fn touch_needs_the_on_threshold_and_pressure_follows_it() {
        let mut t = PenTracker::default();
        let f = t.frames(&ps(ACTION_DOWN, false, 0.01));
        assert!(!f[0].iter().any(|e| e.1 == BTN_TOUCH) && has(&f[0], ABS_PRESSURE, 0));
        let f = t.frames(&ps(ACTION_MOVE, false, 0.5));
        assert!(has(&f[0], BTN_TOUCH, 1) && has(&f[0], ABS_PRESSURE, PRESSURE_MAX / 2));
    }

    #[test]
    fn pressure_is_clamped_to_the_declared_axis() {
        assert_eq!(pressure_axis(1.0), PRESSURE_MAX);
        assert_eq!(pressure_axis(7.0), PRESSURE_MAX);
        assert_eq!(pressure_axis(-1.0), 0);
        assert_eq!(pressure_axis(f32::NAN), 0);
        let mut t = PenTracker::default();
        let f = t.frames(&ps(ACTION_MOVE, false, 9.0));
        assert!(has(&f[0], ABS_PRESSURE, PRESSURE_MAX));
    }

    #[test]
    fn barrel_buttons_change_only_on_transitions() {
        let mut t = PenTracker::default();
        let mut s = ps(ACTION_HOVER, false, 0.0);
        s.barrel1 = true;
        assert!(has(&t.frames(&s)[0], BTN_STYLUS, 1));
        assert!(!t.frames(&s)[0].iter().any(|e| e.1 == BTN_STYLUS));
        s.barrel1 = false;
        assert!(has(&t.frames(&s)[0], BTN_STYLUS, 0));
    }

    #[test]
    fn history_stamps_are_ordered_and_never_in_the_future() {
        let now = 10_000_000;
        assert_eq!(history_stamp(now, 0, 0), now);
        assert_eq!(history_stamp(now, 4_000, 0), now - 4_000);
        // Older than the previous event: clamped up to it.
        assert_eq!(history_stamp(now, 50_000, now - 8_000), now - 8_000);
        // A stale previous stamp cannot push past now.
        assert_eq!(history_stamp(now, 0, now + 5), now);
    }
}
