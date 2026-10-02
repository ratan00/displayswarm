//! Windows input injection.
//!
//! * Pen: `InjectSyntheticPointerInput` on a `PT_PEN` device (pressure, tilt,
//!   eraser, barrel button, hover).
//! * Touch: a `PT_TOUCH` device with up to 10 contacts; every frame carries all
//!   fingers that are down.
//! * Mouse (`INPUT_TYPE_MOUSE`): `SendInput` absolute moves over the virtual
//!   desktop, buttons and wheel.
//! * Keyboard: `SendInput` by scan code.
//!
//! What to inject is decided in [`super::windows_map`] (unit-tested on Linux);
//! this file only turns those decisions into Win32 structures. Pointer
//! positions are virtual-screen pixels, so the process must be per-monitor DPI
//! aware (done in `new`). Synthetic pointer injection needs the process to run
//! interactively; injecting into elevated windows needs the host to be elevated
//! too (UIPI).

use super::windows_map as map;
use super::InputInjector;
use crate::display::OutputRegion;
use crate::protocol::{
    InputEvent, KeyEvent, ACTION_CANCEL, ACTION_DOWN, ACTION_UP, INPUT_TYPE_MOUSE, INPUT_TYPE_STYLUS,
    KEY_FLAG_ALT, KEY_FLAG_CTRL, KEY_FLAG_META, KEY_FLAG_SHIFT,
};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::POINT;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Controls::*;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::*;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::Pointer::*;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::*;

// ===========================================================================
// KEYBOARD
// ===========================================================================
//
// Type-checked with `cargo check --target x86_64-pc-windows-msvc`; never run
// on a real Windows machine yet.
//
//
// WHY SCAN CODES RATHER THAN VIRTUAL KEYS
//
// `SendInput` can inject a keystroke either by virtual key (`wVk`, which is what
// the keyboard layout turns into a character) or by scan code (`wScan` with
// `KEYEVENTF_SCANCODE`, which is the physical key position). Layout
// independence is the reason to prefer scan codes: with `wVk`, the *host's*
// active layout decides what character comes out, so a user on a German layout
// who is sharing their screen with someone typing on an Android QWERTY keyboard
// gets the wrong characters. A scan code is the physical key, and the
// destination layout applies its own mapping to it, which is what the user
// expects.
//
// The scan code is obtained from the OS via `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)`
// rather than hardcoded, so there is exactly one hand-maintained table here (the
// Android keycode -> virtual key mapping) instead of two that could disagree.
// `MAPVK_VK_TO_VSC` is the US/IBM PC set-1 position and is stable across
// layouts for every key this maps.
//
// `KEYEVENTF_EXTENDEDKEY` is required for the keys the PC marks with the E0
// prefix, and omitting it makes the wrong key appear: the arrows become the
// numpad, Insert becomes the keypad's 0, and Delete becomes the keypad's
// decimal point. The list is in `is_extended_scan_code`.

