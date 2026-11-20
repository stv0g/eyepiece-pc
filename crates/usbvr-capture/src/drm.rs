//! Minimal libdrm bindings (stable ABI structs from xf86drm.h / xf86drmMode.h) and a safe
//! wrapper for what capture needs: planes, CRTCs, connectors, framebuffers, vblank.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, CStr};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

#[repr(C)]
pub struct drmModeRes {
    pub count_fbs: c_int,
    pub fbs: *mut u32,
    pub count_crtcs: c_int,
    pub crtcs: *mut u32,
    pub count_connectors: c_int,
    pub connectors: *mut u32,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct drmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: [c_char; 32],
}

#[repr(C)]
pub struct drmModeCrtc {
    pub crtc_id: u32,
    pub buffer_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub mode_valid: c_int,
    pub mode: drmModeModeInfo,
    pub gamma_size: c_int,
}

#[repr(C)]
pub struct drmModeEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
pub struct drmModeConnector {
    pub connector_id: u32,
    pub encoder_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: c_int,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: c_int,
    pub count_modes: c_int,
    pub modes: *mut drmModeModeInfo,
    pub count_props: c_int,
    pub props: *mut u32,
    pub prop_values: *mut u64,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
}

#[repr(C)]
pub struct drmModePlane {
    pub count_formats: u32,
    pub formats: *mut u32,
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
    pub x: u32,
    pub y: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
}

#[repr(C)]
pub struct drmModePlaneRes {
    pub count_planes: u32,
    pub planes: *mut u32,
}

#[repr(C)]
pub struct drmModeFB2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
}

#[repr(C)]
pub struct drmModeObjectProperties {
    pub count_props: u32,
    pub props: *mut u32,
    pub prop_values: *mut u64,
}

#[repr(C)]
pub struct drmModePropertyRes {
    pub prop_id: u32,
    pub flags: u32,
    pub name: [c_char; 32],
    // (values / enums / blobs follow; not accessed)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct drmVBlankReq {
    pub type_: c_uint,
    pub sequence: c_uint,
    pub signal: c_ulong,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct drmVBlankReply {
    pub type_: c_uint,
    pub sequence: c_uint,
    pub tval_sec: c_long,
    pub tval_usec: c_long,
}
#[repr(C)]
pub union drmVBlank {
    pub request: drmVBlankReq,
    pub reply: drmVBlankReply,
}

pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;
pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
pub const DRM_VBLANK_RELATIVE: c_uint = 0x1;
pub const DRM_VBLANK_HIGH_CRTC_SHIFT: c_uint = 1;
pub const DRM_VBLANK_HIGH_CRTC_MASK: c_uint = 0x3e;
pub const DRM_CLOEXEC: u32 = libc::O_CLOEXEC as u32;
pub const DRM_RDWR: u32 = libc::O_RDWR as u32;

#[link(name = "drm")]
extern "C" {
    fn drmSetClientCap(fd: c_int, cap: u64, value: u64) -> c_int;
    fn drmModeGetResources(fd: c_int) -> *mut drmModeRes;
    fn drmModeFreeResources(p: *mut drmModeRes);
    fn drmModeGetCrtc(fd: c_int, id: u32) -> *mut drmModeCrtc;
    fn drmModeFreeCrtc(p: *mut drmModeCrtc);
    fn drmModeGetEncoder(fd: c_int, id: u32) -> *mut drmModeEncoder;
    fn drmModeFreeEncoder(p: *mut drmModeEncoder);
    fn drmModeGetConnector(fd: c_int, id: u32) -> *mut drmModeConnector;
    fn drmModeFreeConnector(p: *mut drmModeConnector);
    fn drmModeGetConnectorTypeName(t: u32) -> *const c_char;
    fn drmModeGetPlaneResources(fd: c_int) -> *mut drmModePlaneRes;
    fn drmModeFreePlaneResources(p: *mut drmModePlaneRes);
    fn drmModeGetPlane(fd: c_int, id: u32) -> *mut drmModePlane;
    fn drmModeFreePlane(p: *mut drmModePlane);
    fn drmModeGetFB2(fd: c_int, id: u32) -> *mut drmModeFB2;
    fn drmModeFreeFB2(p: *mut drmModeFB2);
    fn drmModeObjectGetProperties(fd: c_int, id: u32, t: u32) -> *mut drmModeObjectProperties;
    fn drmModeFreeObjectProperties(p: *mut drmModeObjectProperties);
    fn drmModeGetProperty(fd: c_int, id: u32) -> *mut drmModePropertyRes;
    fn drmModeFreeProperty(p: *mut drmModePropertyRes);
    fn drmPrimeHandleToFD(fd: c_int, handle: u32, flags: u32, prime_fd: *mut c_int) -> c_int;
    fn drmCloseBufferHandle(fd: c_int, handle: u32) -> c_int;
    fn drmWaitVBlank(fd: c_int, vbl: *mut drmVBlank) -> c_int;
}

pub struct Device {
    pub file: File,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct CrtcInfo {
    pub id: u32,
    pub index: u32,
    pub active: bool,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub refresh_hz: f64,
}

#[derive(Debug, Clone)]
pub struct PlaneInfo {
    pub id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub kind: String, // Primary / Overlay / Cursor
}

#[derive(Debug, Clone)]
pub struct ConnectorInfo {
    pub id: u32,
    pub name: String,
    pub connected: bool,
    pub crtc_id: u32,
}

#[derive(Debug, Clone)]
pub struct FbInfo {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

impl Device {
    pub fn open(path: &str) -> Result<Device, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {path}: {e} (user in group 'video'?)"))?;
        let d = Device { file, path: path.into() };
        unsafe { drmSetClientCap(d.fd(), DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) };
        Ok(d)
    }
    pub fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }

