//! Video Frame Management
//!
//! Structures and utilities for handling video frames captured from PipeWire.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::ffi::DamageRegion;
use crate::format::PixelFormat;
use crate::meta::BufferMeta;

// =============================================================================
// Frame Buffer Types (zero-copy DMA-BUF support)
// =============================================================================

/// Frame pixel data: either CPU-resident memory or a GPU DMA-BUF descriptor.
///
/// When a hardware encoder supports DMA-BUF import, passing the `DmaBuf`
/// variant avoids copying pixels through the CPU entirely.
#[derive(Debug)]
pub enum FrameBuffer {
    /// CPU-resident pixel data (shared via Arc for cheap cloning)
    Memory(Arc<Vec<u8>>),

    /// GPU DMA-BUF descriptor — file descriptors referencing GPU memory.
    /// The FDs are dup'd from PipeWire in the process callback and owned
    /// by this struct. Dropping closes the FDs.
    DmaBuf(DmaBufDescriptor),
}

impl Clone for FrameBuffer {
    fn clone(&self) -> Self {
        match self {
            Self::Memory(data) => Self::Memory(Arc::clone(data)),
            // DMA-BUF descriptors can't be cheaply cloned (need dup).
            // Fall back to empty descriptor — callers should not clone DmaBuf frames.
            Self::DmaBuf(_) => {
                tracing::warn!("Cloning FrameBuffer::DmaBuf — this loses the FD!");
                Self::Memory(Arc::new(Vec::new()))
            }
        }
    }
}

/// Describes a DMA-BUF GPU memory buffer with one or more planes.
///
/// Each plane has its own file descriptor, stride, and offset. Multi-planar
/// formats like NV12 use multiple planes; single-plane formats like BGRA
/// use one plane.
#[derive(Debug)]
pub struct DmaBufDescriptor {
    /// Buffer planes (up to 4 for multi-planar formats)
    pub planes: Vec<DmaBufPlane>,

    /// DRM fourcc format code (e.g., DRM_FORMAT_ARGB8888)
    pub drm_format: u32,

    /// DRM format modifier (DRM_FORMAT_MOD_LINEAR = 0, or vendor-specific)
    pub modifier: u64,

    /// Buffer width in pixels
    pub width: u32,

    /// Buffer height in pixels
    pub height: u32,
}

/// A single plane of a DMA-BUF buffer.
#[derive(Debug)]
pub struct DmaBufPlane {
    /// Owned file descriptor for this plane's GPU memory.
    /// Dropping this closes the FD.
    pub fd: OwnedFd,

    /// Byte offset into the buffer for this plane
    pub offset: u32,

    /// Row stride in bytes for this plane
    pub stride: u32,
}

/// Raw frame data from a direct capture channel (no PipeWire).
///
/// Carries pixel data with optional metadata. Fields that are `None`
/// will be filled with defaults by the adapter.
pub struct RawFrameData {
    /// Pixel data.
    pub data: Vec<u8>,
    /// Width in pixels (None = use stream default).
    pub width: Option<u32>,
    /// Height in pixels (None = use stream default).
    pub height: Option<u32>,
    /// Row stride in bytes (None = width * 4).
    pub stride: Option<u32>,
    /// Pixel format (None = BGRx).
    pub format: Option<PixelFormat>,
    /// The real per-output identifier this frame was captured from (the
    /// caller's stream/node ID convention, matching `VideoFrame::monitor_index`
    /// downstream). `None` when the source has no per-stream identity to give
    /// (single-stream direct channels historically had none) -- the adapter
    /// falls back to `0`, which is also correct for that single-stream case.
    pub monitor_index: Option<u32>,
}

/// Video frame captured from PipeWire
#[derive(Clone)]
pub struct VideoFrame {
    /// Unique frame identifier
    pub frame_id: u64,

    /// Presentation timestamp (nanoseconds)
    pub pts: u64,

    /// Decode timestamp (nanoseconds)
    pub dts: u64,

    /// Frame duration (nanoseconds)
    pub duration: u64,

    /// Frame width in pixels
    pub width: u32,

    /// Frame height in pixels
    pub height: u32,

    /// Row stride in bytes
    pub stride: u32,

    /// Pixel format
    pub format: PixelFormat,

    /// Monitor/stream index
    pub monitor_index: u32,

    /// Pixel data: CPU memory or GPU DMA-BUF descriptor
    pub buffer: FrameBuffer,

    /// Capture timestamp
    pub capture_time: SystemTime,

    /// Damage regions (optional optimization)
    pub damage_regions: Vec<DamageRegion>,

    /// SPA buffer metadata (transform, header, crop, damage, cursor)
    pub meta: BufferMeta,

    /// Frame flags
    pub flags: FrameFlags,
}

/// Frame flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFlags {
    bits: u32,
}

impl FrameFlags {
    pub const NONE: u32 = 0;
    pub const DMABUF: u32 = 1 << 0;
    pub const GPU_PROCESSED: u32 = 1 << 1;
    pub const KEYFRAME: u32 = 1 << 2;
    pub const CORRUPTED: u32 = 1 << 3;
    pub const INCOMPLETE: u32 = 1 << 4;