/// Android `KeyEvent.KEYCODE_*` -> a Windows virtual key.
///
/// A `match` rather than arithmetic, for the same reason as the Linux table in
/// `linux.rs` and with the same care: the two numbering schemes are unrelated.
/// Windows virtual keys *are* the ASCII code points for the alphanumeric range,
/// which makes it tempting to write `key_code as u16` somewhere - and that is
/// wrong for every key here (Android `KEYCODE_A` is 29, `VK_A` is 65). It is
/// spelled out.
#[cfg(target_os = "windows")]
fn android_keycode_to_vk(key_code: u16) -> Option<VIRTUAL_KEY> {
    Some(match key_code {
        // Letters: Android A..Z is 29..54; VK_A..VK_Z is 65..90.
        29 => VK_A, 30 => VK_B, 31 => VK_C, 32 => VK_D, 33 => VK_E, 34 => VK_F,
        35 => VK_G, 36 => VK_H, 37 => VK_I, 38 => VK_J, 39 => VK_K, 40 => VK_L,
        41 => VK_M, 42 => VK_N, 43 => VK_O, 44 => VK_P, 45 => VK_Q, 46 => VK_R,
        47 => VK_S, 48 => VK_T, 49 => VK_U, 50 => VK_V, 51 => VK_W, 52 => VK_X,
        53 => VK_Y, 54 => VK_Z,

        // Digits: Android 0..9 is 7..16; VK_0..VK_9 is 48..57.
        7 => VK_0, 8 => VK_1, 9 => VK_2, 10 => VK_3, 11 => VK_4,
        12 => VK_5, 13 => VK_6, 14 => VK_7, 15 => VK_8, 16 => VK_9,

        // Editing / navigation.
        61 => VK_TAB,          // KEYCODE_TAB
        62 => VK_SPACE,        // KEYCODE_SPACE
        66 => VK_RETURN,       // KEYCODE_ENTER
        67 => VK_BACK,         // KEYCODE_DEL is Backspace on Android too
        112 => VK_DELETE,      // KEYCODE_FORWARD_DEL
        111 => VK_ESCAPE,      // KEYCODE_ESCAPE
        19 => VK_UP,           // KEYCODE_DPAD_UP
        20 => VK_DOWN,         // KEYCODE_DPAD_DOWN
        21 => VK_LEFT,         // KEYCODE_DPAD_LEFT
        22 => VK_RIGHT,        // KEYCODE_DPAD_RIGHT
        23 => VK_RETURN,       // KEYCODE_DPAD_CENTER is Enter
        92 => VK_PRIOR,        // KEYCODE_PAGE_UP
        93 => VK_NEXT,         // KEYCODE_PAGE_DOWN
        122 => VK_HOME,        // KEYCODE_MOVE_HOME
        123 => VK_END,         // KEYCODE_MOVE_END
        124 => VK_INSERT,      // KEYCODE_INSERT
        4 => VK_BROWSER_BACK,  // KEYCODE_BACK
        125 => VK_BROWSER_FORWARD, // KEYCODE_FORWARD
        3 => VK_BROWSER_HOME,  // KEYCODE_HOME
        81 => VK_BROWSER_SEARCH, // KEYCODE_SEARCH
        79 => VK_APPS,         // KEYCODE_MENU -> context-menu key

        // Modifiers, left/right kept distinct.
        59 => VK_LSHIFT,   // KEYCODE_SHIFT_LEFT
        60 => VK_RSHIFT,   // KEYCODE_SHIFT_RIGHT
        113 => VK_LCONTROL, // KEYCODE_CTRL_LEFT
        114 => VK_RCONTROL, // KEYCODE_CTRL_RIGHT
        57 => VK_LMENU,    // KEYCODE_ALT_LEFT
        58 => VK_RMENU,    // KEYCODE_ALT_RIGHT
        117 => VK_LWIN,    // KEYCODE_META_LEFT
        118 => VK_RWIN,    // KEYCODE_META_RIGHT
        115 => VK_CAPITAL, // KEYCODE_CAPS_LOCK
        143 => VK_NUMLOCK, // KEYCODE_NUM_LOCK

        // Function keys. Windows numbers F1..F12 contiguously at 112..123, so
        // this range *is* linear - unlike the Linux table.
        131 => VK_F1, 132 => VK_F2, 133 => VK_F3, 134 => VK_F4,
        135 => VK_F5, 136 => VK_F6, 137 => VK_F7, 138 => VK_F8,
        139 => VK_F9, 140 => VK_F10, 141 => VK_F11, 142 => VK_F12,

        // Punctuation, on the US layout. The OEM virtual keys are the only keys
        // whose physical position depends on the layout, which is exactly why
        // the scan-code path above matters for them.
        55 => VK_OEM_COMMA,  // KEYCODE_COMMA
        56 => VK_OEM_PERIOD, // KEYCODE_PERIOD
        68 => VK_OEM_3,      // KEYCODE_GRAVE
        69 => VK_OEM_MINUS,  // KEYCODE_MINUS
        70 => VK_OEM_PLUS,   // KEYCODE_EQUALS
        71 => VK_OEM_4,      // KEYCODE_LEFT_BRACKET
        72 => VK_OEM_6,      // KEYCODE_RIGHT_BRACKET
        73 => VK_OEM_5,      // KEYCODE_BACKSLASH
        74 => VK_OEM_1,      // KEYCODE_SEMICOLON
        75 => VK_OEM_7,      // KEYCODE_APOSTROPHE
        76 => VK_OEM_2,      // KEYCODE_SLASH

        // Numpad, contiguous at 96..105 for the digits plus the operators.
        144 => VK_NUMPAD0, 145 => VK_NUMPAD1, 146 => VK_NUMPAD2, 147 => VK_NUMPAD3,
        148 => VK_NUMPAD4, 149 => VK_NUMPAD5, 150 => VK_NUMPAD6, 151 => VK_NUMPAD7,
        152 => VK_NUMPAD8, 153 => VK_NUMPAD9,
        154 => VK_DIVIDE, 155 => VK_MULTIPLY, 156 => VK_SUBTRACT, 157 => VK_ADD,
        158 => VK_DECIMAL, 160 => VK_RETURN, // numpad Enter shares VK_RETURN

        // Audio / media / system.
        24 => VK_VOLUME_UP,   // KEYCODE_VOLUME_UP
        25 => VK_VOLUME_DOWN, // KEYCODE_VOLUME_DOWN
        164 => VK_VOLUME_MUTE, // KEYCODE_VOLUME_MUTE
        88 => VK_VOLUME_MUTE, // KEYCODE_MUTE
        82 => VK_MEDIA_PLAY_PAUSE, // KEYCODE_MEDIA_PLAY_PAUSE
        83 => VK_MEDIA_STOP,  // KEYCODE_MEDIA_STOP
        84 => VK_MEDIA_NEXT_TRACK, // KEYCODE_MEDIA_NEXT
        85 => VK_MEDIA_PREV_TRACK, // KEYCODE_MEDIA_PREVIOUS
        // Rewind and fast-forward have no named constant in the `windows` crate
        // at this version, so they are written out. VK_POWER, VK_MEDIA_REWIND
        // and VK_MEDIA_FAST_FORWARD are 0x5E, 0xAA and 0xA9 in the Windows SDK's
        // `winuser.h`; the numeric values are the SDK's, not guesses.
        86 => VIRTUAL_KEY(0xAA), // KEYCODE_MEDIA_REWIND
        87 => VIRTUAL_KEY(0xA9), // KEYCODE_MEDIA_FAST_FORWARD
        26 => VIRTUAL_KEY(0x5E), // KEYCODE_POWER

        _ => return None,
    })
}

