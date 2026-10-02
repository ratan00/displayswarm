//! Buffer Metadata Extraction
//!
//! Reads SPA metadata from PipeWire buffers. Compositors attach metadata
//! to signal buffer orientation (VideoTransform), timing (Header),
//! crop regions (VideoCrop), damage rectangles (VideoDamage), and
//! cursor state (Cursor).
//!
//! All metadata types are optional — compositors only provide what they
//! support, and consumers must handle absent metadata gracefully.
//!
//! Extraction goes through libspa 0.10's safe metadata wrappers
//! ([`Buffer::find_meta`]) rather than raw `libspa_sys` pointer access, so
//! this module contains no `unsafe` code.

use libspa::buffer::meta::{MetaCursor, MetaHeader, MetaVideoCrop, MetaVideoDamage, MetaVideoTransform};
use pipewire::buffer::Buffer;
use tracing::{debug, trace};

/// Metadata extracted from a PipeWire buffer's SPA metadata array.
///
/// All fields are `Option` because compositors may not provide all types,
/// even when requested during stream negotiation.
#[derive(Debug, Clone, Default)]
pub struct BufferMeta {
    /// SPA_META_Header: presentation timestamp, flags, sequence number.
    /// Provides compositor-authoritative timing and buffer health signals
    /// (CORRUPTED, DISCONT, GAP flags).
    pub header: Option<HeaderMeta>,

    /// SPA_META_VideoTransform: buffer orientation (0-7).
    /// Maps 1:1 to wl_output.transform values.
    pub transform: Option<u32>,

    /// SPA_META_VideoCrop: source crop region within the buffer.
    /// Compositor may send oversized buffers with crop hints.
    pub crop: Option<CropRegion>,

    /// SPA_META_VideoDamage: changed regions since last buffer.
    /// Enables partial-frame encoding for bandwidth optimization.
    pub damage: Vec<DamageRect>,

    /// SPA_META_Cursor: cursor position, hotspot, and (when present) bitmap.
    pub cursor: Option<CursorMeta>,

    /// Chunk stride from spa_chunk (signed i32).
    /// Negative stride means the buffer is stored bottom-up (GL convention).
    /// Set from the buffer's data chunk, not from SPA metadata.
    pub chunk_stride: i32,

    /// Buffer data type (1=MemPtr, 2=MemFd, 3=DmaBuf).
    /// Different buffer types may require different transform handling
    /// due to compositor-specific code paths for each type.
    pub buffer_type: u32,

    /// Whether the producer set `SPA_DATA_FLAG_MAPPABLE` on the first data block.
    ///
    /// A CPU read of a block without this flag is out of contract. It does not
    /// fail loudly: mapping host-resident GPU memory succeeds and returns
    /// zeros, so this flag is the only advance warning that a mmap will produce
    /// nothing. Set from the data block, not from SPA metadata.
    pub mappable: bool,

    /// Whether the producer set `SPA_CHUNK_FLAG_EMPTY` on the first chunk.
    ///
    /// An explicit "nothing this cycle", as opposed to a zero size, which is
    /// only an absence. Set from the data chunk, not from SPA metadata.
    pub chunk_empty: bool,

    /// Best guess that a `SPA_CHUNK_FLAG_CORRUPTED` chunk is a cursor-only
    /// update rather than a failed paint.
    ///
    /// Mutter marks a buffer that carries only a cursor move corrupted with
    /// size 0, the same wire signature as a genuine recording failure (it
    /// clears the header flags on both paths). The only tell is a valid
    /// SPA_META_Cursor on the buffer, and a recycled slot can carry a stale
    /// one, so this is a heuristic, not a classification.
    pub cursor_only_update: bool,
}

/// Timing and health metadata from SPA_META_Header.
#[derive(Debug, Clone)]
pub struct HeaderMeta {
    /// Presentation timestamp in nanoseconds
    pub pts: i64,
    /// Decode timestamp offset from PTS
    pub dts_offset: i64,
    /// Sequence number (increments per frame)
    pub seq: u64,
    /// Buffer flags (CORRUPTED, DISCONT, GAP, DELTA_UNIT)
    pub flags: u32,
}

impl HeaderMeta {
    /// Producer marked this buffer as not continuous with the previous one.
    ///
    /// A capture consumer that tracks damage regions incrementally has to treat
    /// a discontinuity as a full-frame invalidation: the accumulated damage
    /// state describes a sequence this buffer is not part of.
    pub fn is_discont(&self) -> bool {
        self.flags & crate::ffi::spa_flags::SPA_META_HEADER_FLAG_DISCONT != 0
    }

    /// Producer marked this buffer's data as possibly corrupted.
    ///
    /// Independent of the chunk-level `SPA_CHUNK_FLAG_CORRUPTED`; a producer
    /// may set either, so both are read.
    pub fn is_corrupted(&self) -> bool {
        self.flags & crate::ffi::spa_flags::SPA_META_HEADER_FLAG_CORRUPTED != 0
    }

