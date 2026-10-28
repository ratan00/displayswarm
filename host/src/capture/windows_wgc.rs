//! Windows.Graphics.Capture source (Windows 10 1803+): one monitor or one
//! window, cursor included, event driven. See [`super::windows`].

use super::windows::sys::MonitorInfo;
use super::windows::{RawFrame, RawSource};
use super::windows_dxgi::{create_device, read_texture, D3d};
use super::windows_vp::Nv12Reader;
use crate::display::OutputRegion;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// Time spent getting each picture out of the capture pool (GPU copy or CPU
/// readback), for the benchmark in `windows_bench.rs`.
#[derive(Default)]
pub struct ReadbackStats {
    us: std::sync::atomic::AtomicU64,
    frames: std::sync::atomic::AtomicU64,
}

impl ReadbackStats {
    /// `(total microseconds, frames)`.
    pub fn get(&self) -> (u64, u64) {
        (self.us.load(Ordering::Relaxed), self.frames.load(Ordering::Relaxed))
    }
}

pub struct WgcSource {
    readback: Arc<ReadbackStats>,
    d3d: D3d,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    pool_size: (i32, i32),
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Zero-copy: textures the newest frames are copied into (see `copy_to_ring`).
    ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    /// CPU fallback: GPU scale + NV12 conversion and an asynchronous readback
    /// of the small picture (`None` until the output size is known, or when
    /// the device cannot do it: then `read_texture` hands out full BGRA).
    reader: Option<Nv12Reader>,
    wake_rx: crossbeam_channel::Receiver<()>,
    closed: Arc<AtomicBool>,
    monitor: Option<MonitorInfo>,
    label: String,
}

// Used only from the capture thread; the WinRT objects are agile.
unsafe impl Send for WgcSource {}

const RING_LEN: usize = 4;
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

impl WgcSource {
    pub fn for_monitor(mon: &MonitorInfo) -> Result<Self, String> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| e.to_string())?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(mon.handle) }.map_err(|e| e.to_string())?;
        let r = mon.region();
        let label = format!(
            "{} {}x{} at {},{} (Windows.Graphics.Capture)",
            mon.name, r.width, r.height, r.x, r.y
        );
        Self::start(item, Some(mon.clone()), label)
    }

    pub fn for_window(hwnd: HWND) -> Result<Self, String> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| e.to_string())?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(|e| e.to_string())?;
        let label = format!("window 0x{:x} (Windows.Graphics.Capture)", hwnd.0 as usize);
        Self::start(item, None, label)
    }

    fn start(item: GraphicsCaptureItem, monitor: Option<MonitorInfo>, label: String) -> Result<Self, String> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED); // already-initialised is fine
        }
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("Windows.Graphics.Capture is not supported on this system".into());
        }
        let (adapter, why) = super::windows_dxgi::pick_adapter(monitor.as_ref().map(|m| m.name.as_str()));
        let d3d = create_device(adapter.as_ref())?;
        if let Ok(desc) = d3d.device.cast::<IDXGIDevice>().and_then(|d| unsafe { d.GetAdapter() }).and_then(|a| unsafe { a.GetDesc() }) {
            let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            log::info!("Windows capture runs on {} ({why})", String::from_utf16_lossy(&desc.Description[..end]));
        }
        let dxgi: IDXGIDevice = d3d.device.cast().map_err(|e| e.to_string())?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .map_err(|e| e.to_string())?;
        let size = item.Size().map_err(|e| e.to_string())?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, FORMAT, 2, size)
            .map_err(|e| e.to_string())?;

        let (tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
            let _ = tx.try_send(());
            Ok(())
        }))
        .map_err(|e| e.to_string())?;
        let closed = Arc::new(AtomicBool::new(false));
        let c2 = closed.clone();
        item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            c2.store(true, Ordering::SeqCst);
            Ok(())
        }))
        .map_err(|e| e.to_string())?;

        let session = pool.CreateCaptureSession(&item).map_err(|e| e.to_string())?;
        // Newer-Windows options; older builds reject them, which is fine.
        let _ = session.SetIsCursorCaptureEnabled(true);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(|e| e.to_string())?;
        Ok(Self {
            readback: Arc::default(),
            d3d,
            winrt_device,
            pool,
            session,
            item,
            pool_size: (size.Width, size.Height),
            staging: None,
            ring: Vec::new(),
            ring_next: 0,
            reader: None,
            wake_rx,
            closed,
            monitor,
            label,
        })
    }

    pub fn readback_stats(&self) -> Arc<ReadbackStats> {
        self.readback.clone()
    }

    /// Copies the captured picture (GPU to GPU, ~0.1 ms) into the next ring
    /// texture and hands that out, so the capture pool's surface goes back at
    /// once. The ring is deeper than the frames in flight (slot, encoder,
    /// repeat-last); even a stale texture only shows a newer picture, never a
    /// torn one, because all work goes through the device's one immediate context.
    fn copy_to_ring(&mut self, tex: &ID3D11Texture2D, cw: u32, ch: u32) -> Result<Arc<crate::capture::GpuFrame>, String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let stale = self.ring.first().map_or(true, |r| {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { r.GetDesc(&mut d) };
            (d.Width, d.Height, d.Format) != (desc.Width, desc.Height, desc.Format)
        });
        if stale {
            self.ring.clear();
            let rd = D3D11_TEXTURE2D_DESC {
                MipLevels: 1,
                ArraySize: 1,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..desc
            };
            for _ in 0..RING_LEN {
                let mut t: Option<ID3D11Texture2D> = None;
                unsafe { self.d3d.device.CreateTexture2D(&rd, None, Some(&mut t)) }.map_err(|e| format!("CreateTexture2D: {e}"))?;
                self.ring.push(t.ok_or("no ring texture")?);
            }
            self.ring_next = 0;
        }
        let dst = self.ring[self.ring_next].clone();
        self.ring_next = (self.ring_next + 1) % self.ring.len();
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: cw.min(desc.Width), bottom: ch.min(desc.Height), back: 1 };
        unsafe { self.d3d.context.CopySubresourceRegion(&dst, 0, 0, 0, 0, tex, 0, Some(&region)) };
        Ok(Arc::new(crate::capture::GpuFrame {
            device: self.d3d.device.clone(),
            context: self.d3d.context.clone(),
            texture: dst,
            width: region.right,
            height: region.bottom,
        }))
    }

    /// The newest queued frame, dropping older ones.
    fn submit_newest(&mut self) -> Result<Option<RawFrame>, String> {
        let mut newest = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            newest = Some(f);
        }
        let Some(frame) = newest else { return Ok(None) };
        let content = frame.ContentSize().map_err(|e| e.to_string())?;
        let surface = frame.Surface().map_err(|e| e.to_string())?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| e.to_string())?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| e.to_string())?;
        let (cw, ch) = (content.Width.max(1) as u32, content.Height.max(1) as u32);
        let started = std::time::Instant::now();
        let mut raw = match crate::capture::gpu_frames_enabled().then(|| self.copy_to_ring(&tex, cw, ch)) {
            Some(Ok(g)) => Some(RawFrame { width: cw, height: ch, data: Vec::new(), gpu: Some(g), nv12: false }),
            Some(Err(e)) => {
                // Same as a failed import in the encoder: back to CPU frames for good.
                log::warn!("Windows capture: zero-copy frame failed ({e}); using CPU frames");
                crate::capture::set_gpu_frames(false);
                None
            }
            None => None,
        };
        if raw.is_none() {
            if let Some(reader) = self.reader.as_mut().filter(|_| !crate::capture::gpu_frames_enabled()) {
                match reader.submit(&tex, cw, ch) {
                    Ok(()) => {}
                    Err(e) => {
                        log::warn!("Windows capture: GPU NV12 readback failed ({e}); reading BGRA");
                        self.reader = None;
                    }
                }
            }
            if self.reader.is_none() {
                raw = Some(read_texture(&self.d3d, &tex, &mut self.staging, cw, ch)?);
            }
        }
        self.readback.us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.readback.frames.fetch_add(1, Ordering::Relaxed);
        // The target was resized: rebuild the pool at the new size for the next frames.
        if (content.Width, content.Height) != self.pool_size {
            self.pool_size = (content.Width, content.Height);
            let _ = self.pool.Recreate(&self.winrt_device, FORMAT, 2, content);
        }
        Ok(raw)
    }
}

