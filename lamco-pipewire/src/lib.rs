#![cfg_attr(docsrs, feature(doc_cfg))]

//! # lamco-pipewire
//!
//! High-performance PipeWire integration for Wayland screen capture with
//! DMA-BUF support, adaptive bitrate control, and comprehensive error handling.
//!
//! This crate is part of the [lamco-wayland](https://github.com/lamco-admin/lamco-wayland)
//! workspace and is designed to work with [`lamco-portal`](https://crates.io/crates/lamco-portal)
//! for XDG Desktop Portal integration.
//!
//! # Features
//!
//! - **Zero-Copy DMA-BUF**: Hardware-accelerated frame transfer when available
//! - **Multi-Monitor**: Concurrent handling of multiple monitor streams
//! - **Format Negotiation**: Automatic format selection with fallbacks
//! - **YUV Conversion**: Built-in NV12, I420, YUY2 to BGRA conversion
//! - **Cursor Extraction**: Separate cursor tracking for remote desktop
//! - **Damage Tracking**: Region-based change detection for efficient encoding
//! - **Adaptive Bitrate**: Network-aware bitrate control for streaming
//! - **Error Recovery**: Automatic reconnection and stream recovery
//!
//! # Requirements
//!
//! This crate requires:
//! - **Linux** with a Wayland compositor
//! - **PipeWire** installed and running (typically via your compositor)
//! - **PipeWire development libraries**: `libpipewire-0.3-dev` (Debian/Ubuntu) or `pipewire-devel` (Fedora)
//! - **Rust 1.77+** (for PipeWire bindings compatibility)
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use lamco_pipewire::{PipeWireManager, PipeWireConfig, StreamInfo, SourceType};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create manager with default configuration
//! let mut manager = PipeWireManager::with_default()?;
//!
//! // Connect using portal-provided file descriptor (from lamco-portal)
//! let fd = /* session.pipewire_fd() */;
//! manager.connect(fd).await?;
//!
//! // Create stream for a monitor
//! let stream_info = StreamInfo {
//!     node_id: 42,
//!     position: (0, 0),
//!     size: (1920, 1080),
//!     source_type: SourceType::Monitor,
//! };
//!
//! let handle = manager.create_stream(&stream_info).await?;
//!
//! // Receive frames
//! if let Some(mut rx) = manager.frame_receiver(handle.id).await {
//!     while let Some(frame) = rx.recv().await {
//!         println!("Frame: {}x{}", frame.width, frame.height);
//!     }
//! }
//!
//! manager.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Configuration
//!
//! Customize capture behavior using [`PipeWireConfig`]:
//!
//! ```rust
//! use lamco_pipewire::{PipeWireConfig, PixelFormat};
//!
//! let config = PipeWireConfig::builder()
//!     .buffer_count(4)                          // More buffers for high refresh
//!     .preferred_format(PixelFormat::BGRA)      // Preferred pixel format
//!     .use_dmabuf(true)                         // Enable zero-copy
//!     .max_streams(4)                           // Limit concurrent streams
//!     .enable_cursor(true)                      // Extract cursor separately
//!     .enable_damage_tracking(true)             // Track changed regions
//!     .build();
//! ```
//!
//! # Error Handling
//!
//! The crate uses typed errors via [`PipeWireError`]:
//!
//! ```rust,ignore
//! use lamco_pipewire::{PipeWireManager, PipeWireError, classify_error, ErrorType};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut manager = PipeWireManager::with_default()?;
//!
//! match manager.connect(fd).await {
//!     Ok(()) => println!("Connected!"),
//!     Err(PipeWireError::ConnectionFailed(msg)) => {
//!         eprintln!("Connection failed: {}", msg);
//!     }
//!     Err(PipeWireError::Timeout) => {
//!         eprintln!("Connection timed out - is PipeWire running?");
//!     }
//!     Err(e) => {
//!         // Use error classification for recovery decisions
//!         match classify_error(&e) {
//!             ErrorType::Connection => eprintln!("Retry connection"),
//!             ErrorType::Permission => eprintln!("Check portal permissions"),
//!             _ => eprintln!("Error: {}", e),
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Architecture
//!
//! PipeWire's Rust bindings use `Rc<>` and `NonNull<>` internally, making them
//! **not Send**. This crate solves this with a dedicated thread architecture:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │              Tokio Async Runtime                        │
//! │                                                         │
//! │  Your Application → PipeWireManager                     │
//! │                    (Send + Sync wrapper)                │
//! │                           │                             │
//! │                           │ Commands via mpsc           │
//! │                           ▼                             │
//! └───────────────────────────┼─────────────────────────────┘
//!                             │
//! ┌───────────────────────────▼─────────────────────────────┐
//! │         Dedicated PipeWire Thread                       │
//! │         (std::thread - owns all non-Send types)         │
//! │                                                         │
//! │  MainLoop (Rc) ─> Context (Rc) ─> Core (Rc)            │
//! │                                      │                  │
//! │                                      ▼                  │
//! │                              Streams (NonNull)          │
//! │                                      │                  │
//! │                                      ▼                  │
//! │                              Frame Callbacks            │
//! │                                      │                  │
//! │                                      │ Frames via mpsc  │
//! └──────────────────────────────────────┼──────────────────┘
//!                                        │
//!                                        ▼
//!                              Your application receives frames
//! ```
//!
//! # Platform Notes
//!
//! - **GNOME**: Works out of the box with `xdg-desktop-portal-gnome`
//! - **KDE Plasma**: Use `xdg-desktop-portal-kde`
//! - **wlroots** (Sway, Hyprland): Use `xdg-desktop-portal-wlr`
//! - **X11**: Not supported - Wayland only (use X11 screen capture APIs directly)
//!
//! # Security
//!
//! This crate handles sensitive resources:
//!
//! - **File Descriptors**: The portal FD provides access to screen content.
//!   Never expose it to untrusted code.
//! - **DMA-BUF**: Hardware buffers may contain screen content from other
//!   applications. Handle with appropriate security context.
//! - **Memory Mapping**: Buffer contents should be treated as sensitive data.
//!
//! All screen capture requires user consent via the XDG Desktop Portal
//! permission dialog.
//!
//! # Cargo Features
//!
//! ```toml
//! [dependencies]
//! lamco-pipewire = { version = "0.1", features = ["full"] }
//! ```
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `dmabuf` | Yes | DMA-BUF zero-copy support |
//! | `yuv` | No | YUV format conversion utilities |
//! | `cursor` | No | Hardware cursor extraction |
//! | `damage` | No | Region damage tracking |
//! | `adaptive` | No | Adaptive bitrate control |
//! | `full` | No | All features enabled |
//!
//! # Performance
//!
//! Typical performance on modern hardware:
//!
//! - **Frame latency**: < 2ms (with DMA-BUF)
//! - **Memory usage**: < 100MB per stream
//! - **CPU usage**: < 5% per stream (1080p @ 60Hz)
//! - **Refresh rates**: Tested up to 144Hz

