//! PipeWire Configuration
//!
//! Provides configuration options for PipeWire screen capture with a builder pattern
//! for ergonomic construction.
//!
//! # Examples
//!
//! ```rust
//! use lamco_pipewire::{PipeWireConfig, PixelFormat};
//!
//! // Using builder pattern
//! let config = PipeWireConfig::builder()
//!     .buffer_count(4)
//!     .preferred_format(PixelFormat::BGRA)
//!     .use_dmabuf(true)
//!     .max_streams(4)
//!     .build();
//!
//! // Using struct literal with defaults
//! let config = PipeWireConfig {
//!     buffer_count: 4,
//!     ..Default::default()
//! };
//! ```

use crate::format::PixelFormat;

/// Configuration for PipeWire screen capture
///
/// This struct contains all configuration options for the PipeWire integration.
/// Use [`PipeWireConfig::builder()`] for ergonomic construction or struct literal
/// syntax with [`Default::default()`].
#[derive(Debug, Clone)]
pub struct PipeWireConfig {
    /// Number of buffers to allocate per stream (default: 3)
    ///
    /// Higher values reduce frame drops at the cost of memory and latency.
    /// Recommended: 2-3 for low latency, 4-5 for high refresh rates.
    pub buffer_count: u32,

    /// Preferred pixel format for capture (default: BGRA)
    ///
    /// The actual format may differ based on compositor capabilities.
    /// Format negotiation will fall back to available formats.
    pub preferred_format: Option<PixelFormat>,

    /// Whether to use DMA-BUF for zero-copy transfer (default: true)
    ///
    /// DMA-BUF provides hardware-accelerated, zero-copy frame transfer when
    /// supported by the GPU and compositor. Falls back to memory copy if unavailable.
    pub use_dmabuf: bool,

    /// Connect the capture stream as the graph's driver, so frames keep coming
    /// at the framerate on a static desktop. A compositor whose own stream is
    /// the only one that should drive (COSMIC) needs this off.
    pub drive_graph: bool,

    /// Offer the buffer size and stride as ranges instead of the fixed 32 MB / 0.
    /// COSMIC's producer fixes its own and needs this; GNOME's mutter crashed
    /// (SEGV in its stream param handler) on 2026-10-01 after it was made
    /// the default, so it stays off everywhere else.
    pub ranged_buffers: bool,

    /// When true AND the buffer is DMA-BUF, pass the FD directly to the
    /// consumer instead of mmap+copying pixels to CPU memory.
    ///
    /// Requires the downstream encoder to support DMA-BUF import (e.g.,
    /// Vulkan Video via VK_EXT_external_memory_dma_buf). Falls back to
    /// mmap+copy when the encoder doesn't support it.
    ///
    /// Default: false (conservative — existing mmap path is well-tested)
    pub dmabuf_passthrough: bool,

    /// Maximum number of concurrent streams (default: 8)
    ///
    /// Limits resource usage in multi-monitor scenarios.
    pub max_streams: usize,

    /// Frame buffer size for the receiver channel (default: 30)
    ///
    /// Number of frames that can be buffered before dropping.
    /// Higher values handle burst traffic but increase memory usage.
    pub frame_buffer_size: usize,

    /// Enable hardware cursor extraction (default: false)
    ///
    /// When enabled, cursor position and bitmap are extracted separately
    /// from the video stream. Requires the `cursor` feature.
    pub enable_cursor: bool,

    /// Enable region damage tracking (default: false)
    ///
    /// When enabled, tracks which regions of the frame changed between
    /// captures for efficient encoding. Requires the `damage` feature.
    pub enable_damage_tracking: bool,

    /// Adaptive bitrate configuration (default: None)
    ///
    /// When set, enables adaptive bitrate control for streaming scenarios.
    /// Requires the `adaptive` feature.
    pub adaptive_bitrate: Option<AdaptiveBitrateConfig>,

    /// Stream name prefix (default: "lamco-pw")
    ///
    /// Prefix used for PipeWire stream names. The stream ID is appended.
    pub stream_name_prefix: String,

    /// Connection timeout in milliseconds (default: 5000)
    ///
    /// Maximum time to wait for PipeWire connection to establish.
    pub connection_timeout_ms: u64,

    /// Enable automatic reconnection on disconnect (default: true)
    pub auto_reconnect: bool,

    /// Maximum reconnection attempts (default: 3)
    pub max_reconnect_attempts: u32,

    /// Offer exactly the stream's `size` (a single fixed `VideoSize`) instead
    /// of a 1x1..8192x8192 range whose default is that size (default: false).
    ///
    /// Mutter sizes a virtual monitor from the consumer's negotiated format,
    /// so a virtual-monitor capture needs the requested size to be the only
    /// size on offer.
    pub fixed_size: bool,

