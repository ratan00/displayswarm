//! Region Damage Tracking
//!
//! Tracks which regions of the screen have changed between frames.
//! This is useful for efficient encoding where only changed regions
//! need to be transmitted.
//!
//! # How It Works
//!
//! PipeWire can provide damage metadata indicating which rectangles
//! of the frame have changed. This module aggregates that information
//! and provides utilities for encoding decisions.
//!
//! # Usage
//!
//! ```rust
//! use lamco_pipewire::damage::{DamageTracker, DamageRegion};
//!
//! let mut tracker = DamageTracker::new();
//!
//! // Add damaged regions from PipeWire metadata
//! tracker.add_region(DamageRegion { x: 0, y: 0, width: 100, height: 100 });
//! tracker.add_region(DamageRegion { x: 500, y: 300, width: 200, height: 150 });
//!
//! // Check encoding strategy
//! let frame_size = (1920, 1080);
//! if tracker.should_full_update(frame_size) {
//!     // Encode full frame
//! } else {
//!     // Encode only damaged regions
//!     for region in tracker.damaged_regions() {
//!         // Encode region
//!     }
//! }
//!
//! // Clear for next frame
//! tracker.clear();
//! ```

use std::time::Instant;

/// A damaged (changed) region of the screen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRegion {
    /// X coordinate of top-left corner
    pub x: u32,

    /// Y coordinate of top-left corner
    pub y: u32,

    /// Region width
    pub width: u32,

    /// Region height
    pub height: u32,
}

impl DamageRegion {
    /// Create a new damage region
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Calculate area of the region
    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Check if region contains a point
    #[must_use]
    pub const fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Check if this region overlaps with another
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    /// Merge two overlapping regions into bounding box
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let x2 = (self.x + self.width).max(other.x + other.width);
        let y2 = (self.y + self.height).max(other.y + other.height);

        Self {
            x,
            y,
            width: x2 - x,
            height: y2 - y,
        }
    }

    /// Clip region to frame bounds
    #[must_use]
    pub fn clip(&self, frame_width: u32, frame_height: u32) -> Option<Self> {
        if self.x >= frame_width || self.y >= frame_height {
            return None;
        }

        let x = self.x;
        let y = self.y;
        let width = (self.width).min(frame_width - x);
        let height = (self.height).min(frame_height - y);

        if width == 0 || height == 0 {
            None
        } else {
            Some(Self { x, y, width, height })
        }
    }
}

/// Damage tracking statistics
#[derive(Debug, Clone, Default)]
pub struct DamageStats {
    /// Total frames processed
    pub frames_processed: u64,

    /// Frames with full damage
    pub full_damage_frames: u64,

    /// Frames with partial damage
    pub partial_damage_frames: u64,

    /// Total regions tracked
    pub total_regions: u64,

    /// Average damaged area ratio
    pub avg_damage_ratio: f64,
}

/// Tracks damaged regions between frames
pub struct DamageTracker {
    /// Current damaged regions
    regions: Vec<DamageRegion>,

    /// Threshold for switching to full update (0.0-1.0)
    ///
    /// If damaged area exceeds this fraction of total area,
    /// full update is more efficient.
    full_damage_threshold: f32,

    /// Merge nearby regions if closer than this distance
    merge_distance: u32,

    /// Enable region merging
    enable_merging: bool,

    /// Statistics
    stats: DamageStats,

    /// Last update timestamp
    last_update: Instant,

    /// Maximum regions before forcing full update
    max_regions: usize,
}

impl DamageTracker {
    /// Create a new damage tracker with default settings
    #[must_use]
    pub fn new() -> Self {
        Self {
            regions: Vec::with_capacity(32),
            full_damage_threshold: 0.5, // 50% damage = full update
            merge_distance: 32,
            enable_merging: true,
            stats: DamageStats::default(),
            last_update: Instant::now(),
            max_regions: 64,
        }
    }

