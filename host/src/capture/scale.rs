//! Aspect-preserving, area-averaging scaler for 32-bit packed pixels.
//!
//! Why this exists: the previous `copy_or_scale_frame` was nearest-neighbour
//! and mapped the source onto the *whole* target rectangle. Nearest-neighbour
//! drops most source pixels when shrinking a 4K desktop to a phone-sized
//! stream, which is what made text jagged and shimmering, and ignoring the
//! aspect ratio squashed the picture whenever the desktop and the phone
//! disagreed. Here the source is fitted *inside* the target (bars are black)
//! and every destination pixel is the coverage-weighted average of the source
//! box it stands for (a box / "area" filter, the standard choice for
//! downscaling text without ringing).
//!
//! The filter is separable and evaluated row by row: for each destination row
//! the contributing source rows are accumulated into one `u32` row, which is
//! then reduced horizontally. That keeps the working set to a single source row
//! (cache friendly at 4K) and needs no full-frame intermediate. Channel order
//! is irrelevant -- all four bytes are filtered identically -- so the same code
//! serves RGBA and BGRA.
//!
//! Alpha: filtered output is written fully opaque. The pixel formats PipeWire
//! and X11 hand us are really "x" formats (BGRx/RGBx, alpha undefined), and an
//! averaged garbage alpha would be worse than a constant one.

/// Weight precision: each axis' weights for one destination pixel sum to this.
const WEIGHT_ONE: u32 = 4096;

/// Where a source of `sw`x`sh` lands inside a `dw`x`dh` target when the aspect
/// ratio is preserved: `(x, y, w, h)` of the picture, the rest being bars.
///
/// The picture is never larger than the target and never has a zero side.
pub fn fit_rect(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32, u32, u32) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return (0, 0, dw, dh);
    }
    // Compare sw/sh with dw/dh without floating point: sw*dh vs dw*sh.
    let (fw, fh) = if (sw as u64) * (dh as u64) >= (dw as u64) * (sh as u64) {
        // Source is relatively wider (or equal): width fills, letterbox top/bottom.
        let h = ((sh as u64) * (dw as u64) + (sw as u64) / 2) / (sw as u64);
        (dw, (h as u32).clamp(1, dh))
    } else {
        // Source is relatively taller: height fills, pillarbox left/right.
        let w = ((sw as u64) * (dh as u64) + (sh as u64) / 2) / (sh as u64);
        ((w as u32).clamp(1, dw), dh)
    };
    ((dw - fw) / 2, (dh - fh) / 2, fw, fh)
}

/// Per-axis filter taps: for destination index `i`, source indices
/// `start[i] .. start[i] + count[i]` with weights `weights[off[i] ..]`.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
struct Axis {
    src_len: u32,
    dst_len: u32,
    start: Vec<u32>,
    off: Vec<u32>,
    count: Vec<u32>,
    weights: Vec<u32>,
}

impl Axis {
    fn build(src_len: u32, dst_len: u32) -> Self {
        let mut a = Axis { src_len, dst_len, ..Default::default() };
        let scale = src_len as f64 / dst_len as f64;
        // Never narrower than one source pixel, so upscaling blends two
        // neighbours instead of degenerating to nearest-neighbour.
        let half = scale.max(1.0) / 2.0;
        for i in 0..dst_len {
            let centre = (i as f64 + 0.5) * scale;
            let lo = (centre - half).max(0.0);
            let hi = (centre + half).min(src_len as f64);
            let first = (lo.floor() as u32).min(src_len - 1);
            let last = ((hi.ceil() as u32).max(first + 1)).min(src_len);
            let mut raw: Vec<f64> = (first..last)
                .map(|j| ((hi.min(j as f64 + 1.0)) - lo.max(j as f64)).max(0.0))
                .collect();
            if raw.iter().sum::<f64>() <= 0.0 {
                raw = vec![1.0];
            }
            let total: f64 = raw.iter().sum();
            let mut w: Vec<u32> = raw
                .iter()
                .map(|v| (v / total * WEIGHT_ONE as f64).round() as u32)
                .collect();
            // Rounding can leave the sum off by a few units; give the error to
            // the heaviest tap so flat colours stay exactly flat.
            let sum: u32 = w.iter().sum();
            let big = (0..w.len()).max_by_key(|&k| w[k]).unwrap_or(0);
            if sum > WEIGHT_ONE {
                w[big] = w[big].saturating_sub(sum - WEIGHT_ONE);
            } else {
                w[big] += WEIGHT_ONE - sum;
            }
            a.start.push(first);
            a.off.push(a.weights.len() as u32);
            a.count.push(w.len() as u32);
            a.weights.extend(w);
        }
        a
    }
}