/// Virtual keys whose PC scan code is E0-prefixed and therefore need
/// `KEYEVENTF_EXTENDEDKEY`.
///
/// Getting this list wrong is silent and confusing: without the flag Windows
/// resolves the scan code against the *main* block, so the arrow keys move the
/// numpad and Insert/Delete/Home/End/PageUp/PageDown become keypad digits.
#[cfg(target_os = "windows")]
fn is_extended_scan_code(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        // Arrows.
        VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT |
        // Navigation block.
        VK_INSERT | VK_DELETE | VK_HOME | VK_END | VK_PRIOR | VK_NEXT |
        // Right-hand modifiers, which are E0 on a PC keyboard.
        VK_RCONTROL | VK_RMENU |
        // Keypad operators and Enter.
        VK_DIVIDE | VK_RETURN |
        // Application / browser keys.
        VK_APPS | VK_BROWSER_BACK | VK_BROWSER_FORWARD | VK_BROWSER_HOME | VK_BROWSER_SEARCH
    )
}

/// Injects one keyboard event by scan code.
///
/// One `INPUT` per call rather than a batch, deliberately: the caller needs to
/// interleave modifier presses and releases around it, and a single `INPUT` is
/// the smallest unit that cannot be got subtly wrong.
///
/// `wVk` is left at 0, which is required - Windows ignores it when
/// `KEYEVENTF_SCANCODE` is set, but a stale non-zero value there has been
/// observed to be interpreted on some systems.
///
/// Returns `false` if the injection failed. `SendInput` reports failure two
/// ways and both matter: a 0 return means the call was blocked (typically UIPI,
/// i.e. the target window runs at a higher integrity level, or the session is
/// locked), and a return value *below* the number of events sent means it was
/// partially blocked, which for a one-event call is the same thing.
#[cfg(target_os = "windows")]
fn send_scan_code(vk: VIRTUAL_KEY, key_up: bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    };

    let mut flags = KEYEVENTF_SCANCODE;
    if is_extended_scan_code(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }

    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    if scan == 0 {
        // No physical key for this virtual key (the browser and media keys
        // have none), so there is no scan code to send: inject the virtual key
        // itself, which is what those keys are.
        log::debug!("No PC scan code for virtual key {:?}; sending it as a virtual key", vk.0);
        return send_virtual_key(vk, key_up);
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0), // ignored when KEYEVENTF_SCANCODE is set
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent != 1 {
        log::debug!("SendInput injected {sent}/1 keyboard events (blocked by UIPI?)");
        return false;
    }
    true
}