    /// Create with custom threshold
    #[must_use]
    pub fn with_threshold(threshold: f32) -> Self {
        Self {
            full_damage_threshold: threshold.clamp(0.0, 1.0),
            ..Self::new()
        }
    }

    /// Create with custom settings
    #[must_use]
    pub fn with_settings(threshold: f32, merge_distance: u32, max_regions: usize) -> Self {
        Self {
            full_damage_threshold: threshold.clamp(0.0, 1.0),
            merge_distance,
            max_regions,
            ..Self::new()
        }
    }

    /// Add a damaged region
    pub fn add_region(&mut self, region: DamageRegion) {
        if self.regions.len() >= self.max_regions {
            // Too many regions - will trigger full update
            return;
        }

        if self.enable_merging {
            self.add_with_merge(region);
        } else {
            self.regions.push(region);
        }

        self.stats.total_regions += 1;
        self.last_update = Instant::now();
    }

    /// Add region with optional merging of overlapping regions
    fn add_with_merge(&mut self, region: DamageRegion) {
        // Check for overlapping regions
        let mut merged = region;
        let mut merged_any = true;

        while merged_any {
            merged_any = false;

            let mut i = 0;
            while i < self.regions.len() {
                if self.should_merge(&merged, &self.regions[i]) {
                    merged = merged.merge(&self.regions[i]);
                    self.regions.remove(i);
                    merged_any = true;
                } else {
                    i += 1;
                }
            }
        }

        self.regions.push(merged);
    }

    /// Check if two regions should be merged
    fn should_merge(&self, a: &DamageRegion, b: &DamageRegion) -> bool {
        // Merge if overlapping
        if a.overlaps(b) {
            return true;
        }

        // Merge if close enough
        let dist_x = if a.x + a.width < b.x {
            b.x.saturating_sub(a.x + a.width)
        } else {
            a.x.saturating_sub(b.x + b.width)
        };

        let dist_y = if a.y + a.height < b.y {
            b.y.saturating_sub(a.y + a.height)
        } else {
            a.y.saturating_sub(b.y + b.height)
        };

        dist_x <= self.merge_distance && dist_y <= self.merge_distance
    }

    /// Add multiple regions
    pub fn add_regions(&mut self, regions: impl IntoIterator<Item = DamageRegion>) {
        for region in regions {
            self.add_region(region);
        }
    }

    /// Mark entire frame as damaged
    pub fn mark_full_damage(&mut self, width: u32, height: u32) {
        self.regions.clear();
        self.regions.push(DamageRegion::new(0, 0, width, height));
        self.stats.full_damage_frames += 1;
    }

    /// Get current damaged regions
    #[must_use]
    pub fn damaged_regions(&self) -> &[DamageRegion] {
        &self.regions
    }

    /// Get number of damaged regions
    #[must_use]
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Check if there is any damage
    #[must_use]
    pub fn has_damage(&self) -> bool {
        !self.regions.is_empty()
    }

    /// Calculate total damaged area
    #[must_use]
    pub fn total_damaged_area(&self) -> u64 {
        self.regions.iter().map(DamageRegion::area).sum()
    }

    /// Calculate damage ratio (damaged area / total area)
    #[must_use]
    pub fn damage_ratio(&self, frame_size: (u32, u32)) -> f64 {
        let total_area = u64::from(frame_size.0) * u64::from(frame_size.1);
        if total_area == 0 {
            return 0.0;
        }

        let damaged = self.total_damaged_area();
        damaged as f64 / total_area as f64
    }

    /// Check if full frame update is more efficient
    ///
    /// Returns true if:
    /// - Too many regions (overhead of encoding each)
    /// - Damaged area exceeds threshold
    /// - No damage info available
    #[must_use]
    pub fn should_full_update(&self, frame_size: (u32, u32)) -> bool {
        // No regions = no damage info, assume full update
        if self.regions.is_empty() {
            return true;
        }

        // Too many regions
        if self.regions.len() >= self.max_regions {
            return true;
        }

        // Check damage ratio
        let ratio = self.damage_ratio(frame_size);
        ratio >= f64::from(self.full_damage_threshold)
    }

