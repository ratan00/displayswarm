# DisplaySwarm: review findings and improvement plan

Reviewed at commit `61a15e1` (release 0.3.1), 2026-11-16.

## How to read this

- **Part 1** is what the review found, ordered by how much it matters.
- **Part 2** is the plan for functionality that is advertised, half-built or absent.
- **Part 3** is smaller improvements to code that already works.
- **Part 4** is a suggested order of work.
- **Part 5** lists the sources. Each is marked *fetched* (read during this review) or *cited* (a stable
  reference I did not re-read; check it before relying on a detail).

### Scope and limits of this review

- This was a read-through of the source, not a test run. Nothing was built and no test suite was run, so
  every finding below is from reading code. The protocol, pairing, TLS, server session loop, services,
  IPC and Android build/manifest were read closely; the large platform files (`capture/linux.rs`,
  `input/linux.rs`, `display/vkms.rs`, `transport/aoa.rs`, `lamco-pipewire/`) were only skimmed.
- No finding here was reproduced on hardware. The security findings are reasoned from the code and
  should each get a test that demonstrates them before the fix is written.

### Overall impression

The code is in good shape for a 0.3 project: the wire protocol is documented and has golden vectors
shared by both sides (`protocol/v2-vectors.txt`), there are roughly 780 Rust test functions plus 19
Android unit-test files, privilege escalation is isolated (`display/privilege.rs`), and comments explain
why, not just what. The main gaps are (a) features the README or the protocol promise that are not
wired up, (b) one real weakness in PIN pairing, and (c) no CI that runs the tests.

---

## Part 1. Findings

### 1.1 Security

| # | Severity | Finding | Where |
|---|---|---|---|
| S1 | High on untrusted LANs | PIN pairing can be completed by an active man in the middle | `host/src/transport/pairing.rs:60-67`, `client-android/.../Pairing.kt:65-72` |
| S2 | Medium | Two phones of the same model un-pair each other | `host/src/transport/pairing.rs:193-194` |
| S3 | Medium | Pairing tokens on the phone are in plain SharedPreferences and included in backups | `client-android/.../WifiDiscovery.kt:108-111`, `AndroidManifest.xml:24` |
| S4 | Medium | Unauthenticated peers can hold memory and block pairing | `host/src/transport/pairing.rs:506-545`, `host/src/protocol/wire.rs:78-84` |
| S5 | Low | IPC socket fallback directory in `/tmp` is not ownership-checked | `host/src/ipc/transport.rs:86-98,139` |
| S6 | Low | Windows named pipe uses the default DACL (already marked UNVERIFIED) | `host/src/ipc/transport.rs:182` |
| S7 | Low | Release workflow pins actions by tag, pipes `curl` into `sh`, sets no default token permissions | `.github/workflows/release.yml` |
| S8 | Info | Security-critical doc comment is out of date | `host/src/server.rs:744-760` |

**S1. PIN pairing versus an active attacker.** The proof is `HMAC-SHA256(pin, context || fingerprint ||
device_id)` with a 6-digit PIN as the key. The module comment already notes that a captured proof can
be brute-forced offline. The consequence is larger than the comment suggests, because the attack works
in real time:

1. The phone has no pinned fingerprint on first contact (`Pin::Unpinned` / a null pin in
   `PinnedTls.kt`), and discovery is plain mDNS, which anyone on the LAN can answer.
2. An attacker advertises a look-alike host (or intercepts the connection), and at the same time opens
   its own connection to the real host and asks for a PIN. The host shows the PIN.
3. The user types the PIN into the phone. The phone sends `HMAC(pin, fp_attacker, id)` to the attacker.
4. The attacker tries all 1,000,000 PINs against that proof (well under a second), then sends
   `HMAC(pin, fp_host, id)` to the real host and is paired, receiving a permanent token.

The binding to the certificate fingerprint stops a *replay* of the proof, but not recovery of the PIN.
The result is a trusted device that can view the screen and inject input. QR pairing does not have this
weakness (128-bit secret and the fingerprint travel out of band), but QR pairing is not reachable from
either UI today (see F1).