/// Injects one virtual key by virtual key code.
///
/// Used only for modifiers, which are driven from the `KEY_FLAG_*` *state* bits
/// rather than from a key press of their own. Mixing the two in one event is
/// allowed by `SendInput`, and keeping modifiers on the virtual-key path means
/// the chord does not depend on the host having a scan code for
/// `VK_LSHIFT` (it does, but there is no reason to depend on it).
#[cfg(target_os = "windows")]
fn send_virtual_key(vk: VIRTUAL_KEY, key_up: bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT_0, INPUT_KEYBOARD, KEYBDINPUT};

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 }
}

/// The virtual key each `KEY_FLAG_*` modifier bit is injected as, or `None` if
/// the bit is not a held modifier.
///
/// Caps Lock and Num Lock are excluded, exactly as on the Linux path: they are
/// *latch state* on the wire, not held keys, and toggling the host's lock in
/// response to a level would flip it on every key event. The real
/// `KEYCODE_CAPS_LOCK` press still reaches the host through the keycode path.
#[cfg(target_os = "windows")]
fn modifier_vk_for_flag(flag: u8) -> Option<VIRTUAL_KEY> {
    Some(match flag {
        KEY_FLAG_SHIFT => VK_LSHIFT,
        KEY_FLAG_CTRL => VK_LCONTROL,
        KEY_FLAG_ALT => VK_LMENU,
        KEY_FLAG_META => VK_LWIN,
        _ => return None,
    })
}

/// The modifier bits this injector tracks, as a `KEY_FLAG_*` mask.
#[cfg(target_os = "windows")]
const MODIFIER_BITS: u8 = KEY_FLAG_SHIFT | KEY_FLAG_CTRL | KEY_FLAG_ALT | KEY_FLAG_META;

