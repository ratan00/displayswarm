//! Backend-independent stream logic shared by the ffmpeg and x264 encoders:
//! Annex-B NAL splitting, CONFIG/KEY/DELTA packetisation and IDR scheduling.

use super::EncodedPacket;
use crate::protocol::{FRAME_TYPE_CONFIG, FRAME_TYPE_DELTA, FRAME_TYPE_KEY};

/// Safety IDR period. A long GOP keeps bitrate low on a mostly static desktop,
/// but a periodic IDR lets a client that lost data (or joined late) resync
/// without waiting for an explicit request. The transports are reliable and
/// the phone asks for a keyframe whenever its decoder needs one, so this is a
/// last resort: at 3 s every keyframe burst stalled audio on a weak Wi-Fi link.
pub const SAFETY_IDR_INTERVAL_US: u64 = 10_000_000;

/// VBV buffer size in frames of bitrate (`bitrate / fps * VBV_FRAMES`). One frame
/// gives the lowest latency but starves the IDR (a text-heavy IDR needs several
/// frames' worth of bits to look sharp); two keeps bursts under ~2 frame times.
pub const VBV_FRAMES: u32 = 2;

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Returns `(start_code_offset, nal_header_offset)` for every NAL unit in an Annex-B buffer.
fn nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                out.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                out.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Splits an Annex-B access unit into `(parameter_sets, everything_else)`.
/// Parameter sets are the SPS and PPS NALs, start codes included, in stream order.
pub fn split_parameter_sets(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let starts = nal_starts(data);
    let mut params = Vec::new();
    let mut rest = Vec::with_capacity(data.len());
    for (n, &(sc, hdr)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|s| s.0).unwrap_or(data.len());
        let nal_type = data.get(hdr).map(|b| b & 0x1f).unwrap_or(0);
        if nal_type == NAL_SPS || nal_type == NAL_PPS {
            params.extend_from_slice(&data[sc..end]);
        } else {
            rest.extend_from_slice(&data[sc..end]);
        }
    }
    if starts.is_empty() {
        rest.extend_from_slice(data);
    }
    (params, rest)
}

/// Turns raw encoder output into the packets the transport expects: a keyframe
/// becomes CONFIG (SPS+PPS) + KEY (IDR), anything else a single DELTA.
#[derive(Default)]
pub struct Packetizer {
    /// Last SPS+PPS seen, reused if a keyframe arrives without in-band headers.
    config: Vec<u8>,
    /// Colour signalling every SPS must carry; see [`Packetizer::with_signal`].
    signal: Option<VideoSignal>,
}

impl Packetizer {
    /// A packetizer that writes `signal` into any SPS that does not already
    /// carry it. Media Foundation leaves the VUI colour fields out whatever
    /// the codec context says, and the phone would then have to guess.
    pub fn with_signal(signal: VideoSignal) -> Self {
        Self { config: Vec::new(), signal: Some(signal) }
    }

    /// Seeds the parameter sets from out-of-band headers (extradata / `x264_encoder_headers`).
    pub fn set_config(&mut self, headers: &[u8]) {
        let (params, _) = split_parameter_sets(headers);
        if !params.is_empty() {
            self.config = self.signalled(params);
        }
    }

    fn signalled(&self, params: Vec<u8>) -> Vec<u8> {
        match &self.signal {
            Some(sig) => set_sps_video_signal(&params, sig),
            None => params,
        }
    }

    pub fn packetize(&mut self, data: Vec<u8>, is_key: bool, timestamp_us: u64) -> Vec<EncodedPacket> {
        if !is_key {
            return vec![EncodedPacket { frame_type: FRAME_TYPE_DELTA, data, timestamp_us }];
        }
        let (params, rest) = split_parameter_sets(&data);
        if !params.is_empty() {
            self.config = self.signalled(params);
        }
        let mut out = Vec::with_capacity(2);
        if !self.config.is_empty() {
            out.push(EncodedPacket {
                frame_type: FRAME_TYPE_CONFIG,
                data: self.config.clone(),
                timestamp_us,
            });
        }
        out.push(EncodedPacket { frame_type: FRAME_TYPE_KEY, data: rest, timestamp_us });
        out
    }
}

