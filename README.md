# DisplaySwarm

Use your Android phones or tablets as a second monitor/speaker for your Linux PC, over USB or Wi-Fi. Several devices can be connected at once.

[![Download AppImage](https://img.shields.io/badge/Download-AppImage-2ea44f?style=for-the-badge&logo=linux&logoColor=white)](https://github.com/ratan00/displayswarm/releases/latest)
[![Download APK](https://img.shields.io/badge/Download-Android%20APK-3ddc84?style=for-the-badge&logo=android&logoColor=white)](https://github.com/ratan00/displayswarm/releases/latest)

*Click a badge to open the latest release, then pick the `.AppImage` or `.apk` file under Assets.*

## Features

- Low-latency H.264 video (hardware encoding via VAAPI)
- USB (Android Open Accessory, no ADB needed) or Wi-Fi with TLS and PIN pairing
- Touch, multi-touch gestures and pressure/tilt stylus input
- Extend, mirror, or use the phone as your main screen
- Laptop audio on the phone, phone microphone to the laptop, synced across devices
- Clipboard sharing and file transfer
- Phone battery on the host, per-device settings
- Optional automatic screen scaling for the phone's monitor (KDE, GNOME); switch it off in the host or in the Android app settings

## Works fine on

- KDE Plasma 6 (Wayland)
- GNOME (Wayland). Don't use the display-switch key (F4) while streaming; it can crash gnome-shell (a mutter bug). Change roles in the app instead.
- X11 desktops (Cinnamon, XFCE, MATE, ...)(not 100%)

## Limited support

- COSMIC: partial(mirror only)
- Windows 10/11 host: new, lightly tested (see [Windows](#windows))
- Sway, Hyprland and other wlroots compositors: not supported yet

Tested so far on one machine, so other GPUs and drivers may need fixes.

## Install

Download the latest packages and the Android app from the **[Releases page](https://github.com/ratan00/displayswarm/releases)**:

| Platform | File |
|---|---|
| Arch / CachyOS / Manjaro | `displayswarm-*.pkg.tar.zst` (`sudo pacman -U file`) |
| Debian / Ubuntu | `displayswarm_*_amd64.deb` (`sudo apt install ./file`) |
| Any Linux | `DisplaySwarm-*.AppImage` (`chmod +x` and run) |
| Windows 10/11 (64-bit) | `DisplaySwarm-*-windows-x64.zip` (unzip and run `displayswarm.exe`) |
| Android 8+ | `DisplaySwarm-*.apk` |

The Android app checks the Releases page when you open its Settings and tells you when a newer version is out. It opens the download page; it never installs anything by itself.

Then start DisplaySwarm on the PC, open the app on the phone and connect by USB cable or pick the PC in the list over Wi-Fi. On Wi-Fi a firewall may block the phone; the host has an "Open firewall port" button for ufw/firewalld.

### USB permission (udev rule)

The `.pkg.tar.zst` and `.deb` packages install a udev rule that lets your logged-in user open a phone in Android accessory mode (`18d1:2d00`-`2d05`). Without it, USB only works while USB debugging is on, because systemd grants access only to the accessory modes that include an adb interface. With debugging off the host gets "permission denied" and never reaches the app.

The AppImage cannot install it, so create the rule once and replug the phone (this needs no copy of the repo):

```bash
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="18d1", ATTR{idProduct}=="2d0[0-5]", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-displayswarm-aoa.rules
sudo udevadm control --reload
```

The rule only grants access to the logged-in user while the phone is plugged in, and does not affect the phone's normal (MTP) mode. If USB still does not connect, the host log (`journalctl --user -u displayswarm-hostd`) says "permission denied opening the phone".

## Windows

Unzip `DisplaySwarm-*-windows-x64.zip` anywhere, keep the DLLs next to the `.exe` files and run `displayswarm.exe`; it starts the background host itself. Needs 64-bit Windows 10 (1903+) or 11. Nothing else to install; the zip carries FFmpeg's DLLs.

- **Encoder:** the first one that opens wins: NVENC, then Intel Quick Sync, AMD AMF, Media Foundation, and software libx264 as the last resort. A current GPU driver gives the lowest latency.
- **Firewall:** on Wi-Fi, Windows asks to allow `displayswarm-hostd.exe` the first time it listens. Allow it on private networks. USB needs no rule.
- **Extend mode (optional):** the phone as an extra monitor needs the Virtual Display Driver, installed once: `winget install --id=VirtualDrivers.Virtual-Display-Driver -e`. Mirror works without it.
- **Licence:** the zip bundles the GPL build of FFmpeg, so its licence text is included.

Build from source (PowerShell; needs MSVC build tools, LLVM for bindgen, and the BtbN `win64-gpl-shared` FFmpeg build n8.1 unpacked somewhere):

```powershell
$env:FFMPEG_DIR = 'C:\depsfmpeg'                   # contains bin\, lib\, include$env:LIBCLANG_PATH = 'C:\Program Files\LLVMin'
$env:PATH = "$env:FFMPEG_DIRin;$env:PATH"          # so the exes find the DLLs when run from cargo
cd host; cargo build --release                       # targetelease\displayswarm.exe, displayswarm-hostd.exe
cd ..; packaging\windows\package.ps1                 # the zip
```

## Screenshots

The host window on the PC: connected devices on the left, the selected device's role, live stats and settings on the right. It follows the desktop's light or dark theme.

![DisplaySwarm host window, light theme](docs/screenshots/host.png)

![DisplaySwarm host window, dark theme](docs/screenshots/host-dark.png)

The Android app, choosing what the device does (mirror, extend, mirror one window, drawing tablet, input pad):

![DisplaySwarm Android app, change role dialog](docs/screenshots/phone.jpg)

## Build from source

```bash
# Host (Rust, Slint UI). Needs ffmpeg, x264, pipewire, opus dev libraries and clang.
cd host && cargo build --release          # binaries: displayswarm, displayswarm-hostd

# Android app (JDK 17, Android SDK)
cd client-android && ./gradlew assembleDebug

# Packages
cd packaging/arch && makepkg -f           # Arch package
packaging/debian/build-deb.sh             # .deb
packaging/appimage/build-appimage.sh      # AppImage
```

Tests: `cargo test` in `host/`, `./gradlew testDebugUnitTest` in `client-android/`. Tagging `vX.Y.Z` builds and publishes every package through GitHub Actions.

## Help spread the word

If DisplaySwarm is useful to you, please star the repo and tell others. More fixes and desktop support are coming. Bug reports and pull requests are welcome.

## License

GPL-3.0. Third-party notices are in [NOTICE](NOTICE).