    pub fn crtcs(&self) -> Vec<CrtcInfo> {
        let mut out = vec![];
        unsafe {
            let r = drmModeGetResources(self.fd());
            if r.is_null() {
                return out;
            }
            for i in 0..(*r).count_crtcs as usize {
                let id = *(*r).crtcs.add(i);
                let c = drmModeGetCrtc(self.fd(), id);
                if c.is_null() {
                    continue;
                }
                let m = (*c).mode;
                let refresh = if m.htotal > 0 && m.vtotal > 0 {
                    m.clock as f64 * 1000.0 / (m.htotal as f64 * m.vtotal as f64)
                } else {
                    m.vrefresh as f64
                };
                out.push(CrtcInfo {
                    id,
                    index: i as u32,
                    active: (*c).mode_valid != 0,
                    width: m.hdisplay as u32,
                    height: m.vdisplay as u32,
                    x: (*c).x,
                    y: (*c).y,
                    refresh_hz: refresh,
                });
                drmModeFreeCrtc(c);
            }
            drmModeFreeResources(r);
        }
        out
    }

    pub fn connectors(&self) -> Vec<ConnectorInfo> {
        let mut out = vec![];
        unsafe {
            let r = drmModeGetResources(self.fd());
            if r.is_null() {
                return out;
            }
            for i in 0..(*r).count_connectors as usize {
                let id = *(*r).connectors.add(i);
                let c = drmModeGetConnector(self.fd(), id);
                if c.is_null() {
                    continue;
                }
                let name = format!("{}-{}", cstr(drmModeGetConnectorTypeName((*c).connector_type)), (*c).connector_type_id);
                let mut crtc_id = 0;
                if (*c).encoder_id != 0 {
                    let e = drmModeGetEncoder(self.fd(), (*c).encoder_id);
                    if !e.is_null() {
                        crtc_id = (*e).crtc_id;
                        drmModeFreeEncoder(e);
                    }
                }
                out.push(ConnectorInfo { id, name, connected: (*c).connection == 1, crtc_id });
                drmModeFreeConnector(c);
            }
            drmModeFreeResources(r);
        }
        out
    }