Fix: replace the HMAC proof with a password-authenticated key exchange, which is the construction
designed for "prove knowledge of a short secret without giving an eavesdropper or impostor anything to
guess against". SPAKE2 is specified in RFC 9382; CPace is the CFRG-selected balanced PAKE. Bind the
PAKE transcript to the TLS session (the certificate fingerprint and a TLS exporter value, RFC 8446
section 7.5 / RFC 9266) so the derived key confirms both sides saw the same connection. With a PAKE
each wrong guess costs the attacker one online attempt, and the existing 3-attempt limit then actually
bounds the risk (3 in 1,000,000 per offer). Plan in F2.

**S2. Trust eviction by device name.** `TrustStore::add` drops every existing entry whose `name`
equals the new device's name, to clean up after a reinstall. The app sends `Build.MODEL` as the name
(`WifiDiscovery.kt:276`), so two tablets of the same model each remove the other's pairing whenever one
pairs. That contradicts "several devices can be connected at once". Fix: evict by `device_id` only;
handle stale entries by showing `paired_at`/last-seen in the trusted-devices list and letting the user
remove them (the IPC `revoke` call already exists in `daemon/wifi.rs`).

**S3. Token storage on the phone.** Known hosts (fingerprint plus token, hex) are written to a regular
SharedPreferences file, and the manifest sets `android:allowBackup="true"` with no backup rules.
Android's Auto Backup includes shared-preferences files by default, and the Auto Backup guide says to
exclude device-specific tokens. A restored backup on another device would carry a valid pairing token.
Fix: add `dataExtractionRules` (API 31+) and `fullBackupContent` (older) that exclude the known-hosts
and device-identity preference files, and wrap the token with an Android Keystore key so it is not
readable from a copied file.

**S4. Pre-authentication resource use.** `PairingManager::negotiate` reads with the general
`MessageReader`, so before authentication a peer may send fragments on the video or clipboard channel
and make the host buffer up to 16 MiB per connection (`max_message_len`). There is no cap on
concurrent unauthenticated connections and each may stay for `PAIRING_TIMEOUT` (150 s). Separately, the
attempt counter and lock-out are global: any LAN peer can burn the three attempts of a live offer and
trigger the 30 s lock-out repeatedly, or make a PIN dialog appear every 3 s. Fix: a pre-auth reader
that accepts only `PairRequest` on the control channel with a small size limit; a semaphore on
unauthenticated connections (for example 8 total, 2 per source address); keep the wrong-attempt count
per offer but apply lock-out per source address so one peer cannot block another.

**S5. `/tmp` socket fallback.** When `XDG_RUNTIME_DIR` is unset the socket lives in
`/tmp/displayswarm-<uid>/`. `DirBuilder::create` with `recursive(true)` succeeds if the directory
already exists, whoever owns it, so another local user can pre-create it and then serve a fake daemon
to the UI or read the UI's requests. Fix: after creating, `lstat` the directory and refuse to use it
unless it is a real directory owned by the current uid with mode `0700`; on the client side verify the
peer's uid with `SO_PEERCRED` before sending anything.

**S7. Release workflow.** GitHub's hardening guide recommends pinning third-party actions to a full
commit SHA and granting `GITHUB_TOKEN` the minimum permissions. The workflow uses `@v4`/`@v2` tags
(including the third-party `softprops/action-gh-release`), has no top-level `permissions:` block, and
installs Rust with `curl | sh`. The APK job has the signing keystore in its environment while running
Gradle with no dependency verification, so a compromised dependency could read it. Fix in F9.

**S8. Stale threat-model comment.** The doc comment above `parse_bind_spec` says the TCP transport has
no authentication. That was true before TLS and pairing; now the secured listener binds `0.0.0.0` by
default (`server.rs:871`). Rewrite the comment to describe the current model so the next reader does
not draw the wrong conclusion in either direction.

### 1.2 Advertised or half-built functionality that is not reachable

