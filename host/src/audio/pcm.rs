//! Turns whatever a sound card's mix format is into what the audio channel
//! carries: interleaved stereo f32 at 48 kHz.
//!
//! Pure logic (no WASAPI), so it is unit tested on every platform: sample
//! decoding, a downmix that follows the channel mask, and a streaming
//! linear-interpolation resampler.

use super::codec::SAMPLE_RATE;

/// How the samples of a device format are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    /// Packed 24-bit in 3 bytes.
    I24,
    I32,
}

impl SampleKind {
    pub fn bytes(self) -> usize {
        match self {
            SampleKind::F32 | SampleKind::I32 => 4,
            SampleKind::I16 => 2,
            SampleKind::I24 => 3,
        }
    }
}

/// A device's capture format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrcFormat {
    pub rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    /// Speaker positions (`SPEAKER_*` bits); 0 when unknown.
    pub mask: u32,
}

/// Appends the samples in `bytes` to `out` as f32 in -1..1.
pub fn decode_samples(bytes: &[u8], kind: SampleKind, out: &mut Vec<f32>) {
    match kind {
        SampleKind::F32 => out.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
        SampleKind::I16 => out.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)),
        SampleKind::I24 => out.extend(bytes.chunks_exact(3).map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0)),
        SampleKind::I32 => out.extend(bytes.chunks_exact(4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0)),
    }
}

const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const LFE: u32 = 0x8;
const BL: u32 = 0x10;
const BR: u32 = 0x20;
const FLC: u32 = 0x40;
const FRC: u32 = 0x80;
const BC: u32 = 0x100;
const SL: u32 = 0x200;
const SR: u32 = 0x400;

/// The positions of `channels` when the device did not say (the usual layouts).
fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => FC,
        2 => FL | FR,
        4 => FL | FR | BL | BR,
        6 => FL | FR | FC | LFE | BL | BR,
        8 => FL | FR | FC | LFE | BL | BR | SL | SR,
        _ => 0,
    }
}

/// Per-channel (left, right) weights of a stereo downmix, normalised so that
/// all channels at full scale cannot clip.
fn downmix_weights(channels: u16, mask: u32) -> Vec<(f32, f32)> {
    let n = channels as usize;
    if n == 1 {
        return vec![(1.0, 1.0)];
    }
    let mask = if mask.count_ones() as usize == n { mask } else { default_mask(channels) };
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let mut w: Vec<(f32, f32)> = Vec::with_capacity(n);
    if mask == 0 {
        // Unknown layout: first two channels are front left/right, the rest is dropped.
        w.extend((0..n).map(|i| match i {
            0 => (1.0, 0.0),
            1 => (0.0, 1.0),
            _ => (0.0, 0.0),
        }));
    } else {
        for bit in 0..32 {
            let b = 1u32 << bit;
            if mask & b == 0 {
                continue;
            }
            w.push(match b {
                FL => (1.0, 0.0),
                FR => (0.0, 1.0),
                FC => (H, H),
                LFE => (0.0, 0.0),
                BL | SL => (H, 0.0),
                BR | SR => (0.0, H),
                FLC => (0.5, 0.0),
                FRC => (0.0, 0.5),
                BC => (0.5, 0.5),
                _ => (0.0, 0.0),
            });
        }
        w.resize(n, (0.0, 0.0));
    }
    let l: f32 = w.iter().map(|x| x.0).sum();
    let r: f32 = w.iter().map(|x| x.1).sum();
    let norm = l.max(r).max(1.0);
    w.iter().map(|x| (x.0 / norm, x.1 / norm)).collect()
}

/// Streaming linear-interpolation resampler for interleaved stereo.
struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame in `[prev] ++ input`, index 0 being `prev`.
    pos: f64,
    prev: Option<[f32; 2]>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, prev: None }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        let prev = *self.prev.get_or_insert([input[0], input[1]]);
        let at = |i: usize| -> [f32; 2] { if i == 0 { prev } else { [input[2 * (i - 1)], input[2 * (i - 1) + 1]] } };
        loop {
            let i = self.pos as usize;
            if i + 1 > n {
                break;
            }
            let f = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a[0] + (b[0] - a[0]) * f);
            out.push(a[1] + (b[1] - a[1]) * f);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = Some([input[2 * n - 2], input[2 * n - 1]]);
    }
}

/// Device format -> stereo f32 at [`SAMPLE_RATE`].
pub struct ToStereo48 {
    fmt: SrcFormat,
    weights: Vec<(f32, f32)>,
    resampler: Option<Resampler>,
    samples: Vec<f32>,
    stereo: Vec<f32>,
}

impl ToStereo48 {
    pub fn new(fmt: SrcFormat) -> Self {
        let resampler = (fmt.rate != SAMPLE_RATE).then(|| Resampler::new(fmt.rate, SAMPLE_RATE));
        Self { fmt, weights: downmix_weights(fmt.channels, fmt.mask), resampler, samples: Vec::new(), stereo: Vec::new() }
    }

    /// Bytes of one device frame.
    pub fn frame_bytes(&self) -> usize {
        self.fmt.channels as usize * self.fmt.kind.bytes()
    }

