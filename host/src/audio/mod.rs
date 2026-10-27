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
//! Windows has [`win_service::AudioOutService`] only: a WASAPI loopback of the
//! default output ([`loopback`]), Opus-encoded the same way. There is no
//! virtual microphone (it would need a driver) and the laptop's speakers
//! cannot be delayed, so the host does not offer `FEATURE_MIC` there.
//!
//! PipeWire is a Linux-only dependency. Other platforms build only the
//! device-independent [`sync`] hub and a stub [`audio_services`] that offers
//! no service.

pub mod sync;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod codec;
#[cfg(any(target_os = "windows", test))]
pub mod pcm;
#[cfg(target_os = "windows")]
pub mod loopback;
#[cfg(target_os = "windows")]
pub mod win_service;
#[cfg(target_os = "linux")]
pub mod delay;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "linux")]
pub mod sink;

#[cfg(target_os = "linux")]
pub use service::{audio_services, AudioOutService, MicService};

#[cfg(target_os = "windows")]
pub use win_service::{audio_services, AudioOutService};

/// No audio services on other platforms: the session simply has no audio channel.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn audio_services(_ctx: &crate::services::ServiceCtx) -> Vec<Box<dyn crate::services::SessionService>> {
    Vec::new()
}