// =============================================================================
// CORE MODULES
// =============================================================================

pub mod buffer;
pub mod config;
pub mod connection;
pub mod coordinator;
pub mod error;
pub mod ffi;
pub mod format;
pub mod frame;
pub mod manager;
pub mod meta;
mod pw_lifecycle;
pub mod pw_thread;
pub mod stream;
pub mod tags;
pub mod thread_comm;

// =============================================================================
// FEATURE MODULES
// =============================================================================

/// YUV format conversion utilities
///
/// Requires the `yuv` feature.
#[cfg(feature = "yuv")]
pub mod yuv;

/// Hardware cursor extraction
///
/// Requires the `cursor` feature.
#[cfg(feature = "cursor")]
pub mod cursor;

/// Region damage tracking
///
/// Requires the `damage` feature.
#[cfg(feature = "damage")]
pub mod damage;

/// Audio capture via PipeWire
///
/// Requires the `audio` feature.
#[cfg(feature = "audio")]
#[cfg_attr(docsrs, doc(cfg(feature = "audio")))]
pub mod audio;

/// Adaptive bitrate control
///
/// Requires the `adaptive` feature.
#[cfg(feature = "adaptive")]
pub mod bitrate;

// =============================================================================
// RE-EXPORTS - PRIMARY API
// =============================================================================