| # | What | State in the code |
|---|---|---|
| G1 | **File transfer** (README "Clipboard sharing and file transfer") | Wire messages exist (`FileOffer`, `FileChunk`, `FileControl`, `CH_FILE`) and are tested for encoding. No service handles them on the host, `FEATURE_FILES` is not in `HOST_FEATURES` (`services/mod.rs:39`), and the app never sends or receives them. |
| G2 | **QR pairing** | Host: `open_qr_offer`, `qr_svg`, `qr_text` are implemented and tested but called from nowhere (no IPC method, no UI). App: `QrPairing.parse` and `securityForQr` exist, but there is no scanner and no `displayswarm://` intent filter. |
| G3 | **HEVC / AV1** | The app advertises `CODEC_HEVC` when the phone has a decoder (`MainActivity.kt:591`); the host only has H.264 back ends (`encoder/ffmpeg.rs:25`) and never selects another codec. |
| G4 | **Clipboard on GNOME and X11** | The only back end is `WaylandClipboard` through data-control (`services/clipboard.rs:170`). Mutter does not implement `ext-data-control-v1` (wayland.app lists it as unsupported), and X11 sessions have no Wayland clipboard at all, so on two of the three "works fine" desktops clipboard sync silently does nothing. |
| G5 | **Wi-Fi Direct** | Detection only on both sides (`transport/mod.rs:180`, `WifiDiscovery.kt:313`); the manifest declares the feature. No connection flow. |
| G6 | **Bluetooth HID input mode** | `BluetoothHidInput` (242 lines, with tests and four manifest permissions) is never constructed. |
| G7 | **wlroots compositors (Sway, Hyprland)**, COSMIC extend | README lists them as unsupported / mirror only. |
| G8 | **Windows host** | README: "written, never tested". Several code paths are marked `UNVERIFIED`. |
| G9 | **Continuous integration** | The only workflow is `release.yml`, triggered by tags. No job runs `cargo test`, `clippy`, `./gradlew testDebugUnitTest` or lint on pushes or pull requests, so the golden-vector tests that keep the two protocol implementations in step run only when someone remembers to run them. |

### 1.3 Android platform currency

- `targetSdk = 34`, `compileSdk = 34`, AGP 8.2.2, Kotlin 1.9.22, Compose BOM 2024.02. If the app is
  ever published on Google Play, new apps and updates must target API 36 as of 31 August 2026. Outside
  Play this is not a hard block, but the behaviour changes below arrive anyway as devices update.
- **Orientation lock on tablets.** For apps targeting API 36, `screenOrientation` and
  `setRequestedOrientation()` are ignored on displays with smallest width >= 600 dp. The app depends on
  both (`AndroidManifest.xml:39`, `MainActivity.kt:262`) for its rotation lock, and tablets are a main
  use case. There is a temporary opt-out property (`PROPERTY_COMPAT_ALLOW_RESTRICTED_RESIZABILITY`)
  that stops working at API 37. The durable fix is to treat every window size change as a `Resize`
  (the protocol already supports it) and, when rotation lock is on, rotate the decoded picture inside
  the view instead of locking the activity.
- **Local network permission.** Android 16 introduces local-network protection, opt-in for now and
  stated to be enforced in a later release. It covers outgoing TCP to LAN addresses, UDP, and
  `NsdManager`, which is everything the Wi-Fi path uses. Plan for a runtime permission request with a
  clear explanation, and a USB-only fallback when it is denied.
- **Edge-to-edge** cannot be opted out of when targeting API 36; check the toolbar and the streaming
  surface against system bar and cutout insets.
- `android.enableJetifier=true` is unnecessary (no support-library dependencies) and slows builds.
- The launcher icon is a system drawable (`@android:drawable/ic_menu_slideshow`); it looks different
  per vendor and is not adaptive.
- The 16 KB page-size link flag is already set in `CMakeLists.txt`. Good.

### 1.4 Robustness and performance

- **Keyframe on every dropped frame** (`server.rs:1885-1892`). When the sender is behind, the whole
  frame is dropped and an IDR is forced. On a weak link an IDR is the largest thing that can be sent,
  so congestion causes more congestion. Options, in order of effort: rate-limit forced IDRs (one per
  N hundred ms); x264 periodic intra refresh for the software path; long-term-reference recovery where
  the hardware encoder supports it.
- **IPv4-only listener and mDNS records** (`server.rs:871`, `transport/mdns.rs`). Bind a dual-stack
  socket and include scoped link-local handling, or document IPv4 as a requirement.
