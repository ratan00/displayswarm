//! DisplaySwarm protocol v2: framing, channels and messages.
//!
//! Mirrored byte for byte by `client-android/.../Wire.kt`. Both test suites
//! check their encoders against the same golden vectors in
//! `protocol/v2-vectors.txt` at the repository root, so a change here that is
//! not made on the other side fails a test.
//!
//! # Frame
//!
//! Every byte on the link, in both directions, belongs to a frame (big endian):
//!
//! ```text
//!   off  size  field
//!     0     2  magic    "VM" (0x56 0x4D)
//!     2     1  version  2
//!     3     1  channel  CH_*
//!     4     1  type     MSG_*
//!     5     1  flags    FLAG_MORE = another fragment of this message follows
//!     6     4  len      payload bytes in this frame, <= MAX_FRAGMENT
//!    10   len  payload
//! ```
//!
//! **The first 6 bytes are frozen for every future version**, as are the
//! `HelloAck` status byte and the `Bye` payload. That is what lets any two
//! versions tell each other clearly that they do not match. A v1 packet
//! (`VMCT`, `VMVI`, ...) happens to parse as magic "VM" with a version byte of
//! 'C', 'V', ... which is how a v2 host recognises a v1 app.
//!
//! # Channels and fragmentation
//!
//! A message larger than the sender's fragment size is split into consecutive
//! frames on its channel, all but the last flagged [`FLAG_MORE`]. Frames of
//! *different* channels may interleave between fragments, which is the point:
//! the host sends video in [`VIDEO_FRAGMENT`]-sized pieces, so control, input
//! and (later) audio never wait behind a whole keyframe. Within one channel,
//! fragments of one message are contiguous. The channel number is also the
//! send priority, lowest first.
//!
//! # Payload rules
//!
//! * Integers and IEEE-754 floats are big endian. `str16` is a u16 byte length
//!   and UTF-8. `rest` is all remaining payload bytes.
//! * Decoders ignore trailing bytes, so a later revision can append fields.
//! * An unknown message type is skipped (it decodes as [`Message::Unknown`]).
//! * A known type on the wrong channel, or a malformed frame header, is a
//!   protocol error: the link is reliable, so the stream cannot be trusted
//!   after one and the session ends.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAGIC: [u8; 2] = *b"VM";
pub const VERSION: u8 = 2;
pub const HEADER_SIZE: usize = 10;

/// Frame `flags` bit: more fragments of this message follow.
pub const FLAG_MORE: u8 = 0x01;

/// Largest payload a single frame may carry.
pub const MAX_FRAGMENT: usize = 64 * 1024;

/// Fragment size the host uses for video, so higher-priority frames can be
/// interleaved at least every 16 KiB.
pub const VIDEO_FRAGMENT: usize = 16 * 1024;

// ---- Channels ----------------------------------------------------------------
pub const CH_CONTROL: u8 = 0;
pub const CH_INPUT: u8 = 1;
pub const CH_AUDIO: u8 = 2;
pub const CH_VIDEO: u8 = 3;
pub const CH_CLIPBOARD: u8 = 4;
pub const CH_FILE: u8 = 5;
const CHANNEL_COUNT: usize = 6;

/// Largest reassembled message accepted per channel. Bounds what an
/// unauthenticated peer can make the receiver buffer.
pub fn max_message_len(channel: u8) -> usize {
    match channel {
        CH_VIDEO | CH_CLIPBOARD => 16 * 1024 * 1024,
        CH_FILE => 1024 * 1024,
        _ => MAX_FRAGMENT,
    }
}

// ---- Message types -------------------------------------------------------------
// Control (channel 0)
pub const MSG_HELLO: u8 = 0x01;
pub const MSG_HELLO_ACK: u8 = 0x02;
pub const MSG_PING: u8 = 0x03;
pub const MSG_PONG: u8 = 0x04;
pub const MSG_HEARTBEAT: u8 = 0x05;
pub const MSG_BYE: u8 = 0x06;
pub const MSG_STATS: u8 = 0x07;
pub const MSG_HOST_STATE: u8 = 0x08;
pub const MSG_KEYFRAME_REQUEST: u8 = 0x09;
pub const MSG_SET_BITRATE: u8 = 0x0A;
pub const MSG_RESIZE: u8 = 0x0B;
pub const MSG_SET_ROLE: u8 = 0x0C;
/// Phone -> host: picture quality (`VideoQuality::to_wire`).
pub const MSG_SET_QUALITY: u8 = 0x0D;
/// Phone -> host: the phone's audio pipeline latency in microseconds
/// (`AudioLatency`).
pub const MSG_AUDIO_LATENCY: u8 = 0x0E;
/// Host -> phone: make the sample with host pts T audible at host time
/// T + `delay_us`; 0 = sync off, play as soon as possible (`AudioSync`).
pub const MSG_AUDIO_SYNC: u8 = 0x0F;
/// Phone -> host: which optional services the user has switched on
/// (`SERVICE_*` bits). Sent after the handshake and on every change.
pub const MSG_SERVICE_STATE: u8 = 0x10;

