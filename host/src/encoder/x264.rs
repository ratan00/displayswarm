//! Software H.264 fallback that drives libx264 directly through `x264-sys`.
//!
//! The safe `x264` crate cannot signal VUI colour, force an IDR on demand or
//! change the bitrate live, so this talks to the C API. It is only used when no
//! hardware backend initialises, but it must always work: it depends on nothing
//! but libx264.

use super::convert::{self, nv12_len};
use super::stream::{IdrScheduler, Packetizer, VBV_FRAMES};
use super::{check_frame, BoxError, EncodedPacket, VideoEncoder};
use crate::capture::Frame;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;
use x264_sys::x264::*;

/// `superfast` keeps a 2400x1088 stream real-time on a 4-core laptop while still
/// giving CABAC, 8x8 transforms and sub-pel motion (unlike `ultrafast`, which
/// disables them and is the main reason the old stream looked soft).
const PRESET: &str = "superfast";

pub struct X264Encoder {
    enc: *mut x264_t,
    width: u32,
    height: u32,
    fps: u32,
    nv12: Vec<u8>,
    packetizer: Packetizer,
    idr: IdrScheduler,
    frame_counter: i64,
    /// Constant quality (CRF) instead of ABR: bitrate requests are ignored.
    quality: bool,
}

// Safety: the x264 context is exclusively owned by this struct and x264 has no
// thread affinity, so moving it to the encoder thread is fine.
unsafe impl Send for X264Encoder {}

fn vbv_kbit(bitrate_kbps: u32, fps: u32) -> i32 {
    ((bitrate_kbps as u64 * VBV_FRAMES as u64) / fps.max(1) as u64).max(1) as i32
}

impl X264Encoder {
    pub fn try_new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, BoxError> {
        Self::try_new_with(width, height, fps, bitrate_kbps, None)
    }

    /// `qp`: constant quality (CRF at that value, capped at
    /// [`super::WIRED_MAX_KBPS`]) instead of ABR at `bitrate_kbps`.
    pub fn try_new_with(width: u32, height: u32, fps: u32, bitrate_kbps: u32, qp: Option<u32>) -> Result<Self, BoxError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!("invalid x264 dimensions {}x{} (must be non-zero and even)", width, height).into());
        }
        let fps = if fps > 0 { fps } else { 30 };

        let preset = CString::new(PRESET).unwrap();
        let tune = CString::new("zerolatency").unwrap();
        let profile = CString::new("high").unwrap();
        let mut param = MaybeUninit::<x264_param_t>::uninit();
        // Safety: every x264 call below receives a pointer to a live, initialised x264_param_t.
        unsafe {
            if x264_param_default_preset(param.as_mut_ptr(), preset.as_ptr(), tune.as_ptr()) != 0 {
                return Err("x264_param_default_preset failed".into());
            }
            let p = &mut *param.as_mut_ptr();
            p.i_width = width as i32;
            p.i_height = height as i32;
            p.i_csp = X264_CSP_NV12 as i32;
            p.i_fps_num = fps;
            p.i_fps_den = 1;
            p.i_timebase_num = 1;
            p.i_timebase_den = fps;
            p.i_threads = num_cpus::get_physical().max(1) as i32;
            // We schedule IDRs ourselves (see IdrScheduler); scenecut IDRs on a desktop
            // (window switches) would be huge spikes that a tight VBV cannot absorb.
            p.i_keyint_max = 1 << 30;
            p.i_scenecut_threshold = 0;
            p.b_annexb = 1;
            p.b_repeat_headers = 1; // SPS/PPS in front of every IDR
            p.b_aud = 0;
            match qp {
                Some(q) => {
                    let cap = super::WIRED_MAX_KBPS;
                    p.rc.i_rc_method = X264_RC_CRF as i32;
                    p.rc.f_rf_constant = q as f32;
                    p.rc.i_vbv_max_bitrate = cap as i32;
                    p.rc.i_vbv_buffer_size = vbv_kbit(cap, fps);
                }
                None => {
                    p.rc.i_rc_method = X264_RC_ABR as i32;
                    p.rc.i_bitrate = bitrate_kbps as i32;
                    p.rc.i_vbv_max_bitrate = bitrate_kbps as i32;
                    p.rc.i_vbv_buffer_size = vbv_kbit(bitrate_kbps, fps);
                }
            }
            // VUI: must match convert.rs, which produced the samples.
            p.vui.i_vidformat = 5; // unspecified
            p.vui.b_fullrange = convert::FULL_RANGE as i32;
            p.vui.i_colorprim = convert::COLOUR_PRIMARIES;
            p.vui.i_transfer = convert::TRANSFER_CHARACTERISTICS;
            p.vui.i_colmatrix = convert::MATRIX_COEFFICIENTS;
            if x264_param_apply_profile(p, profile.as_ptr()) != 0 {
                return Err("x264_param_apply_profile(high) failed".into());
            }
        }
        // Safety: fully initialised above.
        let mut param = unsafe { param.assume_init() };
        let enc = unsafe { x264_encoder_open(&mut param) };
        if enc.is_null() {
            return Err("x264_encoder_open failed".into());
        }

        let mut this = Self {
            enc,
            width,
            height,
            fps,
            nv12: vec![0u8; nv12_len(width as usize, height as usize)],
            packetizer: Packetizer::default(),
            idr: IdrScheduler::new(),
            frame_counter: 0,
            quality: qp.is_some(),
        };

        // Seed the parameter sets so a keyframe always yields a CONFIG packet.
        let mut nals: *mut x264_nal_t = ptr::null_mut();
        let mut n = 0i32;
        let len = unsafe { x264_encoder_headers(this.enc, &mut nals, &mut n) };
        if len > 0 && !nals.is_null() {
            let headers = unsafe { std::slice::from_raw_parts((*nals).p_payload, len as usize) };
            this.packetizer.set_config(headers);
        }

        log::info!(
            "x264 encoder ready: {}x{} @ {} fps, {} kbps, preset {}, High, threads {}",
            width, height, fps, bitrate_kbps, PRESET, param.i_threads
        );
        Ok(this)
    }
}

