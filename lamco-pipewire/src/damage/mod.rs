//! Damage Detection and Region Tracking
//!
//! Two complementary damage systems:
//!
//! - **Tracker** ([`DamageTracker`]): Aggregates damage regions from PipeWire
//!   metadata and decides encoding strategy (full vs partial update).
//!
//! - **Detector** ([`DamageDetector`]): SIMD-accelerated frame differencing
//!   that compares consecutive BGRA frames tile-by-tile to find changes.
//!   Useful when PipeWire doesn't provide damage metadata.
//!
//! # Usage
//!
//! ```rust
//! use lamco_pipewire::damage::{DamageTracker, DamageRegion};
//!
//! // Region tracking (from PipeWire metadata)
//! let mut tracker = DamageTracker::new();
//! tracker.add_region(DamageRegion::new(0, 0, 100, 100));
//!
//! if tracker.should_full_update((1920, 1080)) {
//!     // Encode full frame
//! }
//! tracker.clear();
//! ```
//!
//! ```rust,ignore
//! use lamco_pipewire::damage::{DamageConfig, DamageDetector};
//!
//! // Frame comparison (SIMD-accelerated)
//! let mut detector = DamageDetector::with_defaults();
//! let regions = detector.detect(&frame_data, 1920, 1080);
//! ```

mod detector;
mod tracker;

// Tracker (region aggregation from PipeWire metadata)
// Detector (SIMD frame comparison)
pub use detector::{DamageConfig, DamageDetector, DamageDetectorStats, DetectedRegion};
pub use tracker::{DamageRegion, DamageStats, DamageTracker};