- **Clipboard polling** every 500 ms spawns protocol round trips per session. With data-control a
  selection-change event is available; use it and fall back to polling.
- **`server.rs` is 4,456 lines** (about 1,300 of them tests) and holds the accept loop, session
  manager, pipeline threads and role logic. Split into `server/{accept,manager,session,pipeline}.rs`
  with tests beside each. This is a mechanical move that makes the later feature work easier to review.
- **Stale design notes**: `display/mod.rs:27-60` describes how `server.rs` "should" call the module as
  if it were owned elsewhere. Update or delete once verified against the current call sites.
- **Diagnostics**: there is a `daemon/diagnostics.rs`; add a one-click "copy diagnostics" in the host UI
  (desktop, session type, portal version, encoder back end, GPU, kernel) since the README says the
  project was tested on one machine and bug reports will need this.

---

## Part 2. Plan for missing functionality

Each item lists the goal, the design, the steps, and how to know it is done.

### F1. QR pairing end to end (closes G2, mitigates S1 immediately)

**Goal.** A user clicks "Add device" on the host, sees a QR code, scans it with the phone, and is
paired with no PIN and no first-contact trust gap.

**Host.**
1. IPC: add `wifi.open_qr` beside the existing PIN call in `daemon/wifi.rs`. It picks the address to
   embed (the same non-loopback IPv4 list `mdns::txt_records` builds; prefer the interface with the
   default route), calls `PairingManager::open_qr_offer`, and returns the payload, the SVG from
   `qr_svg`, and the time to live.
2. UI (`host/ui/appwindow.slint`, `ui_model.rs`): the "Add device" dialog shows the QR code as the
   primary option with the PIN underneath as the fallback. Slint can render the SVG through
   `slint::Image`; if SVG loading is awkward, render the `qrcode` matrix to a `SharedPixelBuffer`.
3. CLI: `displayswarm-hostd --pair-qr` prints `qr_text` for headless use.
4. Offer state today is a single slot, so opening a QR offer replaces a PIN offer. Keep one offer of
   each kind so the dialog can show both.

**App.**
1. Scanner. Two options:
   - Google code scanner (`play-services-code-scanner`): no camera permission, UI supplied by Play
     services. Smallest change, but depends on Play services, which de-Googled devices lack.
   - CameraX plus a bundled decoder (ZXing core): works everywhere, needs the `CAMERA` permission.
   Recommendation: bundle ZXing with CameraX, because a sideloaded GPL app should not require Play
   services; this is a judgement call the maintainer may reverse.
2. Deep link: add an intent filter for `displayswarm://pair` on `MainActivity` so the system camera app
   can hand the code over; route it to `QrPairing.parse` then `WifiHosts.securityForQr`.
3. On success the existing `onAuthenticated` path stores fingerprint and token.

**Done when.** A loopback test (extend `test_loopback_tls_pairing_then_trusted_reconnect`,
`server.rs:4359`) pairs with a QR secret through the IPC call; an Android unit test covers the deep
link; manual test on one phone.

### F2. PAKE-based PIN pairing (closes S1)

**Goal.** Knowing the PIN is necessary to pair, and seeing a pairing exchange gives an attacker nothing
to guess against offline.

**Design.**
- New pairing mode `PAIR_MODE_PAKE = 3`, leaving modes 0 to 2 intact. Message flow inside the existing
  `PairRequest`/`PairResponse` pair (the `credential` and `token` fields are already opaque bytes):
  1. Phone to host: PAKE message A.
  2. Host to phone: PAKE message B plus host key confirmation.
  3. Phone to host: phone key confirmation. Host replies `PAIR_OK` with the token.
- Password input: the PIN. Identities and transcript: both device id and host fingerprint, plus a TLS
  exporter value with a fixed label, so the confirmation fails if the two ends are on different TLS
  sessions.
- Capability signalling: add a bit to the mDNS TXT record and to the `PAIR_PIN_REQUIRED` response so
  the app knows the host supports mode 3. A new host refuses legacy `PAIR_MODE_PIN` proofs by default,
  with a setting to allow them for one release so old apps can still pair.
