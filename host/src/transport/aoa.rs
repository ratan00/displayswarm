//! Android Open Accessory (AOA 2.0) transport on top of `nusb`.
//!
//! # Design
//!
//! Earlier revisions used `rusb` with one thread alternating a synchronous
//! `write_bulk` (2 ms timeout) with a synchronous `read_bulk` (2 ms timeout).
//! That capped throughput, added jitter, made partial-transfer byte counts
//! ambiguous on timeout, and fed an outbound queue that silently *evicted the
//! oldest chunk* when full, corrupting H.264 frames. The current design has no
//! IO threads of our own at all:
//!
//! * `nusb` runs its own event loop; each bulk endpoint keeps a queue of
//!   asynchronous transfers in the kernel (usbfs URBs / WinUSB overlapped IO).
//! * **OUT** ([`OutPipe`]): up to [`OUT_IN_FLIGHT`] transfers of at most
//!   [`BULK_BUFFER_SIZE`] bytes are in flight. `poll_write` accepts bytes only
//!   while there is a free slot and otherwise returns `Pending` (woken when a
//!   transfer completes). That is **backpressure**: bytes are never dropped or
//!   reordered; deciding to drop a whole frame is the caller's job.
//! * **IN** ([`InPipe`]): [`IN_IN_FLIGHT`] transfers are always pending, so
//!   input from the phone is delivered as soon as it arrives no matter how busy
//!   the OUT direction is (this also removes the old reader/writer contention
//!   where a blocked `read_bulk` starved the writer for its whole timeout).
//! * Both pipes are small state machines over the [`OutEndpoint`] /
//!   [`InEndpoint`] traits so they can be unit-tested with a fake endpoint.
//!
//! # Zero-length packets
//!
//! The device side (`f_accessory`) queues each read as a request of up to
//! `BULK_BUFFER_SIZE` (16384) bytes, which completes when that many bytes have
//! arrived *or* a short packet ends the transfer. A 16384 byte transfer
//! therefore needs no ZLP. A shorter transfer whose length is an exact
//! multiple of the endpoint's max packet size (512 on high speed, 1024 on
//! SuperSpeed) is NOT terminated by a short packet, so the device would keep
//! waiting for more data and the tail of a frame could sit there until the
//! next write. Neither libusb nor nusb ever add a ZLP on their own (nusb never
//! sets `USBDEVFS_URB_ZERO_PACKET`), so [`OutPipe`] explicitly submits an empty
//! transfer after such a chunk. The stream stays byte-exact because the device
//! side treats it as a stream, not as messages.
//!
//! # Disconnects
//!
//! An unplug or device error completes every pending transfer with an error.
//! The reader turns `Disconnected` into EOF and other errors into `io::Error`;
//! the writer returns the error on the current and every later call. Nothing
//! is spawned, so there is no background task or thread to wind down.
//! `nusb` cancels a dropped endpoint's pending transfers and releases the
//! interface once the last handle to it (both halves, via
//! [`UsbInterfaceHolder`]) is gone, so a later session can claim it again.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient, TransferError,
};
use nusb::{Device, DeviceInfo, Endpoint, Interface, MaybeFuture};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// Google Android Open Accessory (AOA 2.0) Constants
pub const GOOGLE_VID: u16 = 0x18D1;
pub const AOA_PID_ACCESSORY: u16 = 0x2D00;
pub const AOA_PID_ACCESSORY_ADB: u16 = 0x2D01;
pub const AOA_PID_AUDIO: u16 = 0x2D02;
pub const AOA_PID_AUDIO_ADB: u16 = 0x2D03;
pub const AOA_PID_ACCESSORY_AUDIO: u16 = 0x2D04;
pub const AOA_PID_ACCESSORY_AUDIO_ADB: u16 = 0x2D05;

pub const AOA_GET_PROTOCOL: u8 = 51;
pub const AOA_SEND_STRING: u8 = 52;
pub const AOA_START_ACCESSORY: u8 = 53;

pub const AOA_STRING_MANUFACTURER: u16 = 0;
pub const AOA_STRING_MODEL: u16 = 1;
pub const AOA_STRING_DESCRIPTION: u16 = 2;
pub const AOA_STRING_VERSION: u16 = 3;
pub const AOA_STRING_URI: u16 = 4;
pub const AOA_STRING_SERIAL: u16 = 5;

pub const USB_CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Matches `BULK_BUFFER_SIZE` in the Android `f_accessory` kernel driver, which
/// clamps reads to this and loops writes at this size. Larger transfers are
/// silently truncated, so every OUT transfer is at most this long.
pub const BULK_BUFFER_SIZE: usize = 16384;

/// Bulk OUT transfers kept in flight (4 x 16 KiB = 64 KiB on the wire).
pub const OUT_IN_FLIGHT: usize = 4;

/// Longest `poll_shutdown` waits for queued OUT data to drain.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Bulk IN transfers kept pending at all times.
pub const IN_IN_FLIGHT: usize = 4;

#[derive(Debug, Clone)]
pub struct AoaConfig {
    pub manufacturer: String,
    pub model: String,
    pub description: String,
    pub version: String,
    pub uri: String,
    pub serial: String,
}

impl Default for AoaConfig {
    fn default() -> Self {
        Self {
            manufacturer: "DisplaySwarm".to_string(),
            model: "DisplaySwarmDisplay".to_string(),
            description: "DisplaySwarm Virtual Display".to_string(),
            version: "1.0".to_string(),
            uri: "https://github.com/ratan00/displayswarm".to_string(),
            serial: "DISPLAYSWARM001".to_string(),
        }
    }
}

/// Information about a connected USB device
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub bus_id: String,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub is_accessory: bool,
}

/// Stable identity of the accessory-mode device, for remembering per-device
/// settings across sessions.
///
/// `serial` is the phone's own USB serial (`iSerialNumber`), NOT the
/// [`AoaConfig::serial`] string we send to it. It is `None` when the device
/// exposes none or it could not be read; then `bus_path` (physical port, e.g.
/// `1-2.3`) is the fallback key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AoaDeviceIdentity {
    pub serial: Option<String>,
    pub bus_path: String,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Checks whether a given VID and PID represent a device already in Google Accessory mode.