    /// Frame rate to offer, in frames per second (default: 30).
    pub framerate: u32,
}

impl Default for PipeWireConfig {
    fn default() -> Self {
        Self {
            buffer_count: 3,
            preferred_format: Some(PixelFormat::BGRA),
            use_dmabuf: true,
            drive_graph: true,
            ranged_buffers: false,
            dmabuf_passthrough: false,
            max_streams: 8,
            frame_buffer_size: 30,
            enable_cursor: false,
            enable_damage_tracking: false,
            adaptive_bitrate: None,
            stream_name_prefix: "lamco-pw".to_string(),
            connection_timeout_ms: 5000,
            auto_reconnect: true,
            max_reconnect_attempts: 3,
            fixed_size: false,
            framerate: 30,
        }
    }
}

impl PipeWireConfig {
    /// Create a new configuration builder
    ///
    /// # Examples
    ///
    /// ```rust
    /// use lamco_pipewire::PipeWireConfig;
    ///
    /// let config = PipeWireConfig::builder()
    ///     .buffer_count(4)
    ///     .use_dmabuf(true)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder() -> PipeWireConfigBuilder {
        PipeWireConfigBuilder::default()
    }

    /// Validate configuration and return any issues
    ///
    /// Returns `Ok(())` if configuration is valid, or a list of issues.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut issues = Vec::new();

        if self.buffer_count == 0 {
            issues.push("buffer_count must be at least 1".to_string());
        }

        if self.buffer_count > 16 {
            issues.push("buffer_count should not exceed 16".to_string());
        }

        if self.max_streams == 0 {
            issues.push("max_streams must be at least 1".to_string());
        }

        if self.frame_buffer_size == 0 {
            issues.push("frame_buffer_size must be at least 1".to_string());
        }

        if self.connection_timeout_ms < 100 {
            issues.push("connection_timeout_ms should be at least 100ms".to_string());
        }

        if self.stream_name_prefix.is_empty() {
            issues.push("stream_name_prefix cannot be empty".to_string());
        }

        if issues.is_empty() { Ok(()) } else { Err(issues) }
    }
}

/// Builder for [`PipeWireConfig`]
///
/// Provides a fluent interface for constructing configuration.
#[derive(Debug, Clone, Default)]
pub struct PipeWireConfigBuilder {
    buffer_count: Option<u32>,
    preferred_format: Option<PixelFormat>,
    use_dmabuf: Option<bool>,
    drive_graph: Option<bool>,
    ranged_buffers: Option<bool>,
    max_streams: Option<usize>,
    frame_buffer_size: Option<usize>,
    enable_cursor: Option<bool>,
    enable_damage_tracking: Option<bool>,
    adaptive_bitrate: Option<AdaptiveBitrateConfig>,
    stream_name_prefix: Option<String>,
    connection_timeout_ms: Option<u64>,
    auto_reconnect: Option<bool>,
    max_reconnect_attempts: Option<u32>,
    fixed_size: Option<bool>,
    framerate: Option<u32>,
}

impl PipeWireConfigBuilder {
    /// Offer exactly the stream's size (see [`PipeWireConfig::fixed_size`])
    #[must_use]
    pub fn fixed_size(mut self, fixed: bool) -> Self {
        self.fixed_size = Some(fixed);
        self
    }

    /// Set the offered frame rate in fps
    #[must_use]
    pub fn framerate(mut self, fps: u32) -> Self {
        self.framerate = Some(fps);
        self
    }

    /// Set number of buffers per stream
    #[must_use]
    pub fn buffer_count(mut self, count: u32) -> Self {
        self.buffer_count = Some(count);
        self
    }

    /// Set preferred pixel format
    #[must_use]
    pub fn preferred_format(mut self, format: PixelFormat) -> Self {
        self.preferred_format = Some(format);
        self
    }

    /// Set whether the stream drives the graph clock (default true)
    #[must_use]
    pub fn drive_graph(mut self, enable: bool) -> Self {
        self.drive_graph = Some(enable);
        self
    }

    /// Offer buffer size/stride as ranges (default false; COSMIC needs it)
    #[must_use]
    pub fn ranged_buffers(mut self, enable: bool) -> Self {
        self.ranged_buffers = Some(enable);
        self
    }

    /// Set whether to use DMA-BUF
    #[must_use]
    pub fn use_dmabuf(mut self, enable: bool) -> Self {
        self.use_dmabuf = Some(enable);
        self
    }

    /// Set maximum concurrent streams
    #[must_use]
    pub fn max_streams(mut self, max: usize) -> Self {
        self.max_streams = Some(max);
        self
    }