    pub fn new() -> Self {
        Self { bits: 0 }
    }

    pub fn from_bits(bits: u32) -> Self {
        Self { bits }
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }

    pub fn has_dmabuf(&self) -> bool {
        self.bits & Self::DMABUF != 0
    }

    pub fn set_dmabuf(&mut self) {
        self.bits |= Self::DMABUF;
    }

    pub fn has_gpu_processed(&self) -> bool {
        self.bits & Self::GPU_PROCESSED != 0
    }

    pub fn set_gpu_processed(&mut self) {
        self.bits |= Self::GPU_PROCESSED;
    }

    pub fn is_keyframe(&self) -> bool {
        self.bits & Self::KEYFRAME != 0
    }

    pub fn set_keyframe(&mut self) {
        self.bits |= Self::KEYFRAME;
    }

    pub fn is_corrupted(&self) -> bool {
        self.bits & Self::CORRUPTED != 0
    }

    pub fn set_corrupted(&mut self) {
        self.bits |= Self::CORRUPTED;
    }

    pub fn is_incomplete(&self) -> bool {
        self.bits & Self::INCOMPLETE != 0
    }
}

impl Default for FrameFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoFrame {
    /// Create a new video frame
    pub fn new(frame_id: u64, width: u32, height: u32, stride: u32, format: PixelFormat, monitor_index: u32) -> Self {
        Self {
            frame_id,
            pts: 0,
            dts: 0,
            duration: 0,
            width,
            height,
            stride,
            format,
            monitor_index,
            buffer: FrameBuffer::Memory(Arc::new(Vec::new())),
            capture_time: SystemTime::now(),
            damage_regions: Vec::new(),
            meta: BufferMeta::default(),
            flags: FrameFlags::new(),
        }
    }

    /// Create frame with data
    pub fn with_data(
        frame_id: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        monitor_index: u32,
        data: Vec<u8>,
    ) -> Self {
        Self {
            frame_id,
            pts: 0,
            dts: 0,
            duration: 0,
            width,
            height,
            stride,
            format,
            monitor_index,
            buffer: FrameBuffer::Memory(Arc::new(data)),
            capture_time: SystemTime::now(),
            damage_regions: Vec::new(),
            meta: BufferMeta::default(),
            flags: FrameFlags::new(),
        }
    }

    /// Get the buffer transform value (0 = None, 1 = 90 CCW, 2 = 180, etc.)
    pub fn transform(&self) -> u32 {
        self.meta.transform.unwrap_or(0)
    }

    /// Check if the buffer has a negative chunk stride (bottom-up GL buffer).
    /// When true, the pixel data is stored bottom-to-top and needs vertical flip.
    pub fn is_bottom_up(&self) -> bool {
        self.meta.chunk_stride < 0
    }

    /// Set timing information
    pub fn set_timing(&mut self, pts: u64, dts: u64, duration: u64) {
        self.pts = pts;
        self.dts = dts;
        self.duration = duration;
    }

    /// Add a damage region
    pub fn add_damage_region(&mut self, region: DamageRegion) {
        if region.is_valid() {
            self.damage_regions.push(region);
        }
    }

    /// Get total damage area
    pub fn total_damage_area(&self) -> u32 {
        self.damage_regions.iter().map(|r| r.width * r.height).sum()
    }

    /// Check if frame has significant damage
    pub fn has_significant_damage(&self, threshold: f32) -> bool {
        if self.damage_regions.is_empty() {
            return true; // No damage info = assume full frame
        }

        let total_pixels = self.width * self.height;
        let damage_pixels = self.total_damage_area();
        (damage_pixels as f32 / total_pixels as f32) >= threshold
    }

    /// Get age of frame
    pub fn age(&self) -> Duration {
        self.capture_time.elapsed().unwrap_or(Duration::ZERO)
    }

    /// Check if frame is fresh
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.age() <= max_age
    }

    /// Get CPU pixel data if available.
    /// Returns None for DMA-BUF frames (use `buffer` field directly).
    pub fn data(&self) -> Option<&Arc<Vec<u8>>> {
        match &self.buffer {
            FrameBuffer::Memory(data) => Some(data),
            FrameBuffer::DmaBuf(_) => None,
        }
    }

    /// Check if this frame carries a DMA-BUF descriptor
    pub fn is_dmabuf(&self) -> bool {
        matches!(self.buffer, FrameBuffer::DmaBuf(_))
    }

    /// Get data size (0 for DMA-BUF frames since data is on GPU)
    pub fn data_size(&self) -> usize {
        match &self.buffer {
            FrameBuffer::Memory(data) => data.len(),
            FrameBuffer::DmaBuf(desc) => {
                // Estimated size based on dimensions
                (desc.width * desc.height * 4) as usize
            }
        }
    }

    /// Check if frame data is valid
    pub fn is_valid(&self) -> bool {
        let has_data = match &self.buffer {
            FrameBuffer::Memory(data) => !data.is_empty(),
            FrameBuffer::DmaBuf(desc) => !desc.planes.is_empty(),
        };
        has_data && !self.flags.is_corrupted() && !self.flags.is_incomplete()
    }

    /// Clone frame data (makes a copy). Returns empty vec for DMA-BUF frames.
    pub fn clone_data(&self) -> Vec<u8> {
        match &self.buffer {
            FrameBuffer::Memory(data) => (**data).clone(),
            FrameBuffer::DmaBuf(_) => Vec::new(),
        }
    }
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrame")
            .field("frame_id", &self.frame_id)
            .field("pts", &self.pts)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("monitor_index", &self.monitor_index)
            .field("data_size", &self.data_size())
            .field("is_dmabuf", &self.is_dmabuf())
            .field("damage_regions", &self.damage_regions.len())
            .field("transform", &self.meta.transform)
            .field("flags", &self.flags.bits())
            .finish()
    }
}