- Algorithm: SPAKE2 (RFC 9382) or CPace. Selection criteria: a maintained Rust implementation and a
  Kotlin/JVM implementation that pass the RFC test vectors. The RustCrypto `spake2` crate exists but
  its documentation states it has not been audited; on Android there is no platform API, so this likely
  means a small implementation over a vetted curve library. **Spike first** (two days): confirm
  interoperability against the RFC vectors on both sides before committing to either.

**Steps.** Spike; add vectors to `protocol/v2-vectors.txt`; host state machine in `pairing.rs`;
Kotlin mirror in `Pairing.kt`; an adversarial test that replays a recorded exchange with every PIN and
shows none verifies; update the module comment and README security note.

**Done when.** The adversarial test passes, the legacy proof is off by default, and both test suites
agree on the vectors.

### F3. File transfer (closes G1)

**Goal.** Send a file from PC to phone and phone to PC, with progress, cancel, and no effect on video.

**Protocol (already defined; fill in the semantics).**
- `FileOffer{id, name, size, mime}` then `FileControl{id, FILE_ACCEPT|FILE_REJECT}` from the receiver.
- `FileChunk{id, offset, data}` in order; chunk size at most 256 KiB so it fits the 1 MiB `CH_FILE`
  message cap with room to spare. The bulk queue already sends after video and applies backpressure
  (`ServiceOut::send_bulk`).
- Flow control: the bulk queue bounds the sender; add a receiver credit (`FileControl` with a new
  `FILE_ACK` op carrying nothing but the id, sent every N chunks) only if testing shows the phone's
  storage is the bottleneck.
- End: `FileControl{FILE_COMPLETE}` from the sender, including a SHA-256 in a trailing field of the
  offer or a final message so the receiver can verify. Trailing fields are allowed by the payload rules.
- Either side may send `FILE_CANCEL`. A session end cancels everything; partial files are deleted.

**Host (`services/files.rs`, new `SessionService`).**
1. Add `FEATURE_FILES` to `HOST_FEATURES` and a `files` toggle in `DeviceSettings`, off unless paired.
2. Receiving: sanitise the name (basename only, no path separators, no leading dot, length cap, replace
   control characters), write to `~/Downloads/DisplaySwarm/` (resolve through `xdg-user-dir`), create
   with `O_EXCL` and a ` (n)` suffix on collision, enforce the offered size, cap total size per
   transfer (configurable, default 2 GiB), write to `name.part` and rename on verified completion.
3. Sending: IPC method `files.send{device_id, path}`; UI gets a "Send file" button per device and
   accepts drag and drop onto the device row. Read on a blocking thread, `blocking_send_bulk` chunks.
4. Consent: an incoming offer raises a desktop notification with Accept/Decline (the tray already has
   `tray/notify.rs`); a per-device "always accept" setting for convenience.
5. Events for progress to the UI through the existing IPC event stream.

**App.**
1. Sending: a toolbar action that launches the system picker (`ACTION_OPEN_DOCUMENT`), and a share
   target (`ACTION_SEND` intent filter) so any app can share to the PC. Read through `ContentResolver`.
2. Receiving: write to the Downloads collection through `MediaStore` (API 29+; no storage permission
   needed for files the app creates), with a progress notification from the existing foreground
   service and a completion notification that opens the file.
3. Only offer `FEATURE_FILES` when the session is paired (or USB).

**Tests.** Golden vectors for the new trailing fields; a host test that round-trips a file through two
in-memory sessions; name-sanitisation table test including `../`, absolute paths, NUL, very long names
and reserved Windows names; cancel mid-transfer leaves no `.part` file; video keeps its frame rate
during a large transfer (extend `test_sender_interleaves_control_between_video_fragments`).

**Done when.** A 1 GiB file transfers both ways over USB and Wi-Fi with the stream still usable, and
the README claim is true.

### F4. Clipboard on GNOME and X11 (closes G4)

- **X11:** add an `X11Clipboard` back end on the existing `x11rb` dependency: own the `CLIPBOARD`
  selection to write, `XFixesSelectSelectionInput` to be told about changes (no polling), `INCR` for
  payloads over the request size limit. Select the back end from `display::env` session type.
