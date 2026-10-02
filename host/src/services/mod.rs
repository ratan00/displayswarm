//! Per-session services beside video and input: audio (Phase 5), clipboard and
//! battery-aware quality (Phase 8), and whatever comes next.
//!
//! Contract:
//! - For each streaming session the server calls the factory in
//!   [`crate::server::Backends::services`] (default: [`default_services`]) once,
//!   after the handshake, with a [`ServiceCtx`].
//! - Every message the phone sends on the audio, clipboard or file channel, and
//!   every `Resize`, is offered to each service in turn; the first whose
//!   [`SessionService::wants`] returns true gets it through `on_message`.
//!   Those calls run on one task: they must not block (hand work to a thread).
//! - Services send through [`ServiceOut`]: `send` for small realtime messages
//!   (audio; they go out before video), `send_bulk` for large ones (clipboard,
//!   files; they go out one fragment at a time *after* video, so a big file
//!   never stalls the picture).
//! - `on_role` is called whenever the effective role changes.
//! - Dropping a service is its teardown (remove virtual audio devices, stop
//!   threads). It happens when the session ends, and must not block for long.
//!
//! Adding a service: implement [`SessionService`], add it to
//! [`default_services`] behind its [`crate::devices::DeviceSettings`] toggle and
//! the negotiated feature bit, and add that bit to [`HOST_FEATURES`].

use std::sync::Arc;

use tokio::sync::mpsc;

pub mod battery;
pub mod clipboard;

use crate::devices::{DeviceSettings, Role};
use crate::protocol::wire::{
    Message, FEATURE_AUDIO_OUT, FEATURE_BATTERY, FEATURE_CLIPBOARD, FEATURE_KEYBOARD, FEATURE_MIC,
    FEATURE_STYLUS, FEATURE_TOUCH,
};

/// `FEATURE_*` bits this host implements. The HelloAck carries
/// `hello.features & HOST_FEATURES`. Each phase adds its bit here.
pub const HOST_FEATURES: u32 = FEATURE_TOUCH
    | FEATURE_STYLUS
    | FEATURE_KEYBOARD
    | FEATURE_AUDIO_OUT
    | FEATURE_MIC
    | FEATURE_CLIPBOARD
    | FEATURE_BATTERY;

/// The `FEATURE_*` bits a session gets: what the phone offered, what this host
/// implements, minus features the device's settings turn off.
pub fn negotiated_features(offered: u32, settings: &DeviceSettings) -> u32 {
    let mut f = offered & HOST_FEATURES;
    if !settings.clipboard {
        f &= !FEATURE_CLIPBOARD;
    }
    // Without this the phone would start recording for a mic the host drops.
    if !settings.mic {
        f &= !FEATURE_MIC;
    }
    if !settings.audio_out {
        f &= !FEATURE_AUDIO_OUT;
    }
    f
}

/// Outgoing queues of one session.
#[derive(Clone)]
pub struct ServiceOut {
    ctrl: mpsc::UnboundedSender<Message>,
    bulk: mpsc::Sender<Message>,
}

impl ServiceOut {
    pub fn new(ctrl: mpsc::UnboundedSender<Message>, bulk: mpsc::Sender<Message>) -> Self {
        Self { ctrl, bulk }
    }

    /// A small realtime message (audio frames). False once the session is gone.
    pub fn send(&self, msg: Message) -> bool {
        self.ctrl.send(msg).is_ok()
    }

    /// A large message, sent after video. Waits while the bulk queue is full
    /// (backpressure). False once the session is gone.
    pub async fn send_bulk(&self, msg: Message) -> bool {
        self.bulk.send(msg).await.is_ok()
    }

    /// [`Self::send_bulk`] from a plain thread.
    pub fn blocking_send_bulk(&self, msg: Message) -> bool {
        self.bulk.blocking_send(msg).is_ok()
    }
}