// Manager (primary entry point)
#[cfg(feature = "audio")]
pub use audio::{
    AudioCapture, AudioCaptureHandle, AudioFormat, AudioSamples, CaptureConfig, PlaybackConfig, VirtualMicrophone,
    VirtualMicrophoneHandle, spawn_audio_capture, spawn_virtual_microphone,
};
#[cfg(feature = "adaptive")]
pub use bitrate::{BitrateController, BitrateStats};
// Buffer management
pub use buffer::{BufferManager, BufferType, ManagedBuffer, SharedBufferManager};
// Configuration
pub use config::{
    AdaptiveBitrateConfig, AdaptiveBitrateConfigBuilder, PipeWireConfig, PipeWireConfigBuilder, QualityPreset,
};
// =============================================================================
// RE-EXPORTS - ADVANCED API
// =============================================================================

// Low-level connection (for advanced use cases)
pub use connection::{ConnectionState, PipeWireConnection, PipeWireEvent};
// Coordinator
pub use coordinator::{CoordinatorStats, DispatcherConfig, FrameDispatcher, MultiStreamCoordinator};
// Stream types
pub use coordinator::{MonitorEvent, MonitorInfo, MultiStreamConfig, SourceType, StreamInfo};
#[cfg(feature = "cursor")]
pub use cursor::{CursorExtractor, CursorInfo, CursorStats};
#[cfg(feature = "damage")]
pub use damage::{
    DamageConfig, DamageDetector, DamageDetectorStats, DamageRegion, DamageStats, DamageTracker, DetectedRegion,
};
// Errors
pub use error::{ErrorContext, ErrorType, PipeWireError, RecoveryAction, Result, RetryConfig, classify_error};
// FFI utilities
pub use ffi::{
    DamageRegion as FfiDamageRegion, DataType, calculate_buffer_size, calculate_stride, drm_fourcc,
    get_bytes_per_pixel, spa_video_format_to_drm_fourcc,
};
// Frame types
pub use format::{PixelFormat, convert_format};
pub use frame::{DmaBufDescriptor, DmaBufPlane, FrameBuffer, FrameCallback, FrameFlags, FrameStats, VideoFrame};
// =============================================================================
// CRATE-LEVEL ITEMS
// =============================================================================
use libspa::param::video::VideoFormat;
pub use manager::{ManagerState, ManagerStats, PipeWireManager, StreamHandle};
// Buffer metadata
pub use meta::{BufferMeta, CropRegion, CursorMeta, DamageRect, HeaderMeta};
// Thread management
pub use pw_thread::{PipeWireThreadCommand, PipeWireThreadManager};
pub use stream::{NegotiatedFormat, PwStreamState, StreamConfig, StreamMetrics, StreamStateEvent, StreamTime};
// =============================================================================
// FEATURE RE-EXPORTS
// =============================================================================
#[cfg(feature = "yuv")]
pub use yuv::{YuvConverter, i420_to_bgra, nv12_to_bgra, yuy2_to_bgra};

/// Crate version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialize PipeWire library
///
/// This should be called once at application startup.
/// It's safe to call multiple times, including concurrently with the
/// crate's own internal PipeWire users (video capture, audio capture,
/// [`connection::PipeWireConnection`]) — every call site shares one
/// process-wide reference count, so `deinit()` only tears PipeWire down once
/// every caller, internal or external, has released it.
///
/// # Examples
///
/// ```rust,ignore
/// fn main() {
///     lamco_pipewire::init();
///     // ... use PipeWire ...
///     lamco_pipewire::deinit();
/// }
/// ```
pub fn init() {
    pw_lifecycle::acquire();
}

