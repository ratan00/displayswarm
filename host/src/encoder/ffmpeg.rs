//! Hardware H.264 encoding through the system ffmpeg (libavcodec).
//!
//! One code path drives every vendor encoder (`h264_vaapi`, `h264_qsv`,
//! `h264_nvenc`, `h264_amf`, `h264_mf`); only the private options differ. That
//! keeps the vendor SDKs out of this crate: ffmpeg already knows how to talk to
//! VA-API, oneVPL, NVENC, AMF and Media Foundation, and it is the one library
//! that behaves the same on Linux and Windows.
//!
//! Colour conversion is done on the CPU with SIMD (`convert.rs`) and the NV12
//! result is uploaded; at 2400x1088 that costs ~1-2 ms, so a GPU conversion
//! filter graph would add moving parts for no measurable win.

use super::convert::{self, nv12_len};
use super::stream::{IdrScheduler, Packetizer, VBV_FRAMES};
use super::{check_frame, BoxError, EncodedPacket, VideoEncoder};
use crate::capture::Frame;
use ffmpeg_next::ffi::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::ptr;

/// Backends in probe order (see `create_default_encoder`). `h264_mf` and `h264_amf`
/// simply fail to be found on platforms where ffmpeg was not built with them.
#[cfg(not(target_os = "windows"))]
pub const HW_BACKENDS: &[&str] = &["h264_vaapi", "h264_qsv", "h264_nvenc", "h264_amf", "h264_mf"];
/// Windows: no VA-API. NVENC first (discrete NVIDIA), then Intel QSV, AMD AMF,
/// and Media Foundation (any vendor's MFT) as the last hardware option before x264.
#[cfg(target_os = "windows")]
pub const HW_BACKENDS: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf"];

const VAAPI_COMPRESSION_LEVEL: i32 = 4;

fn av_err(code: c_int) -> String {
    let mut buf = [0i8; 128];
    // Safety: buf is a valid writable buffer of the stated size.
    unsafe { av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} ({})", s, code)
}

fn check(code: c_int, what: &str) -> Result<c_int, BoxError> {
    if code < 0 {
        Err(format!("{}: {}", what, av_err(code)).into())
    } else {
        Ok(code)
    }
}

/// Everything needed to (re)open a codec context; kept so a bitrate change can reinit.
#[derive(Clone)]
struct Config {
    codec: &'static str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// VA-API render node, if the backend needs one.
    device: Option<String>,
    /// Try VDEnc low-power mode (VA-API only); falls back to the full encoder if it is refused.
    low_power: bool,
    /// Constant quality at this QP instead of CBR (wired links).
    qp: Option<u32>,
}

/// Private options per encoder: low latency, CBR (or constant QP), High
/// profile, single frame in flight.
fn private_options(cfg: &Config) -> Vec<(&'static str, String)> {
    let mut o: Vec<(&'static str, String)> = match cfg.codec {
        "h264_vaapi" => {
            let rc = if cfg.qp.is_some() { "CQP" } else { "CBR" };
            let mut o = vec![("async_depth", "1"), ("rc_mode", rc), ("profile", "high")];
            if cfg.low_power {
                o.push(("low_power", "1"));
            }
            o
        }
        "h264_qsv" => vec![
            ("async_depth", "1"),
            ("preset", "veryfast"),
            ("look_ahead", "0"),
            ("forced_idr", "1"),
            ("profile", "high"),
        ],
        "h264_nvenc" => vec![
            ("preset", "p3"),
            ("tune", "ull"),
            ("rc", "cbr"),
            ("zerolatency", "1"),
            ("delay", "0"),
            ("forced-idr", "1"),
            ("profile", "high"),
        ],
        "h264_amf" => vec![
            ("usage", "ultralowlatency"),
            ("rc", "cbr"),
            ("profile", "high"),
            ("quality", "speed"),
            ("forced_idr", "1"),
        ],
        "h264_mf" => vec![
            ("rate_control", "cbr"),
            ("scenario", "display_remoting"),
            ("hw_encoding", "1"),
        ],
        _ => vec![],
    }
    .into_iter()
    .map(|(k, v)| (k, v.to_string()))
    .collect();
    if let Some(qp) = cfg.qp {
        let qp = qp.to_string();
        match cfg.codec {
            "h264_vaapi" => o.push(("qp", qp)),
            "h264_nvenc" => {
                o.retain(|(k, _)| *k != "rc");
                o.push(("rc", "constqp".into()));
                o.push(("qp", qp));
            }
            "h264_amf" => {
                o.retain(|(k, _)| *k != "rc");
                o.push(("rc", "cqp".into()));
                o.push(("qp_i", qp.clone()));
                o.push(("qp_p", qp));
            }
            // QSV takes global_quality with the qscale flag (see `Inner::open`);
            // Media Foundation keeps CBR.
            _ => {}
        }
    }
    o
}