    /// Converts `frames` device frames. `data` is `None` for a silent packet
    /// (the device flagged it, so its bytes are not to be trusted). The result
    /// is valid until the next call.
    pub fn convert(&mut self, data: Option<&[u8]>, frames: usize, out: &mut Vec<f32>) {
        let ch = self.fmt.channels as usize;
        self.stereo.clear();
        match data {
            Some(bytes) => {
                self.samples.clear();
                decode_samples(&bytes[..(frames * ch * self.fmt.kind.bytes()).min(bytes.len())], self.fmt.kind, &mut self.samples);
                if ch == 2 {
                    self.stereo.extend_from_slice(&self.samples);
                } else {
                    for f in self.samples.chunks_exact(ch) {
                        let (mut l, mut r) = (0.0, 0.0);
                        for (s, w) in f.iter().zip(&self.weights) {
                            l += s * w.0;
                            r += s * w.1;
                        }
                        self.stereo.push(l);
                        self.stereo.push(r);
                    }
                }
            }
            None => self.stereo.resize(frames * 2, 0.0),
        }
        match self.resampler.as_mut() {
            Some(r) => r.process(&self.stereo, out),
            None => out.extend_from_slice(&self.stereo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate: u32, channels: u16, kind: SampleKind, mask: u32) -> SrcFormat {
        SrcFormat { rate, channels, kind, mask }
    }

    #[test]
    fn decodes_every_sample_kind() {
        let mut o = Vec::new();
        decode_samples(&0.25f32.to_le_bytes(), SampleKind::F32, &mut o);
        decode_samples(&(-16384i16).to_le_bytes(), SampleKind::I16, &mut o);
        decode_samples(&[0x00, 0x00, 0x40], SampleKind::I24, &mut o); // 0x400000 = 0.5
        decode_samples(&[0x00, 0x00, 0x00, 0xC0], SampleKind::I32, &mut o); // -0.5
        assert_eq!(o, vec![0.25, -0.5, 0.5, -0.5]);
        // Negative 24-bit sign-extends.
        o.clear();
        decode_samples(&[0x00, 0x00, 0x80], SampleKind::I24, &mut o);
        assert_eq!(o, vec![-1.0]);
    }

    #[test]
    fn stereo_48k_f32_passes_through_untouched() {
        let mut c = ToStereo48::new(fmt(48_000, 2, SampleKind::F32, FL | FR));
        let src = [0.1f32, -0.2, 0.3, -0.4];
        let bytes: Vec<u8> = src.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        c.convert(Some(&bytes), 2, &mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn mono_is_duplicated() {
        let mut c = ToStereo48::new(fmt(48_000, 1, SampleKind::I16, 0));
        let bytes: Vec<u8> = [16384i16, -16384].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        c.convert(Some(&bytes), 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn surround_downmix_keeps_fronts_and_never_clips() {
        // 5.1 with everything at full scale must stay within -1..1.
        let mut c = ToStereo48::new(fmt(48_000, 6, SampleKind::F32, 0x3F));
        let bytes: Vec<u8> = std::iter::repeat(1.0f32).take(6).flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        c.convert(Some(&bytes), 1, &mut out);
        assert!(out.iter().all(|v| *v > 0.5 && *v <= 1.0 + 1e-6), "{out:?}");
        // Only the front-left channel active: it lands on the left only.
        let mut one = vec![0.0f32; 6];
        one[0] = 0.8;
        let bytes: Vec<u8> = one.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        let mut c = ToStereo48::new(fmt(48_000, 6, SampleKind::F32, 0x3F));
        c.convert(Some(&bytes), 1, &mut out);
        assert!(out[0] > 0.2 && out[1] == 0.0, "{out:?}");
    }

    #[test]
    fn lfe_only_is_dropped_and_unknown_layouts_use_first_two() {
        let w = downmix_weights(6, 0x3F);
        assert_eq!(w[3], (0.0, 0.0));
        let w = downmix_weights(3, 0); // no default layout for 3 channels
        assert_eq!((w[0].0, w[1].1, w[2]), (1.0, 1.0, (0.0, 0.0)));
    }

    #[test]
    fn silent_packet_becomes_zeros() {
        let mut c = ToStereo48::new(fmt(48_000, 6, SampleKind::F32, 0x3F));
        let mut out = Vec::new();
        c.convert(None, 10, &mut out);
        assert_eq!(out, vec![0.0; 20]);
    }

    #[test]
    fn resampling_44k1_to_48k_gives_the_right_count_across_chunks() {
        let mut c = ToStereo48::new(fmt(44_100, 2, SampleKind::F32, FL | FR));
        let mut out = Vec::new();
        // One second delivered in irregular chunks.
        let mut left = 44_100usize;
        let mut k = 0;
        while left > 0 {
            let n = [441usize, 100, 1000, 7][k % 4].min(left);
            let bytes: Vec<u8> = (0..n * 2).flat_map(|_| 0.5f32.to_le_bytes()).collect();
            c.convert(Some(&bytes), n, &mut out);
            left -= n;
            k += 1;
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        // A constant stays constant.
        assert!(out.iter().all(|v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampling_keeps_a_tone_at_its_frequency() {
        let mut c = ToStereo48::new(fmt(44_100, 2, SampleKind::F32, FL | FR));
        let n = 4410;
        let bytes: Vec<u8> = (0..n)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44_100.0).sin() * 0.5;
                [v, v]
            })
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        c.convert(Some(&bytes), n, &mut out);
        let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        let rising = left.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((43..=45).contains(&rising), "{rising} cycles in 0.1 s");
    }

    #[test]
    fn downsampling_from_96k_halves_the_frames() {
        let mut c = ToStereo48::new(fmt(96_000, 2, SampleKind::F32, FL | FR));
        let bytes: Vec<u8> = (0..960 * 2).flat_map(|_| 0.1f32.to_le_bytes()).collect();
        let mut out = Vec::new();
        c.convert(Some(&bytes), 960, &mut out);
        assert_eq!(out.len() / 2, 480);
    }
}
