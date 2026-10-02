//! SIMD-Accelerated Damage Detection
//!
//! Tile-based frame differencing to detect changed screen regions,
//! enabling significant bandwidth reduction (90%+ for static content).
//!
//! Supports AVX2 (x86_64), NEON (aarch64), and scalar fallback.
//!
//! # Algorithm
//!
//! 1. Divide frame into configurable tile grid (default 64x64 pixels)
//! 2. SIMD-compare each tile against previous frame
//! 3. Mark tile dirty if difference exceeds threshold
//! 4. Merge adjacent dirty tiles into larger regions
//! 5. Return optimized list of damage regions

#![allow(unsafe_code)]

use std::time::Instant;

/// A rectangular region of the screen that has changed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DetectedRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DetectedRegion {
    #[inline]
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    #[inline]
    pub fn full_frame(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[inline]
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn overlaps(&self, other: &DetectedRegion) -> bool {
        let self_right = self.x + self.width;
        let self_bottom = self.y + self.height;
        let other_right = other.x + other.width;
        let other_bottom = other.y + other.height;

        self.x < other_right && self_right > other.x && self.y < other_bottom && self_bottom > other.y
    }

    #[inline]
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn union(&self, other: &DetectedRegion) -> DetectedRegion {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);

        DetectedRegion {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    pub fn is_adjacent(&self, other: &DetectedRegion, merge_distance: u32) -> bool {
        let self_right = self.x + self.width;
        let self_bottom = self.y + self.height;
        let other_right = other.x + other.width;
        let other_bottom = other.y + other.height;

        let gap_x = if other.x >= self_right {
            other.x - self_right
        } else {
            self.x.saturating_sub(other_right)
        };

        let gap_y = if other.y >= self_bottom {
            other.y - self_bottom
        } else {
            self.y.saturating_sub(other_bottom)
        };

        gap_x <= merge_distance && gap_y <= merge_distance
    }
}

/// Configuration for damage detection
#[derive(Debug, Clone)]
pub struct DamageConfig {
    /// Size of each comparison tile in pixels (default: 64)
    pub tile_size: usize,
    /// Fraction of tile pixels that must differ to mark as dirty (default: 0.05)
    pub diff_threshold: f32,
    /// Maximum per-channel pixel difference to consider "same" (default: 4)
    pub pixel_threshold: u8,
    /// Distance in pixels for merging adjacent dirty tiles (default: 32)
    pub merge_distance: u32,
    /// Minimum region area to report (default: 256)
    pub min_region_area: u64,
}

impl Default for DamageConfig {
    fn default() -> Self {
        Self {
            tile_size: 64,
            diff_threshold: 0.05,
            pixel_threshold: 4,
            merge_distance: 32,
            min_region_area: 256,
        }
    }
}

impl DamageConfig {
    /// Finer granularity, more sensitive detection
    pub fn low_bandwidth() -> Self {
        Self {
            tile_size: 32,
            diff_threshold: 0.02,
            pixel_threshold: 2,
            merge_distance: 16,
            min_region_area: 64,
        }
    }

    /// Coarser detection for high-motion content
    pub fn high_motion() -> Self {
        Self {
            tile_size: 128,
            diff_threshold: 0.10,
            pixel_threshold: 8,
            merge_distance: 64,
            min_region_area: 1024,
        }
    }
}

/// Detection statistics
#[derive(Debug, Clone, Default)]
pub struct DamageDetectorStats {
    pub frames_processed: u64,
    pub frames_skipped: u64,
    pub frames_full: u64,
    pub frames_partial: u64,
    pub total_damage_area: u64,
    pub total_frame_area: u64,
    pub total_detection_time_ns: u64,
    pub avg_damage_ratio: f32,
    pub avg_detection_time_ms: f32,
}

impl DamageDetectorStats {
    /// Bandwidth saved as a percentage (100% = all frames identical)
    pub fn bandwidth_reduction_percent(&self) -> f32 {
        if self.total_frame_area == 0 {
            return 0.0;
        }
        let ratio = self.total_damage_area as f32 / self.total_frame_area as f32;
        (1.0 - ratio) * 100.0
    }

    fn update_averages(&mut self) {
        if self.frames_processed > 0 {
            self.avg_damage_ratio = self.total_damage_area as f32 / self.total_frame_area.max(1) as f32;
            self.avg_detection_time_ms =
                (self.total_detection_time_ns as f64 / self.frames_processed as f64 / 1_000_000.0) as f32;
        }
    }
}

// --- SIMD pixel comparison ---

fn count_different_pixels_scalar(prev: &[u8], curr: &[u8], threshold: u8) -> u32 {
    let mut count = 0u32;

    for (p, c) in prev.chunks_exact(4).zip(curr.chunks_exact(4)) {
        let diff_b = (p[0] as i16 - c[0] as i16).unsigned_abs() as u8;
        let diff_g = (p[1] as i16 - c[1] as i16).unsigned_abs() as u8;
        let diff_r = (p[2] as i16 - c[2] as i16).unsigned_abs() as u8;

        if diff_b > threshold || diff_g > threshold || diff_r > threshold {
            count += 1;
        }
    }

    count
}

// The SIMD kernels below reduce one count per BGRA pixel (not per byte): a pixel
// is "different" when any of its blue/green/red channels differs by more than
// `threshold`, with alpha ignored. This matches `count_different_pixels_scalar`
// exactly. Counting per pixel keeps the horizontal reduction small (at most one
// per 4-byte lane), so it cannot overflow the way an 8-bit byte-sum would.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
fn count_different_pixels_avx2(prev: &[u8], curr: &[u8], threshold: u8) -> u32 {
    use std::arch::x86_64::*;

    if prev.len() < 32 || curr.len() < 32 {
        return count_different_pixels_scalar(prev, curr, threshold);
    }

    // SAFETY: AVX2 target_feature is guaranteed by cfg gate.
    // Pointer arithmetic stays within slice bounds (chunks * 32 <= len).
    unsafe {
        // 0x00FFFFFF per 32-bit lane keeps B/G/R and zeroes alpha (little-endian
        // byte order within the lane is B, G, R, A).
        let bgr_mask = _mm256_set1_epi32(0x00FF_FFFFu32 as i32);
        let threshold_vec = _mm256_set1_epi8(threshold as i8);
        let zero = _mm256_setzero_si256();

        let mut diff_count = 0u32;
        let chunks = prev.len() / 32; // 32 bytes = 8 BGRA pixels

        for i in 0..chunks {
            let offset = i * 32;
            let prev_data = _mm256_loadu_si256(prev.as_ptr().add(offset) as *const __m256i);
            let curr_data = _mm256_loadu_si256(curr.as_ptr().add(offset) as *const __m256i);

            // Per-byte |prev - curr| via saturating subtraction in both directions.
            let diff = _mm256_or_si256(
                _mm256_subs_epu8(prev_data, curr_data),
                _mm256_subs_epu8(curr_data, prev_data),
            );
            // Unsigned ">threshold": saturating-subtract the threshold, leaving a
            // non-zero byte exactly where the difference exceeded it. (A direct
            // _mm256_cmpgt_epi8 is signed and misbehaves once a diff exceeds 127.)
            let over = _mm256_subs_epu8(diff, threshold_vec);

            // Drop alpha, then a 32-bit lane is zero only when the whole pixel is
            // within threshold; count the lanes that are not.
            let over_bgr = _mm256_and_si256(over, bgr_mask);
            let unchanged = _mm256_cmpeq_epi32(over_bgr, zero);
            let unchanged_bits = _mm256_movemask_ps(_mm256_castsi256_ps(unchanged)) as u32;
            diff_count += 8 - unchanged_bits.count_ones();
        }

        let remaining_start = chunks * 32;
        if remaining_start < prev.len() {
            diff_count += count_different_pixels_scalar(&prev[remaining_start..], &curr[remaining_start..], threshold);
        }

        diff_count
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
fn count_different_pixels_neon(prev: &[u8], curr: &[u8], threshold: u8) -> u32 {
    use std::arch::aarch64::*;

    if prev.len() < 16 || curr.len() < 16 {
        return count_different_pixels_scalar(prev, curr, threshold);
    }

    // SAFETY: NEON target_feature is guaranteed by cfg gate.
    // Pointer arithmetic stays within slice bounds (chunks * 16 <= len).
    unsafe {
        // [0xFF, 0xFF, 0xFF, 0x00] per pixel keeps B/G/R and zeroes alpha.
        let bgr_mask = vld1q_u8(
            [
                0xFF, 0xFF, 0xFF, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0xFF, 0xFF, 0xFF, 0x00,
            ]
            .as_ptr(),
        );
        let threshold_vec = vdupq_n_u8(threshold);
        let zero = vdupq_n_u32(0);

        let mut diff_count = 0u32;
        let chunks = prev.len() / 16; // 16 bytes = 4 BGRA pixels

        for i in 0..chunks {
            let offset = i * 16;
            let prev_data = vld1q_u8(prev.as_ptr().add(offset));
            let curr_data = vld1q_u8(curr.as_ptr().add(offset));

            // Per-byte |prev - curr|, then a per-byte ">threshold" mask (0xFF/0x00).
            let diff = vabdq_u8(prev_data, curr_data);
            let exceeds = vcgtq_u8(diff, threshold_vec);

            // Drop alpha, reinterpret each pixel as one 32-bit lane (non-zero iff
            // any B/G/R byte exceeded), and sum the set lanes. The widened 32-bit
            // horizontal add tops out at 4 per chunk, so it cannot overflow.
            let exceeds_bgr = vandq_u8(exceeds, bgr_mask);
            let lanes = vreinterpretq_u32_u8(exceeds_bgr);
            let changed = vcgtq_u32(lanes, zero);
            diff_count += vaddvq_u32(vshrq_n_u32(changed, 31));
        }

        let remaining_start = chunks * 16;
        if remaining_start < prev.len() {
            diff_count += count_different_pixels_scalar(&prev[remaining_start..], &curr[remaining_start..], threshold);
        }

        diff_count
    }
}

#[inline]
fn count_different_pixels(prev: &[u8], curr: &[u8], threshold: u8) -> u32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        count_different_pixels_avx2(prev, curr, threshold)
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        count_different_pixels_neon(prev, curr, threshold)
    }

    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx2"),
        all(target_arch = "aarch64", target_feature = "neon")
    )))]
    {
        count_different_pixels_scalar(prev, curr, threshold)
    }
}