/// Decides which frames become IDRs: the first one, explicit requests, bitrate
/// changes that need a reinit, and a periodic safety IDR based on frame
/// timestamps (frames are not delivered at a fixed cadence, so frame counts
/// would be meaningless when the screen idles).
pub struct IdrScheduler {
    requested: bool,
    last_idr_us: Option<u64>,
}

impl IdrScheduler {
    pub fn new() -> Self {
        Self { requested: true, last_idr_us: None }
    }

    pub fn request(&mut self) {
        self.requested = true;
    }

    /// True if the frame at `timestamp_us` must be an IDR. Does not consume the request.
    pub fn is_due(&self, timestamp_us: u64) -> bool {
        match self.last_idr_us {
            None => true,
            Some(last) => {
                self.requested || timestamp_us.saturating_sub(last) >= SAFETY_IDR_INTERVAL_US
            }
        }
    }

    pub fn mark_idr(&mut self, timestamp_us: u64) {
        self.requested = false;
        self.last_idr_us = Some(timestamp_us);
    }
}

/// The VUI video signal: range and the H.273 colour code points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoSignal {
    pub full_range: bool,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

/// What [`parse_sps_vui`] extracts from an SPS, for tests.
#[cfg(test)]
#[derive(Debug)]
pub struct SpsInfo {
    pub profile_idc: u8,
    pub width: u32,
    pub height: u32,
    /// (colour_primaries, transfer_characteristics, matrix_coefficients) if signalled.
    pub colour: Option<(u8, u8, u8)>,
    pub full_range: bool,
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.pos / 8)? >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { (k / 2 + 1) as i32 } else { -((k / 2) as i32) })
    }
}

#[derive(Default)]
struct BitWriter {
    d: Vec<u8>,
    n: usize,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.n % 8 == 0 {
            self.d.push(0);
        }
        if b & 1 == 1 {
            *self.d.last_mut().unwrap() |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
    }
    fn u(&mut self, n: usize, v: u32) {
        for i in (0..n).rev() {
            self.bit(v >> i);
        }
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros() as usize;
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) as u32);
        }
    }
    /// Copies bits `from..to` of `src`.
    fn copy(&mut self, src: &[u8], from: usize, to: usize) {
        let mut r = Bits { d: src, pos: from };
        while r.pos < to {
            self.bit(r.bit().unwrap_or(0));
        }
    }
    /// rbsp_trailing_bits: a stop bit, then zeros to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        self.d
    }
}

/// The SPS fields up to `vui_parameters_present_flag`.
struct SpsHead {
    #[cfg_attr(not(test), allow(dead_code))]
    profile_idc: u8,
    #[cfg_attr(not(test), allow(dead_code))]
    width: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    height: u32,
    max_num_ref_frames: u32,
    /// Bit position of `vui_parameters_present_flag`.
    vui_flag: usize,
}

/// Walks an SPS RBSP (after the NAL header byte) to its VUI flag. None if it
/// is malformed or uses scaling lists (which this walker does not parse).
fn sps_head(rbsp: &[u8]) -> Option<SpsHead> {
    let mut r = Bits { d: rbsp, pos: 0 };
    let profile_idc = r.u(8)? as u8;
    r.u(16)?; // constraint flags + level_idc
    r.ue()?; // sps id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128].contains(&profile_idc) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            return None; // scaling matrices
        }
    }
    r.ue()?; // log2_max_frame_num - 4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bit()?;
    let w_mbs = r.ue()? + 1;
    let h_map = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let mult = if frame_mbs_only == 1 { 2 } else { 4 };
    let width = w_mbs * 16 - (cl + cr) * 2;
    let height = (2 - frame_mbs_only) * h_map * 16 - (ct + cb) * mult;
    Some(SpsHead { profile_idc, width, height, max_num_ref_frames, vui_flag: r.pos })
}

/// The VUI's video signal as found: video_format, the signal (unspecified code
/// points if not described) and whether the colour description is present.
type FoundSignal = (u32, VideoSignal, bool);