pub fn is_aoa_accessory(vid: u16, pid: u16) -> bool {
    vid == GOOGLE_VID && matches!(pid, 0x2D00..=0x2D05)
}

/// Formats a physical port path like the Linux sysfs name: `<bus>-<p1>.<p2>...`.
pub fn format_bus_path(bus_id: &str, port_chain: &[u8]) -> String {
    if port_chain.is_empty() {
        return bus_id.to_string();
    }
    let ports: Vec<String> = port_chain.iter().map(|p| p.to_string()).collect();
    format!("{}-{}", bus_id, ports.join("."))
}

fn identity_of(info: &DeviceInfo) -> AoaDeviceIdentity {
    AoaDeviceIdentity {
        serial: info.serial_number().map(str::to_owned),
        bus_path: format_bus_path(info.bus_id(), info.port_chain()),
        vendor_id: info.vendor_id(),
        product_id: info.product_id(),
    }
}

/// Step 1: Scan and check connected USB devices.
pub fn check_connected_devices() -> Result<Vec<UsbDeviceInfo>, nusb::Error> {
    Ok(nusb::list_devices()
        .wait()?
        .map(|d| UsbDeviceInfo {
            bus_id: d.bus_id().to_string(),
            address: d.device_address(),
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            is_accessory: is_aoa_accessory(d.vendor_id(), d.product_id()),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Control transfers (mode switch)
// ---------------------------------------------------------------------------

/// Target for vendor control requests.
///
/// nusb's device-level control transfers are not available on Windows (WinUSB
/// only issues them through a claimed interface), so there we claim the first
/// interface and send them through it. Elsewhere the device handle is used
/// directly, which avoids claiming (and detaching the kernel driver from) an
/// MTP/ADB interface just to switch modes.
struct ControlPort {
    #[cfg(not(target_os = "windows"))]
    device: Device,
    #[cfg(target_os = "windows")]
    interface: Interface,
}

impl ControlPort {
    #[cfg(not(target_os = "windows"))]
    async fn new(device: Device) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self { device })
    }

    #[cfg(target_os = "windows")]
    async fn new(device: Device) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let number = device
            .active_configuration()?
            .interfaces()
            .next()
            .map(|i| i.interface_number())
            .ok_or("device has no interfaces")?;
        let interface = device.claim_interface(number).await?;
        Ok(Self { interface })
    }

    async fn vendor_in(&self, request: u8, length: u16) -> Result<Vec<u8>, TransferError> {
        let req = ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value: 0,
            index: 0,
            length,
        };
        #[cfg(target_os = "windows")]
        return self.interface.control_in(req, USB_CONTROL_TIMEOUT).await;
        #[cfg(not(target_os = "windows"))]
        self.device.control_in(req, USB_CONTROL_TIMEOUT).await
    }

    async fn vendor_out(&self, request: u8, index: u16, data: &[u8]) -> Result<(), TransferError> {
        let req = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value: 0,
            index,
            data,
        };
        #[cfg(target_os = "windows")]
        return self.interface.control_out(req, USB_CONTROL_TIMEOUT).await;
        #[cfg(not(target_os = "windows"))]
        self.device.control_out(req, USB_CONTROL_TIMEOUT).await
    }
}

/// Step 2: Query an open USB device for AOA protocol support (Request 51).
async fn get_aoa_protocol(port: &ControlPort) -> Result<u16, TransferError> {
    let buf = port.vendor_in(AOA_GET_PROTOCOL, 2).await?;
    if buf.len() >= 2 {
        let version = u16::from_le_bytes([buf[0], buf[1]]);
        debug!("Device returned AOA protocol version: {}", version);
        Ok(version)
    } else {
        Err(TransferError::Fault)
    }
}

/// The NUL-terminated payload of a "send string" request (Request 52).
fn aoa_string_payload(value: &str) -> Vec<u8> {
    let mut data = value.as_bytes().to_vec();
    if !data.ends_with(&[0]) {
        data.push(0);
    }
    data
}

/// Step 4 result handling: the device drops off the bus while (or right after)
/// it acknowledges `ACCESSORY_START`, so most error kinds are the *expected*
/// outcome; only a malformed request is a real failure.
fn start_error_is_expected(err: &TransferError) -> bool {
    !matches!(err, TransferError::InvalidArgument)
}

/// Performs full AOA handshake on an open device:
/// 1. Get Protocol (51)
/// 2. Send 6 identifying strings (52)
/// 3. Start accessory (53)
async fn perform_aoa_handshake(
    port: &ControlPort,
    config: &AoaConfig,
) -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    let protocol = get_aoa_protocol(port).await?;
    if protocol < 1 {
        return Err("device does not support AOA".into());
    }
    info!("AOA protocol version {} detected on device", protocol);

    // Identifying strings: manufacturer, model, description, version, uri, serial
    let strings = [
        (AOA_STRING_MANUFACTURER, &config.manufacturer),
        (AOA_STRING_MODEL, &config.model),
        (AOA_STRING_DESCRIPTION, &config.description),
        (AOA_STRING_VERSION, &config.version),
        (AOA_STRING_URI, &config.uri),
        (AOA_STRING_SERIAL, &config.serial),
    ];
    for (index, value) in strings {
        port.vendor_out(AOA_SEND_STRING, index, &aoa_string_payload(value))
            .await?;
    }

    match port.vendor_out(AOA_START_ACCESSORY, 0, &[]).await {
        Ok(()) => {}
        // Re-enumeration often drops the device from the bus before it can ACK.
        Err(e) if start_error_is_expected(&e) => {
            debug!("Device disconnected upon switching to accessory mode (expected): {e}");
        }
        Err(e) => return Err(e.into()),
    }
    info!("Sent AOA switch command. Waiting for device re-enumeration...");
    Ok(protocol)
}