// --- Region merging ---

fn merge_regions(mut regions: Vec<DetectedRegion>, merge_distance: u32) -> Vec<DetectedRegion> {
    if regions.len() <= 1 {
        return regions;
    }

    let mut changed = true;
    while changed {
        changed = false;
        let mut merged = Vec::with_capacity(regions.len());
        let mut used = vec![false; regions.len()];

        for i in 0..regions.len() {
            if used[i] {
                continue;
            }

            let mut current = regions[i];
            used[i] = true;

            for j in (i + 1)..regions.len() {
                if used[j] {
                    continue;
                }

                if current.is_adjacent(&regions[j], merge_distance) {
                    current = current.union(&regions[j]);
                    used[j] = true;
                    changed = true;
                }
            }

            merged.push(current);
        }

        regions = merged;
    }

    regions
}

fn tiles_to_regions(
    dirty_tiles: &[bool],
    tiles_x: usize,
    tiles_y: usize,
    tile_size: usize,
    frame_width: u32,
    frame_height: u32,
) -> Vec<DetectedRegion> {
    let mut regions = Vec::new();

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let idx = ty * tiles_x + tx;
            if dirty_tiles[idx] {
                let x = (tx * tile_size) as u32;
                let y = (ty * tile_size) as u32;
                let width = (tile_size as u32).min(frame_width.saturating_sub(x));
                let height = (tile_size as u32).min(frame_height.saturating_sub(y));

                if width > 0 && height > 0 {
                    regions.push(DetectedRegion::new(x, y, width, height));
                }
            }
        }
    }

    regions
}