/// `ServiceState::enabled` bits.
pub const SERVICE_AUDIO_OUT: u8 = 1;
pub const SERVICE_MIC: u8 = 2;
// Phase 8 (channel 0)
pub const MSG_BATTERY_STATUS: u8 = 0x21;
// Pairing (channel 0), only exchanged inside TLS on network transports, before the Hello.
// 0x38/0x39 rather than 0x30: 0x30 and 0x31 are the audio frames.
pub const MSG_PAIR_REQUEST: u8 = 0x38;
pub const MSG_PAIR_RESPONSE: u8 = 0x39;
// Video (channel 3)
pub const MSG_VIDEO_FRAME: u8 = 0x20;
// Audio (channel 2)
pub const MSG_AUDIO_FRAME: u8 = 0x30;
pub const MSG_MIC_FRAME: u8 = 0x31;
// Input (channel 1)
pub const MSG_TOUCH: u8 = 0x40;
pub const MSG_PEN: u8 = 0x41;
pub const MSG_MOUSE: u8 = 0x42;
pub const MSG_SCROLL: u8 = 0x43;
pub const MSG_PINCH: u8 = 0x44;
pub const MSG_KEY: u8 = 0x45;
pub const MSG_TEXT: u8 = 0x46;
// Clipboard (channel 4)
pub const MSG_CLIPBOARD: u8 = 0x50;
// File (channel 5)
pub const MSG_FILE_OFFER: u8 = 0x60;
pub const MSG_FILE_CHUNK: u8 = 0x61;
pub const MSG_FILE_CONTROL: u8 = 0x62;

/// `PairRequest::mode`.
pub const PAIR_MODE_TOKEN: u8 = 0;
pub const PAIR_MODE_PIN: u8 = 1;
pub const PAIR_MODE_QR: u8 = 2;

/// `PairResponse::status`.
pub const PAIR_OK: u8 = 0;
/// A PIN is now shown on the host; send it in a `PAIR_MODE_PIN` request.
pub const PAIR_PIN_REQUIRED: u8 = 1;
pub const PAIR_WRONG: u8 = 2;
/// Too many wrong attempts; the pairing offer is void and the host hangs up.
pub const PAIR_LOCKED: u8 = 3;
/// The token is unknown or was revoked; pair again.
pub const PAIR_UNTRUSTED: u8 = 4;
/// The host is not accepting new devices right now.
pub const PAIR_DISABLED: u8 = 5;

/// The channel a known message type travels on, or `None` for an unknown type.
pub fn channel_of(msg_type: u8) -> Option<u8> {
    Some(match msg_type {
        0x01..=0x10 | MSG_BATTERY_STATUS | MSG_PAIR_REQUEST | MSG_PAIR_RESPONSE => CH_CONTROL,
        MSG_VIDEO_FRAME => CH_VIDEO,
        MSG_AUDIO_FRAME | MSG_MIC_FRAME => CH_AUDIO,
        0x40..=0x46 => CH_INPUT,
        MSG_CLIPBOARD => CH_CLIPBOARD,
        MSG_FILE_OFFER..=MSG_FILE_CONTROL => CH_FILE,
        _ => return None,
    })
}

// ---- Field values --------------------------------------------------------------

/// `Hello::codecs` / `HelloAck::codec` bits.
pub const CODEC_H264: u8 = 0x01;
pub const CODEC_HEVC: u8 = 0x02;
pub const CODEC_AV1: u8 = 0x04;

/// `Hello::features` (what the phone offers) and `HelloAck::features` (what
/// the host will use) bits.
pub const FEATURE_TOUCH: u32 = 1 << 0;
pub const FEATURE_STYLUS: u32 = 1 << 1;
pub const FEATURE_KEYBOARD: u32 = 1 << 2;
pub const FEATURE_AUDIO_OUT: u32 = 1 << 3;
pub const FEATURE_MIC: u32 = 1 << 4;
pub const FEATURE_CLIPBOARD: u32 = 1 << 5;
pub const FEATURE_FILES: u32 = 1 << 6;
/// The phone reports battery and thermal status ([`Message::BatteryStatus`]).
pub const FEATURE_BATTERY: u32 = 1 << 8;

/// `HelloAck::status`.
pub const HELLO_OK: u8 = 0;
pub const HELLO_VERSION_MISMATCH: u8 = 1;
pub const HELLO_BUSY: u8 = 2;
pub const HELLO_REJECTED: u8 = 3;

/// Device roles (`HelloAck::role`, `SetRole`).
///
/// `SetRole` runs both ways: phone -> host asks for a role; host -> phone
/// states the role now in effect (sent after every change, whichever side
/// asked). `HelloAck::role` is [`ROLE_UNSET`] for a device the host has never
/// seen, which is the phone's cue to show the role chooser; the host streams
/// as [`ROLE_MIRROR`] until a role is chosen.
pub const ROLE_MIRROR: u8 = 0;
pub const ROLE_EXTEND: u8 = 1;
pub const ROLE_MIRROR_WINDOW: u8 = 2;
pub const ROLE_PHONE_PRIMARY: u8 = 3;
pub const ROLE_TABLET: u8 = 4;
pub const ROLE_INPUT_PAD: u8 = 5;
/// No role remembered for this device yet (only in `HelloAck`).
pub const ROLE_UNSET: u8 = 0xFF;

/// `Bye::reason`.
pub const BYE_NORMAL: u8 = 0;
pub const BYE_SERVER_STOPPING: u8 = 1;
pub const BYE_ERROR: u8 = 2;
pub const BYE_VERSION_MISMATCH: u8 = 3;
pub const BYE_PROTOCOL_ERROR: u8 = 4;

/// `HostState::state`.
pub const HOST_STATE_AWAITING_PERMISSION: u8 = 1;
pub const HOST_STATE_STREAMING: u8 = 2;
pub const HOST_STATE_CAPTURE_FAILED: u8 = 3;

