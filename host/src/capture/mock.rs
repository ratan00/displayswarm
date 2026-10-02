use super::{Frame, ScreenCapturer};
use crate::protocol::host_monotonic_us;
use std::time::{Duration, Instant};

/// Spacing of synthetic frames (~60 fps), so `next_frame` behaves like a real
/// source instead of producing frames as fast as the CPU allows.
const MOCK_FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub struct MockCapturer {
    width: u32,
    height: u32,
    frame_count: u32,
    last_frame: Option<Instant>,
}

impl MockCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame_count: 0,
            last_frame: None,
        }
    }
}

impl ScreenCapturer for MockCapturer {
    fn capture_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
        self.frame_count = self.frame_count.wrapping_add(1);
        let now_us = host_monotonic_us();

        let total_pixels = (self.width * self.height) as usize;
        let mut buffer = vec![0u8; total_pixels * 4];

        // Draw animated background pattern
        let offset = (self.frame_count * 4) as usize;
        for y in 0..self.height as usize {
            let row_offset = y * self.width as usize * 4;
            let r = ((y + offset) % 256) as u8;
            for x in 0..self.width as usize {
                let idx = row_offset + (x * 4);
                let g = ((x + offset) % 256) as u8;
                let b = 180u8;
                buffer[idx] = r;     // R
                buffer[idx + 1] = g; // G
                buffer[idx + 2] = b; // B
                buffer[idx + 3] = 255; // A
            }
        }

        Ok(Frame {
            width: self.width,
            height: self.height,
            format: super::PixelFormat::Rgba,
            data: buffer,
            timestamp_us: now_us,
        })
    }

    /// Paced at ~60 fps; waits at most `timeout` for the next slot.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Frame>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(prev) = self.last_frame {
            let due = MOCK_FRAME_INTERVAL.saturating_sub(prev.elapsed());
            if due > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(due);
        }
        self.last_frame = Some(Instant::now());
        self.capture_frame().map(Some)
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }
}
