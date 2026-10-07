//! Encoder benchmarks and end-to-end checks on synthetic text-like desktop frames.
//!
//! Run with:
//!   cargo test --release --lib encoder::bench -- --ignored --nocapture
//! Raw streams are written to `$DISPLAYSWARM_BENCH_DIR` (default: the system temp dir)
//! so they can be checked with `ffprobe` / `ffmpeg -i x.h264 -f null -`.

use super::*;
use crate::capture::PixelFormat;
use crate::protocol::{FRAME_TYPE_CONFIG, FRAME_TYPE_DELTA, FRAME_TYPE_KEY};
use std::io::Write;
use std::time::Instant;

const FRAMES: usize = 120;
const SCROLL_PX_PER_FRAME: usize = 3;

/// A tall page of dark "words" on white, so consecutive frames are a scrolling window:
/// sharp high-contrast edges plus real motion, which is what a terminal/IDE looks like.
fn text_page(width: usize, rows: usize) -> Vec<u8> {
    let mut page = vec![255u8; width * rows * 4];
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut y = 8;
    while y + 14 < rows {
        let mut x = 16;
        while x + 8 < width - 16 {
            let word = 2 + (rnd() % 9) as usize;
            for _ in 0..word {
                if x + 8 >= width {
                    break;
                }
                let glyph_h = 8 + (rnd() % 5) as usize;
                let mask = rnd();
                for gy in 0..glyph_h {
                    for gx in 0..6 {
                        if (mask >> ((gy * 6 + gx) % 60)) & 1 == 1 {
                            let o = ((y + gy) * width + x + gx) * 4;
                            page[o..o + 3].copy_from_slice(&[30, 30, 34]);
                        }
                    }
                }
                x += 8;
            }
            x += 8;
        }
        y += 20;
    }
    page
}

fn frames(width: usize, height: usize, n: usize) -> Vec<Frame> {
    let page = text_page(width, height + n * SCROLL_PX_PER_FRAME);
    (0..n)
        .map(|i| {
            let start = i * SCROLL_PX_PER_FRAME * width * 4;
            Frame {
                width: width as u32,
                height: height as u32,
                format: PixelFormat::Rgba,
                data: page[start..start + width * height * 4].to_vec(),
                timestamp_us: 1_000_000 + i as u64 * 16_667,
            }
        })
        .collect()
}

fn bench_dir() -> std::path::PathBuf {
    std::env::var("DISPLAYSWARM_BENCH_DIR").map(Into::into).unwrap_or_else(|_| std::env::temp_dir())
}

#[test]
fn default_encoder_emits_config_key_delta_in_order() {
    let (w, h) = (640usize, 384usize);
    let fr = frames(w, h, 6);
    let mut enc = create_default_encoder(w as u32, h as u32, 60, 4000);
    let mut all = Vec::new();
    for f in &fr {
        let pk = enc.encode(f).expect("encode");
        assert!(!pk.is_empty(), "{}: no output for a frame (added latency)", enc.backend_name());
        for p in &pk {
            assert_eq!(p.timestamp_us, f.timestamp_us);
            // x264 writes a 3-byte start code before the SEI that opens a keyframe;
            // both lengths are valid Annex-B and the phone accepts either.
            assert!(
                p.data.starts_with(&[0, 0, 0, 1]) || p.data.starts_with(&[0, 0, 1]),
                "{}: no Annex-B start code: {:?}",
                enc.backend_name(),
                &p.data[..p.data.len().min(4)]
            );
        }
        all.push(pk);
    }
    assert_eq!(all[0][0].frame_type, FRAME_TYPE_CONFIG);
    assert_eq!(all[0][1].frame_type, FRAME_TYPE_KEY);
    for pk in &all[1..] {
        assert_eq!(pk.len(), 1);
        assert_eq!(pk[0].frame_type, FRAME_TYPE_DELTA);
    }
    enc.request_keyframe();
    let pk = enc.encode(&fr[0]).unwrap();
    assert_eq!(pk[0].frame_type, FRAME_TYPE_CONFIG);
    assert_eq!(pk[1].frame_type, FRAME_TYPE_KEY);
}