- **GNOME Wayland:** Mutter does not offer data-control. Options: (a) run the X11 back end against
  Xwayland when `DISPLAY` is set, which works because Mutter bridges the two clipboards, but only while
  an Xwayland client is running; (b) the RemoteDesktop portal's clipboard support (portal version 2),
  which fits because the host already holds a portal session on GNOME. Start with (a) as the cheap
  path, and record (b) as the proper one after a spike confirms Mutter's portal back end implements it.
- Surface the result: `ServiceCtx` should report "clipboard unavailable on this desktop" to the UI and
  clear `FEATURE_CLIPBOARD` instead of negotiating a feature that does nothing.

### F5. HEVC, then AV1 (closes G3)

1. `encoder/ffmpeg.rs`: parameterise the back-end list by codec (`hevc_vaapi`, `hevc_nvenc`,
   `hevc_qsv`, `hevc_amf`; software `libx265` is too slow for this use, so HEVC is hardware only).
2. Negotiation: `HelloAck` already has a place for the chosen codec; pick HEVC when the phone offers
   it, the host has a hardware HEVC encoder, and the link is a network link (USB is constant-QP with
   bandwidth to spare, so H.264 stays the default there). Add a per-device override.
3. `encoder/stream.rs` parses H.264 SPS for the config packet; add the HEVC VPS/SPS/PPS equivalent.
4. App: `VideoDecoder` selects `video/hevc`; the size and low-latency vendor keys need re-checking per
   chipset (the Qualcomm key at `VideoDecoder.kt:278` is codec-specific in behaviour).
5. Android requires an HEVC decoder only at Main Profile Level 3 on handhelds, which is far below a
   tablet's panel resolution. `hasHevcDecoder()` (`MainActivity.kt:602`) only checks that some
   `video/hevc` decoder exists, software ones included; change it to require a hardware decoder whose
   `MediaCodecInfo.VideoCapabilities` supports the panel size at the session frame rate.
6. AV1 later, same shape: decoders are mandatory from Android 14, but hardware *encoders* on the PC are
   limited to recent GPUs, so gate strictly on a successful encoder probe.

**Done when.** `encoder/bench.rs` reports bitrate at equal PSNR for both codecs and a session on a
weak link visibly improves; fallback to H.264 is automatic when the HEVC encoder fails to open.

### F6. wlroots compositors and COSMIC (closes G7)

Capture (portal ScreenCast through `xdg-desktop-portal-wlr` / `-hyprland`) and input (uinput) should
already work for mirror. Extend needs a virtual output:

- **Hyprland:** `hyprctl output create headless [name]` and `hyprctl output remove <name>` are
  documented. Add `display/backends/hyprland.rs` implementing the existing backend trait: create, set
  mode and scale with `hyprctl keyword monitor`, capture that output, remove on session end and in
  `recover_leftovers`.
- **Sway:** `swaymsg create_output` exists in the source (added "for developer use", headless back end
  support added in 2020) but is not in the man pages, and removal is `output <name> unplug`. Treat as
  experimental: detect by running it and checking for a new `HEADLESS-n` in `swaymsg -t get_outputs`.
- **COSMIC:** check `AvailableSourceTypes` for the VIRTUAL bit (4) on the running portal, which the
  capability probe in `display/env.rs` already reads; if present, the GNOME-style portal virtual
  monitor path may work unchanged. If not, COSMIC stays mirror only until its portal adds it.
- Input mapping to the new output: reuse the region reported by the capturer.
- Detection: extend `Desktop` in `display/model.rs` (`Sway`, `Hyprland`, generic `Wlroots`) from
  `XDG_CURRENT_DESKTOP`, `SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`.

**Done when.** Extend works on one Sway and one Hyprland machine and `display/live_tests.rs` has an
ignored-by-default test for each.

### F7. Wi-Fi Direct and Bluetooth input: decide, then build or remove (G5, G6)

Both are partly present and unreachable, which costs permissions in the manifest and reader attention.

- **Bluetooth HID.** The class is complete enough to try. Add an "Input over Bluetooth" entry on the
  connect screen that registers the HID app and shows pairing instructions. It needs no host software,
  which makes it useful on machines where the host cannot be installed. If it is not wanted, delete the
  class, its test, and the four Bluetooth permissions.
