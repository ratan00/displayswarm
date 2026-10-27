//! Zero-copy capture for the Windows QSV encoder: the Windows.Graphics.Capture
//! texture goes into the encoder without ever touching the CPU. The Windows
//! twin of `zerocopy.rs` (VA-API).
//!
//! Before, every frame was read out of GPU memory by the CPU (a blocking
//! staging-texture readback), scaled and converted to NV12 on the CPU, and
//! uploaded again. Here the BGRA texture (a GPU copy out of the capture pool,
//! on the capture's Direct3D 11 device) is scaled, letterboxed and converted
//! to BT.709 limited-range NV12 by the GPU's video processor
//! (`ID3D11VideoContext::VideoProcessorBlt`) straight into a texture of a
//! D3D11VA frame pool, and that pool is derived to QSV so the oneVPL encoder
//! reads the very same texture (ffmpeg's `hwmap=derive_device=qsv`).
//!
//! The ffmpeg D3D11VA device wraps the capture's own `ID3D11Device`, and the
//! QSV device is derived from it, so the video processor, the capture copy and
//! the encoder all run on one adapter and one immediate context: no sharing
//! handles, no fences.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use ffmpeg_next::ffi::*;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

use super::BoxError;
use crate::capture::scale::fit_rect;
use crate::capture::GpuFrame;

/// `AVD3D11VADeviceContext` (libavutil/hwcontext_d3d11va.h); the bindings do not include it.
#[repr(C)]
struct D3d11DeviceCtx {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

/// `AVD3D11VAFramesContext`.
#[repr(C)]
struct D3d11FramesCtx {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut c_void,
}

fn check(r: i32, what: &str) -> Result<(), BoxError> {
    if r < 0 {
        Err(format!("{what}: {}", super::ffmpeg::av_err(r)).into())
    } else {
        Ok(())
    }
}

/// Frames in the NV12 pool: the encoder holds a few while it works.
const POOL_SIZE: i32 = 8;
/// Input views kept (one per capture ring texture).
const MAX_INPUT_VIEWS: usize = 8;

/// Whether the capture device is on an Intel adapter: QSV can only be derived
/// from a D3D11 device of the Intel GPU (a hybrid laptop's NVIDIA chip makes
/// `MFXVideoCORE_SetHandle` fail).
pub fn supported(device: &ID3D11Device) -> bool {
    let vendor = device
        .cast::<IDXGIDevice>()
        .and_then(|d| unsafe { d.GetAdapter() })
        .and_then(|a| unsafe { a.GetDesc() })
        .map(|d| d.VendorId);
    match vendor {
        Ok(0x8086) => true,
        Ok(v) => {
            log::debug!("zero-copy: capture runs on adapter vendor {v:#x}, not Intel");
            false
        }
        Err(e) => {
            log::debug!("zero-copy: cannot identify the capture adapter: {e}");
            false
        }
    }
}

/// The ffmpeg objects an opened QSV encoder needs to take hardware frames.
pub struct D3d11Hw {
    d3d_dev: *mut AVBufferRef,
    qsv_dev: *mut AVBufferRef,
    d3d_frames: *mut AVBufferRef,
    qsv_frames: *mut AVBufferRef,
}

impl Drop for D3d11Hw {
    fn drop(&mut self) {
        // Safety: each pointer is null or our own reference; unref nulls it.
        unsafe {
            av_buffer_unref(&mut self.qsv_frames);
            av_buffer_unref(&mut self.d3d_frames);
            av_buffer_unref(&mut self.qsv_dev);
            av_buffer_unref(&mut self.d3d_dev);
        }
    }
}

impl D3d11Hw {
    /// An ffmpeg D3D11VA device around `device`, a QSV device derived from it,
    /// and a pool of `w`x`h` NV12 textures (render targets, so the video
    /// processor can write them) derived to QSV.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, w: u32, h: u32) -> Result<Self, BoxError> {
        let mut this = D3d11Hw { d3d_dev: ptr::null_mut(), qsv_dev: ptr::null_mut(), d3d_frames: ptr::null_mut(), qsv_frames: ptr::null_mut() };
        // Safety: plain libavutil setup. The device context takes over one
        // COM reference each (`into_raw`) and releases them on uninit.
        unsafe {
            this.d3d_dev = av_hwdevice_ctx_alloc(AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if this.d3d_dev.is_null() {
                return Err("av_hwdevice_ctx_alloc(D3D11VA) failed".into());
            }
            let dc = &mut *((*this.d3d_dev).data as *mut AVHWDeviceContext);
            let hw = &mut *(dc.hwctx as *mut D3d11DeviceCtx);
            hw.device = device.clone().into_raw();
            hw.device_context = context.clone().into_raw();
            check(av_hwdevice_ctx_init(this.d3d_dev), "init D3D11VA device")?;

            check(
                av_hwdevice_ctx_create_derived(&mut this.qsv_dev, AVHWDeviceType::AV_HWDEVICE_TYPE_QSV, this.d3d_dev, 0),
                "derive QSV device from D3D11",
            )?;

            this.d3d_frames = av_hwframe_ctx_alloc(this.d3d_dev);
            if this.d3d_frames.is_null() {
                return Err("av_hwframe_ctx_alloc failed".into());
            }
            let fc = &mut *((*this.d3d_frames).data as *mut AVHWFramesContext);
            fc.format = AVPixelFormat::AV_PIX_FMT_D3D11;
            fc.sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
            fc.width = w as i32;
            fc.height = h as i32;
            fc.initial_pool_size = POOL_SIZE;
            let fh = &mut *(fc.hwctx as *mut D3d11FramesCtx);
            fh.bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
            check(av_hwframe_ctx_init(this.d3d_frames), "init D3D11 NV12 frame pool")?;

            check(
                av_hwframe_ctx_create_derived(&mut this.qsv_frames, AVPixelFormat::AV_PIX_FMT_QSV, this.qsv_dev, this.d3d_frames, 0),
                "derive QSV frames from D3D11",
            )?;
        }
        Ok(this)
    }

