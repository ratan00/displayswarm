//! Zero-copy capture for the VA-API encoder: a compositor's DMA-BUF goes into
//! the encoder without ever touching the CPU.
//!
//! Before, every frame was read out of GPU memory by the CPU, scaled and
//! converted to NV12 on the CPU, and uploaded again (8 ms of CPU per 1080p
//! frame after the SIMD scaler, 21 ms before it). Here the DMA-BUF is mapped
//! as a VA surface (no copy), and the video engine's own scaler (VPP, through
//! ffmpeg's `scale_vaapi`) produces the NV12 picture at the encoder's size,
//! letterboxed with `pad_vaapi` when the aspect ratios differ, in the BT.709
//! limited-range colour the encoder signals (see `convert.rs`).
//!
//! The DRM -> VA-API mapping follows ffmpeg's own `hwmap` filter: a DRM
//! frames context describes the buffer, a VA-API frames context derived from
//! it receives the mapping.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::ptr;

use ffmpeg_next::ffi::*;

use super::BoxError;
use crate::capture::scale::fit_rect;
use crate::capture::GpuFrame;

// DRM fourccs (drm_fourcc.h) and the matching packed ffmpeg formats.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24': bytes B G R X
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // 'AR24': bytes B G R A
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258; // 'XB24': bytes R G B X
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241; // 'AB24': bytes R G B A

fn sw_format(fourcc: u32) -> Option<AVPixelFormat> {
    Some(match fourcc {
        DRM_FORMAT_XRGB8888 => AVPixelFormat::AV_PIX_FMT_BGR0,
        DRM_FORMAT_ARGB8888 => AVPixelFormat::AV_PIX_FMT_BGRA,
        DRM_FORMAT_XBGR8888 => AVPixelFormat::AV_PIX_FMT_RGB0,
        DRM_FORMAT_ABGR8888 => AVPixelFormat::AV_PIX_FMT_RGBA,
        _ => return None,
    })
}

fn check(r: i32, what: &str) -> Result<(), BoxError> {
    if r < 0 {
        Err(format!("{what}: {}", super::ffmpeg::av_err(r)).into())
    } else {
        Ok(())
    }
}

/// The filter chain for a `sw`x`sh` source into a `dw`x`dh` NV12 picture.
pub fn filter_chain(sw: u32, sh: u32, dw: u32, dh: u32) -> String {
    let (fx, fy, fw, fh) = fit_rect(sw, sh, dw, dh);
    // VA surfaces want even sizes and offsets.
    let even = |v: u32| v & !1;
    let (fx, fy, fw, fh) = (even(fx), even(fy), even(fw).max(2), even(fh).max(2));
    // All three of matrix, primaries and transfer: with the matrix alone the
    // driver (Intel iHD) kept converting with BT.601, which decodes as BT.709
    // with visibly washed-out greens.
    let mut chain = format!(
        "scale_vaapi=w={fw}:h={fh}:format=nv12:out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709:out_range=tv"
    );
    if (fw, fh) != (dw, dh) {
        chain.push_str(&format!(",pad_vaapi=w={dw}:h={dh}:x={fx}:y={fy}:color=black"));
    }
    chain
}

/// Imports DMA-BUF frames of one size and layout and turns them into NV12
/// VA surfaces at the encoder's size. Rebuilt when the source changes.
pub struct VaapiImport {
    drm_device: *mut AVBufferRef,
    drm_frames: *mut AVBufferRef,
    va_frames: *mut AVBufferRef,
    graph: *mut AVFilterGraph,
    src: *mut AVFilterContext,
    sink: *mut AVFilterContext,
    out: *mut AVFrame,
    key: (u32, u32, u32, u64),
}

// Safety: all pointers are owned by this struct and used from one thread.
unsafe impl Send for VaapiImport {}

impl Drop for VaapiImport {
    fn drop(&mut self) {
        // Safety: each pointer is null or owned; the free functions null them.
        unsafe {
            av_frame_free(&mut self.out);
            avfilter_graph_free(&mut self.graph);
            av_buffer_unref(&mut self.va_frames);
            av_buffer_unref(&mut self.drm_frames);
            av_buffer_unref(&mut self.drm_device);
        }
    }
}

