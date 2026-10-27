//! Windows capture -> encode benchmark, no phone needed. It animates a window
//! on the primary monitor (a topmost window repainted every DWM frame, so the
//! desktop changes ~60 times a second), captures the monitor with
//! Windows.Graphics.Capture, fits it to a phone-sized picture and encodes it,
//! the same two threads and pacing as `server.rs` (capture_loop/encoder_loop).
//!
//!   cargo test --release --features encoders --lib windows_bench -- --ignored --nocapture
//!
//! Env: `BENCH_SECS` (default 5), `BENCH_W`/`BENCH_H` (default 1600x720),
//! `BENCH_FPS` (default 60), `DISPLAYSWARM_ZEROCOPY=0` for the CPU path only.
//! It runs the CPU path first, then the zero-copy one, and prints both.

use super::windows::{RawSource, WindowsCapturer};
use super::windows_wgc::WgcSource;
use super::{ScreenCapturer, PixelFormat};
use crate::encoder::VideoEncoder;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

fn env_u32(k: &str, d: u32) -> u32 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// A window that changes every compositor frame until dropped.
struct Animation {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Animation {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || unsafe {
            let hwnd: HWND = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("displayswarm bench"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                1280,
                720,
                None,
                None,
                None,
                None,
            )
            .unwrap_or_default();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let dc = GetDC(hwnd);
            let mut t = 0u32;
            while !s2.load(Ordering::Relaxed) {
                for i in 0..48u32 {
                    let c = (i * 37 + t * 11) & 0xff;
                    let brush = CreateSolidBrush(COLORREF(c | ((255 - c) << 8) | (((i * 53 + t * 7) & 0xff) << 16)));
                    let x = ((i * 97 + t * 13) % 1200) as i32;
                    let y = ((i * 61 + t * 5) % 650) as i32;
                    let r = RECT { left: x, top: y, right: x + 80, bottom: y + 70 };
                    FillRect(dc, &r, brush);
                    let _ = DeleteObject(brush);
                }
                t = t.wrapping_add(1);
                let _ = DwmFlush();
            }
            ReleaseDC(hwnd, dc);
            let _ = DestroyWindow(hwnd);
        });
        std::thread::sleep(Duration::from_millis(500));
        Animation { stop, thread: Some(thread) }
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct Stats {
    captured: AtomicU64,
    cap_us: AtomicU64,
    encoded: AtomicU64,
    enc_us: AtomicU64,
    enc_max_us: AtomicU64,
    bytes: AtomicU64,
}

/// One run of the server's two-stage pipeline for `secs` seconds.
fn run(label: &str, gpu: bool, secs: u64, w: u32, h: u32, fps: u32) {
    crate::capture::set_gpu_frames(false);
    let mut encoder = crate::encoder::create_encoder(w, h, fps, 12_000, None);
    // The encoder turns zero-copy on when it can; the CPU run forces it off.
    if !gpu {
        crate::capture::set_gpu_frames(false);
    }
    let backend = encoder.backend_name();
    let mon = super::windows::sys::find_monitor(None).expect("no monitor");
    let source = WgcSource::for_monitor(&mon).expect("WGC");
    let readback = source.readback_stats();
    let mut capturer = WindowsCapturer::from_source(Box::new(source), w, h, None);

    let slot: Arc<(Mutex<Option<crate::capture::Frame>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
    let stats = Arc::new(Stats::default());
    let run = Arc::new(AtomicBool::new(true));

    let cap = {
        let (slot, stats, run) = (slot.clone(), stats.clone(), run.clone());
        std::thread::spawn(move || {
            let interval = Duration::from_micros(900_000 / fps as u64);
            let mut next_allowed = Instant::now();
            while run.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next_allowed {
                    std::thread::sleep((next_allowed - now).min(Duration::from_millis(10)));
                    continue;
                }
                let t = Instant::now();
                if let Ok(Some(f)) = capturer.next_frame(Duration::from_millis(100)) {
                    stats.cap_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                    stats.captured.fetch_add(1, Ordering::Relaxed);
                    *slot.0.lock().unwrap() = Some(f);
                    slot.1.notify_one();
                    next_allowed = Instant::now() + interval;
                }
            }
        })
    };
    let enc = {
        let (slot, stats, run) = (slot.clone(), stats.clone(), run.clone());
        std::thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                let frame = {
                    let mut g = slot.0.lock().unwrap();
                    if g.is_none() {
                        g = slot.1.wait_timeout(g, Duration::from_millis(100)).unwrap().0;
                    }
                    g.take()
                };
                let Some(frame) = frame else { continue };
                let t = Instant::now();
                let packets = encoder.encode(&frame).expect("encode");
                let us = t.elapsed().as_micros() as u64;
                if packets.is_empty() {
                    continue;
                }
                stats.enc_us.fetch_add(us, Ordering::Relaxed);
                stats.enc_max_us.fetch_max(us, Ordering::Relaxed);
                stats.encoded.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(packets.iter().map(|p| p.data.len() as u64).sum(), Ordering::Relaxed);
            }
        })
    };
    // Warm-up second, then measure.
    std::thread::sleep(Duration::from_secs(1));
    let (c0, e0, b0) = (stats.captured.load(Ordering::Relaxed), stats.encoded.load(Ordering::Relaxed), stats.bytes.load(Ordering::Relaxed));
    let (cu0, eu0) = (stats.cap_us.load(Ordering::Relaxed), stats.enc_us.load(Ordering::Relaxed));
    let (rb0, rn0) = readback.get();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    let dt = t0.elapsed().as_secs_f64();
    let (c1, e1, b1) = (stats.captured.load(Ordering::Relaxed), stats.encoded.load(Ordering::Relaxed), stats.bytes.load(Ordering::Relaxed));
    let (cu1, eu1) = (stats.cap_us.load(Ordering::Relaxed), stats.enc_us.load(Ordering::Relaxed));
    let (rb1, rn1) = readback.get();
    run.store(false, Ordering::Relaxed);
    let _ = (cap.join(), enc.join());
    let (cn, en) = ((c1 - c0).max(1) as f64, (e1 - e0).max(1) as f64);
    println!(
        "[{label}] {backend}, {w}x{h}: captured {:.1} fps, encoded {:.1} fps, {:.0} kbps\n    \
         capture call avg {:.2} ms (of which GPU copy/readback {:.2} ms; the rest is waiting for the next frame + scale)\n    \
         encode call avg {:.2} ms (max {:.2} ms)",
        cn / dt,
        en / dt,
        (b1 - b0) as f64 * 8.0 / 1000.0 / dt,
        (cu1 - cu0) as f64 / cn / 1000.0,
        (rb1 - rb0) as f64 / (rn1 - rn0).max(1) as f64 / 1000.0,
        (eu1 - eu0) as f64 / en / 1000.0,
        stats.enc_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
    );
}

#[test]
#[ignore = "needs a desktop session and a GPU; run by hand"]
fn capture_to_encode_cpu_vs_zero_copy() {
    let (secs, w, h, fps) = (env_u32("BENCH_SECS", 5) as u64, env_u32("BENCH_W", 1600), env_u32("BENCH_H", 720), env_u32("BENCH_FPS", 60));
    let _anim = Animation::start();
    let _ = (PixelFormat::Bgra, <WgcSource as RawSource>::describe);
    run("CPU readback + SIMD scale/convert", false, secs, w, h, fps);
    if std::env::var("DISPLAYSWARM_ZEROCOPY").map(|v| v != "0").unwrap_or(true) {
        run("zero-copy (D3D11 video processor -> QSV)", true, secs, w, h, fps);
    }
}