/// Bounding box of all monitors (`SM_*VIRTUALSCREEN`).
#[cfg(target_os = "windows")]
fn virtual_screen() -> OutputRegion {
    unsafe {
        OutputRegion {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn virtual_screen() -> OutputRegion {
    OutputRegion { x: 0, y: 0, width: 1, height: 1 }
}

/// Dispatches client input through the Windows Synthetic Pointer API and `SendInput`.
pub struct WindowsPointerInjector {
    display_width: u32,
    display_height: u32,
    /// Monitor rectangle (virtual-screen pixels) that pointer input maps onto.
    region: Option<OutputRegion>,
    #[cfg(target_os = "windows")]
    pen_device: Option<HSYNTHETICPOINTERDEVICE>,
    #[cfg(target_os = "windows")]
    touch_device: Option<HSYNTHETICPOINTERDEVICE>,
    pen: map::PenState,
    touch: map::TouchTracker,
    /// Mouse buttons held, as `POINTER_FLAG_BTN_*` bits.
    mouse_buttons: u8,
    /// Sub-pixel remainder of relative (touchpad) motion.
    rel_frac: (f32, f32),
    /// The modifier state currently held down, as a `KEY_FLAG_*` mask. The
    /// client re-sends the state of all four modifiers on every key event.
    pressed_modifiers: u8,
}

// The synthetic device handles are plain kernel handles used from one thread at a time.
#[cfg(target_os = "windows")]
unsafe impl Send for WindowsPointerInjector {}

impl WindowsPointerInjector {
    pub fn new(display_width: u32, display_height: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        log::info!("Initialized Windows input injector (fallback size {}x{})", display_width, display_height);
        #[cfg(target_os = "windows")]
        crate::capture::windows::sys::ensure_dpi_aware();

        #[cfg(target_os = "windows")]
        let (pen_device, touch_device) = {
            // One synthetic device per pointer type; a PT_PEN device cannot carry touch.
            let pen = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) };
            let touch = unsafe { CreateSyntheticPointerDevice(PT_TOUCH, map::MAX_TOUCH_CONTACTS, POINTER_FEEDBACK_DEFAULT) };
            if pen.is_err() {
                log::warn!("CreateSyntheticPointerDevice(PT_PEN) failed: pen input will be dropped");
            }
            if touch.is_err() {
                log::warn!("CreateSyntheticPointerDevice(PT_TOUCH) failed: touch input will be dropped");
            }
            (pen.ok(), touch.ok())
        };

        Ok(Self {
            display_width,
            display_height,
            region: None,
            #[cfg(target_os = "windows")]
            pen_device,
            #[cfg(target_os = "windows")]
            touch_device,
            pen: map::PenState::default(),
            touch: map::TouchTracker::default(),
            mouse_buttons: 0,
            rel_frac: (0.0, 0.0),
            pressed_modifiers: 0,
        })
    }

    /// Where the event lands, in virtual-screen pixels. With no region set it
    /// spans the whole virtual screen (the encoder size is only a fallback for
    /// when the metrics are unavailable).
    fn screen_point(&self, e: &InputEvent) -> (i32, i32) {
        let vs = virtual_screen();
        let fallback = if vs.width > 1 || vs.height > 1 {
            vs
        } else {
            OutputRegion { x: 0, y: 0, width: self.display_width, height: self.display_height }
        };
        map::map_to_screen(e.x_norm, e.y_norm, self.region, fallback)
    }
}

#[cfg(target_os = "windows")]
fn send_mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 }
}

impl InputInjector for WindowsPointerInjector {
    fn set_output_region(&mut self, region: Option<OutputRegion>) {
        self.region = region;
    }

