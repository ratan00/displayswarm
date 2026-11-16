//! Clipboard on Windows, through the Win32 clipboard.
//!
//! * Change detection is `GetClipboardSequenceNumber`, which costs nothing, so
//!   the clipboard is opened only after it changed.
//! * Text is `CF_UNICODETEXT`.
//! * Images: the registered `"PNG"` format when an application offers it
//!   (browsers, Office, the Snipping Tool), else `CF_DIB` (Print Screen, Paint),
//!   which is converted to PNG here. Writing puts both `"PNG"` and `CF_DIB` on
//!   the clipboard so every application can paste.
//!
//! The DIB conversions are plain byte work in [`dib`] and unit-tested on every
//! platform.

use super::clipboard::MAX_TEXT;
#[cfg(windows)]
use super::clipboard::{ClipContent, MAX_IMAGE};

/// `CF_DIB` <-> PNG conversion (32-bit and 24-bit uncompressed DIBs).
pub mod dib {
    /// `BITMAPINFOHEADER` size.
    const HEADER: usize = 40;
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;

    fn u32_at(b: &[u8], off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
    }
    fn i32_at(b: &[u8], off: usize) -> Option<i32> {
        Some(i32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
    }
    fn u16_at(b: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
    }

    /// A packed DIB (`BITMAPINFOHEADER` then pixels) to RGBA rows, top first.
    pub fn to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let header_size = u32_at(dib, 0)? as usize;
        if header_size < HEADER {
            return None;
        }
        let width = i32_at(dib, 4)?;
        let height = i32_at(dib, 8)?;
        let bits = u16_at(dib, 14)?;
        let compression = u32_at(dib, 16)?;
        let colors_used = u32_at(dib, 32)? as usize;
        if width <= 0 || height == 0 || width > 16384 || height.unsigned_abs() > 16384 {
            return None;
        }
        if !(bits == 32 || bits == 24) || !(compression == BI_RGB || (compression == BI_BITFIELDS && bits == 32)) {
            return None;
        }
        // BI_BITFIELDS with a plain 40-byte header is followed by three masks.
        let masks = if compression == BI_BITFIELDS && header_size == HEADER { 12 } else { 0 };
        let offset = header_size + masks + colors_used * 4;
        let (w, h) = (width as usize, height.unsigned_abs() as usize);
        let bpp = bits as usize / 8;
        let stride = (w * bpp + 3) & !3;
        let pixels = dib.get(offset..offset + stride * h)?;
        let bottom_up = height > 0;
        // 32-bit DIBs often carry 0 in the alpha byte meaning "opaque".
        let alpha_used = bits == 32 && pixels.chunks_exact(4).any(|p| p[3] != 0);
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let src_row = if bottom_up { h - 1 - row } else { row };
            let line = &pixels[src_row * stride..src_row * stride + w * bpp];
            for px in line.chunks_exact(bpp) {
                let a = if bpp == 4 && alpha_used { px[3] } else { 255 };
                out.extend_from_slice(&[px[2], px[1], px[0], a]);
            }
        }
        Some((w as u32, h as u32, out))
    }

    /// RGBA rows (top first) to a packed 32-bit bottom-up DIB, the layout every
    /// application reads.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
        let (w, h) = (width as usize, height as usize);
        if rgba.len() < w * h * 4 || w == 0 || h == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(HEADER + w * h * 4);
        out.extend_from_slice(&(HEADER as u32).to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&BI_RGB.to_le_bytes());
        out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]); // resolution, colours used/important
        for row in (0..h).rev() {
            for px in rgba[row * w * 4..(row + 1) * w * 4].chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        Some(out)
    }

    pub fn png_encode(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, width, height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().ok()?;
            w.write_image_data(rgba).ok()?;
        }
        Some(out)
    }

    pub fn png_decode(png_bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
        dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
        let mut reader = dec.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());
        let (w, h) = (info.width, info.height);
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            _ => return None,
        };
        Some((w, h, rgba))
    }
}

/// Whether text fits the sync limit.
#[cfg_attr(not(windows), allow(dead_code))]
fn text_fits(t: &str) -> bool {
    !t.is_empty() && t.len() <= MAX_TEXT
}

#[cfg(windows)]
pub use win::WindowsClipboard;

#[cfg(windows)]
mod win {
    use super::*;
    use crate::services::clipboard::ClipboardBackend;
    use ::windows::core::w;
    use ::windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use ::windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use ::windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;

    pub struct WindowsClipboard {
        png_format: u32,
        last_seq: Option<u32>,
        /// The sequence number our own last write produced.
        own_seq: Option<u32>,
    }

    impl WindowsClipboard {
        pub fn new() -> Self {
            let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
            Self { png_format, last_seq: None, own_seq: None }
        }
    }

    /// Holds the clipboard open; closes it on drop. Retries briefly, since
    /// another application may hold it for a moment.
    struct Open;

