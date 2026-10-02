//! Opus framing for the audio channel: 48 kHz, 10 ms frames.
//!
//! Pure logic (no PipeWire), so it is unit tested: [`FrameChunker`] cuts the
//! irregular buffers PipeWire hands over into exact 10 ms frames and stamps
//! them, [`OpusSender`] encodes and numbers them, [`OpusReceiver`] decodes a
//! stream of packets with loss concealment and drops reordered ones.

use opus::{Application, Channels, Decoder, Encoder};

use crate::protocol::wire::AudioPacket;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one frame (10 ms).
pub const FRAME_SAMPLES: usize = 480;
/// Longest packet Opus can produce for one frame here; generous.
const MAX_PACKET: usize = 1275;
/// Target bitrate per channel: transparent enough for desktop audio.
const BITRATE_PER_CHANNEL: i32 = 64_000;
/// Most missing packets concealed in one go; beyond this the stream restarted.
const MAX_CONCEAL: u32 = 5;

fn opus_channels(n: u8) -> Option<Channels> {
    match n {
        1 => Some(Channels::Mono),
        2 => Some(Channels::Stereo),
        _ => None,
    }
}

/// Cuts interleaved PCM into 10 ms frames and timestamps each one.
pub struct FrameChunker {
    channels: usize,
    pending: Vec<f32>,
}

impl FrameChunker {
    pub fn new(channels: u8) -> Self {
        Self { channels: channels as usize, pending: Vec::new() }
    }

    /// Adds `pcm` (interleaved) that ends at `end_us` on the host clock and
    /// returns every complete frame with the time of its first sample.
    pub fn push(&mut self, pcm: &[f32], end_us: u64) -> Vec<(u64, Vec<f32>)> {
        self.pending.extend_from_slice(pcm);
        let frame_len = FRAME_SAMPLES * self.channels;
        let total_frames = self.pending.len() / self.channels;
        let mut out = Vec::new();
        let mut consumed = 0; // frames (per channel) already cut
        while self.pending.len() - consumed * self.channels >= frame_len {
            // Samples from this frame's first sample up to `end_us`.
            let after = total_frames - consumed;
            let pts = end_us.saturating_sub(after as u64 * 1_000_000 / SAMPLE_RATE as u64);
            let start = consumed * self.channels;
            out.push((pts, self.pending[start..start + frame_len].to_vec()));
            consumed += FRAME_SAMPLES;
        }
        self.pending.drain(..consumed * self.channels);
        out
    }

    /// Drops buffered samples (a stream gap; old audio must not be glued to new).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// Encodes 10 ms frames into numbered [`AudioPacket`]s.
pub struct OpusSender {
    enc: Encoder,
    channels: u8,
    seq: u32,
    buf: Vec<u8>,
}

impl OpusSender {
    pub fn new(channels: u8) -> Result<Self, String> {
        let ch = opus_channels(channels).ok_or("only mono and stereo are supported")?;
        let mut enc = Encoder::new(SAMPLE_RATE, ch, Application::Audio).map_err(|e| e.to_string())?;
        enc.set_bitrate(opus::Bitrate::Bits(BITRATE_PER_CHANNEL * channels as i32)).map_err(|e| e.to_string())?;
        Ok(Self { enc, channels, seq: 0, buf: vec![0; MAX_PACKET] })
    }

    /// `pcm` is exactly one frame of interleaved samples.
    pub fn encode(&mut self, pcm: &[f32], pts_us: u64) -> Result<AudioPacket, String> {
        debug_assert_eq!(pcm.len(), FRAME_SAMPLES * self.channels as usize);
        let n = self.enc.encode_float(pcm, &mut self.buf).map_err(|e| e.to_string())?;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Ok(AudioPacket { seq, pts_us, channels: self.channels, data: self.buf[..n].to_vec() })
    }
}

/// Decodes a packet stream; reordered/duplicate packets are dropped and short
/// gaps are concealed so the output keeps its timing.
pub struct OpusReceiver {
    decoder: Option<(u8, Decoder)>,
    next_seq: Option<u32>,
    /// Samples per channel of the last decoded packet: the size a lost one is assumed to have
    /// (the phone's encoder may use 20 ms frames).
    last_samples: usize,
}

impl Default for OpusReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusReceiver {
    pub fn new() -> Self {
        Self { decoder: None, next_seq: None, last_samples: FRAME_SAMPLES }
    }