impl RawSource for WgcSource {
    fn next_raw(&mut self, timeout: Duration) -> Result<Option<RawFrame>, Box<dyn std::error::Error + Send + Sync>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(f) = self.submit_newest()? {
                return Ok(Some(f));
            }
            // A converted picture the GPU has finished, if any (never waits).
            if let Some(f) = self.reader.as_mut().and_then(|r| r.poll().ok().flatten()) {
                return Ok(Some(f));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            // While a readback is in flight, look again soon: the GPU needs
            // well under a millisecond and the next WGC frame is far off.
            let pending = self.reader.as_ref().is_some_and(|r| r.has_pending());
            let _ = self.wake_rx.recv_timeout(if pending { left.min(Duration::from_micros(250)) } else { left });
        }
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        let disabled = std::env::var("DISPLAYSWARM_GPU_NV12").map(|v| v == "0").unwrap_or(false);
        if disabled || width % 2 != 0 || height % 2 != 0 {
            return;
        }
        match Nv12Reader::probe(&self.d3d, width, height) {
            Ok(()) => {
                log::info!("Windows capture: scale and NV12 conversion on the GPU for the CPU path ({width}x{height})");
                self.reader = Some(Nv12Reader::new(D3d { device: self.d3d.device.clone(), context: self.d3d.context.clone() }, width, height));
            }
            Err(e) => log::warn!("Windows capture: GPU NV12 conversion unavailable ({e}); converting on the CPU"),
        }
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn failure(&self) -> Option<String> {
        self.closed.load(Ordering::SeqCst).then(|| "the captured window or monitor was closed".to_string())
    }

    fn region(&self) -> Option<OutputRegion> {
        // Re-read: the monitor may have moved since the capture began.
        let mon = self.monitor.as_ref()?;
        super::windows::sys::monitors().into_iter().find(|m| m.name == mon.name).map(|m| m.region())
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        let _ = &self.item;
    }
}
