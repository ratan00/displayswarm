pub mod dummy;
pub mod router;

#[cfg(target_os = "linux")]
pub mod gnome;
#[cfg(target_os = "linux")]
pub mod kwin;
#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;
// Windows injector logic that needs no Win32 (coordinate mapping, pen/touch
// flags, contact tracking); compiled everywhere so Linux tests it.
pub mod windows_map;

use crate::protocol::{InputEvent, KeyEvent};

/// Platform-specific synthetic input back-end.
///
/// Two kinds of event reach this trait, both produced from protocol v2 input
/// messages by [`router::InputRouter`]:
///
///  * [`InputEvent`] - one pointer sample: pen, finger or mouse, with button
///    and wheel-notch state in its `flags` byte.
///  * [`KeyEvent`] - one key press or release.
///
/// Implementations must be tolerant: the transport is unauthenticated and
/// unauthenticated peers put arbitrary bytes on the wire, so a malformed or
/// unsupported event should be counted and logged, not treated as fatal.
pub trait InputInjector: Send {
    /// Injects one pointer event.
    fn inject(&mut self, event: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Injects one pen sample that the digitizer took `age_us` microseconds
    /// ago (batched history samples). Back-ends that can timestamp events use
    /// it; the default ignores the age.
    fn inject_pen_sample(
        &mut self,
        event: &InputEvent,
        age_us: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = age_us;
        self.inject(event)
    }

    /// Injects one key press, release or cancel.
    ///
    /// `event.key_code` is an `android.view.KeyEvent.KEYCODE_*` value: the wire
    /// format is Android's numbering, so every implementation has to map it
    /// onto its own platform key codes. That mapping is per-platform and
    /// deliberately *not* shared, because Android's keycode space and each
    /// platform's are unrelated orderings (see the tables in `linux.rs` and
    /// `windows.rs`).
    ///
    /// Returning `Ok(())` for a key that has no mapping is the correct
    /// behaviour - it just means the host cannot type that key. Returning
    /// `Err` is reserved for the injection itself failing (e.g. a closed or
    /// full `uinput` fd), which the caller counts as a failed event.
    fn inject_key(&mut self, event: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Moves the pointer by a relative amount (the phone's touchpad mode), with
    /// the button state and wheel notches, without warping it anywhere.
    fn inject_relative(&mut self, event: &RelativeEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = event;
        Err("relative pointer motion is not supported by this back-end".into())
    }

    /// Zooms by `steps` notches (positive zooms in), as Ctrl + wheel does in
    /// desktop applications.
    fn inject_zoom(&mut self, steps: i32) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = steps;
        Err("zoom is not supported by this back-end".into())
    }

    /// Confines absolute pointer, touch and pen input to one monitor: the
    /// event's normalised coordinates map onto `region` of the global desktop
    /// (logical pixels). `None` maps them onto the whole desktop, as before.
    /// Called when a capture's region becomes known and whenever it changes.
    fn set_output_region(&mut self, region: Option<crate::display::OutputRegion>) {
        let _ = region;
    }
}

/// One relative pointer step: motion in host pixels (fractions are carried by
/// the injector), the buttons now held (`POINTER_FLAG_BTN_*`), and wheel
/// notches (`wheel` positive scrolls up, `hwheel` positive scrolls right).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RelativeEvent {
    pub dx: f32,
    pub dy: f32,
    pub buttons: u8,
    pub wheel: i32,
    pub hwheel: i32,
}

pub fn create_default_injector(display_width: u32, display_height: u32) -> Box<dyn InputInjector> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(inj) = linux::LinuxUinputInjector::new(display_width, display_height) {
            return Box::new(inj);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(inj) = windows::WindowsPointerInjector::new(display_width, display_height) {
            return Box::new(inj);
        }
    }

    Box::new(dummy::DummyInjector::new())
}

/// Re-expresses a normalised position on `region` as a normalised position on
/// the whole desktop `bounds` (both in logical pixels), for back-ends whose
/// absolute device always spans the desktop's bounding box (uinput on GNOME).
/// The input is clamped to `0..=1` first so a stray coordinate cannot leak onto
/// a neighbouring monitor.
pub fn map_norm_into_region(
    x_norm: f32,
    y_norm: f32,
    region: crate::display::OutputRegion,
    bounds: crate::display::OutputRegion,
) -> (f32, f32) {
    let (x, y) = region_point(x_norm, y_norm, region);
    let nx = (x - bounds.x as f64) / (bounds.width.max(1) as f64);
    let ny = (y - bounds.y as f64) / (bounds.height.max(1) as f64);
    (nx.clamp(0.0, 1.0) as f32, ny.clamp(0.0, 1.0) as f32)
}

/// The desktop point (logical pixels) a normalised position on `region` is.
pub fn region_point(x_norm: f32, y_norm: f32, region: crate::display::OutputRegion) -> (f64, f64) {
    let last_x = region.width.saturating_sub(1) as f64;
    let last_y = region.height.saturating_sub(1) as f64;
    (
        region.x as f64 + (x_norm as f64).clamp(0.0, 1.0) * last_x,
        region.y as f64 + (y_norm as f64).clamp(0.0, 1.0) * last_y,
    )
}

#[cfg(test)]
mod region_tests {
    use super::*;
    use crate::display::OutputRegion as R;

    const PANEL: R = R { x: 0, y: 0, width: 1920, height: 1080 };
    const PHONE: R = R { x: 1920, y: 0, width: 1080, height: 2400 };
    const BOUNDS: R = R { x: 0, y: 0, width: 3000, height: 2400 };

    #[test]
    fn corners_of_the_region_land_on_its_edges() {
        let (x, y) = map_norm_into_region(0.0, 0.0, PHONE, BOUNDS);
        assert!((x - 1920.0 / 3000.0).abs() < 1e-6 && y == 0.0);
        let (x, y) = map_norm_into_region(1.0, 1.0, PHONE, BOUNDS);
        assert!((x - 2999.0 / 3000.0).abs() < 1e-6 && (y - 2399.0 / 2400.0).abs() < 1e-6);
    }

    #[test]
    fn region_equal_to_bounds_is_near_identity() {
        let (x, y) = map_norm_into_region(0.5, 0.25, BOUNDS, BOUNDS);
        assert!((x - 0.5).abs() < 1e-3 && (y - 0.25).abs() < 1e-3);
    }

    #[test]
    fn negative_origin_bounds_are_offset() {
        let bounds = R { x: -1920, y: 0, width: 3840, height: 1080 };
        let (x, _) = map_norm_into_region(0.0, 0.0, PANEL, bounds);
        assert!((x - 0.5).abs() < 1e-6);
        let (x, _) = map_norm_into_region(0.0, 0.0, R { x: -1920, ..PANEL }, bounds);
        assert_eq!(x, 0.0);
    }

    #[test]
    fn out_of_range_input_is_clamped_to_the_region() {
        let (x, _) = map_norm_into_region(-3.0, 0.5, PHONE, BOUNDS);
        assert!((x - 1920.0 / 3000.0).abs() < 1e-6);
        let (x, _) = map_norm_into_region(9.0, 0.5, PHONE, BOUNDS);
        assert!(x < 1.0);
        assert_eq!(region_point(2.0, 2.0, PANEL), (1919.0, 1079.0));
    }
}
