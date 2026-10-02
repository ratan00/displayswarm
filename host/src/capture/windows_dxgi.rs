//! DXGI Desktop Duplication: the fallback capture source, and the Direct3D 11
//! helpers the WGC source shares.
//!
//! Differences from WGC that matter here: monitors only (no windows), the
//! mouse cursor is *not* part of the picture, and it cannot duplicate while the
//! secure desktop (UAC, lock screen) is showing - `DXGI_ERROR_ACCESS_LOST` -
//! in which case the duplication is rebuilt on the next call.

use super::windows::{copy_rows, unrotate_bgra, RawFrame, RawSource, Rotation};
use crate::display::OutputRegion;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

/// A Direct3D 11 device and its immediate context.
pub struct D3d {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

/// Creates a BGRA-capable device on `adapter` (the adapter that owns the output
/// for duplication), or the default hardware adapter, or WARP as a last resort.
pub fn create_device(adapter: Option<&IDXGIAdapter>) -> Result<D3d, String> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let attempt = |driver, adapter: Option<&IDXGIAdapter>, device: &mut Option<ID3D11Device>, context: &mut Option<ID3D11DeviceContext>| unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(device),
            None,
            Some(context),
        )
    };
    let hw_driver = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN // required when an adapter is given
    } else {
        windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE
    };
    if let Err(e) = attempt(hw_driver, adapter, &mut device, &mut context) {
        log::warn!("Hardware D3D11 device failed ({e}); using WARP");
        attempt(D3D_DRIVER_TYPE_WARP, None, &mut device, &mut context).map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    }
    let device = device.ok_or("D3D11CreateDevice returned no device")?;
    // Windows.Graphics.Capture uses the device from its own threads while the
    // capture thread copies on the immediate context.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Ok(D3d { device, context: context.ok_or("D3D11CreateDevice returned no context")? })
}

/// Copies `texture` to a CPU-readable staging texture (kept in `staging`
/// between calls) and returns its top-left `width` x `height` as tight BGRA.
pub fn read_texture(
    d3d: &D3d,
    texture: &ID3D11Texture2D,
    staging: &mut Option<(ID3D11Texture2D, u32, u32)>,
    width: u32,
    height: u32,
) -> Result<RawFrame, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let need_new = !matches!(staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
    if need_new {
        let sd = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { d3d.device.CreateTexture2D(&sd, None, Some(&mut tex)) }.map_err(|e| format!("CreateTexture2D: {e}"))?;
        *staging = Some((tex.ok_or("no staging texture")?, desc.Width, desc.Height));
    }
    let (stage, _, _) = staging.as_ref().expect("just created");
    let (w, h) = (width.min(desc.Width), height.min(desc.Height));
    unsafe {
        d3d.context.CopyResource(stage, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        d3d.context.Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| format!("Map: {e}"))?;
        let pitch = mapped.RowPitch as usize;
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize);
        let data = copy_rows(src, pitch, w, h);
        d3d.context.Unmap(stage, 0);
        Ok(RawFrame { width: w, height: h, data })
    }
}

pub struct DxgiSource {
    d3d: D3d,
    output_name: String,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    region: OutputRegion,
    rotation: Rotation,
}

// The COM objects are only touched from the capture thread that owns the source.
unsafe impl Send for DxgiSource {}

/// Finds the adapter and output whose GDI device name is `name`.
fn find_output(name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1, OutputRegion, Rotation), String> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    let mut ai = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(ai) } {
        let mut oi = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(oi) } {
            let desc = unsafe { output.GetDesc() }.map_err(|e| format!("GetDesc: {e}"))?;
            let end = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
            let dev = String::from_utf16_lossy(&desc.DeviceName[..end]);
            if super::windows::output_matches(name, &dev) {
                let r = desc.DesktopCoordinates;
                let region = OutputRegion {
                    x: r.left,
                    y: r.top,
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                };
                let rotation = Rotation::from_dxgi(desc.Rotation.0);
                return Ok((adapter, output.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?, region, rotation));
            }
            oi += 1;
        }
        ai += 1;
    }
    Err(format!("DXGI has no output {name}"))
}

impl DxgiSource {
    pub fn new(output_name: &str) -> Result<Self, String> {
        let (adapter, output, region, rotation) = find_output(output_name)?;
        let d3d = create_device(Some(&adapter.cast::<IDXGIAdapter>().map_err(|e| e.to_string())?))?;
        let duplication =
            unsafe { output.DuplicateOutput(&d3d.device) }.map_err(|e| format!("DuplicateOutput({output_name}): {e}"))?;
        Ok(Self { d3d, output_name: output_name.to_string(), duplication: Some(duplication), staging: None, region, rotation })
    }

    fn rebuild(&mut self) -> Result<(), String> {
        let (_, output, region, rotation) = find_output(&self.output_name)?;
        self.duplication = Some(
            unsafe { output.DuplicateOutput(&self.d3d.device) }.map_err(|e| format!("DuplicateOutput: {e}"))?,
        );
        self.region = region;
        self.rotation = rotation;
        self.staging = None;
        Ok(())
    }
}

impl RawSource for DxgiSource {
    fn next_raw(&mut self, timeout: Duration) -> Result<Option<RawFrame>, Box<dyn std::error::Error + Send + Sync>> {
        if self.duplication.is_none() {
            if self.rebuild().is_err() {
                // Secure desktop or mode change in progress; try again next call.
                std::thread::sleep(timeout.min(Duration::from_millis(50)));
                return Ok(None);
            }
        }
        let dup = self.duplication.as_ref().expect("checked");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { dup.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                log::warn!("DXGI duplication lost ({e}); rebuilding");
                self.duplication = None;
                return Ok(None);
            }
            Err(e) => return Err(format!("AcquireNextFrame: {e}").into()),
        }
        // LastPresentTime == 0: only the pointer moved, no new pixels.
        let result = if info.LastPresentTime == 0 {
            Ok(None)
        } else {
            match resource.as_ref().map(|r| r.cast::<ID3D11Texture2D>()) {
                Some(Ok(tex)) => {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut desc) };
                    read_texture(&self.d3d, &tex, &mut self.staging, desc.Width, desc.Height)
                        .map(|raw| Some(unrotate_bgra(raw, self.rotation)))
                }
                _ => Ok(None),
            }
        };
        let _ = unsafe { dup.ReleaseFrame() };
        result.map_err(Into::into)
    }

    fn describe(&self) -> String {
        format!(
            "{} {}x{} at {},{} (DXGI desktop duplication, no cursor)",
            self.output_name, self.region.width, self.region.height, self.region.x, self.region.y
        )
    }

    fn region(&self) -> Option<OutputRegion> {
        Some(self.region)
    }
}