// --- Main detector ---

/// SIMD-accelerated damage detection engine
///
/// Compares consecutive BGRA frames tile-by-tile to identify changed regions.
/// Supports AVX2, NEON, and scalar paths.
pub struct DamageDetector {
    config: DamageConfig,
    previous_frame: Option<Vec<u8>>,
    previous_dimensions: Option<(u32, u32)>,
    tile_dirty: Vec<bool>,
    tiles_x: usize,
    tiles_y: usize,
    stats: DamageDetectorStats,
    invalidated: bool,
}

impl DamageDetector {
    pub fn new(config: DamageConfig) -> Self {
        Self {
            config,
            previous_frame: None,
            previous_dimensions: None,
            tile_dirty: Vec::new(),
            tiles_x: 0,
            tiles_y: 0,
            stats: DamageDetectorStats::default(),
            invalidated: true,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DamageConfig::default())
    }

    /// Detect changed regions between this frame and the previous one.
    ///
    /// Returns empty if the frame is identical. Returns full-frame damage
    /// on first call or after invalidation.
    ///
    /// `frame` must be BGRA pixel data (4 bytes per pixel).
    #[expect(
        clippy::unwrap_used,
        reason = "previous_frame is guaranteed Some after first frame check"
    )]
    pub fn detect(&mut self, frame: &[u8], width: u32, height: u32) -> Vec<DetectedRegion> {
        let start = Instant::now();
        let frame_area = width as u64 * height as u64;

        // A frame whose length does not match width*height*4 (or whose
        // dimensions overflow usize) is malformed. Rather than panic, resync
        // state and conservatively report the whole frame as damaged.
        let expected_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|p| p.checked_mul(4));
        if expected_len != Some(frame.len()) {
            self.previous_frame = None;
            self.invalidated = true;
            return vec![DetectedRegion::full_frame(width, height)];
        }

        let dimensions_changed = self.previous_dimensions.is_none_or(|(w, h)| w != width || h != height);

        if self.previous_frame.is_none() || self.invalidated || dimensions_changed {
            self.update_tile_grid(width, height);
            self.previous_frame = Some(frame.to_vec());
            self.previous_dimensions = Some((width, height));
            self.invalidated = false;

            self.stats.frames_processed += 1;
            self.stats.frames_full += 1;
            self.stats.total_damage_area += frame_area;
            self.stats.total_frame_area += frame_area;
            self.stats.total_detection_time_ns += start.elapsed().as_nanos() as u64;
            self.stats.update_averages();

            return vec![DetectedRegion::full_frame(width, height)];
        }

        let mut prev_frame = self.previous_frame.take().unwrap();
        let regions = self.detect_changes(&prev_frame, frame, width, height);

        let damage_area: u64 = regions.iter().map(DetectedRegion::area).sum();

        self.stats.frames_processed += 1;
        self.stats.total_damage_area += damage_area;
        self.stats.total_frame_area += frame_area;

        if regions.is_empty() {
            self.stats.frames_skipped += 1;
        } else if damage_area >= frame_area * 9 / 10 {
            self.stats.frames_full += 1;
        } else {
            self.stats.frames_partial += 1;
        }

        self.stats.total_detection_time_ns += start.elapsed().as_nanos() as u64;
        self.stats.update_averages();

        // Reuse allocation for next comparison
        prev_frame.clear();
        prev_frame.extend_from_slice(frame);
        self.previous_frame = Some(prev_frame);

        regions
    }

    /// Force full-frame damage on next detect() call
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub fn stats(&self) -> &DamageDetectorStats {
        &self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = DamageDetectorStats::default();
    }

    pub fn config(&self) -> &DamageConfig {
        &self.config
    }

    /// Update config and invalidate (next frame treated as full damage)
    pub fn set_config(&mut self, config: DamageConfig) {
        self.config = config;
        self.invalidate();
    }

    fn update_tile_grid(&mut self, width: u32, height: u32) {
        self.tiles_x = (width as usize).div_ceil(self.config.tile_size);
        self.tiles_y = (height as usize).div_ceil(self.config.tile_size);
        let total_tiles = self.tiles_x * self.tiles_y;

        if self.tile_dirty.len() != total_tiles {
            self.tile_dirty = vec![false; total_tiles];
        }
    }

    fn detect_changes(&mut self, prev: &[u8], curr: &[u8], width: u32, height: u32) -> Vec<DetectedRegion> {
        let tile_size = self.config.tile_size;
        let stride = (width as usize) * 4;
        let pixel_threshold = self.config.pixel_threshold;
        let tile_pixels = (tile_size * tile_size) as u32;
        let diff_threshold_count = (tile_pixels as f32 * self.config.diff_threshold) as u32;

        self.tile_dirty.fill(false);

        for ty in 0..self.tiles_y {
            for tx in 0..self.tiles_x {
                let tile_x = tx * tile_size;
                let tile_y = ty * tile_size;

                let tile_width = tile_size.min((width as usize).saturating_sub(tile_x));
                let tile_height = tile_size.min((height as usize).saturating_sub(tile_y));

                if tile_width == 0 || tile_height == 0 {
                    continue;
                }

                let diff_count = self.compare_tile(
                    prev,
                    curr,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                    stride,
                    pixel_threshold,
                );

                let idx = ty * self.tiles_x + tx;
                self.tile_dirty[idx] = diff_count > diff_threshold_count;
            }
        }

        let mut regions = tiles_to_regions(&self.tile_dirty, self.tiles_x, self.tiles_y, tile_size, width, height);

        regions = merge_regions(regions, self.config.merge_distance);
        regions.retain(|r| r.area() >= self.config.min_region_area);

        regions
    }

    fn compare_tile(
        &self,
        prev: &[u8],
        curr: &[u8],
        tile_x: usize,
        tile_y: usize,
        tile_width: usize,
        tile_height: usize,
        stride: usize,
        pixel_threshold: u8,
    ) -> u32 {
        let mut total_diff = 0u32;
        let bytes_per_row = tile_width * 4;

        for row in 0..tile_height {
            let y = tile_y + row;
            let offset = y * stride + tile_x * 4;

            if offset + bytes_per_row > prev.len() || offset + bytes_per_row > curr.len() {
                continue;
            }

            let prev_row = &prev[offset..offset + bytes_per_row];
            let curr_row = &curr[offset..offset + bytes_per_row];

            total_diff += count_different_pixels(prev_row, curr_row, pixel_threshold);
        }

        total_diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_solid_frame(width: usize, height: usize, color: [u8; 4]) -> Vec<u8> {
        let mut data = vec![0u8; width * height * 4];
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
        data
    }

    fn create_frame_with_region(
        width: usize,
        height: usize,
        bg_color: [u8; 4],
        region: DetectedRegion,
        region_color: [u8; 4],
    ) -> Vec<u8> {
        let mut data = create_solid_frame(width, height, bg_color);

        for y in region.y..(region.y + region.height) {
            for x in region.x..(region.x + region.width) {
                if (x as usize) < width && (y as usize) < height {
                    let idx = ((y as usize) * width + (x as usize)) * 4;
                    data[idx..idx + 4].copy_from_slice(&region_color);
                }
            }
        }

        data
    }

    #[test]
    fn test_first_frame_full_damage() {
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(640, 480, [0, 0, 0, 255]);

        let damage = detector.detect(&frame, 640, 480);
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0], DetectedRegion::full_frame(640, 480));
    }

    #[test]
    fn test_identical_frames_no_damage() {
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(640, 480, [100, 100, 100, 255]);

        let _ = detector.detect(&frame, 640, 480);
        let damage = detector.detect(&frame, 640, 480);
        assert!(damage.is_empty());
    }

    #[test]
    fn test_partial_change() {
        let mut detector = DamageDetector::new(DamageConfig {
            tile_size: 64,
            diff_threshold: 0.01,
            pixel_threshold: 1,
            merge_distance: 0,
            min_region_area: 1,
        });

        let frame1 = create_solid_frame(256, 256, [0, 0, 0, 255]);
        let changed_region = DetectedRegion::new(0, 0, 64, 64);
        let frame2 = create_frame_with_region(256, 256, [0, 0, 0, 255], changed_region, [255, 255, 255, 255]);

        let _ = detector.detect(&frame1, 256, 256);
        let damage = detector.detect(&frame2, 256, 256);

        assert!(!damage.is_empty());
        let total_area: u64 = damage.iter().map(DetectedRegion::area).sum();
        assert!(total_area >= changed_region.area() / 2);
    }

    #[test]
    fn test_dimension_change_invalidates() {
        let mut detector = DamageDetector::with_defaults();

        let frame1 = create_solid_frame(640, 480, [100, 100, 100, 255]);
        let frame2 = create_solid_frame(800, 600, [100, 100, 100, 255]);

        let _ = detector.detect(&frame1, 640, 480);
        let damage = detector.detect(&frame2, 800, 600);
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0], DetectedRegion::full_frame(800, 600));
    }

    #[test]
    fn test_invalidate() {
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(640, 480, [100, 100, 100, 255]);

        let _ = detector.detect(&frame, 640, 480);
        detector.invalidate();
        let damage = detector.detect(&frame, 640, 480);
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0], DetectedRegion::full_frame(640, 480));
    }

    #[test]
    fn test_stats() {
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(640, 480, [0, 0, 0, 255]);

        for _ in 0..5 {
            let _ = detector.detect(&frame, 640, 480);
        }

        let stats = detector.stats();
        assert_eq!(stats.frames_processed, 5);
        assert_eq!(stats.frames_full, 1);
        assert_eq!(stats.frames_skipped, 4);
        assert!(stats.bandwidth_reduction_percent() > 0.0);
    }

    #[test]
    fn test_config_presets() {
        let low = DamageConfig::low_bandwidth();
        assert_eq!(low.tile_size, 32);

        let high = DamageConfig::high_motion();
        assert_eq!(high.tile_size, 128);
    }

    #[test]
    fn test_scalar_pixel_comparison() {
        let data = vec![100u8; 64];
        assert_eq!(count_different_pixels_scalar(&data, &data, 4), 0);

        let prev = vec![0u8; 64];
        let curr = vec![255u8; 64];
        assert_eq!(count_different_pixels_scalar(&prev, &curr, 4), 16);
    }

    // The scalar path is the reference; whichever SIMD kernel the build selects
    // must agree with it exactly, including ignoring the alpha channel. Runs the
    // active dispatch (AVX2 with target-feature=+avx2, NEON on aarch64, scalar
    // otherwise) against the scalar implementation over varied buffer lengths and
    // thresholds. Lengths cover the chunk tail and the sub-chunk fast path.
    #[test]
    fn test_simd_matches_scalar_random() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };

        let pixel_counts = [1usize, 3, 4, 7, 8, 15, 16, 17, 33, 64, 100, 256];
        for &pixels in &pixel_counts {
            let len = pixels * 4;
            let mut prev = vec![0u8; len];
            let mut curr = vec![0u8; len];
            for b in prev.iter_mut() {
                *b = (next() & 0xFF) as u8;
            }
            // Keep most pixels unchanged, perturb a quarter of bytes (alpha
            // included on purpose, so a divergence in alpha handling is caught).
            for i in 0..len {
                curr[i] = if next() % 4 == 0 {
                    (next() & 0xFF) as u8
                } else {
                    prev[i]
                };
            }
            for &threshold in &[0u8, 1, 4, 16, 64, 200, 255] {
                let simd = count_different_pixels(&prev, &curr, threshold);
                let scalar = count_different_pixels_scalar(&prev, &curr, threshold);
                assert_eq!(
                    simd, scalar,
                    "pixels={pixels} threshold={threshold}: simd={simd} scalar={scalar}"
                );
            }
        }
    }

    #[test]
    fn test_merge_adjacent_regions() {
        let r1 = DetectedRegion::new(0, 0, 64, 64);
        let r2 = DetectedRegion::new(64, 0, 64, 64);
        let regions = merge_regions(vec![r1, r2], 32);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].width, 128);
    }

    #[test]
    fn test_merge_separate_regions() {
        let r1 = DetectedRegion::new(0, 0, 64, 64);
        let r2 = DetectedRegion::new(200, 200, 64, 64);
        let regions = merge_regions(vec![r1, r2], 32);
        assert_eq!(regions.len(), 2);
    }

    #[test]
    fn test_wrong_size_reports_full_frame() {
        // A frame whose length does not match the claimed dimensions degrades to
        // full-frame damage rather than panicking (fuzz-found robustness fix).
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(640, 480, [0, 0, 0, 255]);
        let regions = detector.detect(&frame, 800, 600);
        assert_eq!(regions.len(), 1);
    }

    #[test]
    fn test_4k_frame() {
        let mut detector = DamageDetector::with_defaults();
        let frame = create_solid_frame(3840, 2160, [0, 128, 255, 255]);

        let damage = detector.detect(&frame, 3840, 2160);
        assert_eq!(damage[0], DetectedRegion::full_frame(3840, 2160));

        let damage2 = detector.detect(&frame, 3840, 2160);
        assert!(damage2.is_empty());
    }

    #[test]
    fn detect_handles_frame_size_mismatch_without_panic() {
        // Regression (found by fuzzing): a frame whose length does not match
        // width*height*4 must degrade to full-frame damage, not panic.
        let mut det = DamageDetector::new(DamageConfig::default());
        assert_eq!(det.detect(&[0u8; 10], 64, 64).len(), 1);
        // Dimensions that overflow usize must not panic either.
        assert_eq!(det.detect(&[0u8; 10], u32::MAX, u32::MAX).len(), 1);
    }
}