    /// Get bounding box of all damaged regions
    #[must_use]
    pub fn bounding_box(&self) -> Option<DamageRegion> {
        if self.regions.is_empty() {
            return None;
        }

        let mut result = self.regions[0];
        for region in &self.regions[1..] {
            result = result.merge(region);
        }

        Some(result)
    }

    /// Clear damage for next frame
    pub fn clear(&mut self) {
        self.regions.clear();
        self.stats.frames_processed += 1;
    }

    /// Get statistics
    #[must_use]
    pub fn stats(&self) -> &DamageStats {
        &self.stats
    }

    /// Set full damage threshold
    pub fn set_threshold(&mut self, threshold: f32) {
        self.full_damage_threshold = threshold.clamp(0.0, 1.0);
    }

    /// Enable or disable region merging
    pub fn set_merging(&mut self, enable: bool) {
        self.enable_merging = enable;
    }
}

impl Default for DamageTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_damage_region_basic() {
        let region = DamageRegion::new(10, 20, 100, 50);

        assert_eq!(region.area(), 5000);
        assert!(region.contains(50, 40));
        assert!(!region.contains(0, 0));
    }

    #[test]
    fn test_region_overlap() {
        let a = DamageRegion::new(0, 0, 100, 100);
        let b = DamageRegion::new(50, 50, 100, 100);
        let c = DamageRegion::new(200, 200, 50, 50);

        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn test_region_merge() {
        let a = DamageRegion::new(0, 0, 100, 100);
        let b = DamageRegion::new(50, 50, 100, 100);

        let merged = a.merge(&b);
        assert_eq!(merged.x, 0);
        assert_eq!(merged.y, 0);
        assert_eq!(merged.width, 150);
        assert_eq!(merged.height, 150);
    }

    #[test]
    fn test_region_clip() {
        let region = DamageRegion::new(900, 500, 200, 200);
        let clipped = region.clip(1000, 600);

        assert!(clipped.is_some());
        let c = clipped.expect("should clip");
        assert_eq!(c.width, 100);
        assert_eq!(c.height, 100);
    }

    #[test]
    fn test_damage_tracker_basic() {
        let mut tracker = DamageTracker::new();

        assert!(!tracker.has_damage());

        tracker.add_region(DamageRegion::new(0, 0, 100, 100));
        assert!(tracker.has_damage());
        assert_eq!(tracker.region_count(), 1);

        tracker.clear();
        assert!(!tracker.has_damage());
    }

    #[test]
    fn test_damage_tracker_merge() {
        let mut tracker = DamageTracker::new();

        // Add overlapping regions
        tracker.add_region(DamageRegion::new(0, 0, 100, 100));
        tracker.add_region(DamageRegion::new(50, 50, 100, 100));

        // Should be merged into one
        assert_eq!(tracker.region_count(), 1);
    }

    #[test]
    fn test_should_full_update() {
        let mut tracker = DamageTracker::with_threshold(0.5);
        let frame_size = (100, 100);

        // Less than 50% damage
        tracker.add_region(DamageRegion::new(0, 0, 40, 40));
        assert!(!tracker.should_full_update(frame_size));

        tracker.clear();

        // More than 50% damage
        tracker.add_region(DamageRegion::new(0, 0, 80, 80));
        assert!(tracker.should_full_update(frame_size));
    }

    #[test]
    fn test_bounding_box() {
        let mut tracker = DamageTracker::new();
        tracker.set_merging(false); // Disable to have separate regions

        tracker.add_region(DamageRegion::new(10, 10, 50, 50));
        tracker.add_region(DamageRegion::new(200, 200, 30, 30));

        let bbox = tracker.bounding_box();
        assert!(bbox.is_some());

        let b = bbox.expect("should have bbox");
        assert_eq!(b.x, 10);
        assert_eq!(b.y, 10);
        assert_eq!(b.width, 220);
        assert_eq!(b.height, 220);
    }
}
