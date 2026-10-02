//! Adaptive Bitrate Control
//!
//! Provides bitrate control helpers for streaming scenarios where
//! network conditions may vary.
//!
//! # Overview
//!
//! When streaming screen content over a network, the encoder bitrate
//! needs to adapt to:
//! - Available bandwidth
//! - Frame complexity (more detail = more bits)
//! - Latency requirements
//! - Frame drops/congestion signals
//!
//! This module provides a controller that tracks frame statistics and
//! recommends bitrate adjustments.
//!
//! # Usage
//!
//! ```rust
//! use lamco_pipewire::bitrate::BitrateController;
//! use lamco_pipewire::config::{AdaptiveBitrateConfig, QualityPreset};
//!
//! let config = AdaptiveBitrateConfig::builder()
//!     .min_bitrate_kbps(500)
//!     .max_bitrate_kbps(10000)
//!     .target_fps(60)
//!     .quality_preset(QualityPreset::LowLatency)
//!     .build();
//!
//! let mut controller = BitrateController::new(config);
//!
//! // After encoding each frame, record timing
//! controller.record_frame(5000, 50000); // 5ms encode, 50KB frame
//!
//! // Get recommendations
//! let bitrate = controller.recommended_bitrate();
//! let quality = controller.recommended_quality();
//!
//! if controller.should_skip_frame() {
//!     // Network congested, skip this frame
//! }
//! ```

use std::collections::VecDeque;
use std::time::Instant;

use crate::config::{AdaptiveBitrateConfig, QualityPreset};

/// Frame timing record for bitrate calculations
#[derive(Debug, Clone)]
struct FrameRecord {
    /// Time to encode the frame (microseconds)
    encode_time_us: u64,

    /// Encoded frame size (bytes)
    frame_size: usize,

    /// Timestamp when frame was recorded (retained for future analysis)
    #[allow(dead_code)]
    timestamp: Instant,
}

/// Bitrate controller for adaptive streaming
pub struct BitrateController {
    /// Configuration
    config: AdaptiveBitrateConfig,

    /// Current bitrate in kbps
    current_bitrate: u32,

    /// Frame history for calculations
    frame_history: VecDeque<FrameRecord>,

    /// Congestion indicator (0.0 = clear, 1.0 = severe)
    congestion_level: f64,

    /// Skip counter (for frame skipping)
    skip_counter: u32,

    /// Statistics
    stats: BitrateStats,

    /// Last adjustment time
    last_adjustment: Instant,

    /// Minimum time between adjustments (ms)
    adjustment_interval_ms: u64,
}

impl BitrateController {
    /// Create a new bitrate controller
    #[must_use]
    pub fn new(config: AdaptiveBitrateConfig) -> Self {
        let initial_bitrate = (config.min_bitrate_kbps + config.max_bitrate_kbps) / 2;

        Self {
            config,
            current_bitrate: initial_bitrate,
            frame_history: VecDeque::with_capacity(120),
            congestion_level: 0.0,
            skip_counter: 0,
            stats: BitrateStats::default(),
            last_adjustment: Instant::now(),
            adjustment_interval_ms: 100, // Adjust at most every 100ms
        }
    }

    /// Record frame encoding statistics
    ///
    /// Call this after encoding each frame to update the controller's
    /// model of current conditions.
    ///
    /// # Arguments
    ///
    /// * `encode_time_us` - Time spent encoding in microseconds
    /// * `frame_size` - Encoded frame size in bytes
    pub fn record_frame(&mut self, encode_time_us: u64, frame_size: usize) {
        let record = FrameRecord {
            encode_time_us,
            frame_size,
            timestamp: Instant::now(),
        };

        self.frame_history.push_back(record);

        // Keep only recent history
        while self.frame_history.len() > self.config.calculation_window {
            self.frame_history.pop_front();
        }

        self.stats.frames_recorded += 1;
        self.stats.total_bytes += frame_size as u64;

        // Update bitrate if enough time has passed
        if self.last_adjustment.elapsed().as_millis() >= u128::from(self.adjustment_interval_ms) {
            self.adjust_bitrate();
        }
    }

    /// Record a dropped/skipped frame
    pub fn record_dropped_frame(&mut self) {
        self.stats.frames_dropped += 1;
        self.congestion_level = (self.congestion_level + 0.2).min(1.0);
    }

