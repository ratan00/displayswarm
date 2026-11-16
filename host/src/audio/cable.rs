//! Choices behind the Windows microphone path that need no Windows API, so
//! they are unit-tested everywhere (see [`super::windows`]).

/// Environment variable naming (part of) the playback device the phone's mic goes to.
pub const MIC_DEVICE_ENV: &str = "DISPLAYSWARM_MIC_DEVICE";

/// Names of virtual-cable inputs found without configuration, in order of preference.
pub const KNOWN_CABLES: [&str; 3] = ["CABLE Input", "VB-Audio Virtual Cable", "Virtual Audio Cable"];

/// The playback device to send the mic to among `names`: the one matching
/// `wanted` (case-insensitive substring), else a known virtual cable.
pub fn pick_mic_device(names: &[String], wanted: Option<&str>) -> Option<usize> {
    let find = |needle: &str| {
        let n = needle.to_lowercase();
        names.iter().position(|d| d.to_lowercase().contains(&n))
    };
    match wanted.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => find(w),
        None => KNOWN_CABLES.iter().find_map(|c| find(c)),
    }
}

/// Folds interleaved stereo to mono.
pub fn to_mono(channels: u8, pcm: Vec<f32>) -> Vec<f32> {
    if channels == 2 {
        pcm.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect()
    } else {
        pcm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_device_is_a_virtual_cable_or_the_named_one() {
        let names: Vec<String> =
            ["Speakers (Realtek(R) Audio)", "CABLE Input (VB-Audio Virtual Cable)", "Headphones"].map(String::from).to_vec();
        assert_eq!(pick_mic_device(&names, None), Some(1));
        assert_eq!(pick_mic_device(&names, Some("headph")), Some(2));
        assert_eq!(pick_mic_device(&names, Some("nothing like it")), None);
        assert_eq!(pick_mic_device(&names[..1], None), None, "never the real speakers");
    }

    #[test]
    fn stereo_folds_to_mono() {
        assert_eq!(to_mono(2, vec![1.0, 0.0, 0.5, 0.5]), vec![0.5, 0.5]);
        assert_eq!(to_mono(1, vec![0.25]), vec![0.25]);
    }
}

/// A device's native stream format, as WASAPI's mix format describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFormat {
    pub rate: u32,
    pub channels: u16,
    /// Bits per sample in the buffer (16, 24 or 32).
    pub bits: u16,
    /// IEEE float (32-bit) rather than signed integer PCM.
    pub float: bool,
}

impl NativeFormat {
    pub fn block_bytes(&self) -> usize {
        self.channels as usize * self.bits as usize / 8
    }
}

/// Interleaved stereo f32 from `frames` frames of a native buffer: the first
/// two channels (mono is duplicated), any integer format scaled to -1..1.
/// `None` for a format this does not read.
pub fn native_to_stereo(bytes: &[u8], fmt: NativeFormat) -> Option<Vec<f32>> {
    let width = fmt.bits as usize / 8;
    let block = fmt.block_bytes();
    if block == 0 || !matches!((fmt.float, fmt.bits), (true, 32) | (false, 16) | (false, 24) | (false, 32)) {
        return None;
    }
    let sample = |b: &[u8]| -> f32 {
        match (fmt.float, fmt.bits) {
            (true, _) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (false, 16) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (false, 24) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / block * 2);
    for frame in bytes.chunks_exact(block) {
        let l = sample(&frame[..width]);
        let r = if fmt.channels >= 2 { sample(&frame[width..2 * width]) } else { l };
        out.extend_from_slice(&[l, r]);
    }
    Some(out)
}

/// Streaming linear resampler for interleaved stereo, `from` Hz to `to` Hz.
/// Plenty for a device whose mix rate is not 48 kHz (44.1 kHz, 96 kHz).
pub struct Resampler {
    step: f64,
    /// Position of the next output sample, in input frames, relative to `last`.
    pos: f64,
    /// The last input frame of the previous call.
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to.max(1) as f64, pos: 0.0, last: [0.0; 2] }
    }

    pub fn passthrough(&self) -> bool {
        self.step == 1.0
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.passthrough() {
            return input.to_vec();
        }
        let frames = input.len() / 2;
        let at = |i: isize| -> [f32; 2] {
            if i < 0 {
                self.last
            } else {
                [input[i as usize * 2], input[i as usize * 2 + 1]]
            }
        };
        let mut out = Vec::with_capacity((frames as f64 / self.step) as usize * 2 + 2);
        // Frame -1 is the previous call's last frame, so interpolation spans calls.
        while self.pos + 1.0 <= frames as f64 {
            let i = self.pos.floor() as isize - 1;
            let t = (self.pos - self.pos.floor()) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.step;
        }
        if frames > 0 {
            self.last = at(frames as isize - 1);
            self.pos -= frames as f64;
        }
        out
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn native_formats_become_stereo_f32() {
        let f16 = NativeFormat { rate: 48_000, channels: 2, bits: 16, float: false };
        let bytes: Vec<u8> = [16384i16, -32768].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(native_to_stereo(&bytes, f16).unwrap(), vec![0.5, -1.0]);
        let mono_float = NativeFormat { rate: 44_100, channels: 1, bits: 32, float: true };
        assert_eq!(native_to_stereo(&0.25f32.to_le_bytes(), mono_float).unwrap(), vec![0.25, 0.25]);
        // 24-bit, 6 channels: only front left/right are kept.
        let f24 = NativeFormat { rate: 48_000, channels: 6, bits: 24, float: false };
        let mut frame = vec![0u8; 18];
        frame[..3].copy_from_slice(&[0x00, 0x00, 0x40]); // 0x400000 = 0.5
        frame[3..6].copy_from_slice(&[0x00, 0x00, 0xC0]); // -0.5
        assert_eq!(native_to_stereo(&frame, f24).unwrap(), vec![0.5, -0.5]);
        assert!(native_to_stereo(&[0; 8], NativeFormat { rate: 1, channels: 2, bits: 8, float: false }).is_none());
    }

    #[test]
    fn resampling_keeps_duration_and_continuity() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut total = 0;
        // One second of a ramp in 10 ms pieces.
        let mut last_out = None::<f32>;
        for chunk in 0..100 {
            let input: Vec<f32> = (0..441).flat_map(|i| {
                let v = (chunk * 441 + i) as f32 / 44_100.0;
                [v, v]
            }).collect();
            let out = r.process(&input);
            for pair in out.chunks_exact(2) {
                if let Some(prev) = last_out {
                    assert!(pair[0] >= prev - 1e-6, "monotonic across calls");
                }
                last_out = Some(pair[0]);
            }
            total += out.len() / 2;
        }
        assert!((47_990..=48_010).contains(&total), "{total}");
        let mut same = Resampler::new(48_000, 48_000);
        assert_eq!(same.process(&[1.0, 2.0]), vec![1.0, 2.0]);
    }
}
