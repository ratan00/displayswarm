use super::InputInjector;
use crate::protocol::{InputEvent, KeyEvent, INPUT_TYPE_STYLUS};

pub struct DummyInjector {
    event_count: u64,
    key_count: u64,
    region: Option<crate::display::OutputRegion>,
}

impl DummyInjector {
    pub fn new() -> Self {
        Self { event_count: 0, key_count: 0, region: None }
    }

    /// Number of pointer events handed to [`InputInjector::inject`].
    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Number of key events handed to [`InputInjector::inject_key`].
    pub fn key_count(&self) -> u64 {
        self.key_count
    }
}

impl DummyInjector {
    /// The region last given to [`InputInjector::set_output_region`].
    pub fn output_region(&self) -> Option<crate::display::OutputRegion> {
        self.region
    }
}

impl InputInjector for DummyInjector {
    fn set_output_region(&mut self, region: Option<crate::display::OutputRegion>) {
        self.region = region;
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.event_count += 1;
        if event.event_type == INPUT_TYPE_STYLUS {
            log::debug!(
                "[Stylus] x: {:.3}, y: {:.3}, pressure: {:.3}, tilt: ({:.2}, {:.2})",
                event.x_norm, event.y_norm, event.pressure, event.tilt_x, event.tilt_y
            );
        } else {
            log::debug!(
                "[Touch] ptr: {}, action: {}, x: {:.3}, y: {:.3}",
                event.pointer_id, event.action, event.x_norm, event.y_norm
            );
        }
        Ok(())
    }

    /// No-op, like [`Self::inject`]: this back-end exists so the host still
    /// runs (and still counts events) where no real input device can be
    /// created.
    ///
    /// `trace` rather than `debug`, to match `inject`: a fast typist produces
    /// hundreds of these a second and the useful signal is the stats line, not
    /// the per-key log. `text` is escaped (`{:?}`) rather than printed raw,
    /// because it is attacker-influenced: the transport is unauthenticated, so
    /// a peer could put terminal escape sequences in there.
    fn inject_key(&mut self, event: &KeyEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.key_count += 1;
        log::trace!(
            "[Key] action: {}, key_code: {:#06x}, scan_code: {:#06x}, flags: {:#04x}, text: {:?}",
            event.action, event.key_code, event.scan_code, event.flags, event.text
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ACTION_DOWN, ACTION_UP};

    #[test]
    fn test_dummy_injector_stores_region() {
        let mut inj = DummyInjector::new();
        let r = crate::display::OutputRegion { x: 1, y: 2, width: 3, height: 4 };
        inj.set_output_region(Some(r));
        assert_eq!(inj.output_region(), Some(r));
        inj.set_output_region(None);
        assert_eq!(inj.output_region(), None);
    }

    /// The no-op injector still has to accept events and count them - it is
    /// the fallback the host silently degrades to when no uinput or
    /// synthetic-pointer device could be created, so "it works but does
    /// nothing" is the whole point.
    #[test]
    fn test_dummy_injector_counts_pointer_and_key_events() {
        let mut inj = DummyInjector::new();

        let pointer = crate::protocol::InputEvent {
            event_type: INPUT_TYPE_STYLUS,
            pointer_id: 0,
            action: ACTION_DOWN,
            flags: 0,
            x_norm: 0.5,
            y_norm: 0.5,
            pressure: 0.5,
            tilt_x: 0.0,
            tilt_y: 0.0,
        };
        inj.inject(&pointer).unwrap();
        assert_eq!(inj.event_count(), 1);
        assert_eq!(inj.key_count(), 0);

        inj.inject_key(&KeyEvent::new(ACTION_DOWN, 29, "a")).unwrap();
        assert_eq!(inj.key_count(), 1);
        assert_eq!(inj.event_count(), 1, "the key path must not touch the pointer counter");

        inj.inject_key(&KeyEvent::new(ACTION_UP, 29, "")).unwrap();
        assert_eq!(inj.key_count(), 2);
    }
}
