//! Hardware Cursor Extraction
//!
//! Extracts cursor position and bitmap from PipeWire metadata.
//! This is useful for remote desktop scenarios where the cursor needs
//! to be rendered separately from the video stream.
//!
//! # Why Separate Cursor?
//!
//! In remote desktop applications, rendering the cursor separately provides:
//! - Lower perceived latency (cursor moves immediately on client)
//! - Reduced bandwidth (cursor is small compared to full frame)
//! - Client-side cursor customization
//!
//! # Usage
//!
//! ```rust,ignore
//! use lamco_pipewire::cursor::{CursorExtractor, CursorInfo};
//!
//! let mut extractor = CursorExtractor::new();
//!
//! // Update from PipeWire frame metadata
//! // extractor.update_from_meta(&cursor_meta);
//!
//! if let Some(cursor) = extractor.current_cursor() {
//!     println!("Cursor at {:?}, visible: {}", cursor.position, cursor.visible);
//!     if let Some(bitmap) = &cursor.bitmap {
//!         // Render cursor bitmap at position
//!     }
//! }
//! ```

use std::time::{Duration, Instant};

/// Cursor information extracted from PipeWire
#[derive(Debug, Clone)]
pub struct CursorInfo {
    /// Cursor position (x, y) in screen coordinates
    pub position: (i32, i32),

    /// Hotspot offset within the cursor bitmap
    pub hotspot: (i32, i32),

    /// Cursor bitmap size (width, height)
    pub size: (u32, u32),

    /// Cursor bitmap data (BGRA format)
    ///
    /// `None` if cursor bitmap hasn't changed since last update.
    pub bitmap: Option<Vec<u8>>,

    /// Whether cursor is currently visible
    pub visible: bool,

    /// Timestamp of last update
    pub timestamp: Instant,

    /// Serial number for change detection
    pub serial: u64,
}

impl Default for CursorInfo {
    fn default() -> Self {
        Self {
            position: (0, 0),
            hotspot: (0, 0),
            size: (0, 0),
            bitmap: None,
            visible: true,
            timestamp: Instant::now(),
            serial: 0,
        }
    }
}

impl CursorInfo {
    /// Check if cursor bitmap has changed
    #[must_use]
    pub fn has_bitmap_changed(&self, previous_serial: u64) -> bool {
        self.serial != previous_serial && self.bitmap.is_some()
    }

    /// Get age of cursor data
    #[must_use]
    pub fn age(&self) -> Duration {
        self.timestamp.elapsed()
    }
}

/// Hardware cursor extractor
///
/// Tracks cursor state across frames and provides efficient change detection.
pub struct CursorExtractor {
    /// Current cursor state
    current: CursorInfo,

    /// Previous cursor position for delta calculation
    previous_position: (i32, i32),

    /// Bitmap cache (serial -> bitmap)
    /// Keeps last N cursors for efficient switching
    bitmap_cache: Vec<(u64, Vec<u8>)>,

    /// Maximum cache entries
    max_cache_entries: usize,

    /// Statistics
    stats: CursorStats,
}

impl CursorExtractor {
    /// Create a new cursor extractor
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: CursorInfo::default(),
            previous_position: (0, 0),
            bitmap_cache: Vec::new(),
            max_cache_entries: 8,
            stats: CursorStats::default(),
        }
    }

    /// Create with custom cache size
    #[must_use]
    pub fn with_cache_size(max_entries: usize) -> Self {
        Self {
            max_cache_entries: max_entries,
            ..Self::new()
        }
    }

    /// Update cursor position
    ///
    /// Called when position changes but bitmap hasn't.
    pub fn update_position(&mut self, x: i32, y: i32) {
        self.previous_position = self.current.position;
        self.current.position = (x, y);
        self.current.timestamp = Instant::now();
        self.stats.position_updates += 1;
    }

    /// Update cursor visibility
    pub fn update_visibility(&mut self, visible: bool) {
        if self.current.visible != visible {
            self.current.visible = visible;
            self.stats.visibility_changes += 1;
        }
    }

    /// Update cursor bitmap
    ///
    /// # Arguments
    ///
    /// * `bitmap` - BGRA bitmap data
    /// * `width` - Bitmap width
    /// * `height` - Bitmap height
    /// * `hotspot_x` - Hotspot X offset
    /// * `hotspot_y` - Hotspot Y offset
    pub fn update_bitmap(&mut self, bitmap: Vec<u8>, width: u32, height: u32, hotspot_x: i32, hotspot_y: i32) {
        self.current.serial += 1;
        self.current.size = (width, height);
        self.current.hotspot = (hotspot_x, hotspot_y);
        self.current.timestamp = Instant::now();

        // Cache the bitmap
        self.cache_bitmap(self.current.serial, bitmap.clone());

        self.current.bitmap = Some(bitmap);
        self.stats.bitmap_updates += 1;
    }

    /// Update from raw PipeWire cursor metadata
    ///
    /// This is the main entry point for updating cursor state from
    /// PipeWire's spa_meta_cursor structure.
    ///
    /// # Arguments
    ///
    /// * `position` - Cursor position (x, y)
    /// * `hotspot` - Hotspot offset (x, y)
    /// * `size` - Bitmap size (width, height)
    /// * `bitmap` - Optional bitmap data (BGRA)
    /// * `visible` - Whether cursor is visible
    pub fn update_from_raw(
        &mut self,
        position: (i32, i32),
        hotspot: (i32, i32),
        size: (u32, u32),
        bitmap: Option<Vec<u8>>,
        visible: bool,
    ) {
        self.update_position(position.0, position.1);
        self.update_visibility(visible);

        if let Some(bmp) = bitmap {
            self.update_bitmap(bmp, size.0, size.1, hotspot.0, hotspot.1);
        }
    }

    /// Get current cursor information
    #[must_use]
    pub fn current_cursor(&self) -> Option<&CursorInfo> {
        if self.current.visible {
            Some(&self.current)
        } else {
            None
        }
    }

    /// Get cursor regardless of visibility
    #[must_use]
    pub fn cursor_state(&self) -> &CursorInfo {
        &self.current
    }

    /// Get position delta since last update
    #[must_use]
    pub fn position_delta(&self) -> (i32, i32) {
        (
            self.current.position.0 - self.previous_position.0,
            self.current.position.1 - self.previous_position.1,
        )
    }

    /// Check if cursor has moved
    #[must_use]
    pub fn has_moved(&self) -> bool {
        self.current.position != self.previous_position
    }

    /// Get cached bitmap by serial
    #[must_use]
    pub fn get_cached_bitmap(&self, serial: u64) -> Option<&[u8]> {
        self.bitmap_cache
            .iter()
            .find(|(s, _)| *s == serial)
            .map(|(_, b)| b.as_slice())
    }

    /// Get statistics
    #[must_use]
    pub fn stats(&self) -> &CursorStats {
        &self.stats
    }

    /// Reset cursor state
    pub fn reset(&mut self) {
        self.current = CursorInfo::default();
        self.previous_position = (0, 0);
        self.bitmap_cache.clear();
    }

    /// Add bitmap to cache
    fn cache_bitmap(&mut self, serial: u64, bitmap: Vec<u8>) {
        // Remove oldest if at capacity
        if self.bitmap_cache.len() >= self.max_cache_entries {
            self.bitmap_cache.remove(0);
        }

        self.bitmap_cache.push((serial, bitmap));
    }
}

