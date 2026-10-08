// Linux only: the portal capturer does not exist elsewhere.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};
use displayswarm_host::capture::portal;
use displayswarm_host::capture::linux::NativePortalCapturer;
use displayswarm_host::capture::ScreenCapturer;

#[test]
fn probe_gnome_portal_handshake() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .is_test(true)
        .try_init()
        .ok();

    println!("=== STEP 1: Probing portal::request_screencast_node(None) ===");
    let start = Instant::now();
    let session_res = portal::request_screencast_node(None);
    println!("portal::request_screencast_node result in {:?}: {:?}", start.elapsed(), session_res.as_ref().map(|s| s.describe()));

    match session_res {
        Ok(session) => {
            println!("SUCCESS: Obtained screencast session!");
            println!("  Node ID: {}", session.node_id);
            println!("  Stream ID: {}", session.stream_id);
            println!("  Session Path: {}", session.session_path);
            println!("  Size: {:?}", session.size);
            println!("  Position: {:?}", session.position);
            println!("  Restricted FD present: {}", session.fd.is_some());
        }
        Err(e) => {
            println!("FAILED: portal::request_screencast_node returned error: {}", e);
        }
    }
}

#[test]
fn probe_gnome_native_portal_capturer() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .is_test(true)
        .try_init()
        .ok();

    println!("=== STEP 2: Probing NativePortalCapturer::new(1920, 1080) ===");
    let mut capturer = match NativePortalCapturer::new(1920, 1080) {
        Ok(c) => c,
        Err(e) => {
            println!("FAILED: NativePortalCapturer::new returned error: {}", e);
            return;
        }
    };

    println!("NativePortalCapturer initialized: {}", capturer.describe_source());

    // Loop for a few seconds to observe capture_frame ticks
    let start = Instant::now();
    let mut frame_count = 0;
    while start.elapsed() < Duration::from_secs(5) {
        match capturer.capture_frame() {
            Ok(frame) => {
                frame_count += 1;
                if frame_count == 1 || frame_count % 30 == 0 {
                    println!(
                        "Tick {frame_count}: Frame {}x{}, has_real_frames={}, has_failed={}",
                        frame.width, frame.height, capturer.has_real_frames(), capturer.has_failed()
                    );
                }
            }
            Err(e) => {
                println!("Frame capture returned error: {}", e);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("Probe finished. Total frames captured: {}, has_real_frames={}", frame_count, capturer.has_real_frames());
}