/// Input actions (same numbering as `protocol::ACTION_*`).
pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;
pub const ACTION_CANCEL: u8 = 3;
pub const ACTION_HOVER_MOVE: u8 = 4;
pub const ACTION_HOVER_EXIT: u8 = 5;

/// `Pen::tool`.
pub const PEN_TOOL_PEN: u8 = 0;
pub const PEN_TOOL_ERASER: u8 = 1;

/// `Pen::buttons` bits.
pub const PEN_BUTTON_PRIMARY: u8 = 0x01;
pub const PEN_BUTTON_SECONDARY: u8 = 0x02;

/// `Mouse::buttons` bits.
pub const MOUSE_BUTTON_PRIMARY: u8 = 0x01;
pub const MOUSE_BUTTON_SECONDARY: u8 = 0x02;
pub const MOUSE_BUTTON_TERTIARY: u8 = 0x04;

/// Gesture phase for `Scroll` and `Pinch`.
pub const PHASE_NONE: u8 = 0; // a discrete wheel step
pub const PHASE_BEGIN: u8 = 1;
pub const PHASE_UPDATE: u8 = 2;
pub const PHASE_END: u8 = 3;
pub const PHASE_INERTIA: u8 = 4;

/// `BatteryStatus::thermal`, the values of Android's `PowerManager.THERMAL_STATUS_*`.
pub const THERMAL_NONE: u8 = 0;
pub const THERMAL_LIGHT: u8 = 1;
pub const THERMAL_MODERATE: u8 = 2;
pub const THERMAL_SEVERE: u8 = 3;
pub const THERMAL_CRITICAL: u8 = 4;
pub const THERMAL_EMERGENCY: u8 = 5;
pub const THERMAL_SHUTDOWN: u8 = 6;

/// `FileControl::op`.
pub const FILE_ACCEPT: u8 = 0;
pub const FILE_REJECT: u8 = 1;
pub const FILE_CANCEL: u8 = 2;
pub const FILE_COMPLETE: u8 = 3;

// ---- Message structs -------------------------------------------------------------

/// Phone -> host, first message of a session.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hello {
    /// Native panel size, landscape.
    pub width: u16,
    pub height: u16,
    /// Refresh rate in millihertz (60 Hz = 60000).
    pub refresh_mhz: u32,
    pub density_dpi: u16,
    /// `CODEC_*` bits the phone can decode.
    pub codecs: u8,
    /// `FEATURE_*` bits the phone offers.
    pub features: u32,
    pub max_touch_points: u8,
    /// Stable per-install identifier; the host remembers roles by it.
    pub device_id: String,
    pub device_name: String,
    pub app_version: String,
}

/// Host -> phone reply to [`Hello`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HelloAck {
    /// `HELLO_*`. Frozen as the first payload byte in every version.
    pub status: u8,
    /// Stream size the host will encode.
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    /// The single `CODEC_*` bit chosen.
    pub codec: u8,
    /// `FEATURE_*` bits the host will use.
    pub features: u32,
    /// `ROLE_*`.
    pub role: u8,
    pub host_name: String,
    /// Why, when `status` is not OK.
    pub message: String,
}

/// Phone -> host playback statistics; latencies are averages over the last
/// window, measured from host capture and converted to the host clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub frames_decoded: u32,
    pub frames_dropped: u32,
    pub decode_latency_us: u32,
    pub present_latency_us: u32,
    pub rx_kbps: u32,
}

/// One encoded video unit (SPS/PPS, IDR or P frame).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub codec: u8,
    /// `protocol::FRAME_TYPE_*`.
    pub frame_type: u8,
    /// Several units of one captured frame share an index (config + IDR).
    pub frame_index: u32,
    /// Host monotonic capture time.
    pub capture_us: u64,
    pub data: Vec<u8>,
}