impl Default for CursorExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// Cursor extraction statistics
#[derive(Debug, Clone, Default)]
pub struct CursorStats {
    /// Number of position updates
    pub position_updates: u64,

    /// Number of bitmap updates
    pub bitmap_updates: u64,

    /// Number of visibility changes
    pub visibility_changes: u64,
}

impl CursorStats {
    /// Calculate bitmap update rate (updates per position update)
    #[must_use]
    pub fn bitmap_rate(&self) -> f64 {
        if self.position_updates == 0 {
            0.0
        } else {
            self.bitmap_updates as f64 / self.position_updates as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_info_default() {
        let info = CursorInfo::default();
        assert_eq!(info.position, (0, 0));
        assert!(info.visible);
        assert!(info.bitmap.is_none());
    }

    #[test]
    fn test_cursor_extractor() {
        let mut extractor = CursorExtractor::new();

        // Initial state
        assert!(!extractor.has_moved());

        // Update position
        extractor.update_position(100, 200);
        assert_eq!(extractor.current_cursor().map(|c| c.position), Some((100, 200)));
        assert!(extractor.has_moved());
        assert_eq!(extractor.position_delta(), (100, 200));

        // Update again
        extractor.update_position(150, 250);
        assert_eq!(extractor.position_delta(), (50, 50));
    }

    #[test]
    fn test_bitmap_update() {
        let mut extractor = CursorExtractor::new();

        let bitmap = vec![255u8; 32 * 32 * 4]; // 32x32 BGRA
        extractor.update_bitmap(bitmap.clone(), 32, 32, 0, 0);

        let cursor = extractor.cursor_state();
        assert_eq!(cursor.size, (32, 32));
        assert!(cursor.bitmap.is_some());
        assert_eq!(cursor.serial, 1);
    }

    #[test]
    fn test_visibility() {
        let mut extractor = CursorExtractor::new();

        assert!(extractor.current_cursor().is_some());

        extractor.update_visibility(false);
        assert!(extractor.current_cursor().is_none());

        // cursor_state always returns cursor regardless of visibility
        assert!(!extractor.cursor_state().visible);
    }

    #[test]
    fn test_bitmap_cache() {
        let mut extractor = CursorExtractor::with_cache_size(2);

        // Add three bitmaps (cache size is 2)
        extractor.update_bitmap(vec![1], 1, 1, 0, 0);
        let serial1 = extractor.cursor_state().serial;

        extractor.update_bitmap(vec![2], 1, 1, 0, 0);
        let serial2 = extractor.cursor_state().serial;

        extractor.update_bitmap(vec![3], 1, 1, 0, 0);
        let serial3 = extractor.cursor_state().serial;

        // First should be evicted
        assert!(extractor.get_cached_bitmap(serial1).is_none());
        assert!(extractor.get_cached_bitmap(serial2).is_some());
        assert!(extractor.get_cached_bitmap(serial3).is_some());
    }

    #[test]
    fn test_stats() {
        let mut extractor = CursorExtractor::new();

        extractor.update_position(10, 20);
        extractor.update_position(30, 40);
        extractor.update_bitmap(vec![1], 1, 1, 0, 0);
        extractor.update_visibility(false);
        extractor.update_visibility(true);

        let stats = extractor.stats();
        assert_eq!(stats.position_updates, 2);
        assert_eq!(stats.bitmap_updates, 1);
        assert_eq!(stats.visibility_changes, 2);
    }
}