/// Frame callback type
pub type FrameCallback = Box<dyn Fn(VideoFrame) + Send + Sync>;

/// Frame statistics
#[derive(Debug, Clone, Default)]
pub struct FrameStats {
    /// Total frames processed
    pub frames_processed: u64,

    /// Total bytes processed
    pub bytes_processed: u64,

    /// Frames dropped
    pub frames_dropped: u64,

    /// Average frame size
    pub avg_frame_size: f64,

    /// Average frame rate
    pub avg_fps: f64,

    /// Last frame timestamp
    pub last_frame_time: Option<SystemTime>,

    /// DMA-BUF frames
    pub dmabuf_frames: u64,

    /// Memory frames
    pub memory_frames: u64,
}

impl FrameStats {
    /// Create new frame statistics
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with new frame
    pub fn update(&mut self, frame: &VideoFrame) {
        self.frames_processed += 1;
        self.bytes_processed += frame.data_size() as u64;

        // Update average frame size
        self.avg_frame_size = self.bytes_processed as f64 / self.frames_processed as f64;

        // Update FPS
        if let Some(last_time) = self.last_frame_time {
            if let Ok(elapsed) = frame.capture_time.duration_since(last_time) {
                let interval_secs = elapsed.as_secs_f64();
                if interval_secs > 0.0 {
                    let instant_fps = 1.0 / interval_secs;
                    // Exponential moving average
                    self.avg_fps = self.avg_fps * 0.9 + instant_fps * 0.1;
                }
            }
        }

        self.last_frame_time = Some(frame.capture_time);

        // Track buffer types
        if frame.flags.has_dmabuf() {
            self.dmabuf_frames += 1;
        } else {
            self.memory_frames += 1;
        }
    }

    /// Record dropped frame
    pub fn record_drop(&mut self) {
        self.frames_dropped += 1;
    }

    /// Get drop rate
    pub fn drop_rate(&self) -> f64 {
        if self.frames_processed == 0 {
            0.0
        } else {
            self.frames_dropped as f64 / (self.frames_processed + self.frames_dropped) as f64
        }
    }

    /// Get DMA-BUF usage rate
    pub fn dmabuf_rate(&self) -> f64 {
        if self.frames_processed == 0 {
            0.0
        } else {
            self.dmabuf_frames as f64 / self.frames_processed as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_creation() {
        let frame = VideoFrame::new(1, 1920, 1080, 7680, PixelFormat::BGRA, 0);

        assert_eq!(frame.frame_id, 1);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.format, PixelFormat::BGRA);
    }

    #[test]
    fn test_frame_flags() {
        let mut flags = FrameFlags::new();
        assert!(!flags.has_dmabuf());

        flags.set_dmabuf();
        assert!(flags.has_dmabuf());

        flags.set_gpu_processed();
        assert!(flags.has_gpu_processed());
    }

    #[test]
    fn test_damage_regions() {
        let mut frame = VideoFrame::new(1, 1920, 1080, 7680, PixelFormat::BGRA, 0);

        frame.add_damage_region(DamageRegion::new(0, 0, 100, 100));
        frame.add_damage_region(DamageRegion::new(100, 100, 200, 200));

        assert_eq!(frame.damage_regions.len(), 2);
        assert_eq!(frame.total_damage_area(), 100 * 100 + 200 * 200);
    }

    #[test]
    fn test_frame_stats() {
        let mut stats = FrameStats::new();

        let frame = VideoFrame::with_data(1, 100, 100, 400, PixelFormat::BGRA, 0, vec![0u8; 40000]);

        stats.update(&frame);

        assert_eq!(stats.frames_processed, 1);
        assert_eq!(stats.bytes_processed, 40000);
        assert_eq!(stats.avg_frame_size, 40000.0);
    }

    #[test]
    fn test_drop_rate() {
        let mut stats = FrameStats::new();

        // Process 10 frames
        for i in 0..10 {
            let frame = VideoFrame::new(i, 100, 100, 400, PixelFormat::BGRA, 0);
            stats.update(&frame);
        }

        // Drop 2 frames
        stats.record_drop();
        stats.record_drop();

        // Drop rate should be 2/12 = 0.1666...
        let drop_rate = stats.drop_rate();
        assert!((drop_rate - 0.1666).abs() < 0.001);
    }
}