    /// Set frame buffer size
    #[must_use]
    pub fn frame_buffer_size(mut self, size: usize) -> Self {
        self.frame_buffer_size = Some(size);
        self
    }

    /// Enable hardware cursor extraction
    #[must_use]
    pub fn enable_cursor(mut self, enable: bool) -> Self {
        self.enable_cursor = Some(enable);
        self
    }

    /// Enable region damage tracking
    #[must_use]
    pub fn enable_damage_tracking(mut self, enable: bool) -> Self {
        self.enable_damage_tracking = Some(enable);
        self
    }

    /// Set adaptive bitrate configuration
    #[must_use]
    pub fn adaptive_bitrate(mut self, config: AdaptiveBitrateConfig) -> Self {
        self.adaptive_bitrate = Some(config);
        self
    }

    /// Set stream name prefix
    #[must_use]
    pub fn stream_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.stream_name_prefix = Some(prefix.into());
        self
    }

    /// Set connection timeout in milliseconds
    #[must_use]
    pub fn connection_timeout_ms(mut self, timeout: u64) -> Self {
        self.connection_timeout_ms = Some(timeout);
        self
    }

    /// Set whether to auto-reconnect on disconnect
    #[must_use]
    pub fn auto_reconnect(mut self, enable: bool) -> Self {
        self.auto_reconnect = Some(enable);
        self
    }

    /// Set maximum reconnection attempts
    #[must_use]
    pub fn max_reconnect_attempts(mut self, attempts: u32) -> Self {
        self.max_reconnect_attempts = Some(attempts);
        self
    }

    /// Build the configuration
    ///
    /// Returns a [`PipeWireConfig`] with builder values overriding defaults.
    #[must_use]
    pub fn build(self) -> PipeWireConfig {
        let defaults = PipeWireConfig::default();

        PipeWireConfig {
            buffer_count: self.buffer_count.unwrap_or(defaults.buffer_count),
            preferred_format: self.preferred_format.or(defaults.preferred_format),
            use_dmabuf: self.use_dmabuf.unwrap_or(defaults.use_dmabuf),
            drive_graph: self.drive_graph.unwrap_or(defaults.drive_graph),
            ranged_buffers: self.ranged_buffers.unwrap_or(defaults.ranged_buffers),
            dmabuf_passthrough: defaults.dmabuf_passthrough,
            max_streams: self.max_streams.unwrap_or(defaults.max_streams),
            frame_buffer_size: self.frame_buffer_size.unwrap_or(defaults.frame_buffer_size),
            enable_cursor: self.enable_cursor.unwrap_or(defaults.enable_cursor),
            enable_damage_tracking: self.enable_damage_tracking.unwrap_or(defaults.enable_damage_tracking),
            adaptive_bitrate: self.adaptive_bitrate.or(defaults.adaptive_bitrate),
            stream_name_prefix: self.stream_name_prefix.unwrap_or(defaults.stream_name_prefix),
            connection_timeout_ms: self.connection_timeout_ms.unwrap_or(defaults.connection_timeout_ms),
            auto_reconnect: self.auto_reconnect.unwrap_or(defaults.auto_reconnect),
            max_reconnect_attempts: self.max_reconnect_attempts.unwrap_or(defaults.max_reconnect_attempts),
            fixed_size: self.fixed_size.unwrap_or(defaults.fixed_size),
            framerate: self.framerate.unwrap_or(defaults.framerate),
        }
    }
}

/// Configuration for adaptive bitrate control
///
/// Used for streaming scenarios where bandwidth may vary.
#[derive(Debug, Clone)]
pub struct AdaptiveBitrateConfig {
    /// Minimum bitrate in kbps (default: 500)
    pub min_bitrate_kbps: u32,

    /// Maximum bitrate in kbps (default: 50000)
    pub max_bitrate_kbps: u32,

    /// Target frames per second (default: 30)
    pub target_fps: u32,

    /// Quality preset (default: Balanced)
    pub quality_preset: QualityPreset,

    /// Window size for bitrate calculations in frames (default: 30)
    pub calculation_window: usize,
}

impl Default for AdaptiveBitrateConfig {
    fn default() -> Self {
        Self {
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 50000,
            target_fps: 30,
            quality_preset: QualityPreset::Balanced,
            calculation_window: 30,
        }
    }
}

impl AdaptiveBitrateConfig {
    /// Create a new builder
    #[must_use]
    pub fn builder() -> AdaptiveBitrateConfigBuilder {
        AdaptiveBitrateConfigBuilder::default()
    }