impl Drop for X264Encoder {
    fn drop(&mut self) {
        // Safety: `enc` came from x264_encoder_open and is closed exactly once.
        unsafe { x264_encoder_close(self.enc) };
    }
}

impl VideoEncoder for X264Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedPacket>, BoxError> {
        check_frame(frame, self.width, self.height)?;
        convert::to_nv12(frame.format, &frame.data, self.width as usize, self.height as usize, &mut self.nv12)?;

        let want_idr = self.idr.is_due(frame.timestamp_us);
        let w = self.width as usize;
        let h = self.height as usize;

        // Safety: pic_in points into self.nv12, which outlives the call; x264 copies
        // the input before returning, and the NAL payloads read below are copied
        // out before the next encode invalidates them.
        let (data, is_key) = unsafe {
            let mut pic_in = MaybeUninit::<x264_picture_t>::uninit();
            x264_picture_init(pic_in.as_mut_ptr());
            let pic_in = &mut *pic_in.as_mut_ptr();
            pic_in.i_pts = self.frame_counter;
            pic_in.i_type = if want_idr { X264_TYPE_IDR as i32 } else { X264_TYPE_AUTO as i32 };
            pic_in.img.i_csp = X264_CSP_NV12 as i32;
            pic_in.img.i_plane = 2;
            pic_in.img.plane[0] = self.nv12.as_mut_ptr();
            pic_in.img.i_stride[0] = w as i32;
            pic_in.img.plane[1] = self.nv12.as_mut_ptr().add(w * h);
            pic_in.img.i_stride[1] = w as i32;

            let mut pic_out = MaybeUninit::<x264_picture_t>::uninit();
            let mut nals: *mut x264_nal_t = ptr::null_mut();
            let mut n = 0i32;
            let len = x264_encoder_encode(self.enc, &mut nals, &mut n, pic_in, pic_out.as_mut_ptr());
            if len < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if len == 0 {
                return Ok(Vec::new());
            }
            let pic_out = pic_out.assume_init();
            // All NAL payloads of one call are contiguous in x264's output buffer.
            let data = std::slice::from_raw_parts((*nals).p_payload, len as usize).to_vec();
            (data, pic_out.b_keyframe != 0)
        };
        self.frame_counter += 1;

