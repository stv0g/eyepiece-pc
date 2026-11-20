//! X11 capture via MIT-SHM (XShmGetImage of the root window). No root needed.
//! On Wayland this only sees XWayland windows (KDE: black) — use kms or pipewire there.
//! Paced by a timer, or by DRM vblank (`sync vblank`, needs read access to the card node).

use crate::kms::VblankClock;
use crate::pacer::Pacer;
use crate::*;
use usbvr_link::clock::mono_us;
use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::ImageFormat;

#[derive(Debug, Clone)]
pub struct X11Config {
    /// display name (None = $DISPLAY)
    pub display: Option<String>,
    /// capture region (w, h, x, y); None = whole root window
    pub region: Option<(u32, u32, i32, i32)>,
    pub fps: f64,
    pub sync: SyncMode,
    /// DRM device used for vblank pacing
    pub vblank_device: String,
}

pub struct X11Capture {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
    seg: u32,
    shmid: i32,
    addr: *mut u8,
    w: u32,
    h: u32,
    x: i16,
    y: i16,
    pacer: Pacer,
    vblank: Option<VblankClock>,
}

// the shm pointer is only used from the capture thread
unsafe impl Send for X11Capture {}

impl X11Capture {
    pub fn open(cfg: &X11Config) -> Result<X11Capture, String> {
        let (conn, screen) = x11rb::connect(cfg.display.as_deref()).map_err(|e| format!("X11 connect: {e}"))?;
        let scr = &conn.setup().roots[screen];
        let root = scr.root;
        let (w, h, x, y) = cfg.region.unwrap_or((scr.width_in_pixels as u32, scr.height_in_pixels as u32, 0, 0));
        if scr.root_depth != 24 && scr.root_depth != 32 {
            return Err(format!("X11 root depth {} not supported (need 24/32)", scr.root_depth));
        }
        conn.shm_query_version().map_err(|e| e.to_string())?.reply().map_err(|e| format!("MIT-SHM unavailable: {e}"))?;
        let size = (w * h * 4) as usize;
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(format!("shmget: {}", std::io::Error::last_os_error()));
        }
        let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) } as *mut u8;
        // mark for removal now: freed when both we and the server detach
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        let r = conn.shm_attach(seg, shmid as u32, false).map_err(|e| e.to_string())?.check();
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        r.map_err(|e| format!("XShmAttach: {e} (remote X server?)"))?;
        let vblank = if cfg.sync == SyncMode::Vblank {
            match VblankClock::open(&cfg.vblank_device) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("[usbvr] x11: no vblank clock ({e}), timer pacing");
                    None
                }
            }
        } else {
            None
        };
        let refresh = vblank.as_ref().map(|v| v.refresh_hz());
        Ok(X11Capture {
            conn,
            root,
            seg,
            shmid,
            addr,
            w,
            h,
            x: x as i16,
            y: y as i16,
            pacer: Pacer::new(cfg.fps, refresh),
            vblank,
        })
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        let _ = self.conn.shm_detach(self.seg);
        let _ = self.conn.flush();
        unsafe { libc::shmdt(self.addr as *const _) };
        let _ = self.shmid;
    }
}

impl Capture for X11Capture {
    fn next_frame(&mut self, _force: bool) -> Result<Option<Frame>, String> {
        let t = loop {
            let t = match &self.vblank {
                Some(v) => v.wait().unwrap_or_else(mono_us),
                None => {
                    usbvr_link::clock::sleep_until_us(self.pacer.next_due());
                    mono_us()
                }
            };
            if self.pacer.due(t) {
                self.pacer.taken(t);
                break t;
            }
        };
        self.conn
            .shm_get_image(self.root, self.x, self.y, self.w as u16, self.h as u16, !0, ImageFormat::Z_PIXMAP.into(), self.seg, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("XShmGetImage: {e}"))?;
        let n = (self.w * self.h * 4) as usize;
        let data = unsafe { std::slice::from_raw_parts(self.addr, n) }.to_vec();
        Ok(Some(Frame {
            t_us: t,
            t_ready_us: mono_us(),
            data: FrameData::Cpu(CpuImage {
                width: self.w,
                height: self.h,
                stride: self.w as usize * 4,
                format: CpuFormat::Bgrx,
                data,
            }),
        }))
    }
    fn describe(&self) -> String {
        format!("x11 {}x{}+{}+{} (XShm, {})", self.w, self.h, self.x, self.y, if self.vblank.is_some() { "vblank" } else { "timer" })
    }
    fn refresh_hz(&self) -> Option<f64> {
        self.vblank.as_ref().map(|v| v.refresh_hz())
    }
}