/// Opus audio, host speakers -> phone (`AudioFrame`) or phone mic -> host
/// (`MicFrame`).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioPacket {
    pub seq: u32,
    /// Sender monotonic time of the first sample.
    pub pts_us: u64,
    pub channels: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    pub id: u8,
    /// Normalised 0..1 across the phone's view.
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// Every finger currently on the glass; `action` applies to `action_id`, the
/// others moved (or stayed).
#[derive(Debug, Clone, PartialEq)]
pub struct Touch {
    pub action: u8,
    pub action_id: u8,
    pub points: Vec<TouchPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenSample {
    /// How long before the message's last sample this one was taken.
    pub age_us: u32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    /// Per-axis lean, radians in (-PI/2, PI/2).
    pub tilt_x: f32,
    pub tilt_y: f32,
}

/// A stylus event with its batched history, oldest sample first. `action`
/// applies to the last sample; earlier ones are moves (or hover moves).
#[derive(Debug, Clone, PartialEq)]
pub struct Pen {
    pub action: u8,
    pub tool: u8,
    pub buttons: u8,
    pub samples: Vec<PenSample>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mouse {
    pub action: u8,
    pub buttons: u8,
    /// false: `x`/`y` are normalised positions; true: relative motion in
    /// phone pixels (touchpad mode).
    pub relative: bool,
    pub x: f32,
    pub y: f32,
}

/// Scroll in wheel detents (1.0 = one notch), with Android's `AXIS_VSCROLL`
/// and `AXIS_HSCROLL` signs: `dy > 0` moves the content towards the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scroll {
    pub phase: u8,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pinch {
    pub phase: u8,
    pub cx: f32,
    pub cy: f32,
    /// Relative to the start of the gesture.
    pub scale: f32,
    /// Radians, relative to the start of the gesture.
    pub rotation: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub action: u8,
    /// `android.view.KeyEvent.KEYCODE_*`.
    pub key_code: u16,
    pub scan_code: u16,
    /// `protocol::KEY_FLAG_*` bits.
    pub meta: u8,
    pub text: String,
}

/// Phone -> host: battery and thermal state, sent every ~30 s and on change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatteryStatus {
    /// 0..=100.
    pub percent: u8,
    pub charging: bool,
    /// `THERMAL_*`.
    pub thermal: u8,
    /// Battery temperature in tenths of a degree Celsius (Android's
    /// `EXTRA_TEMPERATURE`); `i16::MIN` = unknown.
    pub temp_decidegrees: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOffer {
    pub id: u32,
    pub size: u64,
    pub name: String,
    pub mime: String,
}

/// Every message of protocol v2.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    HelloAck(HelloAck),
    Ping { id: u32, t_send_us: u64 },
    /// `t_ping_us` echoes the ping; the other two are the responder's clock.
    Pong { id: u32, t_ping_us: u64, t_recv_us: u64, t_reply_us: u64 },
    Heartbeat,
    Bye { reason: u8, text: String },
    Stats(Stats),
    HostState { state: u8, detail: String },
    KeyframeRequest,
    /// Phone -> host: requested cap; host -> phone: the bitrate now in use.
    SetBitrate { kbps: u32 },
    /// The phone's view changed size or rotation (0..3 quarter turns).
    Resize { width: u16, height: u16, density_dpi: u16, rotation: u8 },
    SetRole { role: u8 },
    /// Phone -> host: 0 auto, 1 maximum, 2 balanced, 3 data saver.
    SetQuality { quality: u8 },
    /// Phone -> host: estimated latency from the host's audio timestamp to
    /// the phone's speaker (transit + decode + buffers + output), in µs.
    AudioLatency { latency_us: u32 },
    /// Host -> phone: the common playout delay for multi-phone audio sync.
    AudioSync { delay_us: u32 },
    /// Phone -> host: the services the user wants running (`SERVICE_*`).
    ServiceState { enabled: u8 },
    /// Phone -> host (Phase 8).
    BatteryStatus(BatteryStatus),
    /// Phone -> host, first message on a network connection (see `PAIR_MODE_*`).
    /// `credential` is the device token or a pairing proof; empty asks the host
    /// to show a PIN.
    PairRequest { mode: u8, device_id: String, device_name: String, credential: Vec<u8> },
    /// Host -> phone (`PAIR_*` status). `token` carries the new device token on
    /// a successful first pairing, otherwise it is empty.
    PairResponse { status: u8, message: String, token: Vec<u8> },
    VideoFrame(VideoFrame),
    AudioFrame(AudioPacket),
    MicFrame(AudioPacket),
    Touch(Touch),
    Pen(Pen),
    Mouse(Mouse),
    Scroll(Scroll),
    Pinch(Pinch),
    Key(Key),
    /// Committed text from an IME.
    Text(String),
    Clipboard { mime: String, data: Vec<u8> },
    FileOffer(FileOffer),
    FileChunk { id: u32, offset: u64, data: Vec<u8> },
    FileControl { id: u32, op: u8 },
    /// A type this build does not know; skipped by the receiver.
    Unknown { channel: u8, msg_type: u8, payload: Vec<u8> },
}

impl Message {
    pub fn msg_type(&self) -> u8 {
        match self {
            Self::Hello(_) => MSG_HELLO,
            Self::HelloAck(_) => MSG_HELLO_ACK,
            Self::Ping { .. } => MSG_PING,
            Self::Pong { .. } => MSG_PONG,
            Self::Heartbeat => MSG_HEARTBEAT,
            Self::Bye { .. } => MSG_BYE,
            Self::Stats(_) => MSG_STATS,
            Self::HostState { .. } => MSG_HOST_STATE,
            Self::KeyframeRequest => MSG_KEYFRAME_REQUEST,
            Self::SetBitrate { .. } => MSG_SET_BITRATE,
            Self::Resize { .. } => MSG_RESIZE,
            Self::SetRole { .. } => MSG_SET_ROLE,
            Self::SetQuality { .. } => MSG_SET_QUALITY,
            Self::AudioLatency { .. } => MSG_AUDIO_LATENCY,
            Self::AudioSync { .. } => MSG_AUDIO_SYNC,
            Self::ServiceState { .. } => MSG_SERVICE_STATE,
            Self::BatteryStatus(_) => MSG_BATTERY_STATUS,
            Self::PairRequest { .. } => MSG_PAIR_REQUEST,
            Self::PairResponse { .. } => MSG_PAIR_RESPONSE,
            Self::VideoFrame(_) => MSG_VIDEO_FRAME,
            Self::AudioFrame(_) => MSG_AUDIO_FRAME,
            Self::MicFrame(_) => MSG_MIC_FRAME,
            Self::Touch(_) => MSG_TOUCH,
            Self::Pen(_) => MSG_PEN,
            Self::Mouse(_) => MSG_MOUSE,
            Self::Scroll(_) => MSG_SCROLL,
            Self::Pinch(_) => MSG_PINCH,
            Self::Key(_) => MSG_KEY,
            Self::Text(_) => MSG_TEXT,
            Self::Clipboard { .. } => MSG_CLIPBOARD,
            Self::FileOffer(_) => MSG_FILE_OFFER,
            Self::FileChunk { .. } => MSG_FILE_CHUNK,
            Self::FileControl { .. } => MSG_FILE_CONTROL,
            Self::Unknown { msg_type, .. } => *msg_type,
        }
    }

