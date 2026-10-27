DisplaySwarm for Windows
========================

Use an Android phone or tablet as a second display and input device.

Install
-------
1. Unzip this folder anywhere (for example C:\Program Files\DisplaySwarm or your
   Documents folder). Keep the .dll files next to the .exe files.
2. Run displayswarm.exe. It starts the background host (displayswarm-hostd.exe)
   by itself.
3. Open the DisplaySwarm app on the phone and connect by USB cable, or pick the
   PC in the list over Wi-Fi.

Wi-Fi and the firewall
----------------------
The first time the host listens on the network, Windows Defender Firewall asks
whether to allow it. Choose "Private networks" (and "Public networks" only if you
use a public network) and press Allow. If you dismissed the prompt, allow
displayswarm-hostd.exe under Windows Security > Firewall > Allow an app.
USB needs no firewall rule.

Video encoding
--------------
The host picks the first encoder that works on your PC, in this order:
NVIDIA NVENC, Intel Quick Sync, AMD AMF, Windows Media Foundation, then software
x264. A recent GPU driver gives the lowest latency.

Extend mode (optional)
----------------------
Mirroring works out of the box. To use the phone as an extra monitor ("Extend"),
install the Virtual Display Driver once, from a terminal:

    winget install --id=VirtualDrivers.Virtual-Display-Driver -e

Licence
-------
DisplaySwarm is GPL-3.0 (see LICENSE.txt). This package includes FFmpeg shared
libraries from the BtbN "win64-gpl-shared" build, which are licensed under the GPL
(FFmpeg-LICENSE.txt, and libx264 inside them is GPL too). Their source is at
https://ffmpeg.org/ and https://github.com/BtbN/FFmpeg-Builds. Other notices are in
NOTICE.txt.