/// Reusable scaler. Keeps the filter taps and the row accumulator between
/// frames, so steady-state scaling allocates nothing.
#[derive(Default)]
pub struct Scaler {
    x: Axis,
    y: Axis,
    acc: Vec<u32>,
}

impl Scaler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scales `src` (`sw`x`sh`, rows `src_stride` bytes apart) into `dst`
    /// (`dw`x`dh`, tightly packed), preserving aspect ratio; the bars are opaque
    /// black. Identical sizes are a plain row copy with no filtering.
    ///
    /// `src` must hold at least `(sh-1)*src_stride + sw*4` bytes and `dst`
    /// `dw*dh*4`; otherwise nothing is written (and a warning logged), since a
    /// short buffer must not become a panic on the capture thread.
    #[allow(clippy::too_many_arguments)]
    pub fn scale(
        &mut self,
        src: &[u8],
        sw: u32,
        sh: u32,
        src_stride: usize,
        dst: &mut [u8],
        dw: u32,
        dh: u32,
    ) {
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
            return;
        }
        let row_bytes = dw as usize * 4;
        let need_src = (sh as usize - 1) * src_stride + sw as usize * 4;
        if src.len() < need_src || dst.len() < row_bytes * dh as usize || src_stride < sw as usize * 4 {
            log::warn!(
                "scaler: buffer too small (src {} < {}, dst {} < {}); frame skipped",
                src.len(),
                need_src,
                dst.len(),
                row_bytes * dh as usize
            );
            return;
        }

        // 1:1: no filtering, no bars, just the rows.
        if sw == dw && sh == dh {
            if src_stride == row_bytes {
                dst[..row_bytes * dh as usize].copy_from_slice(&src[..row_bytes * dh as usize]);
            } else {
                for y in 0..dh as usize {
                    dst[y * row_bytes..(y + 1) * row_bytes]
                        .copy_from_slice(&src[y * src_stride..y * src_stride + row_bytes]);
                }
            }
            return;
        }

        let (fx, fy, fw, fh) = fit_rect(sw, sh, dw, dh);

        // Bars: opaque black. Only the bars are cleared here; the picture area
        // is fully overwritten below.
        let fill = |buf: &mut [u8]| {
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
        };
        if fy > 0 {
            fill(&mut dst[..fy as usize * row_bytes]);
        }
        let below = (fy + fh) as usize;
        if below < dh as usize {
            fill(&mut dst[below * row_bytes..dh as usize * row_bytes]);
        }
        let (lbar, rbar) = (fx as usize * 4, (dw - fx - fw) as usize * 4);
        if lbar > 0 || rbar > 0 {
            for y in fy as usize..below {
                let row = &mut dst[y * row_bytes..(y + 1) * row_bytes];
                fill(&mut row[..lbar]);
                fill(&mut row[row_bytes - rbar..]);
            }
        }

        if self.x.src_len != sw || self.x.dst_len != fw || self.x.start.is_empty() {
            self.x = Axis::build(sw, fw);
        }
        if self.y.src_len != sh || self.y.dst_len != fh || self.y.start.is_empty() {
            self.y = Axis::build(sh, fh);
        }
        let acc_len = sw as usize * 4;
        if self.acc.len() != acc_len {
            self.acc = vec![0u32; acc_len];
        }

        for oy in 0..fh as usize {
            // Vertical: weighted sum of the contributing source rows.
            let (ys, yo, yc) = (
                self.y.start[oy] as usize,
                self.y.off[oy] as usize,
                self.y.count[oy] as usize,
            );
            for (k, &wt) in self.y.weights[yo..yo + yc].iter().enumerate() {
                let row = &src[(ys + k) * src_stride..(ys + k) * src_stride + acc_len];
                if k == 0 {
                    for (a, &v) in self.acc.iter_mut().zip(row) {
                        *a = v as u32 * wt;
                    }
                } else {
                    for (a, &v) in self.acc.iter_mut().zip(row) {
                        *a += v as u32 * wt;
                    }
                }
            }

            // Horizontal: reduce the accumulated row into the destination.
            let out_off = (fy as usize + oy) * row_bytes + fx as usize * 4;
            let out = &mut dst[out_off..out_off + fw as usize * 4];
            for (ox, px) in out.chunks_exact_mut(4).enumerate() {
                let (xs, xo, xc) = (
                    self.x.start[ox] as usize,
                    self.x.off[ox] as usize,
                    self.x.count[ox] as usize,
                );
                let mut s = [0u64; 3];
                for (k, &wt) in self.x.weights[xo..xo + xc].iter().enumerate() {
                    let a = &self.acc[(xs + k) * 4..(xs + k) * 4 + 3];
                    s[0] += a[0] as u64 * wt as u64;
                    s[1] += a[1] as u64 * wt as u64;
                    s[2] += a[2] as u64 * wt as u64;
                }
                // Both weight sets sum to WEIGHT_ONE, so divide by its square.
                let d = (WEIGHT_ONE as u64) * (WEIGHT_ONE as u64);
                let half = d / 2;
                px[0] = ((s[0] + half) / d) as u8;
                px[1] = ((s[1] + half) / d) as u8;
                px[2] = ((s[2] + half) / d) as u8;
                px[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    fn at(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn fit_rect_letterboxes_wide_source() {
        // 16:9 into 4:3 -> full width, bars top and bottom.
        assert_eq!(fit_rect(1920, 1080, 800, 600), (0, 75, 800, 450));
    }

    #[test]
    fn fit_rect_pillarboxes_tall_source() {
        // 9:16 into 16:9 -> full height, bars left and right.
        assert_eq!(fit_rect(1080, 1920, 1920, 1080), (656, 0, 608, 1080));
    }

    #[test]
    fn fit_rect_same_aspect_fills() {
        assert_eq!(fit_rect(3840, 2160, 1280, 720), (0, 0, 1280, 720));
    }

    #[test]
    fn one_to_one_copies_exactly_and_honours_stride() {
        let (w, h, stride) = (4u32, 3u32, 24usize);
        let mut src = vec![0xEEu8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..(w as usize * 4) {
                src[y * stride + x] = (y * 16 + x) as u8;
            }
        }
        let mut dst = vec![0u8; (w * h * 4) as usize];
        Scaler::new().scale(&src, w, h, stride, &mut dst, w, h);
        for y in 0..h as usize {
            assert_eq!(&dst[y * 16..(y + 1) * 16], &src[y * stride..y * stride + 16]);
        }
    }

    #[test]
    fn letterbox_has_black_bars_and_untouched_colour() {
        // 16:9 red into a 4:3 target: bars top/bottom, red in the middle.
        let src = solid(160, 90, [200, 10, 20, 255]);
        let mut dst = vec![7u8; 80 * 60 * 4];
        Scaler::new().scale(&src, 160, 90, 160 * 4, &mut dst, 80, 60);
        let (_, fy, _, fh) = fit_rect(160, 90, 80, 60);
        assert!(fy > 0);
        assert_eq!(at(&dst, 80, 40, 0), [0, 0, 0, 255]);
        assert_eq!(at(&dst, 80, 40, 59), [0, 0, 0, 255]);
        assert_eq!(at(&dst, 80, 0, fy), [200, 10, 20, 255]);
        assert_eq!(at(&dst, 80, 79, fy + fh - 1), [200, 10, 20, 255]);
        assert_eq!(at(&dst, 80, 40, 30), [200, 10, 20, 255]);
    }

    #[test]
    fn pillarbox_has_black_side_bars() {
        let src = solid(90, 160, [1, 2, 3, 255]);
        let mut dst = vec![7u8; 160 * 90 * 4];
        Scaler::new().scale(&src, 90, 160, 90 * 4, &mut dst, 160, 90);
        assert_eq!(at(&dst, 160, 0, 45), [0, 0, 0, 255]);
        assert_eq!(at(&dst, 160, 159, 45), [0, 0, 0, 255]);
        assert_eq!(at(&dst, 160, 80, 45), [1, 2, 3, 255]);
    }

    #[test]
    fn downscale_averages_instead_of_dropping_pixels() {
        // 1-pixel black/white checkerboard halved: every output pixel must be
        // mid-grey, where nearest-neighbour would give pure black or white.
        let (w, h) = (8u32, 8u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                let i = ((y * w + x) * 4) as usize;
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let mut dst = vec![0u8; 4 * 4 * 4];
        Scaler::new().scale(&src, w, h, (w * 4) as usize, &mut dst, 4, 4);
        for px in dst.chunks_exact(4) {
            assert!((px[0] as i32 - 128).abs() <= 2, "got {}", px[0]);
        }
    }

    #[test]
    fn short_source_buffer_is_ignored_not_a_panic() {
        let mut dst = vec![9u8; 4 * 4 * 4];
        Scaler::new().scale(&[0u8; 8], 8, 8, 32, &mut dst, 4, 4);
        assert!(dst.iter().all(|&b| b == 9));
    }
}