    pub fn channel(&self) -> u8 {
        match self {
            Self::Unknown { channel, .. } => *channel,
            m => channel_of(m.msg_type()).expect("every known type has a channel"),
        }
    }

    /// Appends the payload (no frame header) to `out`.
    pub fn encode_payload(&self, out: &mut Vec<u8>) {
        let w = out;
        match self {
            Self::Hello(h) => {
                w.extend_from_slice(&h.width.to_be_bytes());
                w.extend_from_slice(&h.height.to_be_bytes());
                w.extend_from_slice(&h.refresh_mhz.to_be_bytes());
                w.extend_from_slice(&h.density_dpi.to_be_bytes());
                w.push(h.codecs);
                w.extend_from_slice(&h.features.to_be_bytes());
                w.push(h.max_touch_points);
                put_str16(w, &h.device_id);
                put_str16(w, &h.device_name);
                put_str16(w, &h.app_version);
            }
            Self::HelloAck(a) => {
                w.push(a.status);
                w.extend_from_slice(&a.width.to_be_bytes());
                w.extend_from_slice(&a.height.to_be_bytes());
                w.extend_from_slice(&a.fps.to_be_bytes());
                w.push(a.codec);
                w.extend_from_slice(&a.features.to_be_bytes());
                w.push(a.role);
                put_str16(w, &a.host_name);
                put_str16(w, &a.message);
            }
            Self::Ping { id, t_send_us } => {
                w.extend_from_slice(&id.to_be_bytes());
                w.extend_from_slice(&t_send_us.to_be_bytes());
            }
            Self::Pong { id, t_ping_us, t_recv_us, t_reply_us } => {
                w.extend_from_slice(&id.to_be_bytes());
                for t in [t_ping_us, t_recv_us, t_reply_us] {
                    w.extend_from_slice(&t.to_be_bytes());
                }
            }
            Self::Heartbeat | Self::KeyframeRequest => {}
            Self::Bye { reason, text } => {
                w.push(*reason);
                put_str16(w, text);
            }
            Self::Stats(s) => {
                for v in [
                    s.frames_decoded,
                    s.frames_dropped,
                    s.decode_latency_us,
                    s.present_latency_us,
                    s.rx_kbps,
                ] {
                    w.extend_from_slice(&v.to_be_bytes());
                }
            }
            Self::HostState { state, detail } => {
                w.push(*state);
                put_str16(w, detail);
            }
            Self::SetBitrate { kbps } => w.extend_from_slice(&kbps.to_be_bytes()),
            Self::Resize { width, height, density_dpi, rotation } => {
                w.extend_from_slice(&width.to_be_bytes());
                w.extend_from_slice(&height.to_be_bytes());
                w.extend_from_slice(&density_dpi.to_be_bytes());
                w.push(*rotation);
            }
            Self::SetRole { role } => w.push(*role),
            Self::SetQuality { quality } => w.push(*quality),
            Self::AudioLatency { latency_us } => w.extend_from_slice(&latency_us.to_be_bytes()),
            Self::AudioSync { delay_us } => w.extend_from_slice(&delay_us.to_be_bytes()),
            Self::ServiceState { enabled } => w.push(*enabled),
            Self::BatteryStatus(b) => {
                w.push(b.percent);
                w.push(b.charging as u8);
                w.push(b.thermal);
                w.extend_from_slice(&b.temp_decidegrees.to_be_bytes());
            }
            Self::PairRequest { mode, device_id, device_name, credential } => {
                w.push(*mode);
                put_str16(w, device_id);
                put_str16(w, device_name);
                w.extend_from_slice(credential);
            }
            Self::PairResponse { status, message, token } => {
                w.push(*status);
                put_str16(w, message);
                w.extend_from_slice(token);
            }
            Self::VideoFrame(v) => {
                w.extend_from_slice(&video_frame_prefix(v.codec, v.frame_type, v.frame_index, v.capture_us));
                w.extend_from_slice(&v.data);
            }
            Self::AudioFrame(a) | Self::MicFrame(a) => {
                w.extend_from_slice(&a.seq.to_be_bytes());
                w.extend_from_slice(&a.pts_us.to_be_bytes());
                w.push(a.channels);
                w.extend_from_slice(&a.data);
            }
            Self::Touch(t) => {
                let n = t.points.len().min(u8::MAX as usize);
                w.push(t.action);
                w.push(t.action_id);
                w.push(n as u8);
                for p in &t.points[..n] {
                    w.push(p.id);
                    put_f32s(w, &[p.x, p.y, p.pressure]);
                }
            }
            Self::Pen(p) => {
                // Keep the newest samples if there are more than fit.
                let skip = p.samples.len().saturating_sub(u8::MAX as usize);
                w.push(p.action);
                w.push(p.tool);
                w.push(p.buttons);
                w.push((p.samples.len() - skip) as u8);
                for s in &p.samples[skip..] {
                    w.extend_from_slice(&s.age_us.to_be_bytes());
                    put_f32s(w, &[s.x, s.y, s.pressure, s.tilt_x, s.tilt_y]);
                }
            }
            Self::Mouse(m) => {
                w.push(m.action);
                w.push(m.buttons);
                w.push(m.relative as u8);
                put_f32s(w, &[m.x, m.y]);
            }
            Self::Scroll(s) => {
                w.push(s.phase);
                put_f32s(w, &[s.x, s.y, s.dx, s.dy]);
            }
            Self::Pinch(p) => {
                w.push(p.phase);
                put_f32s(w, &[p.cx, p.cy, p.scale, p.rotation]);
            }
            Self::Key(k) => {
                w.push(k.action);
                w.extend_from_slice(&k.key_code.to_be_bytes());
                w.extend_from_slice(&k.scan_code.to_be_bytes());
                w.push(k.meta);
                put_str16(w, &k.text);
            }
            Self::Text(t) => put_str16(w, t),
            Self::Clipboard { mime, data } => {
                put_str16(w, mime);
                w.extend_from_slice(data);
            }
            Self::FileOffer(f) => {
                w.extend_from_slice(&f.id.to_be_bytes());
                w.extend_from_slice(&f.size.to_be_bytes());
                put_str16(w, &f.name);
                put_str16(w, &f.mime);
            }
            Self::FileChunk { id, offset, data } => {
                w.extend_from_slice(&id.to_be_bytes());
                w.extend_from_slice(&offset.to_be_bytes());
                w.extend_from_slice(data);
            }
            Self::FileControl { id, op } => {
                w.extend_from_slice(&id.to_be_bytes());
                w.push(*op);
            }
            Self::Unknown { payload, .. } => w.extend_from_slice(payload),
        }
    }