    /// Record network feedback (e.g., from RTCP)
    ///
    /// # Arguments
    ///
    /// * `packet_loss_ratio` - Fraction of packets lost (0.0-1.0)
    /// * `rtt_ms` - Round-trip time in milliseconds
    pub fn record_network_feedback(&mut self, packet_loss_ratio: f64, rtt_ms: u32) {
        // Increase congestion if packet loss is high
        if packet_loss_ratio > 0.05 {
            self.congestion_level = (self.congestion_level + packet_loss_ratio).min(1.0);
        }

        // High RTT also indicates congestion
        let target_rtt = match self.config.quality_preset {
            QualityPreset::LowLatency => 50,
            QualityPreset::Balanced => 150,
            QualityPreset::HighQuality => 300,
        };

        if rtt_ms > target_rtt {
            let rtt_factor = f64::from(rtt_ms - target_rtt) / f64::from(target_rtt);
            self.congestion_level = (self.congestion_level + rtt_factor * 0.1).min(1.0);
        }

        // Decay congestion over time when conditions improve
        if packet_loss_ratio < 0.01 && rtt_ms < target_rtt {
            self.congestion_level = (self.congestion_level - 0.05).max(0.0);
        }
    }

    /// Get recommended bitrate based on current conditions
    #[must_use]
    pub fn recommended_bitrate(&self) -> u32 {
        self.current_bitrate
    }

    /// Get recommended quality level (0-100)
    ///
    /// Higher values = higher quality, lower compression
    #[must_use]
    pub fn recommended_quality(&self) -> u8 {
        let base_quality = match self.config.quality_preset {
            QualityPreset::LowLatency => 30,
            QualityPreset::Balanced => 50,
            QualityPreset::HighQuality => 80,
        };

        // Adjust based on congestion
        let adjusted = base_quality as f64 * (1.0 - self.congestion_level * 0.5);
        adjusted.clamp(10.0, 100.0) as u8
    }

    /// Check if current frame should be skipped due to congestion
    ///
    /// Returns true if frame should be skipped to reduce load.
    #[must_use]
    pub fn should_skip_frame(&mut self) -> bool {
        if self.congestion_level < 0.5 {
            self.skip_counter = 0;
            return false;
        }

        // Skip more frames at higher congestion
        let skip_threshold = match self.config.quality_preset {
            QualityPreset::LowLatency => 2,  // Skip every 2nd frame at high congestion
            QualityPreset::Balanced => 3,    // Skip every 3rd frame
            QualityPreset::HighQuality => 4, // Skip every 4th frame
        };

        self.skip_counter += 1;
        if self.skip_counter >= skip_threshold {
            self.skip_counter = 0;
            self.stats.frames_skipped += 1;
            true
        } else {
            false
        }
    }

    /// Get current congestion level (0.0-1.0)
    #[must_use]
    pub fn congestion_level(&self) -> f64 {
        self.congestion_level
    }

    /// Get statistics
    #[must_use]
    pub fn stats(&self) -> &BitrateStats {
        &self.stats
    }

    /// Reset controller state
    pub fn reset(&mut self) {
        self.current_bitrate = (self.config.min_bitrate_kbps + self.config.max_bitrate_kbps) / 2;
        self.frame_history.clear();
        self.congestion_level = 0.0;
        self.skip_counter = 0;
        self.stats = BitrateStats::default();
    }

    /// Internal bitrate adjustment logic
    fn adjust_bitrate(&mut self) {
        if self.frame_history.is_empty() {
            return;
        }

        // Calculate average encode time and frame size
        let (total_time, total_size) = self.frame_history.iter().fold((0u64, 0usize), |acc, r| {
            (acc.0 + r.encode_time_us, acc.1 + r.frame_size)
        });

        let count = self.frame_history.len() as u64;
        let avg_encode_us = total_time / count;
        let avg_frame_bytes = total_size / count as usize;

        // Target encode time based on FPS
        let target_frame_time_us = 1_000_000 / u64::from(self.config.target_fps);

        // Calculate how much of frame budget we're using for encoding
        let encode_budget_ratio = avg_encode_us as f64 / target_frame_time_us as f64;

        // Estimate current bitrate from frame sizes
        let estimated_bitrate_kbps = (avg_frame_bytes * 8 * self.config.target_fps as usize) / 1000;

        // Adjust bitrate
        let mut new_bitrate = self.current_bitrate;

        // If congested, reduce bitrate
        if self.congestion_level > 0.3 {
            let reduction = (self.congestion_level * 0.2) as f32;
            new_bitrate = (new_bitrate as f32 * (1.0 - reduction)) as u32;
            self.stats.bitrate_decreases += 1;
        }
        // If encode is fast and no congestion, can increase
        else if encode_budget_ratio < 0.5 && self.congestion_level < 0.1 {
            new_bitrate = (new_bitrate as f32 * 1.1) as u32;
            self.stats.bitrate_increases += 1;
        }

        // Clamp to configured range
        new_bitrate = new_bitrate.clamp(self.config.min_bitrate_kbps, self.config.max_bitrate_kbps);

        if new_bitrate != self.current_bitrate {
            self.current_bitrate = new_bitrate;
        }

        self.last_adjustment = Instant::now();

        // Update stats
        self.stats.avg_encode_time_us = avg_encode_us;
        self.stats.avg_frame_size = avg_frame_bytes;
        self.stats.estimated_bitrate_kbps = estimated_bitrate_kbps as u32;
    }
}