impl VaapiImport {
    /// A DRM device on `node` (the encoder's render node). It only describes
    /// the buffers; ffmpeg cannot derive one from a VA-API device.
    fn open_drm(node: &str) -> Result<*mut AVBufferRef, BoxError> {
        let path = CString::new(node).map_err(|_| "bad device path")?;
        let mut drm: *mut AVBufferRef = ptr::null_mut();
        // Safety: plain device creation; the caller owns the reference.
        check(
            unsafe { av_hwdevice_ctx_create(&mut drm, AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, path.as_ptr(), ptr::null_mut(), 0) },
            &format!("open DRM device {node}"),
        )?;
        Ok(drm)
    }

    /// A cheap check that zero-copy can work here at all.
    pub fn supported(va_device: *mut AVBufferRef, node: &str) -> bool {
        if va_device.is_null() {
            return false;
        }
        match Self::open_drm(node) {
            Ok(mut drm) => {
                // Safety: our own reference.
                unsafe { av_buffer_unref(&mut drm) };
                true
            }
            Err(e) => {
                log::debug!("zero-copy: {e}");
                false
            }
        }
    }

    /// Whether frames like `g` can go through this importer as built.
    pub fn matches(&self, g: &GpuFrame) -> bool {
        self.key == (g.width, g.height, g.drm_format, g.modifier)
    }

    /// An importer for frames like `g`, producing `dw`x`dh` NV12 surfaces on
    /// `va_device` (the encoder's device: surfaces must share its VADisplay).
    pub fn new(va_device: *mut AVBufferRef, node: &str, g: &GpuFrame, dw: u32, dh: u32) -> Result<Self, BoxError> {
        let sw = sw_format(g.drm_format).ok_or_else(|| format!("DRM format {:#x} is not supported", g.drm_format))?;
        let mut this = Self {
            drm_device: ptr::null_mut(),
            drm_frames: ptr::null_mut(),
            va_frames: ptr::null_mut(),
            graph: ptr::null_mut(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            out: ptr::null_mut(),
            key: (g.width, g.height, g.drm_format, g.modifier),
        };
        // Safety: plain libav* setup; every object is owned by `this`, whose
        // Drop releases whatever was created if we bail out part-way.
        unsafe {
            this.drm_device = Self::open_drm(node)?;
            this.drm_frames = av_hwframe_ctx_alloc(this.drm_device);
            if this.drm_frames.is_null() {
                return Err("av_hwframe_ctx_alloc(DRM) failed".into());
            }
            let fc = &mut *((*this.drm_frames).data as *mut AVHWFramesContext);
            fc.format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            fc.sw_format = sw;
            fc.width = g.width as i32;
            fc.height = g.height as i32;
            check(av_hwframe_ctx_init(this.drm_frames), "init DRM frames")?;
            check(
                av_hwframe_ctx_create_derived(&mut this.va_frames, AVPixelFormat::AV_PIX_FMT_VAAPI, va_device, this.drm_frames, 0),
                "derive VA-API frames",
            )?;

            this.graph = avfilter_graph_alloc();
            if this.graph.is_null() {
                return Err("avfilter_graph_alloc failed".into());
            }
            let name = CString::new("in").unwrap();
            this.src = avfilter_graph_alloc_filter(this.graph, avfilter_get_by_name(c"buffer".as_ptr()), name.as_ptr());
            if this.src.is_null() {
                return Err("no buffer source".into());
            }
            let par = av_buffersrc_parameters_alloc();
            (*par).format = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*par).width = g.width as i32;
            (*par).height = g.height as i32;
            (*par).time_base = AVRational { num: 1, den: 1_000_000 };
            (*par).sample_aspect_ratio = AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = this.va_frames;
            let r = av_buffersrc_parameters_set(this.src, par);
            av_free(par as *mut _);
            check(r, "buffer source parameters")?;
            check(avfilter_init_str(this.src, ptr::null()), "init buffer source")?;
            let name = CString::new("out").unwrap();
            check(
                avfilter_graph_create_filter(
                    &mut this.sink,
                    avfilter_get_by_name(c"buffersink".as_ptr()),
                    name.as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    this.graph,
                ),
                "create buffer sink",
            )?;

            let chain = filter_chain(g.width, g.height, dw, dh);
            let mut outputs = avfilter_inout_alloc();
            let mut inputs = avfilter_inout_alloc();
            (*outputs).name = av_strdup(c"in".as_ptr());
            (*outputs).filter_ctx = this.src;
            (*outputs).pad_idx = 0;
            (*outputs).next = ptr::null_mut();
            (*inputs).name = av_strdup(c"out".as_ptr());
            (*inputs).filter_ctx = this.sink;
            (*inputs).pad_idx = 0;
            (*inputs).next = ptr::null_mut();
            let spec = CString::new(chain.clone()).unwrap();
            let r = avfilter_graph_parse_ptr(this.graph, spec.as_ptr(), &mut inputs, &mut outputs, ptr::null_mut());
            avfilter_inout_free(&mut inputs);
            avfilter_inout_free(&mut outputs);
            check(r, &format!("parse filters {chain:?}"))?;
            check(avfilter_graph_config(this.graph, ptr::null_mut()), &format!("configure filters {chain:?}"))?;

            this.out = av_frame_alloc();
            if this.out.is_null() {
                return Err("av_frame_alloc failed".into());
            }
            log::info!(
                "zero-copy: importing {}x{} DMA-BUFs (fourcc {:#x}, modifier {:#x}) through {chain}",
                g.width, g.height, g.drm_format, g.modifier
            );
        }
        Ok(this)
    }