    /// New references for the codec context.
    pub fn frames_ref(&self) -> *mut AVBufferRef {
        // Safety: our own live reference.
        unsafe { av_buffer_ref(self.qsv_frames) }
    }

    pub fn device_ref(&self) -> *mut AVBufferRef {
        // Safety: our own live reference.
        unsafe { av_buffer_ref(self.qsv_dev) }
    }
}

/// The video processor and its per-texture views: BGRA texture in, NV12 pool
/// texture out, at the encoder's size.
pub struct D3d11Import {
    hw: D3d11Hw,
    video_context: ID3D11VideoContext,
    video_context1: ID3D11VideoContext1,
    video_device: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    inputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    outputs: HashMap<(usize, isize), ID3D11VideoProcessorOutputView>,
    d3d_frame: *mut AVFrame,
    qsv_frame: *mut AVFrame,
    /// What the processor is set up for: source size and the key of the device.
    src: (u32, u32),
    dst: (u32, u32),
    device_ptr: usize,
}

// Safety: the COM objects are agile and the ffmpeg pointers are owned here and
// used from the encoder thread only.
unsafe impl Send for D3d11Import {}

impl Drop for D3d11Import {
    fn drop(&mut self) {
        // Safety: owned frames; `hw` drops after, releasing the pools.
        unsafe {
            av_frame_free(&mut self.qsv_frame);
            av_frame_free(&mut self.d3d_frame);
        }
    }
}

impl D3d11Import {
    /// Whether this import still fits `g` (same device, same source size).
    pub fn matches(&self, g: &GpuFrame) -> bool {
        self.device_ptr == g.device.as_raw() as usize && self.src == (g.width, g.height)
    }

    pub fn hw(&self) -> &D3d11Hw {
        &self.hw
    }