/// Bitrate control statistics
#[derive(Debug, Clone, Default)]
pub struct BitrateStats {
    /// Frames recorded
    pub frames_recorded: u64,

    /// Frames dropped due to encoder overload
    pub frames_dropped: u64,

    /// Frames skipped due to congestion
    pub frames_skipped: u64,

    /// Total bytes encoded
    pub total_bytes: u64,

    /// Number of bitrate increases
    pub bitrate_increases: u64,

    /// Number of bitrate decreases
    pub bitrate_decreases: u64,

    /// Average encode time (microseconds)
    pub avg_encode_time_us: u64,

    /// Average frame size (bytes)
    pub avg_frame_size: usize,

    /// Estimated actual bitrate (kbps)
    pub estimated_bitrate_kbps: u32,
}

impl BitrateStats {
    /// Calculate effective FPS
    #[must_use]
    pub fn effective_fps(&self, target_fps: u32) -> f64 {
        if self.frames_recorded == 0 {
            return 0.0;
        }

        let total = self.frames_recorded + self.frames_dropped + self.frames_skipped;
        (self.frames_recorded as f64 / total as f64) * f64::from(target_fps)
    }

    /// Calculate drop rate
    #[must_use]
    pub fn drop_rate(&self) -> f64 {
        let total = self.frames_recorded + self.frames_dropped + self.frames_skipped;
        if total == 0 {
            return 0.0;
        }

        (self.frames_dropped + self.frames_skipped) as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AdaptiveBitrateConfig {
        AdaptiveBitrateConfig {
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 10000,
            target_fps: 30,
            quality_preset: QualityPreset::Balanced,
            calculation_window: 10,
        }
    }

    #[test]
    fn test_controller_creation() {
        let controller = BitrateController::new(test_config());

        // Should start at midpoint
        assert_eq!(controller.recommended_bitrate(), 5250);
        assert_eq!(controller.congestion_level(), 0.0);
    }

    #[test]
    fn test_frame_recording() {
        let mut controller = BitrateController::new(test_config());

        // Record some frames
        for _ in 0..5 {
            controller.record_frame(5000, 50000); // 5ms, 50KB
        }

        assert_eq!(controller.stats().frames_recorded, 5);
        assert!(controller.stats().total_bytes > 0);
    }

    #[test]
    fn test_congestion_response() {
        let mut controller = BitrateController::new(test_config());

        // Simulate packet loss
        controller.record_network_feedback(0.1, 200);
        assert!(controller.congestion_level() > 0.0);

        // Should recommend lower quality
        let quality = controller.recommended_quality();
        assert!(quality < 50);
    }

    #[test]
    fn test_frame_skipping() {
        let mut controller = BitrateController::new(test_config());

        // Low congestion - no skipping
        assert!(!controller.should_skip_frame());

        // High congestion - should skip
        controller.congestion_level = 0.8;
        let mut skipped = false;
        for _ in 0..10 {
            if controller.should_skip_frame() {
                skipped = true;
                break;
            }
        }
        assert!(skipped);
    }

    #[test]
    fn test_quality_presets() {
        let mut config = test_config();

        config.quality_preset = QualityPreset::LowLatency;
        let low_latency = BitrateController::new(config.clone());

        config.quality_preset = QualityPreset::HighQuality;
        let high_quality = BitrateController::new(config);

        assert!(low_latency.recommended_quality() < high_quality.recommended_quality());
    }

    #[test]
    fn test_stats() {
        let mut controller = BitrateController::new(test_config());

        controller.record_frame(5000, 50000);
        controller.record_dropped_frame();

        let stats = controller.stats();
        assert_eq!(stats.frames_recorded, 1);
        assert_eq!(stats.frames_dropped, 1);
        assert!(stats.drop_rate() > 0.0);
    }

    #[test]
    fn test_reset() {
        let mut controller = BitrateController::new(test_config());

        controller.record_frame(5000, 50000);
        controller.congestion_level = 0.5;

        controller.reset();

        assert_eq!(controller.congestion_level(), 0.0);
        assert_eq!(controller.stats().frames_recorded, 0);
    }
}