    fn plane_type(&self, id: u32) -> String {
        unsafe {
            let p = drmModeObjectGetProperties(self.fd(), id, DRM_MODE_OBJECT_PLANE);
            if p.is_null() {
                return "?".into();
            }
            let mut t = "?".to_string();
            for i in 0..(*p).count_props as usize {
                let pr = drmModeGetProperty(self.fd(), *(*p).props.add(i));
                if pr.is_null() {
                    continue;
                }
                if cstr((*pr).name.as_ptr()) == "type" {
                    t = match *(*p).prop_values.add(i) {
                        0 => "Overlay",
                        1 => "Primary",
                        2 => "Cursor",
                        _ => "?",
                    }
                    .into();
                }
                drmModeFreeProperty(pr);
            }
            drmModeFreeObjectProperties(p);
            t
        }
    }

    pub fn planes(&self) -> Vec<PlaneInfo> {
        let mut out = vec![];
        unsafe {
            let r = drmModeGetPlaneResources(self.fd());
            if r.is_null() {
                return out;
            }
            for i in 0..(*r).count_planes as usize {
                let id = *(*r).planes.add(i);
                if let Some(p) = self.plane(id) {
                    out.push(p);
                }
            }
            drmModeFreePlaneResources(r);
        }
        out
    }

    /// Current state of one plane (cheap: one ioctl, plus the type property).
    pub fn plane(&self, id: u32) -> Option<PlaneInfo> {
        let (crtc_id, fb_id) = self.plane_fb(id)?;
        Some(PlaneInfo { id, crtc_id, fb_id, kind: self.plane_type(id) })
    }

    /// (crtc_id, fb_id) of a plane: one GETPLANE ioctl.
    pub fn plane_fb(&self, id: u32) -> Option<(u32, u32)> {
        unsafe {
            let p = drmModeGetPlane(self.fd(), id);
            if p.is_null() {
                return None;
            }
            let r = ((*p).crtc_id, (*p).fb_id);
            drmModeFreePlane(p);
            Some(r)
        }
    }

    /// Framebuffer metadata. Without CAP_SYS_ADMIN the kernel returns zero handles.
    pub fn fb2(&self, fb_id: u32) -> Option<FbInfo> {
        unsafe {
            let f = drmModeGetFB2(self.fd(), fb_id);
            if f.is_null() {
                return None;
            }
            let i = FbInfo {
                width: (*f).width,
                height: (*f).height,
                fourcc: (*f).pixel_format,
                modifier: if (*f).flags & DRM_MODE_FB_MODIFIERS != 0 {
                    (*f).modifier
                } else {
                    crate::DRM_FORMAT_MOD_INVALID
                },
                handles: (*f).handles,
                pitches: (*f).pitches,
                offsets: (*f).offsets,
            };
            drmModeFreeFB2(f);
            Some(i)
        }
    }

    pub fn prime_fd(&self, handle: u32) -> Result<OwnedFd, String> {
        let mut fd: c_int = -1;
        let r = unsafe { drmPrimeHandleToFD(self.fd(), handle, DRM_CLOEXEC | DRM_RDWR, &mut fd) };
        if r < 0 || fd < 0 {
            // read-only export is enough for capture
            let r = unsafe { drmPrimeHandleToFD(self.fd(), handle, DRM_CLOEXEC, &mut fd) };
            if r < 0 || fd < 0 {
                return Err(format!("drmPrimeHandleToFD: {}", errno()));
            }
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn close_handle(&self, handle: u32) {
        unsafe { drmCloseBufferHandle(self.fd(), handle) };
    }

    /// Wait for the next vblank of CRTC `index`; returns its CLOCK_MONOTONIC time in µs.
    pub fn wait_vblank(&self, crtc_index: u32) -> Result<u64, std::io::Error> {
        let mut v = drmVBlank {
            request: drmVBlankReq {
                type_: DRM_VBLANK_RELATIVE | ((crtc_index << DRM_VBLANK_HIGH_CRTC_SHIFT) & DRM_VBLANK_HIGH_CRTC_MASK),
                sequence: 1,
                signal: 0,
            },
        };
        let r = unsafe { drmWaitVBlank(self.fd(), &mut v) };
        if r != 0 {
            return Err(errno());
        }
        let rep = unsafe { v.reply };
        Ok(rep.tval_sec as u64 * 1_000_000 + rep.tval_usec as u64)
    }
}
