//! Phase 5 audio: host output to the phone and the phone's mic to the host.
//!
//! - [`AudioOutService`]: a PipeWire virtual sink "DisplaySwarm - <device>"; what
//!   applications play into it is Opus-encoded (48 kHz, stereo, 10 ms) and sent
//!   as `AudioFrame`. Optionally it is the default output while connected.
//! - [`MicService`]: `MicFrame`s are decoded into a virtual PipeWire source
//!   "DisplaySwarm Mic - <device>" any application can record from.
//!
//! Dropping either service removes its virtual device.
//!
//! PipeWire and Opus are Linux-only dependencies. Other platforms build only
//! the device-independent [`sync`] hub and a stub [`audio_services`] that
//! offers no service (Windows WASAPI loopback is W3).

pub mod sync;

#[cfg(target_os = "linux")]
pub mod codec;
#[cfg(target_os = "linux")]
pub mod delay;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "linux")]
pub mod sink;

#[cfg(target_os = "linux")]
pub use service::{audio_services, AudioOutService, MicService};

/// No audio services off Linux yet: the session simply has no audio channel.
#[cfg(not(target_os = "linux"))]
pub fn audio_services(_ctx: &crate::services::ServiceCtx) -> Vec<Box<dyn crate::services::SessionService>> {
    Vec::new()
}