/// Scans connected USB devices and attempts to trigger AOA mode on any compatible device.
/// Returns true if an AOA switch command was sent.
async fn probe_and_switch_devices(devices: &[DeviceInfo], config: &AoaConfig) -> bool {
    for info in devices {
        // Hubs never speak AOA and opening them only produces permission noise.
        if info.class() == 0x09 {
            continue;
        }
        let Ok(device) = info.open().await else {
            continue;
        };
        let Ok(port) = ControlPort::new(device).await else {
            continue;
        };
        if let Ok(version) = get_aoa_protocol(&port).await {
            if version >= 1 {
                info!(
                    "Found compatible Android device (VID: {:04x}, PID: {:04x}) supporting AOA v{}",
                    info.vendor_id(),
                    info.product_id(),
                    version
                );
                if perform_aoa_handshake(&port, config).await.is_ok() {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Endpoint discovery
// ---------------------------------------------------------------------------

/// Picks a (bulk IN, bulk OUT) address pair out of an interface's endpoints.
fn pick_bulk_pair(endpoints: impl Iterator<Item = (u8, TransferType)>) -> Option<(u8, u8)> {
    let (mut ep_in, mut ep_out) = (None, None);
    for (addr, ty) in endpoints {
        if ty != TransferType::Bulk {
            continue;
        }
        if addr & 0x80 != 0 {
            ep_in.get_or_insert(addr);
        } else {
            ep_out.get_or_insert(addr);
        }
    }
    Some((ep_in?, ep_out?))
}

/// The ADB interface that accompanies the accessory interface in PIDs 2D01,
/// 2D03 and 2D05 (class 0xFF, subclass 0x42, protocol 1). It also has a bulk
/// pair but is not ours.
fn is_adb_interface(class: u8, subclass: u8, protocol: u8) -> bool {
    class == 0xFF && subclass == 0x42 && protocol == 0x01
}

/// Resolves `(interface, bulk IN, bulk OUT)` for the accessory interface.
fn find_accessory_interface(device: &Device) -> Option<(u8, u8, u8)> {
    let config = device.active_configuration().ok()?;
    for group in config.interfaces() {
        let alt = group.first_alt_setting();
        if is_adb_interface(alt.class(), alt.subclass(), alt.protocol()) {
            continue;
        }
        let eps = alt.endpoints().map(|e| (e.address(), e.transfer_type()));
        if let Some((ep_in, ep_out)) = pick_bulk_pair(eps) {
            return Some((group.interface_number(), ep_in, ep_out));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Pure chunking helpers
// ---------------------------------------------------------------------------

/// Bytes of `remaining` to put into the next OUT transfer.
fn next_chunk_len(remaining: usize) -> usize {
    remaining.min(BULK_BUFFER_SIZE)
}

/// Whether an OUT transfer of `len` bytes must be followed by a zero-length
/// packet to terminate it on the device side (see the module docs).
fn needs_zlp(len: usize, max_packet: usize) -> bool {
    max_packet > 0 && len > 0 && len < BULK_BUFFER_SIZE && len % max_packet == 0
}

/// IN transfer size: as close to [`BULK_BUFFER_SIZE`] as possible while being
/// the nonzero multiple of the packet size that nusb requires.
fn in_transfer_len(max_packet: usize) -> usize {
    if max_packet == 0 || max_packet > BULK_BUFFER_SIZE {
        return BULK_BUFFER_SIZE;
    }
    (BULK_BUFFER_SIZE / max_packet) * max_packet
}

// ---------------------------------------------------------------------------
// Throughput statistics
// ---------------------------------------------------------------------------

/// Logs throughput at debug level about once a second while traffic flows.
struct Meter {
    label: &'static str,
    window_bytes: u64,
    total_bytes: u64,
    window_start: Instant,
}

impl Meter {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            window_bytes: 0,
            total_bytes: 0,
            window_start: Instant::now(),
        }
    }

    fn add(&mut self, bytes: usize) {
        self.window_bytes += bytes as u64;
        self.total_bytes += bytes as u64;
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            debug!(
                "AOA {}: {:.2} MiB/s ({} bytes total)",
                self.label,
                self.window_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0),
                self.total_bytes
            );
            self.window_bytes = 0;
            self.window_start = Instant::now();
        }
    }
}

/// Sticky error: once a pipe fails it keeps failing the same way.
#[derive(Clone)]
struct StickyError {
    kind: io::ErrorKind,
    msg: String,
}

impl StickyError {
    fn of(e: &io::Error) -> Self {
        Self {
            kind: e.kind(),
            msg: e.to_string(),
        }
    }

    fn to_io(&self) -> io::Error {
        io::Error::new(self.kind, self.msg.clone())
    }
}

fn map_transfer_error(e: TransferError) -> io::Error {
    match e {
        // Distinct kind so the reader can report EOF for an unplug.
        TransferError::Disconnected => io::Error::new(io::ErrorKind::NotConnected, e),
        other => io::Error::from(other),
    }
}

// ---------------------------------------------------------------------------
// OUT pipe
// ---------------------------------------------------------------------------

/// Abstraction of a bulk OUT endpoint with a queue of in-flight transfers.
trait OutEndpoint {
    /// Transfers submitted and not yet reaped.
    fn pending(&self) -> usize;
    /// Queues one transfer carrying exactly `data` (possibly empty = ZLP).
    fn submit(&mut self, data: &[u8]);
    /// Reaps the oldest transfer. Only called while `pending() > 0`. Must
    /// register the waker when returning `Pending`. `Ok` means the whole
    /// transfer was sent; a partial transfer is reported as an error.
    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// Backpressured writer state machine over an [`OutEndpoint`].
struct OutPipe<E> {
    ep: E,
    max_in_flight: usize,
    max_packet: usize,
    error: Option<StickyError>,
    meter: Meter,
    /// Lengths of submitted, unreaped transfers (for stats).
    lens: VecDeque<usize>,
}

impl<E: OutEndpoint> OutPipe<E> {
    fn new(ep: E, max_in_flight: usize, max_packet: usize) -> Self {
        Self {
            ep,
            max_in_flight: max_in_flight.max(1),
            max_packet,
            error: None,
            meter: Meter::new("OUT"),
            lens: VecDeque::new(),
        }
    }

    /// Reaps every transfer that has completed; leaves the waker registered if
    /// some are still in flight.
    fn reap(&mut self, cx: &mut Context<'_>) {
        while self.error.is_none() && self.ep.pending() > 0 {
            match self.ep.poll_complete(cx) {
                Poll::Ready(Ok(())) => {
                    let len = self.lens.pop_front().unwrap_or(0);
                    self.meter.add(len);
                }
                Poll::Ready(Err(e)) => {
                    warn!("AOA bulk write error: {e}");
                    self.error = Some(StickyError::of(&e));
                }
                Poll::Pending => break,
            }
        }
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if let Some(e) = &self.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.reap(cx);
        if let Some(e) = &self.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if self.ep.pending() >= self.max_in_flight {
            // `reap` registered the waker on the oldest in-flight transfer.
            return Poll::Pending;
        }

        let n = next_chunk_len(buf.len());
        self.ep.submit(&buf[..n]);
        self.lens.push_back(n);
        if needs_zlp(n, self.max_packet) {
            self.ep.submit(&[]);
            self.lens.push_back(0);
        }
        Poll::Ready(Ok(n))
    }

    /// Completes when every submitted transfer has been acknowledged by the
    /// USB host controller, a real "sent" point for the caller.
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.reap(cx);
        if let Some(e) = &self.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if self.ep.pending() == 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

/// [`OutEndpoint`] over a real nusb bulk OUT endpoint.
struct NusbOut {
    ep: Endpoint<Bulk, Out>,
    /// Buffers returned by completed transfers, reused to avoid an mmap per
    /// transfer (zero-copy buffers on Linux).
    pool: Vec<Buffer>,
    /// Submitted length of each in-flight transfer, in submission order.
    submitted: VecDeque<usize>,
}

impl OutEndpoint for NusbOut {
    fn pending(&self) -> usize {
        self.ep.pending()
    }

    fn submit(&mut self, data: &[u8]) {
        let mut buf = self
            .pool
            .pop()
            .unwrap_or_else(|| self.ep.allocate(BULK_BUFFER_SIZE));
        buf.clear();
        buf.extend_from_slice(data);
        self.submitted.push_back(data.len());
        self.ep.submit(buf);
    }

    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let c = ready!(self.ep.poll_next_complete(cx));
        let expected = self.submitted.pop_front().unwrap_or(0);
        let result = match c.status {
            Err(e) => Err(map_transfer_error(e)),
            // A short OUT transfer without an error would leave the stream
            // desynchronised; treat it as fatal rather than guess.
            Ok(()) if c.actual_len != expected => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "short bulk OUT transfer: {} of {} bytes",
                    c.actual_len, expected
                ),
            )),
            Ok(()) => Ok(()),
        };
        if self.pool.len() < OUT_IN_FLIGHT + 1 {
            self.pool.push(c.buffer);
        }
        Poll::Ready(result)
    }
}

// ---------------------------------------------------------------------------
// IN pipe
// ---------------------------------------------------------------------------

/// Abstraction of a bulk IN endpoint with a queue of pending transfers.
trait InEndpoint {
    type Buf: Deref<Target = [u8]>;
    fn pending(&self) -> usize;
    /// Queues one more receive transfer.
    fn submit(&mut self);
    /// Reaps the oldest transfer (only called while `pending() > 0`).
    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Self::Buf>>;
    /// Gives a consumed buffer back for reuse.
    fn recycle(&mut self, buf: Self::Buf);
}

/// Ordered reader state machine over an [`InEndpoint`].
struct InPipe<E: InEndpoint> {
    ep: E,
    depth: usize,
    current: Option<(E::Buf, usize)>,
    eof: bool,
    error: Option<StickyError>,
    meter: Meter,
}

impl<E: InEndpoint> InPipe<E> {
    fn new(ep: E, depth: usize) -> Self {
        let mut pipe = Self {
            ep,
            depth: depth.max(1),
            current: None,
            eof: false,
            error: None,
            meter: Meter::new("IN"),
        };
        pipe.refill();
        pipe
    }

    fn refill(&mut self) {
        while self.ep.pending() < self.depth {
            self.ep.submit();
        }
    }

    fn poll_read(&mut self, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some((buf, off)) = &mut self.current {
                let n = (buf.len() - *off).min(out.remaining());
                out.put_slice(&buf[*off..*off + n]);
                *off += n;
                if *off >= buf.len() {
                    if let Some((done, _)) = self.current.take() {
                        self.ep.recycle(done);
                    }
                }
                return Poll::Ready(Ok(()));
            }
            if let Some(e) = &self.error {
                return Poll::Ready(Err(e.to_io()));
            }
            if self.eof || self.ep.pending() == 0 {
                return Poll::Ready(Ok(()));
            }
            match ready!(self.ep.poll_complete(cx)) {
                Ok(buf) => {
                    // Replace the consumed transfer straight away so the queue
                    // stays full while the caller drains this buffer.
                    self.refill();
                    if buf.is_empty() {
                        // Zero-length packet from the device: not EOF.
                        self.ep.recycle(buf);
                        continue;
                    }
                    self.meter.add(buf.len());
                    self.current = Some((buf, 0));
                }
                Err(e) if e.kind() == io::ErrorKind::NotConnected => {
                    debug!("AOA bulk read terminated: device disconnected");
                    self.eof = true;
                }
                Err(e) => {
                    warn!("AOA bulk read error: {e}");
                    self.error = Some(StickyError::of(&e));
                }
            }
        }
    }
}

/// [`InEndpoint`] over a real nusb bulk IN endpoint.
struct NusbIn {
    ep: Endpoint<Bulk, In>,
    transfer_len: usize,
    pool: Vec<Buffer>,
}

impl InEndpoint for NusbIn {
    type Buf = Buffer;

    fn pending(&self) -> usize {
        self.ep.pending()
    }

    fn submit(&mut self) {
        let mut buf = self
            .pool
            .pop()
            .unwrap_or_else(|| self.ep.allocate(self.transfer_len));
        buf.clear();
        self.ep.submit(buf);
    }

    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Buffer>> {
        let c = ready!(self.ep.poll_next_complete(cx));
        Poll::Ready(match c.status {
            Ok(()) => Ok(c.buffer),
            Err(e) => Err(map_transfer_error(e)),
        })
    }

    fn recycle(&mut self, buf: Buffer) {
        if self.pool.len() < IN_IN_FLIGHT + 1 {
            self.pool.push(buf);
        }
    }
}

// ---------------------------------------------------------------------------
// Public stream types
// ---------------------------------------------------------------------------

/// RAII wrapper that keeps the claimed interface alive while either half
/// exists, and logs its release. nusb releases the interface when the last
/// handle (this, or an endpoint) is dropped, which is what lets a later
/// session claim it again.
struct UsbInterfaceHolder {
    _interface: Interface,
    number: u8,
    alive: Arc<AtomicBool>,
    /// Registered in [`CLAIMED_PATHS`] for as long as this holder lives.
    bus_path: String,
}

impl Drop for UsbInterfaceHolder {
    fn drop(&mut self) {
        debug!("Releasing USB interface {}", self.number);
        self.alive.store(false, Ordering::SeqCst);
        claimed_paths().remove(&self.bus_path);
    }
}

/// Bus paths of the accessories this process currently holds a claimed
/// interface on.
///
/// The server re-probes USB every 1.5 s for new phones. Without this set, every
/// probe tried to claim the interface an active session already owns, failed
/// with EBUSY, and logged an error each time. It is also what lets several
/// phones be attached at once: the probe skips the ones already in use and
/// picks up the next.
static CLAIMED_PATHS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

fn claimed_paths() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    CLAIMED_PATHS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Lets the owner of a session reset the accessory's USB device.
///
/// Used when a phone is in accessory mode but the DisplaySwarm app is not running
/// (for example the host restarted while the phone stayed in accessory mode,
/// or the user closed the app): nothing will ever send a ClientHello, and the
/// phone will not re-launch the app on its own because, from its point of view,
/// the accessory never went away. A USB reset makes it re-enumerate in its
/// normal mode; the next probe switches it back to accessory mode, and that
/// fresh `USB_ACCESSORY_ATTACHED` is what makes Android start the app again.
#[derive(Clone)]
pub struct AoaResetHandle {
    device: Device,
}

impl AoaResetHandle {
    pub async fn reset(&self) {
        match self.device.reset().await {
            Ok(()) => info!("Reset the AOA device so it re-enumerates and relaunches the app"),
            Err(e) => warn!("AOA device reset failed: {e}"),
        }
    }
}

/// Asynchronous reader for AOA bulk IN endpoint
pub struct AoaReader {
    pipe: InPipe<NusbIn>,
    _holder: Arc<UsbInterfaceHolder>,
}

impl AsyncRead for AoaReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // A disconnect yields EOF (Ok with nothing filled); other errors surface.
        self.pipe.poll_read(cx, buf)
    }
}

