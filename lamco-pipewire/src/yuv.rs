//! YUV Format Conversion Utilities
//!
//! Provides conversion from YUV color formats to RGB/BGRA for display.
//! These conversions are useful when PipeWire provides frames in compressed
//! YUV formats (NV12, I420, YUY2) that need to be converted for rendering.
//!
//! # Supported Formats
//!
//! - **NV12**: YUV 4:2:0 with interleaved UV plane (common for hardware encoders)
//! - **I420**: YUV 4:2:0 with separate U and V planes (aka YV12)
//! - **YUY2**: YUV 4:2:2 packed format (YUYV)
//!
//! # Performance
//!
//! These are reference implementations prioritizing correctness over speed.
//! For production use with high frame rates, consider:
//! - SIMD-accelerated implementations
//! - GPU-based conversion (OpenGL/Vulkan shaders)
//! - Hardware decoder output directly to RGB
//!
//! # Examples
//!
//! ```rust,no_run
//! use lamco_pipewire::yuv::{nv12_to_bgra, YuvConverter};
//!
//! // Direct conversion
//! let nv12_data: &[u8] = &[0u8; 3110400]; // 1920x1080 NV12 frame
//! let bgra = nv12_to_bgra(nv12_data, 1920, 1080);
//!
//! // Using converter with format detection
//! let converter = YuvConverter::new();
//! ```

use crate::format::PixelFormat;

/// Convert NV12 to BGRA
///
/// NV12 is YUV 4:2:0 with:
/// - Y plane: width * height bytes
/// - UV plane: width * height / 2 bytes (interleaved U, V)
///
/// # Arguments
///
/// * `src` - Source NV12 data
/// * `width` - Frame width (must be even)
/// * `height` - Frame height (must be even)
///
/// # Returns
///
/// BGRA data (width * height * 4 bytes)
///
/// # Panics
///
/// Panics if source data is too small for the given dimensions.
#[must_use]
pub fn nv12_to_bgra(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;

    let y_plane_size = w * h;
    let uv_plane_size = w * h / 2;

    assert!(
        src.len() >= y_plane_size + uv_plane_size,
        "NV12 source data too small: need {}, got {}",
        y_plane_size + uv_plane_size,
        src.len()
    );

    let y_plane = &src[..y_plane_size];
    let uv_plane = &src[y_plane_size..y_plane_size + uv_plane_size];

    let mut dst = vec![0u8; w * h * 4];

    for y in 0..h {
        for x in 0..w {
            let y_idx = y * w + x;
            let uv_idx = (y / 2) * w + (x / 2) * 2;

            let y_val = i32::from(y_plane[y_idx]);
            let u_val = i32::from(uv_plane[uv_idx]);
            let v_val = i32::from(uv_plane[uv_idx + 1]);

            let (r, g, b) = yuv_to_rgb(y_val, u_val, v_val);

            let dst_idx = y_idx * 4;
            dst[dst_idx] = b;
            dst[dst_idx + 1] = g;
            dst[dst_idx + 2] = r;
            dst[dst_idx + 3] = 255; // Alpha
        }
    }

    dst
}

/// Convert I420 to BGRA
///
/// I420 is YUV 4:2:0 with separate planes:
/// - Y plane: width * height bytes
/// - U plane: width/2 * height/2 bytes
/// - V plane: width/2 * height/2 bytes
///
/// # Arguments
///
/// * `src` - Source I420 data
/// * `width` - Frame width (must be even)
/// * `height` - Frame height (must be even)
///
/// # Returns
///
/// BGRA data (width * height * 4 bytes)
#[must_use]
pub fn i420_to_bgra(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;

    let y_plane_size = w * h;
    let uv_plane_size = (w / 2) * (h / 2);

    assert!(
        src.len() >= y_plane_size + uv_plane_size * 2,
        "I420 source data too small"
    );

    let y_plane = &src[..y_plane_size];
    let u_plane = &src[y_plane_size..y_plane_size + uv_plane_size];
    let v_plane = &src[y_plane_size + uv_plane_size..y_plane_size + uv_plane_size * 2];

    let mut dst = vec![0u8; w * h * 4];

    for y in 0..h {
        for x in 0..w {
            let y_idx = y * w + x;
            let uv_idx = (y / 2) * (w / 2) + (x / 2);

            let y_val = i32::from(y_plane[y_idx]);
            let u_val = i32::from(u_plane[uv_idx]);
            let v_val = i32::from(v_plane[uv_idx]);

            let (r, g, b) = yuv_to_rgb(y_val, u_val, v_val);

            let dst_idx = y_idx * 4;
            dst[dst_idx] = b;
            dst[dst_idx + 1] = g;
            dst[dst_idx + 2] = r;
            dst[dst_idx + 3] = 255;
        }
    }

    dst
}

