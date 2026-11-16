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
//! On Windows the same Opus path runs on WASAPI instead ([`windows`]): a
//! loopback capture of the PC's playback, and the phone's mic played into a
//! virtual cable. Other platforms build only the device-independent [`sync`]
//! hub and a stub [`audio_services`] that offers no service.

pub mod cable;
pub mod sync;

#[cfg(any(target_os = "linux", windows))]
pub mod codec;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod delay;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "linux")]
pub mod sink;

#[cfg(target_os = "linux")]
pub use service::{audio_services, AudioOutService, MicService};

#[cfg(windows)]
pub use self::windows::audio_services;

/// No audio services on this platform: the session simply has no audio channel.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn audio_services(_ctx: &crate::services::ServiceCtx) -> Vec<Box<dyn crate::services::SessionService>> {
    Vec::new()
}