    impl Open {
        fn new() -> Option<Self> {
            for _ in 0..10 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Some(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// Copies the bytes of clipboard format `fmt` (clipboard open), up to `limit`.
    fn bytes_of(fmt: u32, limit: usize) -> Option<Vec<u8>> {
        unsafe {
            let h = GetClipboardData(fmt).ok()?;
            let g = HGLOBAL(h.0);
            let size = GlobalSize(g);
            if size == 0 || size > limit {
                return None;
            }
            let p = GlobalLock(g);
            if p.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(p as *const u8, size).to_vec();
            let _ = GlobalUnlock(g);
            Some(v)
        }
    }

    /// Puts `data` on the (open, emptied) clipboard as `fmt`.
    fn put(fmt: u32, data: &[u8]) -> Result<(), String> {
        unsafe {
            let g = GlobalAlloc(GMEM_MOVEABLE, data.len()).map_err(|e| e.to_string())?;
            let p = GlobalLock(g);
            if p.is_null() {
                let _ = GlobalFree(g);
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
            let _ = GlobalUnlock(g);
            // On success the clipboard owns the memory; on failure we free it.
            if let Err(e) = SetClipboardData(fmt, HANDLE(g.0)) {
                let _ = GlobalFree(g);
                return Err(e.to_string());
            }
            Ok(())
        }
    }

    impl ClipboardBackend for WindowsClipboard {
        fn read(&mut self) -> Option<ClipContent> {
            let seq = unsafe { GetClipboardSequenceNumber() };
            if self.last_seq == Some(seq) {
                return None;
            }
            self.last_seq = Some(seq);
            if self.own_seq == Some(seq) {
                return None; // what we just wrote
            }
            let _open = Open::new()?;
            unsafe {
                if self.png_format != 0 && IsClipboardFormatAvailable(self.png_format).is_ok() {
                    if let Some(png) = bytes_of(self.png_format, MAX_IMAGE + 1) {
                        return Some(ClipContent::Png(png));
                    }
                }
                if IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok() {
                    let raw = bytes_of(CF_UNICODETEXT, MAX_TEXT * 2 + 2)?;
                    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                    let text = String::from_utf16_lossy(&units[..end]);
                    return text_fits(&text).then_some(ClipContent::Text(text));
                }
                if IsClipboardFormatAvailable(CF_DIB).is_ok() {
                    let raw = bytes_of(CF_DIB, 256 * 1024 * 1024)?;
                    let (w, h, rgba) = dib::to_rgba(&raw)?;
                    let png = dib::png_encode(w, h, &rgba)?;
                    return Some(ClipContent::Png(png));
                }
            }
            None
        }

        fn write(&mut self, c: &ClipContent) -> Result<(), String> {
            let _open = Open::new().ok_or("the clipboard is busy")?;
            unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
            match c {
                ClipContent::Text(t) => {
                    let bytes: Vec<u8> = t.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
                    put(CF_UNICODETEXT, &bytes)?;
                }
                ClipContent::Png(p) => {
                    if self.png_format != 0 {
                        put(self.png_format, p)?;
                    }
                    if let Some(dibbytes) = dib::png_decode(p).and_then(|(w, h, rgba)| dib::from_rgba(w, h, &rgba)) {
                        put(CF_DIB, &dibbytes)?;
                    }
                }
            }
            drop(_open);
            let seq = unsafe { GetClipboardSequenceNumber() };
            self.own_seq = Some(seq);
            self.last_seq = Some(seq);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dib;

    #[test]
    fn dib_round_trips_through_rgba_and_png() {
        // 2x2: red, green / blue, half-transparent white.
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
        let d = dib::from_rgba(2, 2, &rgba).unwrap();
        let (w, h, back) = dib::to_rgba(&d).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(back, rgba, "bottom-up rows and BGRA order come back the same");
        let png = dib::png_encode(w, h, &back).unwrap();
        assert_eq!(dib::png_decode(&png).unwrap(), (2, 2, rgba));
    }

    #[test]
    fn a_24_bit_bottom_up_dib_with_row_padding_reads_opaque() {
        // 1x2, 24-bit: each 3-byte row is padded to 4.
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&1i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        d.extend_from_slice(&[0, 0, 255, 0]); // bottom row: red (BGR) + pad
        d.extend_from_slice(&[255, 0, 0, 0]); // top row: blue
        let (_, _, rgba) = dib::to_rgba(&d).unwrap();
        assert_eq!(rgba, vec![0, 0, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn a_32_bit_dib_with_zero_alpha_is_opaque_and_garbage_is_refused() {
        let d = dib::from_rgba(1, 1, &[10, 20, 30, 0]).unwrap();
        assert_eq!(dib::to_rgba(&d).unwrap().2, vec![10, 20, 30, 255]);
        assert!(dib::to_rgba(&[1, 2, 3]).is_none());
        let mut huge = d.clone();
        huge[4..8].copy_from_slice(&100_000i32.to_le_bytes());
        assert!(dib::to_rgba(&huge).is_none());
    }
}

/// Needs a Windows desktop session (or Wine with a display):
/// `cargo test clipboard_windows -- --ignored`.
#[cfg(all(test, windows))]
mod live_tests {
    use super::*;
    use crate::services::clipboard::ClipboardBackend;

    #[test]
    #[ignore = "needs a desktop session"]
    fn writes_and_reads_text_and_images() {
        let mut a = WindowsClipboard::new();
        let mut b = WindowsClipboard::new();
        let _ = b.read();
        a.write(&ClipContent::Text("caf\u{e9} \u{1F600}".into())).unwrap();
        assert_eq!(a.read(), None, "our own write is not read back");
        assert_eq!(b.read(), Some(ClipContent::Text("caf\u{e9} \u{1F600}".into())));
        assert_eq!(b.read(), None, "unchanged");
        let png = dib::png_encode(2, 2, &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 9, 9, 9, 255]).unwrap();
        a.write(&ClipContent::Png(png.clone())).unwrap();
        assert_eq!(b.read(), Some(ClipContent::Png(png)));
    }
}