/// Convert YUY2 to BGRA
///
/// YUY2 is YUV 4:2:2 packed format:
/// - Each 4-byte macro pixel: Y0, U, Y1, V
/// - Represents 2 horizontal pixels sharing U and V
///
/// # Arguments
///
/// * `src` - Source YUY2 data
/// * `width` - Frame width (must be even)
/// * `height` - Frame height
///
/// # Returns
///
/// BGRA data (width * height * 4 bytes)
#[must_use]
pub fn yuy2_to_bgra(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;

    assert!(w.is_multiple_of(2), "YUY2 width must be even");
    assert!(src.len() >= w * h * 2, "YUY2 source data too small");

    let mut dst = vec![0u8; w * h * 4];

    for y in 0..h {
        for x in (0..w).step_by(2) {
            let src_idx = (y * w + x) * 2;

            let y0 = i32::from(src[src_idx]);
            let u = i32::from(src[src_idx + 1]);
            let y1 = i32::from(src[src_idx + 2]);
            let v = i32::from(src[src_idx + 3]);

            // First pixel
            let (r0, g0, b0) = yuv_to_rgb(y0, u, v);
            let dst_idx0 = (y * w + x) * 4;
            dst[dst_idx0] = b0;
            dst[dst_idx0 + 1] = g0;
            dst[dst_idx0 + 2] = r0;
            dst[dst_idx0 + 3] = 255;

            // Second pixel
            let (r1, g1, b1) = yuv_to_rgb(y1, u, v);
            let dst_idx1 = (y * w + x + 1) * 4;
            dst[dst_idx1] = b1;
            dst[dst_idx1 + 1] = g1;
            dst[dst_idx1 + 2] = r1;
            dst[dst_idx1 + 3] = 255;
        }
    }

    dst
}

/// Convert single YUV pixel to RGB
///
/// Uses BT.601 color matrix (standard for SD video):
/// R = 1.164(Y-16) + 1.596(V-128)
/// G = 1.164(Y-16) - 0.813(V-128) - 0.391(U-128)
/// B = 1.164(Y-16) + 2.018(U-128)
#[inline]
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    // Scale factors (multiplied by 256 for integer math)
    const Y_SCALE: i32 = 298; // 1.164 * 256
    const V_TO_R: i32 = 409; // 1.596 * 256
    const U_TO_G: i32 = 100; // 0.391 * 256
    const V_TO_G: i32 = 208; // 0.813 * 256
    const U_TO_B: i32 = 516; // 2.018 * 256

    let y = y - 16;
    let u = u - 128;
    let v = v - 128;

    let r = (Y_SCALE * y + V_TO_R * v + 128) >> 8;
    let g = (Y_SCALE * y - U_TO_G * u - V_TO_G * v + 128) >> 8;
    let b = (Y_SCALE * y + U_TO_B * u + 128) >> 8;

    (r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8)
}

/// YUV format converter with caching and format detection
pub struct YuvConverter {
    /// Reusable output buffer to avoid allocations
    output_buffer: Vec<u8>,
}

impl YuvConverter {
    /// Create a new YUV converter
    #[must_use]
    pub fn new() -> Self {
        Self {
            output_buffer: Vec::new(),
        }
    }

    /// Convert YUV data to BGRA
    ///
    /// # Arguments
    ///
    /// * `src` - Source YUV data
    /// * `width` - Frame width
    /// * `height` - Frame height
    /// * `format` - Source pixel format
    ///
    /// # Returns
    ///
    /// Reference to internal BGRA buffer (valid until next conversion)
    pub fn convert_to_bgra(&mut self, src: &[u8], width: u32, height: u32, format: PixelFormat) -> Option<&[u8]> {
        let w = width as usize;
        let h = height as usize;

        // The primitive converters assume a correctly sized source and panic
        // otherwise. Validate dimensions and length here, with checked
        // arithmetic, so this fallible entry point returns None rather than
        // panicking or over-allocating on a malformed frame.
        if width == 0 || height == 0 {
            return None;
        }
        let pixels = w.checked_mul(h)?;
        pixels.checked_mul(4)?; // BGRA output must not overflow usize
        let required = match format {
            // 4:2:0 chroma subsampling (NV12, I420) requires even dimensions;
            // an odd width or height would index past the half-size chroma planes.
            PixelFormat::NV12 | PixelFormat::I420 => {
                if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
                    return None;
                }
                pixels.checked_add(pixels / 2)?
            }
            PixelFormat::YUY2 => {
                if !w.is_multiple_of(2) {
                    return None; // 4:2:2 requires an even width
                }
                pixels.checked_mul(2)?
            }
            // Already in an RGB family, or unsupported: no conversion.
            _ => return None,
        };
        if src.len() < required {
            return None;
        }

