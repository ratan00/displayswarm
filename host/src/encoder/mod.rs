//! H.264 encoding. `create_default_encoder` probes the best available backend:
//! ffmpeg hardware encoders (Linux: VA-API -> QSV -> NVENC -> AMF -> Media
//! Foundation; Windows: NVENC -> QSV -> AMF -> Media Foundation), then software
//! x264. The backends are behind the `encoders` feature (default on). The rest of the host only sees [`VideoEncoder`].

#[cfg(feature = "encoders")]
pub mod convert;
#[cfg(feature = "encoders")]
pub mod ffmpeg;
pub mod stream;
#[cfg(feature = "encoders")]
pub mod x264;

#[cfg(all(test, feature = "encoders"))]
mod bench;

use crate::capture::Frame;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct EncodedPacket {
    pub frame_type: u8,
    pub data: Vec<u8>,
    pub timestamp_us: u64,
}

pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, Box<dyn std::error::Error + Send + Sync>>;
    fn request_keyframe(&mut self);

    /// Changes the target bitrate on the fly (adaptive bitrate). Backends that
    /// cannot reconfigure live may ignore it.
    fn set_bitrate(&mut self, kbps: u32) {
        let _ = kbps;
    }

    /// Short name of the active backend, e.g. "h264_vaapi" or "x264", for logs and UI.
    fn backend_name(&self) -> String {
        "unknown".into()
    }
}

/// Rejects frames that do not match the size the encoder was opened with; hardware
/// contexts cannot be resized, and a short buffer would read out of bounds.
#[cfg_attr(not(feature = "encoders"), allow(dead_code))]
pub(crate) fn check_frame(frame: &Frame, width: u32, height: u32) -> Result<(), BoxError> {
    if frame.width != width || frame.height != height {
        return Err(format!(
            "frame is {}x{} but encoder is {}x{}",
            frame.width, frame.height, width, height
        )
        .into());
    }
    if frame.data.len() < (width as usize) * (height as usize) * 4 {
        return Err(format!("frame buffer too small: {} bytes", frame.data.len()).into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "encoders"), allow(dead_code))]
pub(crate) enum Backend {
    Ffmpeg(&'static str),
    X264,
}

/// Probe order. x264 is last and needs nothing but libx264, so there is always a fallback.
pub(crate) fn backend_chain() -> Vec<Backend> {
    #[cfg(feature = "encoders")]
    let mut v: Vec<Backend> = ffmpeg::HW_BACKENDS.iter().map(|n| Backend::Ffmpeg(n)).collect();
    #[cfg(not(feature = "encoders"))]
    let mut v: Vec<Backend> = Vec::new();
    v.push(Backend::X264);
    v
}

/// Constant-quality QP for wired links (USB). At 15-16 an H.264 desktop is
/// within a fraction of a dB of what 4:2:0 chroma allows (measured ~41 dB PSNR
/// on a KDE desktop, the same as an NV12 round trip with no encoding), and a
/// static screen costs almost nothing. `DISPLAYSWARM_QP` overrides it.
pub const WIRED_QP: u32 = 16;

/// The wired QP, honouring `DISPLAYSWARM_QP` (clamped to 1..=51).
pub fn wired_qp() -> u32 {
    std::env::var("DISPLAYSWARM_QP").ok().and_then(|v| v.parse().ok()).map_or(WIRED_QP, |q: u32| q.clamp(1, 51))
}

/// Bitrate cap for constant quality where the encoder can enforce one (x264),
/// so a burst of full-screen motion cannot outrun a USB 2 link.
pub const WIRED_MAX_KBPS: u32 = 120_000;

/// `qp`: constant quality at that QP (wired links) instead of CBR at `kbps`.
pub(crate) fn open_backend(
    b: Backend,
    w: u32,
    h: u32,
    fps: u32,
    kbps: u32,
    qp: Option<u32>,
) -> Result<Box<dyn VideoEncoder>, BoxError> {
    #[cfg(feature = "encoders")]
    return Ok(match b {
        Backend::Ffmpeg(name) => Box::new(ffmpeg::FfmpegEncoder::try_new(name, w, h, fps, kbps, qp)?),
        Backend::X264 => Box::new(x264::X264Encoder::try_new_with(w, h, fps, kbps, qp)?),
    });
    #[cfg(not(feature = "encoders"))]
    {
        let _ = (w, h, fps, kbps, qp);
        Err(format!("{b:?}: built without the `encoders` feature").into())
    }
}

/// Wraps the chosen backend and, if it fails at runtime (GPU reset, driver
/// error), drops down the chain instead of ending the stream.
struct AutoEncoder {
    chain: Vec<Backend>,
    idx: usize,
    cur: Box<dyn VideoEncoder>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    qp: Option<u32>,
}

impl AutoEncoder {
    /// Opens the first backend at or after `from` that initialises.
    fn open_from(&mut self, from: usize) -> Result<(), BoxError> {
        let mut last: BoxError = "no encoder backend available".into();
        for i in from..self.chain.len() {
            match open_backend(self.chain[i], self.width, self.height, self.fps, self.bitrate_kbps, self.qp) {
                Ok(enc) => {
                    log::info!("Video encoder backend: {}", enc.backend_name());
                    self.idx = i;
                    self.cur = enc;
                    return Ok(());
                }
                Err(e) => {
                    log::info!("Encoder backend {:?} unavailable: {}", self.chain[i], e);
                    last = e;
                }
            }
        }
        Err(last)
    }
}

impl VideoEncoder for AutoEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, BoxError> {
        loop {
            match self.cur.encode(frame) {
                Ok(p) => return Ok(p),
                Err(e) => {
                    // Wrong frame size is the caller's bug, not a backend failure.
                    if frame.width != self.width || frame.height != self.height {
                        return Err(e);
                    }
                    log::error!("Encoder {} failed: {}; falling back", self.cur.backend_name(), e);
                    // A freshly opened encoder starts with an IDR by itself.
                    self.open_from(self.idx + 1)?;
                }
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.cur.request_keyframe();
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.bitrate_kbps = kbps;
        self.cur.set_bitrate(kbps);
    }

    fn backend_name(&self) -> String {
        self.cur.backend_name()
    }
}

/// Encoder returned when not even x264 could be opened (e.g. invalid dimensions):
/// every `encode` reports the reason instead of the host panicking at startup.
struct FailedEncoder(String);

impl VideoEncoder for FailedEncoder {
    fn encode(&mut self, _frame: &Frame) -> Result<Vec<EncodedPacket>, BoxError> {
        Err(format!("no working H.264 encoder: {}", self.0).into())
    }
    fn request_keyframe(&mut self) {}
    fn backend_name(&self) -> String {
        "none".into()
    }
}

pub fn create_default_encoder(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Box<dyn VideoEncoder> {
    create_encoder(width, height, fps, bitrate_kbps, None)
}

/// [`create_default_encoder`], with `qp` selecting constant quality (see
/// [`WIRED_QP`]) instead of constant bitrate.
pub fn create_encoder(width: u32, height: u32, fps: u32, bitrate_kbps: u32, qp: Option<u32>) -> Box<dyn VideoEncoder> {
    let mut auto = AutoEncoder {
        chain: backend_chain(),
        idx: 0,
        cur: Box::new(FailedEncoder(String::new())),
        width,
        height,
        fps,
        bitrate_kbps,
        qp,
    };
    match auto.open_from(0) {
        Ok(()) => Box::new(auto),
        Err(e) => {
            log::error!("No H.264 encoder could be initialised: {}", e);
            Box::new(FailedEncoder(e.to_string()))
        }
    }
}