/// Deinitialize PipeWire library
///
/// This should be called at application shutdown after all PipeWire
/// resources *this caller* created have been dropped. This is bookkeeping
/// only — it does not call the real `pipewire::deinit()`, which reliably
/// segfaults a PipeWire-internal worker thread even with a single
/// registered caller across the process's entire lifetime.
pub fn deinit() {
    pw_lifecycle::release();
}

/// Get supported video formats in order of preference
///
/// Returns formats ordered by preference for screen capture:
/// 1. BGRx/BGRA - Common for desktop compositors
/// 2. RGBx/RGBA - Alternative RGB formats
/// 3. RGB/BGR - 24-bit formats (less common)
/// 4. NV12/YUY2/I420 - YUV formats (compressed, require conversion)
#[must_use]
pub fn supported_formats() -> Vec<VideoFormat> {
    vec![
        VideoFormat::BGRx, // Preferred: no alpha channel overhead
        VideoFormat::BGRA, // Common format with alpha
        VideoFormat::RGBx, // Alternative without alpha
        VideoFormat::RGBA, // Alternative with alpha
        VideoFormat::RGB,  // 24-bit fallback
        VideoFormat::BGR,  // 24-bit fallback
        VideoFormat::NV12, // YUV 4:2:0 (compressed)
        VideoFormat::YUY2, // YUV 4:2:2 (compressed)
        VideoFormat::I420, // YUV 4:2:0 planar
    ]
}

/// Check if DMA-BUF is likely supported
///
/// This is a heuristic check based on DRM device availability.
/// The actual DMA-BUF support is determined during format negotiation.
///
/// # Returns
///
/// `true` if DRM devices are found, suggesting DMA-BUF may be available.
#[must_use]
pub fn is_dmabuf_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        let drm_paths = ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/renderD128"];
        drm_paths.iter().any(|path| Path::new(path).exists())
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Get recommended buffer count for a given refresh rate
///
/// Higher refresh rates benefit from more buffers to prevent frame drops.
///
/// # Arguments
///
/// * `refresh_rate` - Monitor refresh rate in Hz
///
/// # Returns
///
/// Recommended number of buffers (2-5)
#[must_use]
pub fn recommended_buffer_count(refresh_rate: u32) -> u32 {
    match refresh_rate {
        0..=30 => 2,   // Low refresh: 2 buffers sufficient
        31..=60 => 3,  // Standard: 3 buffers
        61..=120 => 4, // High refresh: 4 buffers
        _ => 5,        // Very high refresh: 5 buffers
    }
}

/// Calculate optimal frame buffer size for a channel
///
/// Returns the recommended channel buffer size to hold approximately
/// 1 second of frames, capped at 144 frames.
///
/// # Arguments
///
/// * `refresh_rate` - Monitor refresh rate in Hz
///
/// # Returns
///
/// Recommended channel buffer size (30-144)
#[must_use]
pub fn recommended_frame_buffer_size(refresh_rate: u32) -> usize {
    (refresh_rate as usize).clamp(30, 144)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_formats() {
        let formats = supported_formats();
        assert!(!formats.is_empty());
        assert_eq!(formats[0], VideoFormat::BGRx);
    }

    #[test]
    fn test_recommended_buffer_count() {
        assert_eq!(recommended_buffer_count(30), 2);
        assert_eq!(recommended_buffer_count(60), 3);
        assert_eq!(recommended_buffer_count(144), 5);
    }

    #[test]
    fn test_recommended_frame_buffer_size() {
        assert_eq!(recommended_frame_buffer_size(30), 30);
        assert_eq!(recommended_frame_buffer_size(60), 60);
        assert_eq!(recommended_frame_buffer_size(144), 144);
        assert_eq!(recommended_frame_buffer_size(200), 144); // Capped at 144
        assert_eq!(recommended_frame_buffer_size(10), 30); // Minimum 30
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_dmabuf_check() {
        // Just verify it doesn't crash
        let _ = is_dmabuf_supported();
    }

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }
}