        if is_key {
            self.idr.mark_idr(frame.timestamp_us);
        }
        Ok(self.packetizer.packetize(data, is_key, frame.timestamp_us))
    }

    fn request_keyframe(&mut self) {
        self.idr.request();
    }

    /// x264 supports live rate-control changes, so no IDR or reinit is needed.
    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == 0 || self.quality {
            return;
        }
        // Safety: `enc` is valid; the parameter struct is fetched, edited and handed back.
        unsafe {
            let mut param = MaybeUninit::<x264_param_t>::uninit();
            x264_encoder_parameters(self.enc, param.as_mut_ptr());
            let mut param = param.assume_init();
            param.rc.i_bitrate = kbps as i32;
            param.rc.i_vbv_max_bitrate = kbps as i32;
            param.rc.i_vbv_buffer_size = vbv_kbit(kbps, self.fps);
            if x264_encoder_reconfig(self.enc, &mut param) != 0 {
                log::warn!("x264 rejected live bitrate change to {} kbps", kbps);
            }
        }
    }

    fn backend_name(&self) -> String {
        "x264".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixelFormat;
    use crate::protocol::{FRAME_TYPE_CONFIG, FRAME_TYPE_DELTA, FRAME_TYPE_KEY};

    fn frame(w: u32, h: u32, ts: u64, shade: u8) -> Frame {
        let mut data = vec![shade; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Frame { width: w, height: h, format: PixelFormat::Rgba, data, timestamp_us: ts }
    }

    #[test]
    fn keyframe_then_delta_and_forced_idr() {
        let mut enc = X264Encoder::try_new(320, 192, 30, 2000).unwrap();
        let p1 = enc.encode(&frame(320, 192, 1000, 40)).unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0].frame_type, FRAME_TYPE_CONFIG);
        assert_eq!(p1[1].frame_type, FRAME_TYPE_KEY);
        assert!(p1.iter().all(|p| p.timestamp_us == 1000));
        assert_eq!(&p1[0].data[..4], &[0, 0, 0, 1]);

        let p2 = enc.encode(&frame(320, 192, 34_000, 41)).unwrap();
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].frame_type, FRAME_TYPE_DELTA);
        assert_eq!(p2[0].timestamp_us, 34_000);

        enc.request_keyframe();
        let p3 = enc.encode(&frame(320, 192, 67_000, 42)).unwrap();
        assert_eq!(p3[0].frame_type, FRAME_TYPE_CONFIG);
        assert_eq!(p3[1].frame_type, FRAME_TYPE_KEY);
    }

    #[test]
    fn safety_idr_after_interval_even_when_idle() {
        let mut enc = X264Encoder::try_new(320, 192, 30, 2000).unwrap();
        enc.encode(&frame(320, 192, 0, 10)).unwrap();
        let p = enc.encode(&frame(320, 192, 5_000_000, 10)).unwrap();
        assert_eq!(p[1].frame_type, FRAME_TYPE_KEY);
    }

    #[test]
    fn rejects_bad_dimensions_and_mismatched_frames() {
        assert!(X264Encoder::try_new(0, 64, 30, 1000).is_err());
        assert!(X264Encoder::try_new(65, 64, 30, 1000).is_err());
        let mut enc = X264Encoder::try_new(64, 64, 30, 1000).unwrap();
        assert!(enc.encode(&frame(32, 32, 0, 1)).is_err());
    }

    #[test]
    fn set_bitrate_live_does_not_break_stream() {
        let mut enc = X264Encoder::try_new(320, 192, 30, 2000).unwrap();
        enc.encode(&frame(320, 192, 0, 10)).unwrap();
        enc.set_bitrate(500);
        let p = enc.encode(&frame(320, 192, 33_000, 11)).unwrap();
        assert_eq!(p.len(), 1);
    }

    /// The SPS must carry the colour description and range we convert with.
    #[test]
    fn sps_signals_bt709_limited_range() {
        let mut enc = X264Encoder::try_new(320, 192, 30, 2000).unwrap();
        let p = enc.encode(&frame(320, 192, 0, 10)).unwrap();
        let sps = &p[0].data;
        let info = crate::encoder::stream::parse_sps_vui(sps).expect("SPS with VUI");
        assert_eq!(info.profile_idc, 100, "High profile");
        assert_eq!(info.colour, Some((1, 1, 1)), "BT.709 primaries/transfer/matrix");
        assert!(!info.full_range);
    }
}