    pub fn new(g: &GpuFrame, dw: u32, dh: u32) -> Result<Self, BoxError> {
        if !supported(&g.device) {
            return Err("the capture is not on an Intel GPU".into());
        }
        let hw = D3d11Hw::new(&g.device, &g.context, dw, dh)?;
        let video_device: ID3D11VideoDevice = g.device.cast().map_err(|e| format!("no ID3D11VideoDevice: {e}"))?;
        let video_context: ID3D11VideoContext = g.context.cast().map_err(|e| format!("no ID3D11VideoContext: {e}"))?;
        let video_context1: ID3D11VideoContext1 = g.context.cast().map_err(|e| format!("no ID3D11VideoContext1: {e}"))?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: g.width,
            InputHeight: g.height,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: dw,
            OutputHeight: dh,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // Safety: valid descriptors; the COM calls only read them.
        let (enumerator, processor) = unsafe {
            let e = video_device.CreateVideoProcessorEnumerator(&desc).map_err(|e| format!("CreateVideoProcessorEnumerator: {e}"))?;
            let p = video_device.CreateVideoProcessor(&e, 0).map_err(|e| format!("CreateVideoProcessor: {e}"))?;
            (e, p)
        };
        // Safety: setting fixed state on our own processor.
        unsafe {
            // RGB in full range, BT.709 limited-range YCbCr out: what convert.rs
            // does on the CPU and the encoder signals in the VUI.
            video_context1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video_context1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let black = D3D11_VIDEO_COLOR { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            let (fx, fy, fw, fh) = fit_rect(g.width, g.height, dw, dh);
            // Even sizes and offsets, as the chroma planes need.
            let even = |v: u32| (v & !1) as i32;
            let dest = RECT { left: even(fx), top: even(fy), right: even(fx) + even(fw).max(2), bottom: even(fy) + even(fh).max(2) };
            let src = RECT { left: 0, top: 0, right: g.width as i32, bottom: g.height as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&RECT { left: 0, top: 0, right: dw as i32, bottom: dh as i32 }));
            video_context.VideoProcessorSetStreamOutputRate(&processor, 0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, true, None);
        }
        // Safety: allocation only.
        let (d3d_frame, qsv_frame) = unsafe { (av_frame_alloc(), av_frame_alloc()) };
        if d3d_frame.is_null() || qsv_frame.is_null() {
            return Err("ffmpeg allocation failed".into());
        }
        Ok(Self {
            hw,
            video_context,
            video_context1,
            video_device,
            enumerator,
            processor,
            inputs: Vec::new(),
            outputs: HashMap::new(),
            d3d_frame,
            qsv_frame,
            src: (g.width, g.height),
            dst: (dw, dh),
            device_ptr: g.device.as_raw() as usize,
        })
    }

    fn input_view(&mut self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView, BoxError> {
        if let Some((_, v)) = self.inputs.iter().find(|(t, _)| t == tex) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        // Safety: valid texture and descriptor.
        unsafe { self.video_device.CreateVideoProcessorInputView(tex, &self.enumerator, &desc, Some(&mut view)) }
            .map_err(|e| format!("CreateVideoProcessorInputView: {e}"))?;
        let view = view.ok_or("no input view")?;
        if self.inputs.len() >= MAX_INPUT_VIEWS {
            self.inputs.remove(0);
        }
        self.inputs.push((tex.clone(), view.clone()));
        Ok(view)
    }

    fn output_view(&mut self, tex: &ID3D11Texture2D, index: isize) -> Result<ID3D11VideoProcessorOutputView, BoxError> {
        let key = (tex.as_raw() as usize, index);
        if let Some(v) = self.outputs.get(&key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV { MipSlice: 0, FirstArraySlice: index as u32, ArraySize: 1 },
            },
        };
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        // Safety: valid texture and descriptor.
        unsafe { self.video_device.CreateVideoProcessorOutputView(tex, &self.enumerator, &desc, Some(&mut view)) }
            .map_err(|e| format!("CreateVideoProcessorOutputView: {e}"))?;
        let view = view.ok_or("no output view")?;
        self.outputs.insert(key, view.clone());
        Ok(view)
    }

    /// Scales and converts `g` into a fresh pool texture and returns it as a
    /// QSV frame, valid until the next call.
    pub fn process(&mut self, g: &GpuFrame) -> Result<*mut AVFrame, BoxError> {
        let input = self.input_view(&g.texture)?;
        // Safety: the frames are ours; the pool texture pointer stays valid as
        // long as `d3d_frame` holds its reference.
        unsafe {
            av_frame_unref(self.d3d_frame);
            av_frame_unref(self.qsv_frame);
            check(av_hwframe_get_buffer(self.hw.d3d_frames, self.d3d_frame, 0), "get NV12 texture")?;
            let raw = (*self.d3d_frame).data[0] as *mut c_void;
            let index = (*self.d3d_frame).data[1] as isize;
            let tex = ID3D11Texture2D::from_raw_borrowed(&raw).ok_or("NV12 frame without a texture")?.clone();
            let output = self.output_view(&tex, index)?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
                ppFutureSurfaces: ptr::null_mut(),
                ppPastSurfacesRight: ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: ptr::null_mut(),
            };
            let mut streams = [stream];
            let r = self.video_context.VideoProcessorBlt(&self.processor, &output, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r.map_err(|e| format!("VideoProcessorBlt: {e}"))?;

            let q = &mut *self.qsv_frame;
            q.format = AVPixelFormat::AV_PIX_FMT_QSV as i32;
            q.hw_frames_ctx = av_buffer_ref(self.hw.qsv_frames);
            check(
                av_hwframe_map(self.qsv_frame, self.d3d_frame, (AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_WRITE) as i32),
                "map the NV12 texture to QSV",
            )?;
            q.width = self.dst.0 as i32;
            q.height = self.dst.1 as i32;
        }
        let _ = &self.video_context1;
        Ok(self.qsv_frame)
    }
}