/// Lets a service ask the session's encoder for a different bitrate.
#[derive(Clone)]
pub struct QualityHandle {
    /// The bitrate the session started with (kbps).
    pub nominal_kbps: u32,
    request: Arc<dyn Fn(u32) + Send + Sync>,
}

impl QualityHandle {
    pub fn new(nominal_kbps: u32, request: impl Fn(u32) + Send + Sync + 'static) -> Self {
        Self { nominal_kbps, request: Arc::new(request) }
    }

    /// A handle that goes nowhere (tests).
    pub fn detached() -> Self {
        Self::new(20_000, |_| {})
    }

    /// Asks the encoder thread to switch to `kbps` (applied on its next frame).
    pub fn request_bitrate(&self, kbps: u32) {
        (self.request)(kbps)
    }
}

/// What a service knows about its session.
#[derive(Clone)]
pub struct ServiceCtx {
    pub device_id: String,
    pub device_name: String,
    /// `FEATURE_*` bits both sides agreed on (the HelloAck's).
    pub features: u32,
    pub settings: DeviceSettings,
    pub role: Role,
    pub out: ServiceOut,
    pub quality: QualityHandle,
}

pub trait SessionService: Send {
    /// For logs.
    fn name(&self) -> &'static str;
    /// Whether `msg` is for this service.
    fn wants(&self, msg: &Message) -> bool;
    /// A message from the phone. Must not block.
    fn on_message(&mut self, msg: Message);
    /// The effective role changed.
    fn on_role(&mut self, _role: Role) {}
    /// The device's settings changed while the session runs.
    fn on_settings(&mut self, _settings: &DeviceSettings) {}
}

pub type ServiceFactory = dyn Fn(&ServiceCtx) -> Vec<Box<dyn SessionService>> + Send + Sync;

/// The services a real session gets.
pub fn default_services(ctx: &ServiceCtx) -> Vec<Box<dyn SessionService>> {
    let mut v: Vec<Box<dyn SessionService>> = Vec::new();
    v.extend(crate::audio::audio_services(ctx));
    if ctx.settings.clipboard && ctx.features & FEATURE_CLIPBOARD != 0 {
        v.push(Box::new(clipboard::ClipboardService::start(ctx)));
    }
    if ctx.features & FEATURE_BATTERY != 0 {
        v.push(Box::new(battery::BatteryService::new(ctx)));
    }
    v
}

/// Hands `msg` to the first service that wants it. Returns whether one did.
pub fn dispatch(services: &mut [Box<dyn SessionService>], msg: Message) -> bool {
    match services.iter_mut().find(|s| s.wants(&msg)) {
        Some(s) => {
            s.on_message(msg);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Clip(Arc<Mutex<Vec<Message>>>);
    impl SessionService for Clip {
        fn name(&self) -> &'static str {
            "clip"
        }
        fn wants(&self, m: &Message) -> bool {
            matches!(m, Message::Clipboard { .. })
        }
        fn on_message(&mut self, m: Message) {
            self.0.lock().unwrap().push(m);
        }
    }

    #[test]
    fn settings_switch_features_off() {
        let all = FEATURE_CLIPBOARD | FEATURE_BATTERY | FEATURE_TOUCH;
        assert_eq!(negotiated_features(all, &DeviceSettings::default()), all);
        let off = DeviceSettings { clipboard: false, ..DeviceSettings::default() };
        assert_eq!(negotiated_features(all, &off), FEATURE_BATTERY | FEATURE_TOUCH);
    }

    #[test]
    fn dispatch_goes_to_the_service_that_wants_it() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut v: Vec<Box<dyn SessionService>> = vec![Box::new(Clip(got.clone()))];
        assert!(dispatch(&mut v, Message::Clipboard { mime: "text/plain".into(), data: b"x".to_vec() }));
        assert!(!dispatch(&mut v, Message::FileControl { id: 1, op: 0 }));
        assert_eq!(got.lock().unwrap().len(), 1);
    }
}