    /// Maps `g` and runs it through the filters. The returned NV12 VA frame
    /// belongs to the importer and is valid until the next call.
    pub fn process(&mut self, g: &GpuFrame) -> Result<*mut AVFrame, BoxError> {
        // Safety: the DRM descriptor is owned by the frame's buffer (freed
        // with it); `g.fd` stays open for the whole call, and the VA surface
        // made from it holds its own reference to the buffer afterwards.
        unsafe {
            let mut drm = av_frame_alloc();
            let mut va = av_frame_alloc();
            let result = (|| -> Result<(), BoxError> {
                if drm.is_null() || va.is_null() {
                    return Err("av_frame_alloc failed".into());
                }
                let size = std::mem::size_of::<AVDRMFrameDescriptor>();
                let desc = av_mallocz(size) as *mut AVDRMFrameDescriptor;
                if desc.is_null() {
                    return Err("av_mallocz failed".into());
                }
                (*drm).buf[0] = av_buffer_create(desc as *mut u8, size, Some(av_buffer_default_free), ptr::null_mut(), 0);
                if (*drm).buf[0].is_null() {
                    av_free(desc as *mut _);
                    return Err("av_buffer_create failed".into());
                }
                let d = &mut *desc;
                d.nb_objects = 1;
                d.objects[0].fd = g.fd.as_raw_fd();
                d.objects[0].size = g.offset as usize + g.stride as usize * g.height as usize;
                d.objects[0].format_modifier = g.modifier;
                d.nb_layers = 1;
                d.layers[0].format = g.drm_format;
                d.layers[0].nb_planes = 1;
                d.layers[0].planes[0].object_index = 0;
                d.layers[0].planes[0].offset = g.offset as isize;
                d.layers[0].planes[0].pitch = g.stride as isize;
                (*drm).data[0] = desc as *mut u8;
                (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
                (*drm).width = g.width as i32;
                (*drm).height = g.height as i32;
                (*drm).hw_frames_ctx = av_buffer_ref(self.drm_frames);

                (*va).format = AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
                (*va).hw_frames_ctx = av_buffer_ref(self.va_frames);
                check(av_hwframe_map(va, drm, AV_HWFRAME_MAP_READ as i32), "map DMA-BUF to VA surface")?;
                check(av_buffersrc_add_frame_flags(self.src, va, 0), "feed filters")?;
                av_frame_unref(self.out);
                check(av_buffersink_get_frame(self.sink, self.out), "scale on the GPU")
            })();
            av_frame_free(&mut va);
            av_frame_free(&mut drm);
            result.map(|()| self.out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_aspect_needs_no_padding() {
        assert_eq!(
            filter_chain(3840, 2160, 1920, 1080),
            "scale_vaapi=w=1920:h=1080:format=nv12:out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709:out_range=tv"
        );
    }

    #[test]
    fn wider_target_is_pillarboxed_on_even_offsets() {
        // 16:9 desktop on a 20:9 phone: full height, bars left and right.
        let c = filter_chain(1920, 1080, 1600, 720);
        assert!(c.starts_with("scale_vaapi=w=1280:h=720:"), "{c}");
        assert!(c.ends_with(",pad_vaapi=w=1600:h=720:x=160:y=0:color=black"), "{c}");
    }

    #[test]
    fn known_drm_formats_map_to_packed_rgb() {
        assert_eq!(sw_format(DRM_FORMAT_XRGB8888), Some(AVPixelFormat::AV_PIX_FMT_BGR0));
        assert_eq!(sw_format(0x3231_564e), None, "NV12 is not a capture format");
    }
}