    /// Returns the interleaved f32 samples for `p` (preceded by concealment
    /// for up to [`MAX_CONCEAL`] lost packets), plus the channel count.
    /// Empty when the packet is stale or undecodable.
    pub fn decode(&mut self, p: &AudioPacket) -> (u8, Vec<f32>) {
        let Some(ch) = opus_channels(p.channels) else { return (p.channels, Vec::new()) };
        if self.decoder.as_ref().map(|d| d.0) != Some(p.channels) {
            match Decoder::new(SAMPLE_RATE, ch) {
                Ok(d) => self.decoder = Some((p.channels, d)),
                Err(_) => return (p.channels, Vec::new()),
            }
            self.next_seq = None;
        }
        let dec = &mut self.decoder.as_mut().unwrap().1;
        let n = p.channels as usize;
        let mut out = Vec::new();
        if let Some(next) = self.next_seq {
            let gap = p.seq.wrapping_sub(next);
            if gap >= u32::MAX / 2 {
                return (p.channels, out); // older than what was already played
            }
            for _ in 0..gap.min(MAX_CONCEAL) {
                let mut lost = vec![0f32; self.last_samples * n];
                if dec.decode_float(&[], &mut lost, false).is_ok() {
                    out.extend_from_slice(&lost);
                }
            }
        }
        // 120 ms is the longest frame a packet may carry.
        let mut pcm = vec![0f32; 5760 * n];
        match dec.decode_float(&p.data, &mut pcm, false) {
            Ok(samples) => {
                out.extend_from_slice(&pcm[..samples * n]);
                self.last_samples = samples;
                self.next_seq = Some(p.seq.wrapping_add(1));
            }
            Err(_) => return (p.channels, Vec::new()),
        }
        (p.channels, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, ch: usize, start: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 440.0 * (start + i) as f32 / SAMPLE_RATE as f32).sin() * 0.5;
                std::iter::repeat(v).take(ch)
            })
            .collect()
    }

    #[test]
    fn chunker_emits_whole_frames_and_keeps_the_rest() {
        let mut c = FrameChunker::new(2);
        assert!(c.push(&tone(300, 2, 0), 10_000).is_empty());
        let f = c.push(&tone(700, 2, 300), 30_000);
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|(_, s)| s.len() == FRAME_SAMPLES * 2));
        // 40 samples were left over; 440 more complete a frame.
        let f = c.push(&tone(440, 2, 1000), 40_000);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn chunker_timestamps_the_first_sample() {
        let mut c = FrameChunker::new(1);
        // 960 samples (20 ms) ending at t=100 ms: frames start at 80 and 90 ms.
        let f = c.push(&tone(960, 1, 0), 100_000);
        assert_eq!(f.iter().map(|x| x.0).collect::<Vec<_>>(), vec![80_000, 90_000]);
        // Leftover samples are older than the end of the next push.
        let mut c = FrameChunker::new(1);
        assert!(c.push(&tone(240, 1, 0), 5_000).is_empty());
        let f = c.push(&tone(240, 1, 240), 10_000);
        assert_eq!(f[0].0, 0);
    }

    #[test]
    fn chunker_clear_drops_partial_frames() {
        let mut c = FrameChunker::new(2);
        c.push(&tone(400, 2, 0), 1);
        c.clear();
        assert!(c.push(&tone(400, 2, 0), 2).is_empty());
    }

    #[test]
    fn opus_round_trip_stereo_keeps_the_tone() {
        let mut tx = OpusSender::new(2).unwrap();
        let mut rx = OpusReceiver::new();
        let mut decoded = Vec::new();
        for k in 0..30 {
            let pcm = tone(FRAME_SAMPLES, 2, k * FRAME_SAMPLES);
            let p = tx.encode(&pcm, k as u64 * 10_000).unwrap();
            assert_eq!(p.seq, k as u32);
            assert_eq!(p.channels, 2);
            assert!(!p.data.is_empty() && p.data.len() < MAX_PACKET);
            let (ch, s) = rx.decode(&p);
            assert_eq!(ch, 2);
            assert_eq!(s.len(), FRAME_SAMPLES * 2);
            decoded.extend(s);
        }
        // Once the codec has warmed up the tone is still there at about the same level.
        let tail = &decoded[decoded.len() - 2 * FRAME_SAMPLES * 10..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.35).abs() < 0.1, "rms {rms}");
    }

    #[test]
    fn receiver_conceals_a_lost_packet_and_drops_stale_ones() {
        let mut tx = OpusSender::new(1).unwrap();
        let mut rx = OpusReceiver::new();
        let pk: Vec<_> = (0..5).map(|k| tx.encode(&tone(FRAME_SAMPLES, 1, k * FRAME_SAMPLES), 0).unwrap()).collect();
        assert_eq!(rx.decode(&pk[0]).1.len(), FRAME_SAMPLES);
        // pk[1] lost: pk[2] arrives with one concealed frame before it.
        assert_eq!(rx.decode(&pk[2]).1.len(), 2 * FRAME_SAMPLES);
        // pk[1] shows up late: dropped, and a duplicate of pk[2] too.
        assert!(rx.decode(&pk[1]).1.is_empty());
        assert!(rx.decode(&pk[2]).1.is_empty());
        assert_eq!(rx.decode(&pk[3]).1.len(), FRAME_SAMPLES);
    }

    #[test]
    fn receiver_rejects_bad_channel_counts_and_garbage() {
        let mut rx = OpusReceiver::new();
        let bad = AudioPacket { seq: 0, pts_us: 0, channels: 6, data: vec![1, 2, 3] };
        assert!(rx.decode(&bad).1.is_empty());
        let junk = AudioPacket { seq: 0, pts_us: 0, channels: 2, data: vec![0xFF; 3] };
        let _ = rx.decode(&junk); // must not panic
    }

    #[test]
    fn sender_sequence_wraps() {
        let mut tx = OpusSender::new(1).unwrap();
        tx.seq = u32::MAX;
        let pcm = vec![0.0; FRAME_SAMPLES];
        assert_eq!(tx.encode(&pcm, 0).unwrap().seq, u32::MAX);
        assert_eq!(tx.encode(&pcm, 0).unwrap().seq, 0);
    }
}
