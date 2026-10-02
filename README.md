# DisplaySwarm: Virtual Secondary Display & Digitizer

Turn your Android device into a high-performance second monitor with ultra-low latency, 60/120 Hz refresh rates, and active stylus/pressure sensitivity (similar to SuperDisplay). Supports **Windows & Linux** hosts and **Android** clients.


---

## Architecture Overview

```
DisplaySwarm/
├── host/                    # Host Engine in Rust with Slint GUI
│   ├── Cargo.toml
│   ├── build.rs             # Slint UI compiler
│   ├── src/
│   │   ├── main.rs          # GUI and Tokio async runtime coordinator
│   │   ├── protocol/        # Protocol v2 (wire.rs) and the injector event model
│   │   ├── server.rs        # Video/input streaming loop
│   │   ├── transport/       # USB AOA (Android Open Accessory) & TCP transports
│   │   ├── capture/         # Screen capture (Wayland portal + PipeWire, X11, Windows DXGI)
│   │   ├── display/         # Virtual display provisioning (Linux vkms)
│   │   ├── encoder/         # H.264 video encoder (x264)
│   │   ├── input/           # Input injection (Linux /dev/uinput & Windows SyntheticPointer) and router.rs (protocol input -> injector events)
│   │   └── bin/aoa_probe.rs # USB AOA diagnostic tool
│   ├── tests/gnome_probe.rs # GNOME portal handshake probe
│   └── ui/
│       └── appwindow.slint  # Native UI (framerate, bitrate, status, port)
├── lamco-pipewire/          # Vendored PipeWire capture crate (patched)
├── client-android/          # Android Client Application
│   └── app/src/main/
│       ├── AndroidManifest.xml
│       ├── java/com/displayswarm/client/
│       │   ├── MainActivity.kt   # Fullscreen immersive activity & HUD
│       │   ├── AoaManager.kt     # USB accessory (AOA) connection
│       │   ├── Wire.kt           # Protocol v2 codec matching the host
│       │   ├── Protocol.kt       # Protocol constants
│       │   ├── VideoDecoder.kt   # Low-latency MediaCodec -> SurfaceView
│       │   ├── InputManager.kt   # Stylus pressure, tilt & touch capture
│       │   └── NetworkClient.kt  # Socket client & metrics tracking
│       └── res/
├── protocol/
│   └── v2-vectors.txt       # Golden protocol v2 bytes checked by both test suites
└── scripts/
    ├── test_protocol.py     # Standalone verification host
    ├── test_client.py       # Standalone verification client
    └── test_uinput.py       # Checks /dev/uinput permissions
```

---

## Protocol

Phone and host speak **protocol v2** over USB (AOA) or TCP. Every byte in both directions is a frame: a 10-byte header `"VM"` | version | channel | type | flags | length (big endian), then the payload. Messages live on six channels (control, input, audio, video, clipboard, file) and large messages are split into fragments, so input and control are never stuck behind a video keyframe. The framing, the message list and the version-mismatch behaviour (an app and host of different versions show each other a clear error) are defined in `host/src/protocol/wire.rs`; `protocol/v2-vectors.txt` holds the golden bytes both codecs are tested against. The old `scripts/test_*.py` helpers speak the retired v1 format.

---

## Quick Start

### 1. Building & Running the Host (Rust + Slint)

Ensure Rust is installed (`rustup default stable` or `sudo pacman -S rust cargo`).

```bash
cd host
cargo run --release
```

The Slint UI will launch:
* Choose your target FPS (30 - 120 FPS) and Bitrate (5 - 50 Mbps).
* Click **Start Server** (defaults to port `9999`).

### 2. Running the Android Client

Open `client-android` in Android Studio or build with Gradle:

```bash
cd client-android
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

### 3. Connecting via USB (AOA)

1. Connect your Android device via USB.
2. The host switches the phone into Android Open Accessory mode and the DisplaySwarm app opens automatically. Accept the USB accessory prompt on the phone.
3. On Wayland, accept the screen-share dialog on the desktop.

If the phone is not detected, run `cargo run --release --bin aoa_probe` in `host/` to diagnose.

### 4. Connecting via TCP (ADB Reverse)

As an alternative to AOA:
1. Enable USB Debugging on the Android device.
2. Reverse the port so the Android device can reach your host via `127.0.0.1:9999`:
   ```bash
   adb reverse tcp:9999 tcp:9999
   ```
3. Open **DisplaySwarm** on your Android device and tap **Connect**.

### Environment variables (Linux host)

| Variable | Purpose |
|----------|---------|
| `DISPLAYSWARM_BIND` | TCP bind address (default `127.0.0.1:9999`) |
| `DISPLAYSWARM_CAPTURE` | Capture backend: `auto` (default), `native`, `pipewire`, `x11`, `mock` |
| `DISPLAYSWARM_CAPTURE_OUTPUT` | Capture a specific output/monitor |
| `DISPLAYSWARM_RESET_RESTORE_TOKEN` | Discard the saved screen-share permission |
| `DISPLAYSWARM_RESTORE_TOKEN_PATH` | Custom location for the saved permission token |

---

## Stylus & Digitizer Support

The Android client captures:
* **Tool Type**: Automatic discrimination between finger touch, S-Pen/stylus tip, and eraser.
* **Pressure**: Full normalized float (0.0 to 1.0) mapped to 4096 levels on the host.
* **Tilt**: Stylus X and Y tilt angles for realistic brush/drawing behaviors.
* **Buttons**: Barrel buttons (primary & secondary).

On Linux, inputs are dispatched via `/dev/uinput` (creates a virtual tablet).  
On Windows, inputs are dispatched via Windows Synthetic Pointer API (`InjectSyntheticPointerInput`).
