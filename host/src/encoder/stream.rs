//! Backend-independent stream logic shared by the ffmpeg and x264 encoders:
//! Annex-B NAL splitting, CONFIG/KEY/DELTA packetisation and IDR scheduling.

use super::EncodedPacket;
use crate::protocol::{FRAME_TYPE_CONFIG, FRAME_TYPE_DELTA, FRAME_TYPE_KEY};

/// Safety IDR period. A long GOP keeps bitrate low on a mostly static desktop,
/// but a periodic IDR lets a client that lost data (or joined late) resync
/// without waiting for an explicit request.
pub const SAFETY_IDR_INTERVAL_US: u64 = 3_000_000;

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
}

impl Packetizer {
    /// Seeds the parameter sets from out-of-band headers (extradata / `x264_encoder_headers`).
    pub fn set_config(&mut self, headers: &[u8]) {
        let (params, _) = split_parameter_sets(headers);
        if !params.is_empty() {
            self.config = params;
        }
    }

    pub fn packetize(&mut self, data: Vec<u8>, is_key: bool, timestamp_us: u64) -> Vec<EncodedPacket> {
        if !is_key {
            return vec![EncodedPacket { frame_type: FRAME_TYPE_DELTA, data, timestamp_us }];
        }
        let (params, rest) = split_parameter_sets(&data);
        if !params.is_empty() {
            self.config = params;
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

#[cfg(test)]
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

#[cfg(test)]
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

/// Minimal SPS parser (Annex-B input, first SPS NAL) that reaches the VUI colour
/// fields. Returns None if the SPS is malformed or uses scaling lists.
#[cfg(test)]
pub fn parse_sps_vui(annexb: &[u8]) -> Option<SpsInfo> {
    let starts = nal_starts(annexb);
    let &(_, hdr) = starts.iter().find(|&&(_, h)| annexb.get(h).map(|b| b & 0x1f) == Some(NAL_SPS))?;
    let end = starts.iter().map(|s| s.0).find(|&s| s > hdr).unwrap_or(annexb.len());
    // Strip emulation-prevention bytes (00 00 03 -> 00 00).
    let mut rbsp = Vec::new();
    let mut zeros = 0;
    for &b in &annexb[hdr + 1..end] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { d: &rbsp, pos: 0 };
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
            return None; // scaling matrices not supported by this helper
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
    r.ue()?; // max_num_ref_frames
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
    let mut info = SpsInfo { profile_idc, width, height, colour: None, full_range: false };
    if r.bit()? == 0 {
        return Some(info); // no VUI
    }
    if r.bit()? == 1 && r.u(8)? == 255 {
        r.u(32)?;
    }
    if r.bit()? == 1 {
        r.bit()?;
    }
    if r.bit()? == 1 {
        r.u(3)?;
        info.full_range = r.bit()? == 1;
        if r.bit()? == 1 {
            info.colour = Some((r.u(8)? as u8, r.u(8)? as u8, r.u(8)? as u8));
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
