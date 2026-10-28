//! Direct3D 11 video processor helpers: BGRA texture -> scaled, letterboxed
//! BT.709 limited-range NV12, on the GPU.
//!
//! [`configure`] is shared with the zero-copy encoder import
//! (`encoder/zerocopy_win.rs`). [`Nv12Reader`] is the CPU fallback: the same
//! conversion on the capture device, then a pipelined readback of the small
//! NV12 picture (1600x720 is 1.7 MB, against 8.3 MB of 1080p BGRA) through a
//! ring of staging textures that is polled without waiting, so the capture
//! thread neither converts on the CPU nor stalls on the GPU.

use super::scale::fit_rect;
use super::windows::RawFrame;
use super::windows_dxgi::D3d;
use std::collections::VecDeque;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

/// Sets the fixed state of `processor`: RGB full range in, BT.709 limited-range
/// YCbCr out (what convert.rs does on the CPU and the VUI signals), the `sw` x
/// `sh` source fitted into `dw` x `dh` with black bars.
pub fn configure(video_context: &ID3D11VideoContext, processor: &ID3D11VideoProcessor, sw: u32, sh: u32, dw: u32, dh: u32) {
    // Safety: setting state on our own processor with valid descriptors.
    unsafe {
        if let Ok(vc1) = video_context.cast::<ID3D11VideoContext1>() {
            vc1.VideoProcessorSetStreamColorSpace1(processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            vc1.VideoProcessorSetOutputColorSpace1(processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
        }
        let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
        video_context.VideoProcessorSetOutputBackgroundColor(processor, false, &black);
        let (fx, fy, fw, fh) = fit_rect(sw, sh, dw, dh);
        // Even sizes and offsets, as the chroma planes need.
        let even = |v: u32| (v & !1) as i32;
        let dest = RECT { left: even(fx), top: even(fy), right: even(fx) + even(fw).max(2), bottom: even(fy) + even(fh).max(2) };
        let src = RECT { left: 0, top: 0, right: sw as i32, bottom: sh as i32 };
        video_context.VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&src));
        video_context.VideoProcessorSetStreamDestRect(processor, 0, true, Some(&dest));
        video_context.VideoProcessorSetOutputTargetRect(processor, true, Some(&RECT { left: 0, top: 0, right: dw as i32, bottom: dh as i32 }));
        video_context.VideoProcessorSetStreamOutputRate(processor, 0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, true, None);
    }
}

/// Staging textures in flight (converted pictures waiting to be mapped).
const STAGING_LEN: usize = 4;
/// Input views kept (the capture pool has two or three surfaces).
const MAX_INPUT_VIEWS: usize = 8;

struct Pipeline {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    inputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    target: ID3D11Texture2D,
    target_view: ID3D11VideoProcessorOutputView,
    staging: Vec<ID3D11Texture2D>,
    /// Staging slots holding a submitted picture, oldest first.
    pending: VecDeque<usize>,
    next_slot: usize,
    /// `(texture size, content size)` this pipeline was built for.
    key: ((u32, u32), (u32, u32)),
}

/// Converts captured textures to NV12 on the GPU and reads them back
/// asynchronously. Lives on the capture thread.
pub struct Nv12Reader {
    d3d: D3d,
    out: (u32, u32),
    pipe: Option<Pipeline>,
}

// Used from the capture thread only; the COM objects are agile.
unsafe impl Send for Nv12Reader {}

impl Nv12Reader {
    pub fn new(d3d: D3d, out_w: u32, out_h: u32) -> Self {
        Self { d3d, out: (out_w & !1, out_h & !1), pipe: None }
    }

    /// Whether the device can do BGRA -> NV12 at all.
    pub fn probe(d3d: &D3d, out_w: u32, out_h: u32) -> Result<(), String> {
        let mut r = Self::new(D3d { device: d3d.device.clone(), context: d3d.context.clone() }, out_w, out_h);
        r.build((1920, 1080), (1920, 1080))
    }

    fn build(&mut self, tex: (u32, u32), content: (u32, u32)) -> Result<(), String> {
        let (dw, dh) = self.out;
        let video_device: ID3D11VideoDevice = self.d3d.device.cast().map_err(|e| format!("no ID3D11VideoDevice: {e}"))?;
        let video_context: ID3D11VideoContext = self.d3d.context.cast().map_err(|e| format!("no ID3D11VideoContext: {e}"))?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: tex.0,
            InputHeight: tex.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: dw,
            OutputHeight: dh,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // Safety: valid descriptors; the COM calls only read them.
        unsafe {
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc).map_err(|e| format!("CreateVideoProcessorEnumerator: {e}"))?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).map_err(|e| format!("CreateVideoProcessor: {e}"))?;
            configure(&video_context, &processor, content.0, content.1, dw, dh);