    /// Create configuration optimized for low latency
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            min_bitrate_kbps: 1000,
            max_bitrate_kbps: 20000,
            target_fps: 60,
            quality_preset: QualityPreset::LowLatency,
            calculation_window: 15,
        }
    }

    /// Create configuration optimized for high quality
    #[must_use]
    pub fn high_quality() -> Self {
        Self {
            min_bitrate_kbps: 5000,
            max_bitrate_kbps: 100000,
            target_fps: 30,
            quality_preset: QualityPreset::HighQuality,
            calculation_window: 60,
        }
    }
}

/// Builder for [`AdaptiveBitrateConfig`]
#[derive(Debug, Clone, Default)]
pub struct AdaptiveBitrateConfigBuilder {
    min_bitrate_kbps: Option<u32>,
    max_bitrate_kbps: Option<u32>,
    target_fps: Option<u32>,
    quality_preset: Option<QualityPreset>,
    calculation_window: Option<usize>,
}

impl AdaptiveBitrateConfigBuilder {
    /// Set minimum bitrate in kbps
    #[must_use]
    pub fn min_bitrate_kbps(mut self, kbps: u32) -> Self {
        self.min_bitrate_kbps = Some(kbps);
        self
    }

    /// Set maximum bitrate in kbps
    #[must_use]
    pub fn max_bitrate_kbps(mut self, kbps: u32) -> Self {
        self.max_bitrate_kbps = Some(kbps);
        self
    }

    /// Set target FPS
    #[must_use]
    pub fn target_fps(mut self, fps: u32) -> Self {
        self.target_fps = Some(fps);
        self
    }

    /// Set quality preset
    #[must_use]
    pub fn quality_preset(mut self, preset: QualityPreset) -> Self {
        self.quality_preset = Some(preset);
        self
    }

    /// Set calculation window size
    #[must_use]
    pub fn calculation_window(mut self, frames: usize) -> Self {
        self.calculation_window = Some(frames);
        self
    }

    /// Build the configuration
    #[must_use]
    pub fn build(self) -> AdaptiveBitrateConfig {
        let defaults = AdaptiveBitrateConfig::default();

        AdaptiveBitrateConfig {
            min_bitrate_kbps: self.min_bitrate_kbps.unwrap_or(defaults.min_bitrate_kbps),
            max_bitrate_kbps: self.max_bitrate_kbps.unwrap_or(defaults.max_bitrate_kbps),
            target_fps: self.target_fps.unwrap_or(defaults.target_fps),
            quality_preset: self.quality_preset.unwrap_or(defaults.quality_preset),
            calculation_window: self.calculation_window.unwrap_or(defaults.calculation_window),
        }
    }
}

/// Quality preset for adaptive bitrate control
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityPreset {
    /// Optimize for lowest latency (faster encoding, lower quality)
    LowLatency,

    /// Balance between latency and quality (default)
    #[default]
    Balanced,

    /// Optimize for highest quality (slower encoding, higher quality)
    HighQuality,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PipeWireConfig::default();

        assert_eq!(config.buffer_count, 3);
        assert!(config.use_dmabuf);
        assert_eq!(config.max_streams, 8);
        assert_eq!(config.stream_name_prefix, "lamco-pw");
    }

    #[test]
    fn test_builder_pattern() {
        let config = PipeWireConfig::builder()
            .buffer_count(5)
            .use_dmabuf(false)
            .max_streams(4)
            .stream_name_prefix("test-capture")
            .build();

        assert_eq!(config.buffer_count, 5);
        assert!(!config.use_dmabuf);
        assert_eq!(config.max_streams, 4);
        assert_eq!(config.stream_name_prefix, "test-capture");
    }

    #[test]
    fn test_config_validation() {
        let valid_config = PipeWireConfig::default();
        assert!(valid_config.validate().is_ok());

        let invalid_config = PipeWireConfig {
            buffer_count: 0,
            ..Default::default()
        };
        assert!(invalid_config.validate().is_err());
    }

    #[test]
    fn test_adaptive_bitrate_presets() {
        let low_latency = AdaptiveBitrateConfig::low_latency();
        assert_eq!(low_latency.quality_preset, QualityPreset::LowLatency);
        assert_eq!(low_latency.target_fps, 60);

        let high_quality = AdaptiveBitrateConfig::high_quality();
        assert_eq!(high_quality.quality_preset, QualityPreset::HighQuality);
        assert!(high_quality.max_bitrate_kbps > low_latency.max_bitrate_kbps);
    }

    #[test]
    fn test_adaptive_bitrate_builder() {
        let config = AdaptiveBitrateConfig::builder()
            .min_bitrate_kbps(1000)
            .max_bitrate_kbps(30000)
            .target_fps(60)
            .quality_preset(QualityPreset::LowLatency)
            .build();

        assert_eq!(config.min_bitrate_kbps, 1000);
        assert_eq!(config.max_bitrate_kbps, 30000);
        assert_eq!(config.target_fps, 60);
    }
}