/// An opened codec context plus the hardware/scratch objects tied to its lifetime.
struct Inner {
    ctx: *mut AVCodecContext,
    hw_device: *mut AVBufferRef,
    hw_frames: *mut AVBufferRef,
    sw_frame: *mut AVFrame,
    hw_frame: *mut AVFrame,
    pkt: *mut AVPacket,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Safety: each pointer is either null or owned by us; the ffmpeg free
        // functions take a pointer-to-pointer and null it.
        unsafe {
            av_packet_free(&mut self.pkt);
            av_frame_free(&mut self.sw_frame);
            av_frame_free(&mut self.hw_frame);
            avcodec_free_context(&mut self.ctx);
            av_buffer_unref(&mut self.hw_frames);
            av_buffer_unref(&mut self.hw_device);
        }
    }
}

impl Inner {
    fn open(cfg: &Config) -> Result<Self, BoxError> {
        let name = CString::new(cfg.codec).unwrap();
        // Safety: plain libavcodec setup; every pointer is checked before use and
        // `Inner::drop` releases whatever was created if we bail out part-way.
        unsafe {
            let codec = avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                return Err(format!("{} is not available in this ffmpeg build", cfg.codec).into());
            }
            let mut this = Inner {
                ctx: avcodec_alloc_context3(codec),
                hw_device: ptr::null_mut(),
                hw_frames: ptr::null_mut(),
                sw_frame: av_frame_alloc(),
                hw_frame: av_frame_alloc(),
                pkt: av_packet_alloc(),
            };
            if this.ctx.is_null() || this.sw_frame.is_null() || this.hw_frame.is_null() || this.pkt.is_null() {
                return Err("ffmpeg allocation failed".into());
            }
            let c = &mut *this.ctx;
            let fps = cfg.fps.max(1) as i32;
            let bitrate = cfg.bitrate_kbps as i64 * 1000;

            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.time_base = AVRational { num: 1, den: 1_000_000 }; // frame pts are microseconds
            c.framerate = AVRational { num: fps, den: 1 };
            // We schedule IDRs ourselves via pict_type; never let the encoder add its own.
            c.gop_size = 1 << 20;
            c.max_b_frames = 0;
            match cfg.qp {
                // Constant quality: no bitrate target for the rate control to
                // starve frames against (CBR with a one-frame VBV gave a fresh
                // desktop keyframe ~20 dB PSNR, visibly blocky).
                Some(qp) if cfg.codec != "h264_mf" => {
                    if cfg.codec == "h264_qsv" {
                        c.flags |= AV_CODEC_FLAG_QSCALE as c_int;
                        c.global_quality = (qp as i32) * FF_QP2LAMBDA as i32;
                    }
                }
                _ => {
                    c.bit_rate = bitrate;
                    c.rc_max_rate = bitrate;
                    c.rc_min_rate = bitrate; // CBR for encoders that key off min/max
                    c.rc_buffer_size = ((bitrate * VBV_FRAMES as i64) / fps as i64).max(1) as i32;
                }
            }
            c.profile = 100; // High
            if cfg.codec == "h264_vaapi" {
                // Driver speed/quality trade-off (1 = best, 7 = fastest). Tunable for
                // experiments via DISPLAYSWARM_VAAPI_LEVEL.
                c.compression_level = std::env::var("DISPLAYSWARM_VAAPI_LEVEL")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(VAAPI_COMPRESSION_LEVEL);
            }
            c.thread_count = 1; // hardware encoders do not benefit from frame threads
            // VUI colour description; must match convert.rs.
            c.color_primaries = AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.colorspace = AVColorSpace::AVCOL_SPC_BT709;
            c.color_range = if convert::FULL_RANGE {
                AVColorRange::AVCOL_RANGE_JPEG
            } else {
                AVColorRange::AVCOL_RANGE_MPEG
            };

            if cfg.codec == "h264_vaapi" {
                let dev = cfg.device.as_ref().map(|d| CString::new(d.as_str()).unwrap());
                check(
                    av_hwdevice_ctx_create(
                        &mut this.hw_device,
                        AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                        dev.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
                        ptr::null_mut(),
                        0,
                    ),
                    "open VA-API device",
                )?;
                this.hw_frames = av_hwframe_ctx_alloc(this.hw_device);
                if this.hw_frames.is_null() {
                    return Err("av_hwframe_ctx_alloc failed".into());
                }
                let fc = &mut *((*this.hw_frames).data as *mut AVHWFramesContext);
                fc.format = AVPixelFormat::AV_PIX_FMT_VAAPI;
                fc.sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
                fc.width = cfg.width as i32;
                fc.height = cfg.height as i32;
                fc.initial_pool_size = 8;
                check(av_hwframe_ctx_init(this.hw_frames), "init VA-API frame pool")?;
                c.hw_frames_ctx = av_buffer_ref(this.hw_frames);
                c.pix_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
            } else {
                c.pix_fmt = AVPixelFormat::AV_PIX_FMT_NV12;
            }

            for (k, v) in private_options(cfg) {
                let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
                let r = av_opt_set(c.priv_data, k.as_ptr(), v.as_ptr(), 0);
                if r < 0 {
                    log::debug!("{}: option {:?}={:?} rejected: {}", cfg.codec, k, v, av_err(r));
                }
            }

            check(avcodec_open2(this.ctx, codec, ptr::null_mut()), &format!("open {}", cfg.codec))?;

            let sf = &mut *this.sw_frame;
            sf.format = AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            sf.width = cfg.width as i32;
            sf.height = cfg.height as i32;
            Ok(this)
        }
    }

    /// Feeds one NV12 image and collects whatever the encoder returns. With
    /// `async_depth=1` and no B-frames this is exactly one packet per frame.
    fn encode(
        &mut self,
        cfg: &Config,
        nv12: &mut [u8],
        pts: i64,
        force_idr: bool,
    ) -> Result<Vec<(Vec<u8>, bool)>, BoxError> {
        let (w, h) = (cfg.width as usize, cfg.height as usize);
        // Safety: sw_frame points at `nv12`, which stays alive and unmodified until
        // send_frame returns; ffmpeg copies or transfers it before then.
        unsafe {
            let sf = &mut *self.sw_frame;
            sf.data[0] = nv12.as_mut_ptr();
            sf.data[1] = nv12.as_mut_ptr().add(w * h);
            sf.linesize[0] = w as i32;
            sf.linesize[1] = w as i32;
            sf.pts = pts;
            sf.pict_type = if force_idr {
                AVPictureType::AV_PICTURE_TYPE_I
            } else {
                AVPictureType::AV_PICTURE_TYPE_NONE
            };
            sf.color_range = (*self.ctx).color_range;
            sf.colorspace = (*self.ctx).colorspace;
            sf.color_primaries = (*self.ctx).color_primaries;
            sf.color_trc = (*self.ctx).color_trc;

            let send = if self.hw_frames.is_null() {
                avcodec_send_frame(self.ctx, self.sw_frame)
            } else {
                let hf = self.hw_frame;
                av_frame_unref(hf);
                check(av_hwframe_get_buffer(self.hw_frames, hf, 0), "get VA-API surface")?;
                check(av_hwframe_transfer_data(hf, self.sw_frame, 0), "upload NV12 to VA-API surface")?;
                (*hf).pts = pts;
                (*hf).pict_type = sf.pict_type;
                (*hf).color_range = sf.color_range;
                (*hf).colorspace = sf.colorspace;
                (*hf).color_primaries = sf.color_primaries;
                (*hf).color_trc = sf.color_trc;
                avcodec_send_frame(self.ctx, hf)
            };
            check(send, "send frame")?;

            let mut out = Vec::with_capacity(1);
            loop {
                let r = avcodec_receive_packet(self.ctx, self.pkt);
                if r == AVERROR(EAGAIN) || r == AVERROR_EOF {
                    break;
                }
                check(r, "receive packet")?;
                let p = &*self.pkt;
                let data = std::slice::from_raw_parts(p.data, p.size as usize).to_vec();
                out.push((data, p.flags & AV_PKT_FLAG_KEY as c_int != 0));
                av_packet_unref(self.pkt);
            }
            Ok(out)
        }
    }

    /// SPS/PPS from the codec extradata, if the backend provides any.
    fn extradata(&self) -> Vec<u8> {
        // Safety: extradata is valid for extradata_size bytes while the context lives.
        unsafe {
            let c = &*self.ctx;
            if c.extradata.is_null() || c.extradata_size <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.extradata, c.extradata_size as usize).to_vec()
            }
        }
    }
}