    /// Touchpad mode: a relative `SendInput` move (fractions carried), button
    /// transitions and wheel notches, without warping the cursor.
    fn inject_relative(&mut self, e: &super::RelativeEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(target_os = "windows")]
        {
            let (dx, dy) = map::take_relative(&mut self.rel_frac, e.dx, e.dy);
            let mut ok = true;
            if dx != 0 || dy != 0 {
                ok &= send_mouse(MOUSEEVENTF_MOVE, dx, dy, 0);
            }
            ok &= self.send_button_changes(e.buttons);
            if e.wheel != 0 {
                ok &= send_mouse(MOUSEEVENTF_WHEEL, 0, 0, e.wheel * map::WHEEL_DELTA);
            }
            if e.hwheel != 0 {
                ok &= send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, e.hwheel * map::WHEEL_DELTA);
            }
            return if ok { Ok(()) } else { Err("SendInput was blocked or failed for a relative mouse event".into()) };
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = e;
            Ok(())
        }
    }

    /// Ctrl + wheel, as desktop applications zoom.
    fn inject_zoom(&mut self, steps: i32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(target_os = "windows")]
        {
            let ctrl_held = self.pressed_modifiers & KEY_FLAG_CTRL != 0;
            if !ctrl_held {
                send_virtual_key(VK_LCONTROL, false);
            }
            let ok = send_mouse(MOUSEEVENTF_WHEEL, 0, 0, steps.saturating_mul(map::WHEEL_DELTA));
            if !ctrl_held {
                send_virtual_key(VK_LCONTROL, true);
            }
            return if ok { Ok(()) } else { Err("SendInput was blocked or failed for a zoom".into()) };
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = steps;
            Ok(())
        }
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (x, y) = self.screen_point(event);

        #[cfg(target_os = "windows")]
        {
            if event.event_type == INPUT_TYPE_MOUSE {
                return self.inject_mouse(event, x, y);
            }
            // Synthetic pointers are positioned relative to the virtual screen's origin.
            let (px, py) = map::injection_point(x, y, virtual_screen());
            let ok = if event.event_type == INPUT_TYPE_STYLUS {
                self.inject_pen(event, px, py)
            } else {
                self.inject_touch(event, px, py)
            };
            return if ok { Ok(()) } else { Err("InjectSyntheticPointerInput failed".into()) };
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = INPUT_TYPE_MOUSE;
            log::trace!("Windows pointer input: ({x}, {y}) pressure={:.2}", event.pressure);
            Ok(())
        }
    }

    /// Injects one key press, release or cancel by scan code (layout independent);
    /// modifier state comes from the `KEY_FLAG_*` bits. Unmapped keys are dropped.
    fn inject_key(&mut self, event: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(target_os = "windows")]
        {
            let modifier_bits = event.flags & MODIFIER_BITS;

            let Some(vk) = android_keycode_to_vk(event.key_code) else {
                log::debug!("Android keycode {} is not mapped to a Windows virtual key; dropping", event.key_code);
                return Ok(());
            };

            let pressing = event.action == ACTION_DOWN;
            let releasing = event.action == ACTION_UP || event.action == ACTION_CANCEL;
            if !pressing && !releasing {
                log::debug!("Ignoring key event with unexpected action {}", event.action);
                return Ok(());
            }

            // Modifiers that came into being since the last event go down first.
            for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
                if modifier_bits & flag != 0 && self.pressed_modifiers & flag == 0 {
                    if let Some(v) = modifier_vk_for_flag(flag) {
                        send_virtual_key(v, false);
                    }
                }
            }

            // A failed SendInput is most often UIPI (a higher-integrity window is focused).
            if !send_scan_code(vk, !pressing) {
                self.pressed_modifiers = modifier_bits;
                return Err("SendInput was blocked or failed for a key event".into());
            }

            // Modifiers no longer held are released after the key.
            for flag in [KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META] {
                if modifier_bits & flag == 0 && self.pressed_modifiers & flag != 0 {
                    if let Some(v) = modifier_vk_for_flag(flag) {
                        send_virtual_key(v, true);
                    }
                }
            }

            self.pressed_modifiers = modifier_bits;
            return Ok(());
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = (KEY_FLAG_SHIFT, KEY_FLAG_CTRL, KEY_FLAG_ALT, KEY_FLAG_META, ACTION_UP);
            log::trace!("Windows key input: action {}, key_code {:#06x}", event.action, event.key_code);
            self.pressed_modifiers = event.flags;
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
impl WindowsPointerInjector {
    fn inject_pen(&mut self, e: &InputEvent, x: i32, y: i32) -> bool {
        let Some(dev) = self.pen_device else { return false };
        let f = self.pen.frame(e);
        let info = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: e.pointer_id as u32,
                ptPixelLocation: POINT { x, y },
                pointerFlags: POINTER_FLAGS(f.pointer_flags),
                ..Default::default()
            },
            penFlags: f.pen_flags,
            penMask: f.pen_mask,
            pressure: f.pressure,
            tiltX: f.tilt_x,
            tiltY: f.tilt_y,
            ..Default::default()
        };
        let frame = [POINTER_TYPE_INFO { r#type: PT_PEN, Anonymous: POINTER_TYPE_INFO_0 { penInfo: info } }];
        unsafe { InjectSyntheticPointerInput(dev, &frame) }.is_ok()
    }

    fn inject_touch(&mut self, e: &InputEvent, x: i32, y: i32) -> bool {
        let Some(dev) = self.touch_device else { return false };
        let Some(contacts) = self.touch.frame(e, x, y) else { return true };
        let frame: Vec<POINTER_TYPE_INFO> = contacts
            .iter()
            .map(|c| POINTER_TYPE_INFO {
                r#type: PT_TOUCH,
                Anonymous: POINTER_TYPE_INFO_0 {
                    touchInfo: POINTER_TOUCH_INFO {
                        pointerInfo: POINTER_INFO {
                            pointerType: PT_TOUCH,
                            pointerId: c.id,
                            ptPixelLocation: POINT { x: c.x, y: c.y },
                            pointerFlags: POINTER_FLAGS(c.flags),
                            ..Default::default()
                        },
                        touchMask: map::TOUCH_MASK_PRESSURE,
                        pressure: c.pressure,
                        ..Default::default()
                    },
                },
            })
            .collect();
        unsafe { InjectSyntheticPointerInput(dev, &frame) }.is_ok()
    }

    /// Presses and releases whatever differs between the buttons held so far
    /// and `held` (`POINTER_FLAG_BTN_*` bits), releases first.
    fn send_button_changes(&mut self, held: u8) -> bool {
        let mut ok = true;
        for (button, down) in map::mouse_transitions(self.mouse_buttons, held) {
            let flag = match (button, down) {
                (map::MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (map::MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (map::MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (map::MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (map::MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (map::MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            ok &= send_mouse(flag, 0, 0, 0);
        }
        self.mouse_buttons = held;
        ok
    }

    fn inject_mouse(&mut self, e: &InputEvent, x: i32, y: i32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (ax, ay) = map::to_absolute(x, y, virtual_screen());
        let mut ok = send_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, ax, ay, 0);
        let held = e.flags & (crate::protocol::POINTER_FLAG_BTN_PRIMARY
            | crate::protocol::POINTER_FLAG_BTN_SECONDARY
            | crate::protocol::POINTER_FLAG_BTN_TERTIARY);
        // An UP/CANCEL means nothing is held whatever the flags say.
        let held = if e.action == ACTION_UP || e.action == ACTION_CANCEL { 0 } else { held };
        ok &= self.send_button_changes(held);
        let (v, h) = map::scroll_deltas(e.flags);
        if v != 0 {
            ok &= send_mouse(MOUSEEVENTF_WHEEL, 0, 0, v);
        }
        if h != 0 {
            ok &= send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, h);
        }
        if ok {
            Ok(())
        } else {
            Err("SendInput was blocked or failed for a mouse event".into())
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsPointerInjector {
    fn drop(&mut self) {
        unsafe {
            if let Some(dev) = self.pen_device.take() {
                DestroySyntheticPointerDevice(dev);
            }
            if let Some(dev) = self.touch_device.take() {
                DestroySyntheticPointerDevice(dev);
            }
        }
    }
}

/// Runs on Windows only: the platform-independent flag constants must equal the SDK's.
#[cfg(all(test, target_os = "windows"))]
mod sdk_constants {
    use super::*;

    #[test]
    fn map_constants_match_the_sdk() {
        assert_eq!(map::PF_NEW, POINTER_FLAG_NEW.0);
        assert_eq!(map::PF_INRANGE, POINTER_FLAG_INRANGE.0);
        assert_eq!(map::PF_INCONTACT, POINTER_FLAG_INCONTACT.0);
        assert_eq!(map::PF_FIRSTBUTTON, POINTER_FLAG_FIRSTBUTTON.0);
        assert_eq!(map::PF_SECONDBUTTON, POINTER_FLAG_SECONDBUTTON.0);
        assert_eq!(map::PF_PRIMARY, POINTER_FLAG_PRIMARY.0);
        assert_eq!(map::PF_CONFIDENCE, POINTER_FLAG_CONFIDENCE.0);
        assert_eq!(map::PF_CANCELED, POINTER_FLAG_CANCELED.0);
        assert_eq!(map::PF_DOWN, POINTER_FLAG_DOWN.0);
        assert_eq!(map::PF_UPDATE, POINTER_FLAG_UPDATE.0);
        assert_eq!(map::PF_UP, POINTER_FLAG_UP.0);
        assert_eq!(map::PEN_BARREL, PEN_FLAG_BARREL);
        assert_eq!(map::PEN_INVERTED, PEN_FLAG_INVERTED);
        assert_eq!(map::PEN_ERASER, PEN_FLAG_ERASER);
        assert_eq!(map::PEN_MASK_PRESSURE, PEN_MASK_PRESSURE);
        assert_eq!(map::PEN_MASK_TILT_X, PEN_MASK_TILT_X);
        assert_eq!(map::PEN_MASK_TILT_Y, PEN_MASK_TILT_Y);
        assert_eq!(map::TOUCH_MASK_PRESSURE, TOUCH_MASK_PRESSURE);
    }
}