    /// Buffer carries media-neutral filler rather than real content.
    pub fn has_gap(&self) -> bool {
        self.flags & crate::ffi::spa_flags::SPA_META_HEADER_FLAG_GAP != 0
    }
}

/// Crop region from SPA_META_VideoCrop.
#[derive(Debug, Clone)]
pub struct CropRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Damage rectangle from SPA_META_VideoDamage.
#[derive(Debug, Clone)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Cursor metadata from SPA_META_Cursor.
#[derive(Debug, Clone)]
pub struct CursorMeta {
    /// Cursor ID (0 = invalid/no cursor)
    pub id: u32,
    /// Position on screen
    pub position: (i32, i32),
    /// Hotspot offset within the cursor bitmap
    pub hotspot: (i32, i32),
    /// Raw offset to bitmap data within the SPA cursor struct.
    /// 0 = no bitmap, >= sizeof(spa_meta_cursor) = bitmap follows.
    /// Prefer [`CursorMeta::bitmap`] for the decoded pixels.
    pub bitmap_offset: u32,
    /// Decoded cursor bitmap, when the compositor attached one.
    ///
    /// Compositors send the cursor image only on a cursor change (most frames
    /// carry position-only updates), so this is usually `None`.
    pub bitmap: Option<CursorBitmap>,
}

/// A cursor image attached to SPA_META_Cursor.
#[derive(Debug, Clone)]
pub struct CursorBitmap {
    /// Raw SPA video format id of the pixels (e.g. BGRA).
    pub format: u32,
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// Row stride in bytes (signed; negative means bottom-up).
    pub stride: i32,
    /// Pixel bytes, `height * stride.abs()` long, copied out of the buffer.
    pub pixels: Vec<u8>,
}

/// Extract all metadata from a PipeWire buffer's SPA metadata array.
///
/// Uses the safe libspa 0.10 metadata wrappers; the borrowed metadata is
/// copied into owned [`BufferMeta`] fields so the result outlives the buffer.
pub fn extract_buffer_meta(buffer: &Buffer<'_>) -> BufferMeta {
    let mut meta = BufferMeta::default();

    if let Some(header) = buffer.find_meta::<MetaHeader>() {
        let flags = header.flags().bits();
        trace!(
            "SPA_META_Header: pts={}, dts_offset={}, seq={}, flags={:#x}",
            header.pts(),
            header.dts_offset(),
            header.seq(),
            flags
        );
        meta.header = Some(HeaderMeta {
            pts: header.pts(),
            dts_offset: header.dts_offset(),
            seq: header.seq(),
            flags,
        });
    }

    if let Some(transform) = buffer.find_meta::<MetaVideoTransform>() {
        let value = transform.transform().as_raw();
        debug!("SPA_META_VideoTransform: value={}", value);
        meta.transform = Some(value);
    }

    if let Some(crop) = buffer.find_meta::<MetaVideoCrop>() {
        let region = crop.meta_region();
        let position = region.position();
        let size = region.size();
        trace!(
            "SPA_META_VideoCrop: {}x{} at ({},{})",
            size.width, size.height, position.x, position.y
        );
        meta.crop = Some(CropRegion {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        });
    }

    if let Some(damage) = buffer.find_meta::<MetaVideoDamage>() {
        // The iterator handles validity and the zero-size terminator internally.
        meta.damage = damage
            .iter()
            .map(|region| {
                let position = region.position();
                let size = region.size();
                DamageRect {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                }
            })
            .collect();
        if !meta.damage.is_empty() {
            trace!("SPA_META_VideoDamage: {} regions", meta.damage.len());
        }
    }

    if let Some(cursor) = buffer.find_meta::<MetaCursor>() {
        // id == 0 means there is no new cursor data this frame.
        if cursor.id() != 0 {
            let position = cursor.position();
            let hotspot = cursor.hotspot();
            let bitmap = cursor.bitmap().and_then(|bitmap| {
                bitmap.bitmap_data().map(|pixels| {
                    let size = bitmap.size();
                    CursorBitmap {
                        format: bitmap.format().as_raw(),
                        width: size.width,
                        height: size.height,
                        stride: bitmap.stride(),
                        pixels: pixels.to_vec(),
                    }
                })
            });
            trace!(
                "SPA_META_Cursor: id={}, pos=({},{}), hotspot=({},{}), bitmap_offset={}, bitmap={}",
                cursor.id(),
                position.x,
                position.y,
                hotspot.x,
                hotspot.y,
                cursor.bitmap_offset(),
                bitmap.is_some()
            );
            meta.cursor = Some(CursorMeta {
                id: cursor.id(),
                position: (position.x, position.y),
                hotspot: (hotspot.x, hotspot.y),
                bitmap_offset: cursor.bitmap_offset(),
                bitmap,
            });
        }
    }

    meta
}

/// Most damage rectangles requested per buffer. Mutter reports up to 32 before
/// collapsing to one bounding box, KWin up to 16; requesting a range up to this
/// lets each producer use its own limit (see `request_buffer_metadata`).
pub const MAX_DAMAGE_REGIONS: usize = 32;