    /// Decodes one reassembled payload that arrived on `channel`.
    pub fn decode(channel: u8, msg_type: u8, payload: &[u8]) -> io::Result<Message> {
        let Some(expected) = channel_of(msg_type) else {
            return Ok(Self::Unknown { channel, msg_type, payload: payload.to_vec() });
        };
        if expected != channel {
            return Err(invalid(format!(
                "message type {msg_type:#04x} on channel {channel}, expected {expected}"
            )));
        }
        let mut r = Rd { b: payload, pos: 0 };
        let m = match msg_type {
            MSG_HELLO => Self::Hello(Hello {
                width: r.u16()?,
                height: r.u16()?,
                refresh_mhz: r.u32()?,
                density_dpi: r.u16()?,
                codecs: r.u8()?,
                features: r.u32()?,
                max_touch_points: r.u8()?,
                device_id: r.str16()?,
                device_name: r.str16()?,
                app_version: r.str16()?,
            }),
            MSG_HELLO_ACK => Self::HelloAck(HelloAck {
                status: r.u8()?,
                width: r.u16()?,
                height: r.u16()?,
                fps: r.u16()?,
                codec: r.u8()?,
                features: r.u32()?,
                role: r.u8()?,
                host_name: r.str16()?,
                message: r.str16()?,
            }),
            MSG_PING => Self::Ping { id: r.u32()?, t_send_us: r.u64()? },
            MSG_PONG => Self::Pong {
                id: r.u32()?,
                t_ping_us: r.u64()?,
                t_recv_us: r.u64()?,
                t_reply_us: r.u64()?,
            },
            MSG_HEARTBEAT => Self::Heartbeat,
            MSG_BYE => Self::Bye { reason: r.u8()?, text: r.str16()? },
            MSG_STATS => Self::Stats(Stats {
                frames_decoded: r.u32()?,
                frames_dropped: r.u32()?,
                decode_latency_us: r.u32()?,
                present_latency_us: r.u32()?,
                rx_kbps: r.u32()?,
            }),
            MSG_HOST_STATE => Self::HostState { state: r.u8()?, detail: r.str16()? },
            MSG_KEYFRAME_REQUEST => Self::KeyframeRequest,
            MSG_SET_BITRATE => Self::SetBitrate { kbps: r.u32()? },
            MSG_RESIZE => Self::Resize {
                width: r.u16()?,
                height: r.u16()?,
                density_dpi: r.u16()?,
                rotation: r.u8()?,
            },
            MSG_SET_ROLE => Self::SetRole { role: r.u8()? },
            MSG_SET_QUALITY => Self::SetQuality { quality: r.u8()? },
            MSG_AUDIO_LATENCY => Self::AudioLatency { latency_us: r.u32()? },
            MSG_AUDIO_SYNC => Self::AudioSync { delay_us: r.u32()? },
            MSG_SERVICE_STATE => Self::ServiceState { enabled: r.u8()? },
            MSG_BATTERY_STATUS => Self::BatteryStatus(BatteryStatus {
                percent: r.u8()?,
                charging: r.u8()? != 0,
                thermal: r.u8()?,
                temp_decidegrees: r.u16()? as i16,
            }),
            MSG_PAIR_REQUEST => Self::PairRequest {
                mode: r.u8()?,
                device_id: r.str16()?,
                device_name: r.str16()?,
                credential: r.rest(),
            },
            MSG_PAIR_RESPONSE => Self::PairResponse { status: r.u8()?, message: r.str16()?, token: r.rest() },
            MSG_VIDEO_FRAME => Self::VideoFrame(VideoFrame {
                codec: r.u8()?,
                frame_type: r.u8()?,
                frame_index: r.u32()?,
                capture_us: r.u64()?,
                data: r.rest(),
            }),
            MSG_AUDIO_FRAME | MSG_MIC_FRAME => {
                let a = AudioPacket { seq: r.u32()?, pts_us: r.u64()?, channels: r.u8()?, data: r.rest() };
                if msg_type == MSG_AUDIO_FRAME { Self::AudioFrame(a) } else { Self::MicFrame(a) }
            }
            MSG_TOUCH => {
                let action = r.u8()?;
                let action_id = r.u8()?;
                let n = r.u8()? as usize;
                let mut points = Vec::with_capacity(n);
                for _ in 0..n {
                    points.push(TouchPoint { id: r.u8()?, x: r.f32()?, y: r.f32()?, pressure: r.f32()? });
                }
                Self::Touch(Touch { action, action_id, points })
            }
            MSG_PEN => {
                let action = r.u8()?;
                let tool = r.u8()?;
                let buttons = r.u8()?;
                let n = r.u8()? as usize;
                let mut samples = Vec::with_capacity(n);
                for _ in 0..n {
                    samples.push(PenSample {
                        age_us: r.u32()?,
                        x: r.f32()?,
                        y: r.f32()?,
                        pressure: r.f32()?,
                        tilt_x: r.f32()?,
                        tilt_y: r.f32()?,
                    });
                }
                Self::Pen(Pen { action, tool, buttons, samples })
            }
            MSG_MOUSE => Self::Mouse(Mouse {
                action: r.u8()?,
                buttons: r.u8()?,
                relative: r.u8()? != 0,
                x: r.f32()?,
                y: r.f32()?,
            }),
            MSG_SCROLL => Self::Scroll(Scroll {
                phase: r.u8()?,
                x: r.f32()?,
                y: r.f32()?,
                dx: r.f32()?,
                dy: r.f32()?,
            }),
            MSG_PINCH => Self::Pinch(Pinch {
                phase: r.u8()?,
                cx: r.f32()?,
                cy: r.f32()?,
                scale: r.f32()?,
                rotation: r.f32()?,
            }),
            MSG_KEY => Self::Key(Key {
                action: r.u8()?,
                key_code: r.u16()?,
                scan_code: r.u16()?,
                meta: r.u8()?,
                text: r.str16()?,
            }),
            MSG_TEXT => Self::Text(r.str16()?),
            MSG_CLIPBOARD => Self::Clipboard { mime: r.str16()?, data: r.rest() },
            MSG_FILE_OFFER => Self::FileOffer(FileOffer {
                id: r.u32()?,
                size: r.u64()?,
                name: r.str16()?,
                mime: r.str16()?,
            }),
            MSG_FILE_CHUNK => Self::FileChunk { id: r.u32()?, offset: r.u64()?, data: r.rest() },
            MSG_FILE_CONTROL => Self::FileControl { id: r.u32()?, op: r.u8()? },
            _ => unreachable!("channel_of covers exactly the types matched here"),
        };
        Ok(m)
    }