/// Reads the VUI (from just after its present flag) up to and including the
/// video signal. Returns the bit range of `video_signal_type_present_flag` and
/// its fields, and the signal if present; `r` is left after it.
fn vui_signal(r: &mut Bits) -> Option<(usize, usize, Option<FoundSignal>)> {
    if r.bit()? == 1 && r.u(8)? == 255 {
        r.u(32)?; // sar width/height
    }
    if r.bit()? == 1 {
        r.bit()?; // overscan_appropriate
    }
    let start = r.pos;
    let mut found = None;
    if r.bit()? == 1 {
        let format = r.u(3)?;
        let full_range = r.bit()? == 1;
        let described = r.bit()? == 1;
        // 2 = unspecified.
        let (primaries, transfer, matrix) =
            if described { (r.u(8)? as u8, r.u(8)? as u8, r.u(8)? as u8) } else { (2, 2, 2) };
        found = Some((format, VideoSignal { full_range, primaries, transfer, matrix }, described));
    }
    Some((start, r.pos, found))
}

fn write_signal(w: &mut BitWriter, format: u32, sig: &VideoSignal) {
    w.bit(1); // video_signal_type_present_flag
    w.u(3, format);
    w.bit(sig.full_range as u32);
    w.bit(1); // colour_description_present_flag
    w.u(8, sig.primaries as u32);
    w.u(8, sig.transfer as u32);
    w.u(8, sig.matrix as u32);
}

/// Rewrites one SPS RBSP so its VUI carries `sig`. None if it already does,
/// or if the SPS cannot be parsed (it is then passed on unchanged).
fn rewrite_sps_rbsp(rbsp: &[u8], sig: &VideoSignal) -> Option<Vec<u8>> {
    let head = sps_head(rbsp)?;
    // The payload ends at the rbsp stop bit, the last 1 in the buffer.
    let last = rbsp.iter().rposition(|&b| b != 0)?;
    let stop = last * 8 + 7 - rbsp[last].trailing_zeros() as usize;
    let mut r = Bits { d: rbsp, pos: head.vui_flag };
    let mut w = BitWriter::default();
    w.copy(rbsp, 0, head.vui_flag);
    if r.bit()? == 0 {
        // No VUI: add one with the signal, plus a bitstream restriction saying
        // frames come out in decode order (no reorder delay at the decoder).
        w.bit(1);
        w.bit(0); // aspect_ratio_info_present_flag
        w.bit(0); // overscan_info_present_flag
        write_signal(&mut w, 5, sig); // 5 = unspecified video format
        w.bit(0); // chroma_loc_info_present_flag
        w.bit(0); // timing_info_present_flag
        w.bit(0); // nal_hrd_parameters_present_flag
        w.bit(0); // vcl_hrd_parameters_present_flag
        w.bit(0); // pic_struct_present_flag
        w.bit(1); // bitstream_restriction_flag
        w.bit(1); // motion_vectors_over_pic_boundaries_flag
        w.ue(2); // max_bytes_per_pic_denom
        w.ue(1); // max_bits_per_mb_denom
        w.ue(16); // log2_max_mv_length_horizontal
        w.ue(16); // log2_max_mv_length_vertical
        w.ue(0); // max_num_reorder_frames
        w.ue(head.max_num_ref_frames); // max_dec_frame_buffering
        return Some(w.finish());
    }
    w.bit(1);
    let vui_start = r.pos;
    let (sig_start, sig_end, found) = vui_signal(&mut r)?;
    if let Some((_, have, true)) = found {
        if have == *sig {
            return None;
        }
    }
    w.copy(rbsp, vui_start, sig_start);
    write_signal(&mut w, found.map_or(5, |f| f.0), sig);
    w.copy(rbsp, sig_end, stop);
    Some(w.finish())
}

/// Strips emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    rbsp
}

/// Inserts emulation-prevention bytes so no start code appears in the payload.
fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
}

/// Makes every SPS in an Annex-B buffer signal `sig` in its VUI. Other NAL
/// units, and SPSs that already do (or cannot be parsed), are copied unchanged.
pub fn set_sps_video_signal(annexb: &[u8], sig: &VideoSignal) -> Vec<u8> {
    let starts = nal_starts(annexb);
    let Some(&(first, _)) = starts.first() else { return annexb.to_vec() };
    let mut out = Vec::with_capacity(annexb.len() + 8);
    out.extend_from_slice(&annexb[..first]);
    for (n, &(sc, hdr)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|s| s.0).unwrap_or(annexb.len());
        let is_sps = annexb.get(hdr).map(|b| b & 0x1f) == Some(NAL_SPS);
        match is_sps.then(|| rewrite_sps_rbsp(&unescape(&annexb[hdr + 1..end]), sig)).flatten() {
            Some(rbsp) => {
                out.extend_from_slice(&annexb[sc..=hdr]);
                escape(&rbsp, &mut out);
            }
            None => out.extend_from_slice(&annexb[sc..end]),
        }
    }
    out
}

