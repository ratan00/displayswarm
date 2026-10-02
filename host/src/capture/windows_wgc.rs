//! Windows.Graphics.Capture source (Windows 10 1803+): one monitor or one
//! window, cursor included, event driven. See [`super::windows`].

use super::windows::sys::MonitorInfo;
use super::windows::{RawFrame, RawSource};
use super::windows_dxgi::{create_device, read_texture, D3d};
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
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

pub struct WgcSource {
    d3d: D3d,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    pool_size: (i32, i32),
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    wake_rx: crossbeam_channel::Receiver<()>,
    closed: Arc<AtomicBool>,
    monitor: Option<MonitorInfo>,
    label: String,
}

// Used only from the capture thread; the WinRT objects are agile.
unsafe impl Send for WgcSource {}

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
        let d3d = create_device(None)?;
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
            d3d,
            winrt_device,
            pool,
            session,
            item,
            pool_size: (size.Width, size.Height),
            staging: None,
            wake_rx,
            closed,
            monitor,
            label,
        })
    }

    /// The newest queued frame, dropping older ones.
    fn take_newest(&mut self) -> Result<Option<RawFrame>, String> {
        let mut newest = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            newest = Some(f);
        }
        let Some(frame) = newest else { return Ok(None) };
        let content = frame.ContentSize().map_err(|e| e.to_string())?;
        let surface = frame.Surface().map_err(|e| e.to_string())?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| e.to_string())?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| e.to_string())?;
        let raw = read_texture(&self.d3d, &tex, &mut self.staging, content.Width.max(1) as u32, content.Height.max(1) as u32)?;
        // The target was resized: rebuild the pool at the new size for the next frames.
        if (content.Width, content.Height) != self.pool_size {
            self.pool_size = (content.Width, content.Height);
            let _ = self.pool.Recreate(&self.winrt_device, FORMAT, 2, content);
        }
        Ok(Some(raw))
    }
}

impl RawSource for WgcSource {
    fn next_raw(&mut self, timeout: Duration) -> Result<Option<RawFrame>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(f) = self.take_newest()? {
            return Ok(Some(f));
        }
        if self.wake_rx.recv_timeout(timeout).is_err() {
            return Ok(None);
        }
        Ok(self.take_newest()?)
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