    /// The whole message as frames, fragmented at [`MAX_FRAGMENT`].
    pub fn encode(&self) -> Vec<u8> {
        self.encode_fragmented(MAX_FRAGMENT)
    }

    /// The whole message as frames, fragmented at `max_fragment` bytes.
    pub fn encode_fragmented(&self, max_fragment: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        self.encode_payload(&mut payload);
        let mut out = Vec::with_capacity(payload.len() + HEADER_SIZE);
        write_frames(self.channel(), self.msg_type(), &[&payload], max_fragment, &mut out);
        out
    }
}

/// The 14 bytes of a `VideoFrame` payload that precede the codec data, for
/// senders that frame video without copying it into a [`Message`] first.
pub fn video_frame_prefix(codec: u8, frame_type: u8, frame_index: u32, capture_us: u64) -> [u8; 14] {
    let mut p = [0u8; 14];
    p[0] = codec;
    p[1] = frame_type;
    p[2..6].copy_from_slice(&frame_index.to_be_bytes());
    p[6..14].copy_from_slice(&capture_us.to_be_bytes());
    p
}

// ---- Frame header ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub channel: u8,
    pub msg_type: u8,
    pub flags: u8,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut h = [0u8; HEADER_SIZE];
        h[..2].copy_from_slice(&MAGIC);
        h[2] = self.version;
        h[3] = self.channel;
        h[4] = self.msg_type;
        h[5] = self.flags;
        h[6..].copy_from_slice(&self.len.to_be_bytes());
        h
    }

    /// Parses a header, checking only the magic. Version, length and channel
    /// checks are the caller's, because what to do about them differs between
    /// the handshake and a running session.
    pub fn parse(h: &[u8; HEADER_SIZE]) -> io::Result<Header> {
        if h[..2] != MAGIC {
            return Err(invalid(format!("bad frame magic {:02x}{:02x}", h[0], h[1])));
        }
        Ok(Header {
            version: h[2],
            channel: h[3],
            msg_type: h[4],
            flags: h[5],
            len: u32::from_be_bytes([h[6], h[7], h[8], h[9]]),
        })
    }

    /// Validates a header in a running v2 session.
    pub fn check(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(invalid(format!("frame version {} in a v{VERSION} session", self.version)));
        }
        if self.channel as usize >= CHANNEL_COUNT {
            return Err(invalid(format!("unknown channel {}", self.channel)));
        }
        if self.len as usize > MAX_FRAGMENT {
            return Err(invalid(format!("frame of {} bytes exceeds {MAX_FRAGMENT}", self.len)));
        }
        if self.flags & !FLAG_MORE != 0 {
            return Err(invalid(format!("reserved frame flags {:#04x} set", self.flags)));
        }
        Ok(())
    }
}

