//! Test aid: the KMS path without CAP_SYS_ADMIN. Real vblank pacing and page-flip
//! detection on a real plane (GETPLANE / WAIT_VBLANK need no privileges), but the
//! framebuffer content comes from a 3-buffer "swapchain" of VAAPI surfaces in the same
//! 10-bit format, exported as DMA-BUF and fed through the exact same
//! DmaBuf → DRM_PRIME → hwmap(VAAPI) → convert → encode path as real captures.
//! Measures everything except the GetFB2/PrimeHandleToFD export (a few ioctls).

use crate::ffi::*;
use crate::hw::{fourcc_to_pix, hwdevice, DrmFrames};
use crate::source::{Source, SourceFrame};
use std::os::fd::{BorrowedFd, OwnedFd};
use usbvr_capture::kms::KmsCapture;
use usbvr_capture::{DmaBuf, DmaObject, DmaPlane};
use usbvr_link::clock::mono_us;

pub struct SimKmsSource {
    kms: KmsCapture,
    _vaapi: BufRef,
    _frames: BufRef,
    surfaces: Vec<Frame>,
    drm: DrmFrames,
    i: usize,
    desc: String,
}
unsafe impl Send for SimKmsSource {}

impl SimKmsSource {
    pub fn new(kms: KmsCapture, kms_device: &str, vaapi_device: &str) -> Result<SimKmsSource, String> {
        let (w, h, fourcc) = kms.fb_size().ok_or("plane has no framebuffer")?;
        let sw = fourcc_to_pix(fourcc).ok_or("unsupported fb format")?;
        let vaapi = hwdevice(AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some(vaapi_device))?;
        unsafe {
            let r = av_hwframe_ctx_alloc(vaapi.0);
            let fc = (*r).data as *mut AVHWFramesContext;
            (*fc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = sw;
            (*fc).width = w as i32;
            (*fc).height = h as i32;
            (*fc).initial_pool_size = 4;
            let frames = BufRef(r);
            check(av_hwframe_ctx_init(r), "sim VAAPI frames")?;
            // 3 different pictures, like a compositor swapchain
            let mut g = crate::source::TestSource::new(w, h, 100, Some(&format!(
                "testsrc2=size={w}x{h}:rate=100,format={}",
                pix_fmt_name(sw as i32)
            )))?;
            let mut surfaces = vec![];
            for _ in 0..3 {
                let sf = g.next(false)?.unwrap();
                let s = Frame::alloc();
                check(av_hwframe_get_buffer(frames.0, s.0, 0), "sim surface")?;
                check(av_hwframe_transfer_data(s.0, sf.frame.0, 0), "sim upload")?;
                surfaces.push(s);
            }
            let desc = format!("SIMULATED kms ({}; content: VAAPI {}x{} {} surfaces)", kms_describe(&kms), w, h, pix_fmt_name(sw as i32));
            Ok(SimKmsSource { kms, _vaapi: vaapi, _frames: frames, surfaces, drm: DrmFrames::new(kms_device)?, i: 0, desc })
        }
    }

    fn export(&mut self) -> Result<DmaBuf, String> {
        let s = &self.surfaces[self.i % self.surfaces.len()];
        self.i += 1;
        unsafe {
            let m = Frame::alloc();
            (*m.0).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            check(av_hwframe_map(m.0, s.0, AV_HWFRAME_MAP_READ as i32), "sim map to DRM")?;
            let d = &*((*m.0).data[0] as *const AVDRMFrameDescriptor);
            let mut objects = vec![];
            for o in &d.objects[..d.nb_objects as usize] {
                let fd: OwnedFd = BorrowedFd::borrow_raw(o.fd).try_clone_to_owned().map_err(|e| e.to_string())?;
                objects.push(DmaObject { fd, size: o.size });
            }
            let mut planes = vec![];
            let l = &d.layers[0];
            for p in &l.planes[..l.nb_planes as usize] {
                planes.push(DmaPlane { object: p.object_index as usize, offset: p.offset as u32, pitch: p.pitch as u32 });
            }
            Ok(DmaBuf {
                width: (*s.0).width as u32,
                height: (*s.0).height as u32,
                fourcc: l.format,
                modifier: d.objects[0].format_modifier,
                objects,
                planes,
            })
        }
    }
}

fn kms_describe(k: &KmsCapture) -> String {
    use usbvr_capture::Capture;
    k.describe()
}

impl Source for SimKmsSource {
    fn next(&mut self, force: bool) -> Result<Option<SourceFrame>, String> {
        // simulated content changes every refresh: take every due vblank
        let _ = force;
        let Some((_fb, t)) = self.kms.next_event(true)? else { return Ok(None) };
        let d = self.export()?;
        let frame = self.drm.wrap(d)?;
        Ok(Some(SourceFrame { frame, t_us: t, t_ready_us: mono_us() }))
    }
    fn describe(&self) -> String {
        self.desc.clone()
    }
    fn refresh_hz(&self) -> Option<f64> {
        use usbvr_capture::Capture;
        self.kms.refresh_hz()
    }
}