/// Any backend's SPS must carry the BT.709 / limited-range signalling that
/// `convert.rs` produced the samples with.
#[test]
fn every_working_backend_signals_bt709_limited_high() {
    let (w, h) = (640usize, 384usize);
    let fr = frames(w, h, 1);
    for b in backend_chain() {
        let Ok(mut enc) = open_backend(b, w as u32, h as u32, 60, 4000, None) else { continue };
        let pk = enc.encode(&fr[0]).unwrap();
        let info = stream::parse_sps_vui(&pk[0].data).expect("parsable SPS");
        assert_eq!((info.width, info.height), (640, 384), "{}", enc.backend_name());
        assert_eq!(info.profile_idc, 100, "{} should be High profile", enc.backend_name());
        assert_eq!(info.colour, Some((1, 1, 1)), "{} colour description", enc.backend_name());
        assert!(!info.full_range, "{} range", enc.backend_name());
    }
}

#[test]
fn set_bitrate_and_keyframe_request_work_on_every_backend() {
    let (w, h) = (640usize, 384usize);
    let fr = frames(w, h, 4);
    for b in backend_chain() {
        let Ok(mut enc) = open_backend(b, w as u32, h as u32, 60, 4000, None) else { continue };
        enc.encode(&fr[0]).unwrap();
        enc.encode(&fr[1]).unwrap();
        enc.set_bitrate(2000);
        let pk = enc.encode(&fr[2]).unwrap();
        assert!(!pk.is_empty(), "{}", enc.backend_name());
        let pk = enc.encode(&fr[3]).unwrap();
        assert!(!pk.is_empty(), "{}", enc.backend_name());
    }
}

/// Isolates the CPU colour-conversion cost from the encoder's own time.
#[test]
#[ignore]
fn bench_convert_only() {
    let (w, h) = (2400usize, 1088usize);
    let fr = frames(w, h, 20);
    let mut out = vec![0u8; convert::nv12_len(w, h)];
    let t = Instant::now();
    for f in &fr {
        convert::to_nv12(f.format, &f.data, w, h, &mut out).unwrap();
    }
    println!("BENCH convert {}x{} avg {:.2} ms", w, h, t.elapsed().as_secs_f64() * 1000.0 / 20.0);
}

#[test]
#[ignore]
fn bench_backends() {
    let dir = bench_dir();
    for (w, h) in [(2400usize, 1088usize), (1920, 1088)] {
        let fr = frames(w, h, FRAMES);
        for b in backend_chain() {
            let Ok(mut enc) = open_backend(b, w as u32, h as u32, 60, 20_000, None) else {
                println!("{}x{} {:?}: unavailable", w, h, b);
                continue;
            };
            let name = enc.backend_name();
            let mut times = Vec::with_capacity(FRAMES);
            let mut bytes = 0usize;
            let mut stream = Vec::new();
            for f in &fr {
                let t = Instant::now();
                let pk = enc.encode(f).expect("encode");
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                assert!(!pk.is_empty(), "{}: frame produced no packet", name);
                for p in pk {
                    bytes += p.data.len();
                    stream.extend_from_slice(&p.data);
                }
            }
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            let max = times.iter().cloned().fold(0.0, f64::max);
            println!(
                "BENCH {}x{} {:<11} avg {:6.2} ms  max {:6.2} ms  {:7.0} bytes/frame",
                w, h, name, avg, max, bytes as f64 / FRAMES as f64
            );
            let path = dir.join(format!("displayswarm_{}_{}x{}.h264", name, w, h));
            std::fs::File::create(&path).and_then(|mut f| f.write_all(&stream)).unwrap();
        }
    }
}