        let result = match format {
            PixelFormat::NV12 => nv12_to_bgra(src, width, height),
            PixelFormat::I420 => i420_to_bgra(src, width, height),
            PixelFormat::YUY2 => yuy2_to_bgra(src, width, height),
            _ => return None,
        };

        self.output_buffer = result;
        Some(&self.output_buffer)
    }

    /// Check if format needs YUV conversion
    #[must_use]
    pub fn needs_conversion(format: PixelFormat) -> bool {
        matches!(format, PixelFormat::NV12 | PixelFormat::I420 | PixelFormat::YUY2)
    }

    /// Get required buffer size for BGRA output
    #[must_use]
    pub fn output_size(width: u32, height: u32) -> usize {
        (width as usize) * (height as usize) * 4
    }
}

impl Default for YuvConverter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yuv_to_rgb() {
        // Black (Y=16, U=128, V=128)
        let (r, g, b) = yuv_to_rgb(16, 128, 128);
        assert_eq!((r, g, b), (0, 0, 0));

        // White (Y=235, U=128, V=128)
        let (r, g, b) = yuv_to_rgb(235, 128, 128);
        assert!(r > 250 && g > 250 && b > 250);
    }

    #[test]
    fn test_nv12_to_bgra() {
        // 2x2 black frame in NV12
        // Y plane: 4 bytes of 16 (black)
        // UV plane: 2 bytes of 128, 128
        let nv12 = vec![16, 16, 16, 16, 128, 128];
        let bgra = nv12_to_bgra(&nv12, 2, 2);

        assert_eq!(bgra.len(), 16); // 2x2x4
        // All pixels should be near-black
        assert!(bgra[0] < 5 && bgra[1] < 5 && bgra[2] < 5);
        assert_eq!(bgra[3], 255); // Alpha
    }

    #[test]
    fn test_i420_to_bgra() {
        // 2x2 black frame in I420
        let i420 = vec![
            16, 16, 16, 16,  // Y plane
            128, // U plane (1 byte for 2x2)
            128, // V plane
        ];
        let bgra = i420_to_bgra(&i420, 2, 2);

        assert_eq!(bgra.len(), 16);
        assert!(bgra[0] < 5 && bgra[1] < 5 && bgra[2] < 5);
    }

    #[test]
    fn test_yuy2_to_bgra() {
        // 2x2 black frame in YUY2
        // Row 1: Y0, U, Y1, V (2 pixels)
        // Row 2: Y0, U, Y1, V (2 pixels)
        let yuy2 = vec![
            16, 128, 16, 128, // Row 1
            16, 128, 16, 128, // Row 2
        ];
        let bgra = yuy2_to_bgra(&yuy2, 2, 2);

        assert_eq!(bgra.len(), 16);
        assert!(bgra[0] < 5 && bgra[1] < 5 && bgra[2] < 5);
    }

    #[test]
    fn test_yuv_converter() {
        let mut converter = YuvConverter::new();

        assert!(YuvConverter::needs_conversion(PixelFormat::NV12));
        assert!(YuvConverter::needs_conversion(PixelFormat::I420));
        assert!(!YuvConverter::needs_conversion(PixelFormat::BGRA));

        // Test conversion
        let nv12 = vec![16, 16, 16, 16, 128, 128];
        let result = converter.convert_to_bgra(&nv12, 2, 2, PixelFormat::NV12);
        assert!(result.is_some());
        assert_eq!(result.expect("should have result").len(), 16);
    }

    #[test]
    fn convert_to_bgra_rejects_malformed_without_panic() {
        // Regression (found by fuzzing): convert_to_bgra returns Option, so a
        // malformed frame must yield None, never panic or over-read.
        let mut conv = YuvConverter::new();
        // Source too small for the claimed dimensions.
        assert!(conv.convert_to_bgra(&[0u8; 4], 64, 64, PixelFormat::NV12).is_none());
        // Odd dimensions are invalid for 4:2:0 (NV12 / I420): the empty chroma
        // plane used to be indexed out of bounds.
        assert!(conv.convert_to_bgra(&[0u8; 16], 1, 1, PixelFormat::I420).is_none());
        assert!(conv.convert_to_bgra(&[0u8; 16], 3, 2, PixelFormat::NV12).is_none());
        // Odd width is invalid for 4:2:2 (YUY2).
        assert!(conv.convert_to_bgra(&[0u8; 16], 3, 2, PixelFormat::YUY2).is_none());
        // Dimensions that overflow usize must not panic.
        assert!(
            conv.convert_to_bgra(&[0u8; 16], u32::MAX, u32::MAX, PixelFormat::NV12)
                .is_none()
        );
        // A correctly sized 2x2 NV12 frame (4 Y + 2 UV bytes) still converts.
        assert!(conv.convert_to_bgra(&[0u8; 6], 2, 2, PixelFormat::NV12).is_some());
    }
}