- **Wi-Fi Direct.** A full flow needs NetworkManager's P2P D-Bus interface on the host, group
  negotiation, DHCP on the group interface, and the Android `WifiP2pManager` flow with its location or
  nearby-devices permission. That is a lot for a case USB tethering or the phone's hotspot already
  covers (the audio path comments mention hotspot use). Recommendation: remove the detection code and
  the manifest feature, and document "use the phone's hotspot or USB" instead.

### F8. Windows host (G8)

Not a coding task first; it is a verification task.
1. Add a `windows-latest` job that runs `cargo check` and `cargo test` (the `encoders` feature can be
   off, as `Cargo.toml` already allows) so it at least keeps compiling.
2. Work through the `UNVERIFIED` markers (`grep -rn UNVERIFIED host/src`) on a real machine with the
   Virtual Display Driver installed: connector order, rotation direction, multi-monitor input mapping.
3. Tighten the named-pipe DACL to the current user's SID (S6).
4. Packaging (MSI or winget manifest) only after 1 to 3.
Until then keep the README wording as it is.

### F9. Continuous integration and supply chain (closes G9, S7)

1. New `ci.yml` on `push` and `pull_request`:
   - Host: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --locked` in the same
     container the `.deb` job uses so system libraries match. Tests that need PipeWire or a compositor
     are already `#[ignore]`d (20 of them); keep them out of CI.
   - `lamco-pipewire`: `cargo test`.
   - Android: `./gradlew lintDebug testDebugUnitTest assembleDebug`.
   - `cargo deny check` or `cargo audit` against the RustSec advisory database.
2. Harden `release.yml`: top-level `permissions: contents: read`; pin every action to a commit SHA;
   install Rust from a pinned `rustup-init` with a checked SHA-256 instead of `curl | sh`; enable
   Gradle dependency verification (`gradle/verification-metadata.xml`) so the signing job cannot pull
   an altered artifact; make the release job depend on the CI workflow passing for that commit.
3. Dependabot (or Renovate) for cargo, gradle and actions.
4. Publish SHA-256 sums with each release, and build provenance attestations if wanted.

### F10. Android platform update (closes 1.3)

1. Toolchain: AGP, Gradle, Kotlin 2.x with the Compose compiler Gradle plugin (replacing
   `composeOptions.kotlinCompilerExtensionVersion`), current Compose BOM, `compileSdk`/`targetSdk` 36.
   Do this as its own change with no behaviour edits.
2. Then the behaviour work: orientation handling on large screens, edge-to-edge insets, predictive back
   (the toolbar and dialogs intercept back), local-network permission handling, backup rules (S3).
3. An adaptive launcher icon.

---

## Part 3. Smaller improvements

- `pairing.rs`: `ct_eq` is hand-rolled; `ring::constant_time` or the `subtle` crate is the usual
  choice. Not a bug as written.
- `pairing.rs:193`: see S2.
- `UpdateChecker.kt`: unauthenticated GitHub API calls are rate-limited per IP; cache the result for a
  day so opening Settings repeatedly behind a shared NAT does not exhaust it.
- systemd unit: add `NoNewPrivileges=` only if `pkexec` is not launched from the daemon (it is, for
  vkms and firewall, so do not add it blindly); `Documentation=` points at `https://github.com/` and
  should point at the repository.
- Two copies of the unit file (`host/dist/` and `packaging/`) differ only in `ExecStart`; generate one
  from the other.
- `.gitignore` lists `dist/` twice and ignores `dist/` while `host/dist/` is tracked; tidy.
- `scripts/test_protocol.py` blocks indefinitely when no host is running; give it a connect timeout and
  a clear message.
- README: state the security model in a short section (TLS with pinned certificate, pairing, what
  "Skip pairing" means, that USB sessions are not paired and rely on physical access plus Android's
  accessory prompt).
- Add `SECURITY.md` with a contact for reports, and `CONTRIBUTING.md` with the build and test commands.

---

## Part 4. Suggested order