/// Minimal SPS parser (Annex-B input, first SPS NAL) that reaches the VUI colour
/// fields. Returns None if the SPS is malformed or uses scaling lists.
#[cfg(test)]
pub fn parse_sps_vui(annexb: &[u8]) -> Option<SpsInfo> {
    let starts = nal_starts(annexb);
    let &(_, hdr) = starts.iter().find(|&&(_, h)| annexb.get(h).map(|b| b & 0x1f) == Some(NAL_SPS))?;
    let end = starts.iter().map(|s| s.0).find(|&s| s > hdr).unwrap_or(annexb.len());
    let rbsp = unescape(&annexb[hdr + 1..end]);
    let head = sps_head(&rbsp)?;
    let mut info =
        SpsInfo { profile_idc: head.profile_idc, width: head.width, height: head.height, colour: None, full_range: false };
    let mut r = Bits { d: &rbsp, pos: head.vui_flag };
    if r.bit()? == 0 {
        return Some(info); // no VUI
    }
    if let (_, _, Some((_, sig, described))) = vui_signal(&mut r)? {
        info.full_range = sig.full_range;
        if described {
            info.colour = Some((sig.primaries, sig.transfer, sig.matrix));
        }
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, len: usize, long_start: bool) -> Vec<u8> {
        let mut v = if long_start { vec![0, 0, 0, 1] } else { vec![0, 0, 1] };
        v.push(0x60 | kind);
        v.extend(std::iter::repeat(0xAA).take(len));
        v
    }

    #[test]
    fn splits_sps_pps_from_idr() {
        let sps = nal(7, 5, true);
        let pps = nal(8, 3, false);
        let idr = nal(5, 20, true);
        let au: Vec<u8> = [sps.clone(), pps.clone(), idr.clone()].concat();
        let (params, rest) = split_parameter_sets(&au);
        assert_eq!(params, [sps, pps].concat());
        assert_eq!(rest, idr);
    }

    #[test]
    fn keyframe_without_inband_headers_reuses_cached_config() {
        let mut p = Packetizer::default();
        p.set_config(&[nal(7, 4, true), nal(8, 2, true)].concat());
        let out = p.packetize(nal(5, 10, true), true, 42);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].frame_type, FRAME_TYPE_CONFIG);
        assert_eq!(out[1].frame_type, FRAME_TYPE_KEY);
        assert!(out.iter().all(|x| x.timestamp_us == 42));
    }

    #[test]
    fn delta_is_single_packet_passed_through() {
        let mut p = Packetizer::default();
        let d = nal(1, 10, true);
        let out = p.packetize(d.clone(), false, 7);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frame_type, FRAME_TYPE_DELTA);
        assert_eq!(out[0].data, d);
    }

    const BT709: VideoSignal = VideoSignal { full_range: false, primaries: 1, transfer: 1, matrix: 1 };

    /// SPS from Windows Media Foundation (`h264_mf`, 640x384): a VUI with
    /// timing info but no video signal.
    const MF_SPS: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0x95, 0xa0, 0x28, 0x0c, 0x68, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00,
        0x00, 0x1e, 0x03, 0x68, 0x22, 0x11, 0xa8,
    ];
    /// SPS from libx264 (640x384, High) signalling only the matrix: (2, 2, 1).
    const X264_SPS: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xb4, 0x05, 0x01, 0x8d, 0x80, 0xb5, 0x02, 0x02, 0x01, 0x40, 0x00,
        0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x1e, 0x23, 0xc6, 0x0c, 0xa8,
    ];

    /// The VUI after the video signal (timing etc.) as raw bits, to check it survives a rewrite.
    fn vui_tail(annexb: &[u8]) -> Vec<u32> {
        let rbsp = unescape(&annexb[5..]);
        let head = sps_head(&rbsp).unwrap();
        let mut r = Bits { d: &rbsp, pos: head.vui_flag + 1 };
        vui_signal(&mut r).unwrap();
        let last = rbsp.iter().rposition(|&b| b != 0).unwrap();
        let stop = last * 8 + 7 - rbsp[last].trailing_zeros() as usize;
        (r.pos..stop).map(|_| r.bit().unwrap()).collect()
    }

    #[test]
    fn media_foundation_sps_gains_the_colour_description() {
        assert_eq!(parse_sps_vui(MF_SPS).unwrap().colour, None);
        let out = set_sps_video_signal(MF_SPS, &BT709);
        let info = parse_sps_vui(&out).unwrap();
        assert_eq!((info.width, info.height, info.profile_idc), (640, 384, 66));
        assert_eq!(info.colour, Some((1, 1, 1)));
        assert!(!info.full_range);
        assert_eq!(vui_tail(&out), vui_tail(MF_SPS), "timing info and the rest of the VUI are kept");
    }

    #[test]
    fn a_different_signal_is_rewritten_in_place() {
        assert_eq!(parse_sps_vui(X264_SPS).unwrap().colour, Some((2, 2, 1)));
        let out = set_sps_video_signal(X264_SPS, &BT709);
        let info = parse_sps_vui(&out).unwrap();
        assert_eq!((info.width, info.height, info.profile_idc), (640, 384, 100));
        assert_eq!(info.colour, Some((1, 1, 1)));
        assert_eq!(vui_tail(&out), vui_tail(X264_SPS));
        let full = VideoSignal { full_range: true, ..BT709 };
        assert!(parse_sps_vui(&set_sps_video_signal(X264_SPS, &full)).unwrap().full_range);
    }

    #[test]
    fn sps_already_signalling_the_colour_is_untouched() {
        let once = set_sps_video_signal(MF_SPS, &BT709);
        assert_eq!(set_sps_video_signal(&once, &BT709), once);
    }

    #[test]
    fn sps_without_vui_gets_one() {
        // Baseline 640x384, poc type 2, one reference frame, no VUI.
        let mut w = BitWriter::default();
        w.u(8, 66);
        w.u(8, 0xc0);
        w.u(8, 31);
        w.ue(0); // sps id
        w.ue(0); // log2_max_frame_num - 4
        w.ue(2); // poc type
        w.ue(1); // max_num_ref_frames
        w.bit(0);
        w.ue(39); // 640 / 16 - 1
        w.ue(23); // 384 / 16 - 1
        w.bit(1); // frame_mbs_only
        w.bit(1); // direct_8x8_inference
        w.bit(0); // no cropping
        w.bit(0); // no VUI
        let mut sps = vec![0, 0, 0, 1, 0x67];
        escape(&w.finish(), &mut sps);
        assert_eq!(parse_sps_vui(&sps).unwrap().colour, None);

        let out = set_sps_video_signal(&sps, &BT709);
        let info = parse_sps_vui(&out).unwrap();
        assert_eq!((info.width, info.height), (640, 384));
        assert_eq!(info.colour, Some((1, 1, 1)));
        // ffmpeg's h264 parser reads the same VUI back, see the bench test on h264_mf.
    }

    #[test]
    fn rewrite_keeps_other_nals_and_escapes_the_payload() {
        let pps = nal(8, 3, false);
        let idr = nal(5, 10, true);
        let au: Vec<u8> = [MF_SPS, &pps, &idr].concat();
        let out = set_sps_video_signal(&au, &BT709);
        assert!(out.ends_with(&[pps.as_slice(), idr.as_slice()].concat()));
        // No start code may appear inside the rewritten SPS.
        let sps_len = out.len() - pps.len() - idr.len();
        assert!(!out[4..sps_len].windows(3).any(|w| w == [0, 0, 1] || w == [0, 0, 0]));
        let mut p = Packetizer::with_signal(BT709);
        let pk = p.packetize(au, true, 0);
        assert_eq!(parse_sps_vui(&pk[0].data).unwrap().colour, Some((1, 1, 1)));
    }

    #[test]
    fn idr_scheduler_first_request_and_interval() {
        let mut s = IdrScheduler::new();
        assert!(s.is_due(0));
        s.mark_idr(0);
        assert!(!s.is_due(1_000_000));
        s.request();
        assert!(s.is_due(1_000_001));
        s.mark_idr(1_000_001);
        assert!(!s.is_due(2_000_000));
        assert!(s.is_due(1_000_001 + SAFETY_IDR_INTERVAL_US));
    }
}