/// Frames one message whose payload is the concatenation of `parts`, splitting
/// it into fragments of at most `max_fragment` bytes. An empty payload is one
/// empty frame.
pub fn write_frames(channel: u8, msg_type: u8, parts: &[&[u8]], max_fragment: usize, out: &mut Vec<u8>) {
    let max_fragment = max_fragment.clamp(1, MAX_FRAGMENT);
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut remaining = total;
    let (mut part, mut off) = (0usize, 0usize);
    loop {
        let n = remaining.min(max_fragment);
        remaining -= n;
        let header = Header {
            version: VERSION,
            channel,
            msg_type,
            flags: if remaining > 0 { FLAG_MORE } else { 0 },
            len: n as u32,
        };
        out.extend_from_slice(&header.encode());
        let mut need = n;
        while need > 0 {
            let avail = &parts[part][off..];
            let take = avail.len().min(need);
            out.extend_from_slice(&avail[..take]);
            need -= take;
            off += take;
            if off == parts[part].len() {
                part += 1;
                off = 0;
            }
        }
        if remaining == 0 {
            break;
        }
    }
}

// ---- Reassembly ----------------------------------------------------------------------

/// Joins fragments back into messages, one in-progress message per channel.
#[derive(Default)]
pub struct Reassembler {
    partial: [Option<(u8, Vec<u8>)>; CHANNEL_COUNT],
}

impl Reassembler {
    /// Adds one checked frame. Returns the complete payload once its last
    /// fragment has arrived.
    pub fn push(&mut self, h: &Header, payload: Vec<u8>) -> io::Result<Option<Vec<u8>>> {
        let slot = &mut self.partial[h.channel as usize];
        let more = h.flags & FLAG_MORE != 0;
        let buf = match slot.take() {
            None if !more => return Ok(Some(payload)),
            None => payload,
            Some((t, mut buf)) => {
                if t != h.msg_type {
                    return Err(invalid(format!(
                        "channel {}: fragment of type {:#04x} inside a type {t:#04x} message",
                        h.channel, h.msg_type
                    )));
                }
                buf.extend_from_slice(&payload);
                buf
            }
        };
        if buf.len() > max_message_len(h.channel) {
            return Err(invalid(format!("channel {} message exceeds {} bytes", h.channel, max_message_len(h.channel))));
        }
        if more {
            *slot = Some((h.msg_type, buf));
            Ok(None)
        } else {
            Ok(Some(buf))
        }
    }
}

/// Reads whole messages from a running v2 session.
pub struct MessageReader<R> {
    reader: R,
    reassembler: Reassembler,
}

impl<R: AsyncRead + Unpin> MessageReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, reassembler: Reassembler::default() }
    }

    /// The next complete message. `UnexpectedEof` when the stream ends,
    /// `InvalidData` on a protocol error.
    pub async fn next(&mut self) -> io::Result<Message> {
        loop {
            let mut raw = [0u8; HEADER_SIZE];
            self.reader.read_exact(&mut raw).await?;
            let header = Header::parse(&raw)?;
            header.check()?;
            let mut payload = vec![0u8; header.len as usize];
            self.reader.read_exact(&mut payload).await?;
            if let Some(full) = self.reassembler.push(&header, payload)? {
                return Message::decode(header.channel, header.msg_type, &full);
            }
        }
    }
}

// ---- Protocol v1 compatibility ------------------------------------------------------

/// Bytes that make a protocol-v1 app show `text` as an error: a v1 ServerHello
/// (which ends its handshake) followed by a v1 `VMMS` Bye(ERROR).
pub fn legacy_v1_rejection(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    // ServerHello: "VMCT" type=2 status=1 width=16 height=16 fps=0 reserved=0
    out.extend_from_slice(b"VMCT");
    out.extend_from_slice(&[2, 1, 0, 16, 0, 16, 0, 0]);
    // VMMS Bye: "VMMS" type=5 reserved=0 len(u16) | reason=2 | UTF-8 text
    let text = truncate_utf8(text, 1023);
    out.extend_from_slice(b"VMMS");
    out.extend_from_slice(&[5, 0]);
    out.extend_from_slice(&((1 + text.len()) as u16).to_be_bytes());
    out.push(2);
    out.extend_from_slice(text.as_bytes());
    out
}

// ---- Helpers ------------------------------------------------------------------------------

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn put_f32s(w: &mut Vec<u8>, vals: &[f32]) {
    for v in vals {
        w.extend_from_slice(&v.to_be_bytes());
    }
}

fn put_str16(w: &mut Vec<u8>, s: &str) {
    let s = truncate_utf8(s, u16::MAX as usize);
    w.extend_from_slice(&(s.len() as u16).to_be_bytes());
    w.extend_from_slice(s.as_bytes());
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Bounds-checked payload reader.
struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.b.len() - self.pos < n {
            // InvalidData, not UnexpectedEof: the frame arrived whole, so this
            // is a malformed message, not the end of the stream.
            return Err(invalid("message payload too short".into()));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    /// Invalid UTF-8 is replaced rather than rejected: text is never worth
    /// ending a session over.
    fn str16(&mut self) -> io::Result<String> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn rest(&mut self) -> Vec<u8> {
        let s = self.b[self.pos..].to_vec();
        self.pos = self.b.len();
        s
    }
}

#[cfg(test)]
mod tests;