/// Asynchronous writer for AOA bulk OUT endpoint.
///
/// `poll_write` applies backpressure (`Pending`) when the in-flight window is
/// full and never drops or reorders bytes. `poll_flush` resolves once every
/// submitted transfer has completed.
pub struct AoaWriter {
    pipe: OutPipe<NusbOut>,
    /// Bounds how long `poll_shutdown` waits for in-flight data to drain.
    shutdown_timer: Option<Pin<Box<tokio::time::Sleep>>>,
    _holder: Arc<UsbInterfaceHolder>,
}

impl AsyncWrite for AoaWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pipe.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pipe.poll_flush(cx)
    }

    /// Drains in-flight data so the drop that follows does not cancel it, but
    /// never blocks for long: if the phone has stopped reading (app closed,
    /// cable wedged) the transfers would never complete and the caller's
    /// teardown would hang with the interface still claimed. A failed pipe has
    /// nothing left to flush, so that is success too.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        match this.pipe.poll_flush(cx) {
            Poll::Ready(_) => Poll::Ready(Ok(())),
            Poll::Pending => {
                let timer = this.shutdown_timer.get_or_insert_with(|| {
                    Box::pin(tokio::time::sleep(SHUTDOWN_DRAIN_TIMEOUT))
                });
                if timer.as_mut().poll(cx).is_ready() {
                    debug!("AOA shutdown: gave up draining in-flight transfers");
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Bidirectional AOA raw USB stream supporting Tokio AsyncRead and AsyncWrite
pub struct AoaStream {
    reader: AoaReader,
    writer: AoaWriter,
    alive: Arc<AtomicBool>,
    identity: AoaDeviceIdentity,
    device: Device,
}

impl AoaStream {
    fn new(
        device: Device,
        interface: Interface,
        number: u8,
        endpoint_in: u8,
        endpoint_out: u8,
        identity: AoaDeviceIdentity,
    ) -> Result<Self, nusb::Error> {
        let ep_in = interface.endpoint::<Bulk, In>(endpoint_in)?;
        let ep_out = interface.endpoint::<Bulk, Out>(endpoint_out)?;
        let in_mps = ep_in.max_packet_size();
        let out_mps = ep_out.max_packet_size();

        let alive = Arc::new(AtomicBool::new(true));
        let holder = Arc::new(UsbInterfaceHolder {
            _interface: interface,
            number,
            alive: alive.clone(),
            bus_path: identity.bus_path.clone(),
        });

        let reader = AoaReader {
            pipe: InPipe::new(
                NusbIn {
                    ep: ep_in,
                    transfer_len: in_transfer_len(in_mps),
                    pool: Vec::new(),
                },
                IN_IN_FLIGHT,
            ),
            _holder: holder.clone(),
        };
        let writer = AoaWriter {
            shutdown_timer: None,
            pipe: OutPipe::new(
                NusbOut {
                    ep: ep_out,
                    pool: Vec::new(),
                    submitted: VecDeque::new(),
                },
                OUT_IN_FLIGHT,
                out_mps,
            ),
            _holder: holder,
        };
        Ok(Self {
            reader,
            writer,
            alive,
            identity,
            device,
        })
    }

    /// A handle that can reset this device later, even after the stream has
    /// been split. See [`AoaResetHandle`].
    pub fn reset_handle(&self) -> AoaResetHandle {
        AoaResetHandle { device: self.device.clone() }
    }

    /// Split stream into separate async reader and writer halves.
    ///
    /// The interface stays claimed until BOTH halves are dropped.
    pub fn into_split(self) -> (AoaReader, AoaWriter) {
        (self.reader, self.writer)
    }

    /// True until both halves have been dropped.
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Identity of the accessory-mode device.
    pub fn identity(&self) -> &AoaDeviceIdentity {
        &self.identity
    }

    /// The phone's USB serial number, if it exposes one.
    pub fn serial(&self) -> Option<String> {
        self.identity.serial.clone()
    }

    /// Physical port path such as `1-2.3`.
    pub fn bus_path(&self) -> &str {
        &self.identity.bus_path
    }
}

impl AsyncRead for AoaStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl AsyncWrite for AoaStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

/// Connects to a Google Accessory:
/// 1. Checks if accessory device already exists on bus.
/// 2. If not, probes devices and sends AOA handshake (51, 52, 53) to switch device into accessory mode.
/// 3. Waits (by the caller retrying) for re-enumeration as Google Accessory (VID 0x18D1, PID 0x2D00..=0x2D05).
/// 4. Opens bulk endpoints and returns initialized `AoaStream`.
///
/// `Ok(None)` means "not ready yet, call again".
pub async fn connect_aoa_device(
    config: &AoaConfig,
) -> Result<Option<AoaStream>, Box<dyn std::error::Error + Send + Sync>> {
    let devices: Vec<DeviceInfo> = nusb::list_devices().await?.collect();

    // 1. Check if already enumerated as accessory
    let accessories: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| is_aoa_accessory(d.vendor_id(), d.product_id()))
        .collect();
    let unclaimed = {
        let claimed = claimed_paths();
        accessories
            .iter()
            .copied()
            .find(|d| !claimed.contains(&identity_of(d).bus_path))
    };
    if let Some(info) = unclaimed {
        return open_accessory_stream(info).await;
    }
    if !accessories.is_empty() {
        // Every accessory on the bus already belongs to a live session. Do not
        // probe other devices either: switching a phone that is mid-session
        // would be wrong, and a new phone will show up as its own accessory.
        return Ok(None);
    }

    // 2. Probe and initiate handshake
    if probe_and_switch_devices(&devices, config).await {
        info!("AOA handshake initiated. Waiting for re-enumeration on next tick...");
    }

    Ok(None)
}

async fn open_accessory_stream(
    info: &DeviceInfo,
) -> Result<Option<AoaStream>, Box<dyn std::error::Error + Send + Sync>> {
    let device = info.open().await?;
    let Some((iface, ep_in, ep_out)) = find_accessory_interface(&device) else {
        return Ok(None);
    };
    info!(
        "Found Google Accessory (PID: {:04x}) on iface {}, Bulk IN: 0x{:02x}, Bulk OUT: 0x{:02x}",
        info.product_id(),
        iface,
        ep_in,
        ep_out
    );

    // On Linux this also detaches any kernel driver bound to the interface;
    // elsewhere it is a plain claim.
    let interface = device.detach_and_claim_interface(iface).await?;
    info!("Successfully claimed AOA interface {}", iface);

    let identity = identity_of(info);
    claimed_paths().insert(identity.bus_path.clone());
    info!(
        "AOA device identity: serial={:?} path={}",
        identity.serial, identity.bus_path
    );
    // On failure the path must not stay registered, or the device would be
    // skipped forever.
    let path = identity.bus_path.clone();
    match AoaStream::new(device, interface, iface, ep_in, ep_out, identity) {
        Ok(stream) => Ok(Some(stream)),
        Err(e) => {
            claimed_paths().remove(&path);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    #[test]
    fn test_is_aoa_accessory() {
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_ACCESSORY));
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_ACCESSORY_ADB));
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_AUDIO));
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_AUDIO_ADB));
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_ACCESSORY_AUDIO));
        assert!(is_aoa_accessory(GOOGLE_VID, AOA_PID_ACCESSORY_AUDIO_ADB));

        // Non-accessory Google devices (e.g. ADB, MTP)
        assert!(!is_aoa_accessory(GOOGLE_VID, 0x4EE7));
        assert!(!is_aoa_accessory(GOOGLE_VID, 0x4EE1));

        // Other vendor IDs
        assert!(!is_aoa_accessory(0x04e8, 0x2D00));
        assert!(!is_aoa_accessory(0x18d2, 0x2D00));
    }

    #[test]
    fn test_aoa_config_defaults() {
        let config = AoaConfig::default();
        assert_eq!(config.manufacturer, "DisplaySwarm");
        assert_eq!(config.model, "DisplaySwarmDisplay");
        assert_eq!(config.description, "DisplaySwarm Virtual Display");
        assert_eq!(config.version, "1.0");
        assert_eq!(config.uri, "https://github.com/ratan00/displayswarm");
        assert_eq!(config.serial, "DISPLAYSWARM001");
    }

    #[test]
    fn test_aoa_constants() {
        assert_eq!(AOA_GET_PROTOCOL, 51);
        assert_eq!(AOA_SEND_STRING, 52);
        assert_eq!(AOA_START_ACCESSORY, 53);

        assert_eq!(AOA_STRING_MANUFACTURER, 0);
        assert_eq!(AOA_STRING_MODEL, 1);
        assert_eq!(AOA_STRING_DESCRIPTION, 2);
        assert_eq!(AOA_STRING_VERSION, 3);
        assert_eq!(AOA_STRING_URI, 4);
        assert_eq!(AOA_STRING_SERIAL, 5);
    }

    #[test]
    fn test_check_connected_devices() {
        // Must not panic; the result depends on the machine (no USB is fine).
        let _ = check_connected_devices();
    }

    #[test]
    fn test_string_payload_is_nul_terminated_once() {
        assert_eq!(aoa_string_payload("ab"), b"ab\0".to_vec());
        assert_eq!(aoa_string_payload("ab\0"), b"ab\0".to_vec());
    }

    #[test]
    fn test_start_error_classification() {
        assert!(start_error_is_expected(&TransferError::Disconnected));
        assert!(start_error_is_expected(&TransferError::Stall));
        assert!(!start_error_is_expected(&TransferError::InvalidArgument));
    }

    #[test]
    fn test_bus_path_format() {
        assert_eq!(format_bus_path("1", &[2, 3]), "1-2.3");
        assert_eq!(format_bus_path("3", &[4]), "3-4");
        assert_eq!(format_bus_path("1", &[]), "1");
    }

    #[test]
    fn test_pick_bulk_pair() {
        use TransferType::*;
        let eps = [(0x83, Interrupt), (0x81, Bulk), (0x02, Bulk)];
        assert_eq!(pick_bulk_pair(eps.into_iter()), Some((0x81, 0x02)));
        let no_out = [(0x81, Bulk)];
        assert_eq!(pick_bulk_pair(no_out.into_iter()), None);
        let iso = [(0x81, Isochronous), (0x02, Bulk)];
        assert_eq!(pick_bulk_pair(iso.into_iter()), None);
    }

    #[test]
    fn test_adb_interface_detection() {
        assert!(is_adb_interface(0xFF, 0x42, 0x01));
        assert!(!is_adb_interface(0xFF, 0x00, 0x00));
    }

    #[test]
    fn test_chunking_helpers() {
        assert_eq!(next_chunk_len(1), 1);
        assert_eq!(next_chunk_len(16384), 16384);
        assert_eq!(next_chunk_len(100_000), 16384);

        // Short multiples of the packet size need a ZLP; full chunks and
        // non-multiples do not.
        assert!(needs_zlp(512, 512));
        assert!(needs_zlp(1024, 512));
        assert!(needs_zlp(16384 - 512, 512));
        assert!(!needs_zlp(16384, 512));
        assert!(!needs_zlp(16384, 1024));
        assert!(!needs_zlp(513, 512));
        assert!(!needs_zlp(0, 512));
        assert!(!needs_zlp(512, 0));

        assert_eq!(in_transfer_len(512), 16384);
        assert_eq!(in_transfer_len(1024), 16384);
        assert_eq!(in_transfer_len(64), 16384);
        assert_eq!(in_transfer_len(0), 16384);
        assert_eq!(in_transfer_len(3000), 15000);
    }

    // ---- fake endpoints --------------------------------------------------

    struct CountWaker(AtomicUsize);
    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountWaker>, Waker) {
        let c = Arc::new(CountWaker(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    #[derive(Default)]
    struct FakeOut {
        /// In-flight transfers, oldest first.
        queue: VecDeque<Vec<u8>>,
        /// Transfers the "device" has finished and that may be reaped.
        ready: usize,
        /// Everything completed, in order.
        sent: Vec<Vec<u8>>,
        fail: Option<io::ErrorKind>,
        waker: Option<Waker>,
    }

    impl FakeOut {
        fn complete(&mut self, n: usize) {
            self.ready += n;
            if let Some(w) = self.waker.take() {
                w.wake();
            }
        }
    }

    impl OutEndpoint for FakeOut {
        fn pending(&self) -> usize {
            self.queue.len()
        }
        fn submit(&mut self, data: &[u8]) {
            self.queue.push_back(data.to_vec());
        }
        fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Some(kind) = self.fail {
                self.queue.pop_front();
                return Poll::Ready(Err(io::Error::new(kind, "fake failure")));
            }
            if self.ready > 0 {
                self.ready -= 1;
                let t = self.queue.pop_front().unwrap();
                self.sent.push(t);
                Poll::Ready(Ok(()))
            } else {
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn out_pipe(max_in_flight: usize, mps: usize) -> OutPipe<FakeOut> {
        OutPipe::new(FakeOut::default(), max_in_flight, mps)
    }

    #[test]
    fn test_out_chunks_large_write_to_16k() {
        let mut pipe = out_pipe(8, 512);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let data = vec![7u8; 40_000];
        let mut off = 0;
        while off < data.len() {
            match pipe.poll_write(&mut cx, &data[off..]) {
                Poll::Ready(Ok(n)) => {
                    assert!(n <= BULK_BUFFER_SIZE);
                    off += n;
                }
                other => panic!("unexpected {:?}", other.map(|r| r.is_ok())),
            }
        }
        let sizes: Vec<usize> = pipe.ep.queue.iter().map(|t| t.len()).collect();
        // 16384 + 16384 + 7232 (7232 = 14.125 * 512, no ZLP)
        assert_eq!(sizes, vec![16384, 16384, 7232]);
    }

    #[test]
    fn test_out_backpressure_and_wake() {
        let mut pipe = out_pipe(4, 512);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let chunk = vec![1u8; 16384];

        for _ in 0..4 {
            assert!(matches!(pipe.poll_write(&mut cx, &chunk), Poll::Ready(Ok(16384))));
        }
        // Window full: no bytes accepted, nothing dropped.
        assert!(pipe.poll_write(&mut cx, &chunk).is_pending());
        assert_eq!(pipe.ep.queue.len(), 4);
        assert_eq!(count.0.load(Ordering::SeqCst), 0);

        // A completion wakes the writer and frees exactly one slot.
        pipe.ep.complete(1);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(matches!(pipe.poll_write(&mut cx, &chunk), Poll::Ready(Ok(16384))));
        assert!(pipe.poll_write(&mut cx, &chunk).is_pending());
        assert_eq!(pipe.ep.sent.len(), 1);
    }

    #[test]
    fn test_out_preserves_order_and_bytes() {
        let mut pipe = out_pipe(2, 512);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut off = 0;
        while off < data.len() {
            match pipe.poll_write(&mut cx, &data[off..]) {
                Poll::Ready(Ok(n)) => off += n,
                Poll::Pending => pipe.ep.complete(1),
                Poll::Ready(Err(e)) => panic!("{e}"),
            }
        }
        let n = pipe.ep.queue.len();
        pipe.ep.complete(n);
        assert!(pipe.poll_flush(&mut cx).is_ready());
        let all: Vec<u8> = pipe.ep.sent.concat();
        assert_eq!(all, data);
    }

    #[test]
    fn test_out_zlp_after_short_packet_multiple() {
        let mut pipe = out_pipe(8, 512);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(pipe.poll_write(&mut cx, &[0u8; 1024]), Poll::Ready(Ok(1024))));
        assert!(matches!(pipe.poll_write(&mut cx, &[0u8; 100]), Poll::Ready(Ok(100))));
        let sizes: Vec<usize> = pipe.ep.queue.iter().map(|t| t.len()).collect();
        assert_eq!(sizes, vec![1024, 0, 100]);
    }

    #[test]
    fn test_out_flush_waits_for_all_transfers() {
        let mut pipe = out_pipe(4, 512);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(pipe.poll_flush(&mut cx).is_ready()); // idle => immediately done
        for _ in 0..3 {
            let _ = pipe.poll_write(&mut cx, &[9u8; 10]);
        }
        assert!(pipe.poll_flush(&mut cx).is_pending());
        pipe.ep.complete(2);
        assert!(count.0.load(Ordering::SeqCst) >= 1);
        assert!(pipe.poll_flush(&mut cx).is_pending());
        pipe.ep.complete(1);
        assert!(pipe.poll_flush(&mut cx).is_ready());
        assert_eq!(pipe.ep.pending(), 0);
    }

    #[test]
    fn test_out_error_is_sticky() {
        let mut pipe = out_pipe(4, 512);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(pipe.poll_write(&mut cx, &[1u8; 10]), Poll::Ready(Ok(10))));
        pipe.ep.fail = Some(io::ErrorKind::NotConnected);
        match pipe.poll_write(&mut cx, &[1u8; 10]) {
            Poll::Ready(Err(e)) => assert_eq!(e.kind(), io::ErrorKind::NotConnected),
            _ => panic!("expected error"),
        }
        pipe.ep.fail = None;
        assert!(matches!(pipe.poll_write(&mut cx, &[1u8; 10]), Poll::Ready(Err(_))));
        assert!(matches!(pipe.poll_flush(&mut cx), Poll::Ready(Err(_))));
    }

    #[derive(Default)]
    struct FakeIn {
        pending: usize,
        /// Completed transfers, oldest first; `Err` entries simulate failures.
        done: VecDeque<io::Result<Vec<u8>>>,
        submitted: usize,
        waker: Option<Waker>,
    }

    impl FakeIn {
        fn push(&mut self, r: io::Result<Vec<u8>>) {
            self.done.push_back(r);
            if let Some(w) = self.waker.take() {
                w.wake();
            }
        }
    }

    impl InEndpoint for FakeIn {
        type Buf = Vec<u8>;
        fn pending(&self) -> usize {
            self.pending
        }
        fn submit(&mut self) {
            self.pending += 1;
            self.submitted += 1;
        }
        fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Vec<u8>>> {
            match self.done.pop_front() {
                Some(r) => {
                    self.pending -= 1;
                    Poll::Ready(r)
                }
                None => {
                    self.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
        fn recycle(&mut self, _buf: Vec<u8>) {}
    }

    fn read_some(
        pipe: &mut InPipe<FakeIn>,
        cx: &mut Context<'_>,
        cap: usize,
    ) -> Poll<io::Result<Vec<u8>>> {
        let mut storage = vec![0u8; cap];
        let mut rb = ReadBuf::new(&mut storage);
        match pipe.poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().to_vec())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    #[test]
    fn test_in_keeps_queue_full_and_orders_bytes() {
        let mut pipe = InPipe::new(FakeIn::default(), 4);
        assert_eq!(pipe.ep.pending, 4);
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(read_some(&mut pipe, &mut cx, 8).is_pending());
        pipe.ep.push(Ok(vec![1, 2, 3, 4, 5]));
        pipe.ep.push(Ok(vec![6, 7]));
        assert_eq!(count.0.load(Ordering::SeqCst), 1);

        // Partial reads of one transfer, then the next, in order.
        let mut got = Vec::new();
        for cap in [2usize, 2, 8, 8] {
            if let Poll::Ready(Ok(b)) = read_some(&mut pipe, &mut cx, cap) {
                got.extend(b);
            }
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 7]);
        // Queue was replenished after each completed transfer.
        assert_eq!(pipe.ep.pending, 4);
        assert_eq!(pipe.ep.submitted, 6);
    }

    #[test]
    fn test_in_zero_length_packet_is_not_eof() {
        let mut pipe = InPipe::new(FakeIn::default(), 2);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        pipe.ep.push(Ok(vec![]));
        pipe.ep.push(Ok(vec![42]));
        match read_some(&mut pipe, &mut cx, 4) {
            Poll::Ready(Ok(b)) => assert_eq!(b, vec![42]),
            _ => panic!("expected data after ZLP"),
        }
    }

    #[test]
    fn test_in_disconnect_is_eof_and_other_errors_surface() {
        let mut pipe = InPipe::new(FakeIn::default(), 2);
        let (_c, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        pipe.ep.push(Ok(vec![9]));
        pipe.ep.push(Err(io::Error::new(io::ErrorKind::NotConnected, "gone")));
        assert!(matches!(read_some(&mut pipe, &mut cx, 4), Poll::Ready(Ok(b)) if b == [9]));
        // EOF: Ok with nothing filled, and it stays EOF.
        assert!(matches!(read_some(&mut pipe, &mut cx, 4), Poll::Ready(Ok(b)) if b.is_empty()));
        assert!(matches!(read_some(&mut pipe, &mut cx, 4), Poll::Ready(Ok(b)) if b.is_empty()));

        let mut pipe = InPipe::new(FakeIn::default(), 2);
        pipe.ep.push(Err(io::Error::new(io::ErrorKind::ConnectionReset, "stall")));
        assert!(matches!(read_some(&mut pipe, &mut cx, 4), Poll::Ready(Err(_))));
        assert!(matches!(read_some(&mut pipe, &mut cx, 4), Poll::Ready(Err(_))));
    }
}