            let mut td = D3D11_TEXTURE2D_DESC {
                Width: dw,
                Height: dh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut target: Option<ID3D11Texture2D> = None;
            self.d3d.device.CreateTexture2D(&td, None, Some(&mut target)).map_err(|e| format!("NV12 target: {e}"))?;
            let target = target.ok_or("no NV12 target")?;
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut target_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device
                .CreateVideoProcessorOutputView(&target, &enumerator, &vd, Some(&mut target_view))
                .map_err(|e| format!("NV12 output view: {e}"))?;

            td.Usage = D3D11_USAGE_STAGING;
            td.BindFlags = 0;
            td.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = Vec::new();
            for _ in 0..STAGING_LEN {
                let mut t: Option<ID3D11Texture2D> = None;
                self.d3d.device.CreateTexture2D(&td, None, Some(&mut t)).map_err(|e| format!("NV12 staging: {e}"))?;
                staging.push(t.ok_or("no NV12 staging texture")?);
            }
            self.pipe = Some(Pipeline {
                video_device,
                video_context,
                enumerator,
                processor,
                inputs: Vec::new(),
                target,
                target_view: target_view.ok_or("no NV12 output view")?,
                staging,
                pending: VecDeque::new(),
                next_slot: 0,
                key: (tex, content),
            });
        }
        Ok(())
    }

    /// Converts the top-left `cw` x `ch` of `tex` and queues its readback.
    /// Never waits for the GPU.
    pub fn submit(&mut self, tex: &ID3D11Texture2D, cw: u32, ch: u32) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let key = ((desc.Width, desc.Height), (cw.min(desc.Width), ch.min(desc.Height)));
        if self.pipe.as_ref().map_or(true, |p| p.key != key) {
            self.pipe = None;
            self.build(key.0, key.1)?;
        }
        let p = self.pipe.as_mut().expect("just built");
        let input = match p.inputs.iter().find(|(t, _)| t == tex) {
            Some((_, v)) => v.clone(),
            None => {
                let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
                };
                let mut view: Option<ID3D11VideoProcessorInputView> = None;
                // Safety: valid texture and descriptor.
                unsafe { p.video_device.CreateVideoProcessorInputView(tex, &p.enumerator, &vd, Some(&mut view)) }
                    .map_err(|e| format!("CreateVideoProcessorInputView: {e}"))?;
                let view = view.ok_or("no input view")?;
                if p.inputs.len() >= MAX_INPUT_VIEWS {
                    p.inputs.remove(0);
                }
                p.inputs.push((tex.clone(), view.clone()));
                view
            }
        };
        // The oldest picture is dropped if the reader fell that far behind.
        if p.pending.len() >= STAGING_LEN {
            p.pending.pop_front();
        }
        let slot = p.next_slot;
        p.next_slot = (slot + 1) % STAGING_LEN;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        // Safety: views and textures are alive; the stream's input reference is released below.
        let r = unsafe { p.video_context.VideoProcessorBlt(&p.processor, &p.target_view, 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.map_err(|e| format!("VideoProcessorBlt: {e}"))?;
        unsafe { self.d3d.context.CopyResource(&p.staging[slot], &p.target) };
        p.pending.push_back(slot);
        Ok(())
    }

    /// Whether a submitted picture has not been collected yet.
    pub fn has_pending(&self) -> bool {
        self.pipe.as_ref().is_some_and(|p| !p.pending.is_empty())
    }

    /// The oldest submitted picture if the GPU has finished it (`Ok(None)`
    /// otherwise, without waiting), as a tight NV12 [`RawFrame`].
    pub fn poll(&mut self) -> Result<Option<RawFrame>, String> {
        let (dw, dh) = self.out;
        let Some(p) = self.pipe.as_mut() else { return Ok(None) };
        let Some(&slot) = p.pending.front() else { return Ok(None) };
        let stage = &p.staging[slot];
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // Safety: the staging texture is ours; the mapping is read within its bounds and unmapped.
        unsafe {
            match self.d3d.context.Map(stage, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut mapped)) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => return Ok(None),
                Err(e) => return Err(format!("Map NV12: {e}")),
            }
            let pitch = mapped.RowPitch as usize;
            let (w, h) = (dw as usize, dh as usize);
            let src = mapped.pData as *const u8;
            let mut data = Vec::<u8>::with_capacity(w * h * 3 / 2);
            // Y plane, then the interleaved UV plane right after it (`h` rows in).
            for y in 0..h {
                data.extend_from_slice(std::slice::from_raw_parts(src.add(y * pitch), w));
            }
            for y in 0..h / 2 {
                data.extend_from_slice(std::slice::from_raw_parts(src.add((h + y) * pitch), w));
            }
            self.d3d.context.Unmap(stage, 0);
            p.pending.pop_front();
            Ok(Some(RawFrame { width: dw, height: dh, data, gpu: None, nv12: true }))
        }
    }
}