| Phase | Items | Why first |
|---|---|---|
| 1 | F9 step 1 (CI), S2, S8, S4, S5 | Small, low risk, and CI protects everything after it |
| 2 | F1 (QR pairing), S3 (backup rules, Keystore) | Removes the first-contact gap for users who scan |
| 3 | F2 (PAKE) | Closes S1 for PIN users; needs the spike |
| 4 | F3 (file transfer), F4 (clipboard back ends) | Makes the README true |
| 5 | F10 (Android update), F9 steps 2 to 4 | Platform deadlines and release hardening |
| 6 | F5 (HEVC), keyframe policy | Quality on weak links |
| 7 | F6 (wlroots), F7 (decide), F8 (Windows) | Wider reach |

Open decisions for the maintainer: QR scanner library (F1), SPAKE2 versus CPace (F2), keep or remove
Bluetooth HID and Wi-Fi Direct (F7), and whether Google Play distribution is a goal (sets the urgency
of F10).

---

## Part 5. Sources

Fetched during this review:

- Google Play target API level requirements: <https://developer.android.com/google/play/requirements/target-sdk>
- Android 16 behaviour changes for apps targeting API 36 (orientation and resizability on large
  screens, edge-to-edge, predictive back, local network permission): <https://developer.android.com/about/versions/16/behavior-changes-16>
- Android Auto Backup (what is backed up, `allowBackup`, `dataExtractionRules`, guidance on tokens): <https://developer.android.com/identity/data/autobackup>
- Android supported media formats (HEVC and AV1 decoder requirements): <https://developer.android.com/media/platform/supported-formats>
- Google code scanner (ML Kit): <https://developers.google.com/ml-kit/vision/barcode-scanning/code-scanner>
- Android Gradle plugin release notes: <https://developer.android.com/build/releases/gradle-plugin>
- RFC 9382, SPAKE2: <https://www.rfc-editor.org/rfc/rfc9382.html>
- GitHub Actions secure use reference (SHA pinning, token permissions): <https://docs.github.com/en/actions/reference/security/secure-use>
- XDG Desktop Portal ScreenCast (source types, cursor modes, persist modes, version 6): <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
- `ext-data-control-v1` and its compositor support table: <https://wayland.app/protocols/ext-data-control-v1>
- Hyprland wiki, `hyprctl output create headless` (seen through search results, page not opened): <https://wiki.hypr.land/Configuring/Advanced-and-Cool/Using-hyprctl/>
- Sway `create_output`: not in sway(5); known from the source history only (seen through search
  results). `sway-output(5)`: <https://man.archlinux.org/man/extra/sway/sway-output.5.en>

Cited without re-reading (verify the specific detail before relying on it):

- RFC 8446 section 7.5, TLS 1.3 exporters: <https://www.rfc-editor.org/rfc/rfc8446.html#section-7.5>
- RFC 9266, channel bindings for TLS 1.3: <https://www.rfc-editor.org/rfc/rfc9266.html>
- CPace (CFRG draft): <https://datatracker.ietf.org/doc/draft-irtf-cfrg-cpace/>
- XDG Desktop Portal RemoteDesktop (clipboard support): <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
- Android Keystore: <https://developer.android.com/privacy-and-security/keystore>
- Android storage, documents and MediaStore: <https://developer.android.com/training/data-storage/shared/documents-files>, <https://developer.android.com/training/data-storage/shared/media>
- Android 16 KB page sizes: <https://developer.android.com/guide/practices/page-sizes>
- Android Open Accessory protocol: <https://source.android.com/docs/core/interaction/accessories/aoa>
- Compose compiler Gradle plugin: <https://developer.android.com/develop/ui/compose/compiler>
- Wi-Fi Direct on Android: <https://developer.android.com/develop/connectivity/wifi/wifi-direct>
- NetworkManager Wi-Fi P2P D-Bus interface: <https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Device.WifiP2P.html>
- Gradle dependency verification: <https://docs.gradle.org/current/userguide/dependency_verification.html>
- FFmpeg codecs documentation (VAAPI, NVENC, QSV encoders): <https://ffmpeg.org/ffmpeg-codecs.html>
- `unix(7)`, `SO_PEERCRED`: <https://man7.org/linux/man-pages/man7/unix.7.html>
- RustSec advisory database: <https://rustsec.org/>
