//! SIMD RGB -> NV12 conversion for the CPU paths (x264 and the ffmpeg upload path).
//!
//! The matrix and range here are the single source of truth for the colour the
//! encoders signal in the SPS VUI ([`COLOUR_PRIMARIES`] etc.). If the picture is
//! converted with BT.709 but signalled as BT.601 (or the reverse) the phone
//! decodes visibly shifted colours, so both sides read these constants.

use crate::capture::PixelFormat;
use yuvutils_rs::{
    bgra_to_yuv_nv12, rgba_to_yuv_nv12, BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode,
    YuvRange, YuvStandardMatrix,
};

/// H.273 code points written into the VUI. 1 = BT.709 for all three.
pub const COLOUR_PRIMARIES: i32 = 1;
pub const TRANSFER_CHARACTERISTICS: i32 = 1;
pub const MATRIX_COEFFICIENTS: i32 = 1;

/// Limited (studio) range: Y 16..235, UV 16..240. Every hardware H.264 decoder
/// treats limited as the default, so this is the least surprising choice for
/// MediaCodec; full range is signalled inconsistently across Android vendors.
pub const FULL_RANGE: bool = false;

const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt709;
const RANGE: YuvRange = if FULL_RANGE { YuvRange::Full } else { YuvRange::Limited };

/// Size in bytes of a tightly packed NV12 image (Y plane then interleaved UV).
pub fn nv12_len(width: usize, height: usize) -> usize {
    width * height + width * (height / 2)
}

/// Converts a packed 32-bit RGBA/BGRA image into NV12 using AVX2/SSE code paths.
/// `nv12_out` must hold at least [`nv12_len`] bytes; Y stride and UV stride are `width`.
pub fn to_nv12(
    format: PixelFormat,
    src: &[u8],
    width: usize,
    height: usize,
    nv12_out: &mut [u8],
) -> Result<(), String> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(format!("invalid NV12 dimensions {}x{}", width, height));
    }
    if src.len() < width * height * 4 {
        return Err(format!(
            "source too small: {} bytes for {}x{} (need {})",
            src.len(),
            width,
            height,
            width * height * 4
        ));
    }
    if nv12_out.len() < nv12_len(width, height) {
        return Err("destination buffer too small for NV12".into());
    }

    let (y, uv) = nv12_out.split_at_mut(width * height);
    let mut image = YuvBiPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride: width as u32,
        uv_plane: BufferStoreMut::Borrowed(&mut uv[..width * (height / 2)]),
        uv_stride: width as u32,
        width: width as u32,
        height: height as u32,
    };
    let stride = (width * 4) as u32;
    let result = match format {
        PixelFormat::Rgba => {
            rgba_to_yuv_nv12(&mut image, src, stride, RANGE, MATRIX, YuvConversionMode::Balanced)
        }
        PixelFormat::Bgra => {
            bgra_to_yuv_nv12(&mut image, src, stride, RANGE, MATRIX, YuvConversionMode::Balanced)
        }
    };
    result.map_err(|e| format!("yuv conversion failed: {:?}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Converts a flat-colour 16x16 image and returns (Y, U, V) of its first pixel/block.
    fn flat(format: PixelFormat, px: [u8; 4]) -> (u8, u8, u8) {
        let (w, h) = (16usize, 16usize);
        let src: Vec<u8> = px.iter().cycle().take(w * h * 4).copied().collect();
        let mut out = vec![0u8; nv12_len(w, h)];
        to_nv12(format, &src, w, h, &mut out).unwrap();
        (out[0], out[w * h], out[w * h + 1])
    }

    fn near(a: u8, b: i32, tol: i32) -> bool {
        (a as i32 - b).abs() <= tol
    }

    #[test]
    fn full_range_black_and_white_map_to_limited_y() {
        // The source RGB is full range (0..255); the stream is limited range.
        for fmt in [PixelFormat::Rgba, PixelFormat::Bgra] {
            let (y, u, v) = flat(fmt, [0, 0, 0, 255]);
            assert!(near(y, 16, 1), "black Y={y}");
            assert!(near(u, 128, 1) && near(v, 128, 1), "black UV={u},{v}");
            let (y, u, v) = flat(fmt, [255, 255, 255, 255]);
            assert!(near(y, 235, 1), "white Y={y}");
            assert!(near(u, 128, 1) && near(v, 128, 1), "white UV={u},{v}");
        }
    }

    #[test]
    fn primaries_use_bt709_coefficients() {
        // Y = 16 + 219 * (0.2126 R + 0.7152 G + 0.0722 B)
        let (y, _, _) = flat(PixelFormat::Rgba, [255, 0, 0, 255]);
        assert!(near(y, 63, 2), "red Y={y}");
        let (y, _, _) = flat(PixelFormat::Rgba, [0, 255, 0, 255]);
        assert!(near(y, 173, 2), "green Y={y}");
        let (y, _, _) = flat(PixelFormat::Rgba, [0, 0, 255, 255]);
        assert!(near(y, 32, 2), "blue Y={y}");
    }

    #[test]
    fn rgba_and_bgra_agree_on_channel_order() {
        // Pure red: R=255 in RGBA byte 0, in BGRA byte 2. Red has V well above 128.
        let (_, _, v_rgba) = flat(PixelFormat::Rgba, [255, 0, 0, 255]);
        let (_, _, v_bgra) = flat(PixelFormat::Bgra, [0, 0, 255, 255]);
        assert!(v_rgba > 200 && v_rgba == v_bgra, "v_rgba={v_rgba} v_bgra={v_bgra}");
        // Pure blue: U well above 128.
        let (_, u_rgba, _) = flat(PixelFormat::Rgba, [0, 0, 255, 255]);
        let (_, u_bgra, _) = flat(PixelFormat::Bgra, [255, 0, 0, 255]);
        assert!(u_rgba > 200 && u_rgba == u_bgra);
    }

    #[test]
    fn rejects_bad_buffers() {
        let mut out = vec![0u8; nv12_len(16, 16)];
        assert!(to_nv12(PixelFormat::Rgba, &[0; 10], 16, 16, &mut out).is_err());
        assert!(to_nv12(PixelFormat::Rgba, &vec![0; 16 * 16 * 4], 16, 16, &mut out[..10]).is_err());
        assert!(to_nv12(PixelFormat::Rgba, &[], 0, 0, &mut out).is_err());
    }
}