pub struct FfmpegEncoder {
    cfg: Config,
    inner: Inner,
    nv12: Vec<u8>,
    packetizer: Packetizer,
    idr: IdrScheduler,
    last_pts: i64,
    /// Bitrate to apply at the next IDR (ffmpeg cannot change it on an open context).
    pending_bitrate: Option<u32>,
}

// Safety: the codec context and buffers are owned exclusively by this struct and
// only ever used from one thread at a time.
unsafe impl Send for FfmpegEncoder {}

fn candidate_devices(codec: &str) -> Vec<Option<String>> {
    if codec != "h264_vaapi" {
        return vec![None];
    }
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("DISPLAYSWARM_VAAPI_DEVICE") {
        v.push(Some(d));
    }
    if let Ok(rd) = std::fs::read_dir("/dev/dri") {
        let mut nodes: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().into_owned())
            .filter(|p| p.contains("renderD"))
            .collect();
        nodes.sort();
        v.extend(nodes.into_iter().map(Some));
    }
    v.push(None); // let ffmpeg choose
    v
}

impl FfmpegEncoder {
    /// Opens `codec` (e.g. "h264_vaapi"). Fails if the encoder is missing or the
    /// hardware refuses to initialise; the caller then tries the next backend.
    pub fn try_new(
        codec: &'static str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        qp: Option<u32>,
    ) -> Result<Self, BoxError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!("invalid dimensions {}x{}", width, height).into());
        }
        // Quiet ffmpeg while probing: a missing GPU is normal, not an error worth printing.
        unsafe { av_log_set_level(AV_LOG_FATAL as c_int) };
        let mut last_err: BoxError = "no device".into();
        for device in candidate_devices(codec) {
            for low_power in [true, false] {
                if low_power && codec != "h264_vaapi" {
                    continue;
                }
                let cfg = Config { codec, width, height, fps, bitrate_kbps, device: device.clone(), low_power, qp };
                match Inner::open(&cfg) {
                    Ok(inner) => {
                        unsafe { av_log_set_level(AV_LOG_ERROR as c_int) };
                        let rate = match qp {
                            Some(q) if codec != "h264_mf" => format!("constant quality QP {q}"),
                            _ => format!("{bitrate_kbps} kbps CBR"),
                        };
                        log::info!(
                            "{} ready: {}x{} @ {} fps, {}, device {:?}, low_power {}",
                            codec, width, height, fps, rate, device, low_power
                        );
                        let mut packetizer = Packetizer::default();
                        packetizer.set_config(&inner.extradata());
                        return Ok(Self {
                            cfg,
                            inner,
                            nv12: vec![0u8; nv12_len(width as usize, height as usize)],
                            packetizer,
                            idr: IdrScheduler::new(),
                            last_pts: -1,
                            pending_bitrate: None,
                        });
                    }
                    Err(e) => {
                        log::debug!("{} init failed (device {:?}, low_power {}): {}", codec, device, low_power, e);
                        last_err = e;
                    }
                }
            }
        }
        unsafe { av_log_set_level(AV_LOG_ERROR as c_int) };
        Err(last_err)
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, BoxError> {
        check_frame(frame, self.cfg.width, self.cfg.height)?;
        let want_idr = self.idr.is_due(frame.timestamp_us);

        // A bitrate change reinitialises the context; do it only on a frame that is
        // an IDR anyway so the stream never sees a surprise keyframe.
        if want_idr {
            if let Some(kbps) = self.pending_bitrate.take() {
                let mut cfg = self.cfg.clone();
                cfg.bitrate_kbps = kbps;
                match Inner::open(&cfg) {
                    Ok(inner) => {
                        log::info!("{}: bitrate {} -> {} kbps (reinitialised at IDR)", cfg.codec, self.cfg.bitrate_kbps, kbps);
                        self.inner = inner;
                        self.cfg = cfg;
                    }
                    Err(e) => log::warn!("{}: bitrate reinit failed, keeping old context: {}", cfg.codec, e),
                }
            }
        }

        convert::to_nv12(
            frame.format,
            &frame.data,
            self.cfg.width as usize,
            self.cfg.height as usize,
            &mut self.nv12,
        )?;

        // pts must be strictly increasing for the encoder's rate control.
        let pts = (frame.timestamp_us as i64).max(self.last_pts + 1);
        self.last_pts = pts;

        let cfg = self.cfg.clone();
        let out = self.inner.encode(&cfg, &mut self.nv12, pts, want_idr)?;

        let mut packets = Vec::with_capacity(2);
        for (data, is_key) in out {
            if is_key {
                self.idr.mark_idr(frame.timestamp_us);
            }
            packets.extend(self.packetizer.packetize(data, is_key, frame.timestamp_us));
        }
        Ok(packets)
    }

    fn request_keyframe(&mut self) {
        self.idr.request();
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if self.cfg.qp.is_some() && self.cfg.codec != "h264_mf" {
            // Constant quality has no bitrate to change.
            log::debug!("{}: ignoring a {kbps} kbps request in constant-quality mode", self.cfg.codec);
            return;
        }
        if kbps == 0 || kbps == self.cfg.bitrate_kbps {
            self.pending_bitrate = None;
            return;
        }
        self.pending_bitrate = Some(kbps);
        // The reinit needs an IDR; ask for one now instead of waiting for the safety IDR.
        self.idr.request();
    }

    fn backend_name(&self) -> String {
        self.cfg.codec.to_string()
    }
}
