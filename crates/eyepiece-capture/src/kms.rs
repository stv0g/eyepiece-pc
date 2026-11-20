//! Linux KMS/DRM capture: exports the framebuffer currently scanned out on a plane as
//! DMA-BUF (zero-copy; the encoder side imports it into VAAPI), synchronized to that
//! CRTC's vblank. Works under any compositor (KWin/Mutter/wlroots/Xorg).
//!
//! Needs CAP_SYS_ADMIN: drmModeGetFB2 only returns buffer handles to privileged callers
//! (`sudo setcap cap_sys_admin+ep <binary>`). Opening the card node needs group `video`.
//!
//! 10-bit framebuffers (AR30/AB30/XR30/XB30, KWin 6 default) are handled directly:
//! the alpha variants are relabelled as their X twins (same layout, see `opaque_fourcc`).

use crate::drm::{CrtcInfo, Device};
use crate::pacer::Pacer;
use crate::*;
use eyepiece_link::clock::mono_us;

#[derive(Debug, Clone)]
pub struct KmsConfig {
    pub device: String,
    /// plane id; 0 = auto (primary plane of `output`, else the first active primary plane)
    pub plane: u32,
    /// connector name, e.g. "eDP-1", "DP-2"
    pub output: Option<String>,
    pub fps: f64,
    pub sync: SyncMode,
    /// testing without CAP_SYS_ADMIN: open even if buffer handles are hidden (only
    /// `next_event` is usable then)
    pub simulate: bool,
}

pub struct KmsCapture {
    dev: Device,
    plane: u32,
    crtc: CrtcInfo,
    sync: SyncMode,
    pacer: Pacer,
    last_fb: u32,
    dirty: bool,
    vblank_ok: bool,
    fb_desc: String,
}

/// One line per plane / connector (for --list).
pub fn list(device: &str) -> Result<String, String> {
    let dev = Device::open(device)?;
    let crtcs = dev.crtcs();
    let conns = dev.connectors();
    let mut s = format!("{device}:\n");
    for c in &conns {
        if !c.connected {
            continue;
        }
        let cr = crtcs.iter().find(|x| x.id == c.crtc_id);
        s += &format!(
            "  output {:<10} crtc {:>3} {}\n",
            c.name,
            c.crtc_id,
            cr.map(|x| format!("{}x{} @ {:.2} Hz at +{}+{}", x.width, x.height, x.refresh_hz, x.x, x.y))
                .unwrap_or_else(|| "off".into())
        );
    }
    for p in dev.planes() {
        if p.fb_id == 0 {
            continue;
        }
        let out = conns.iter().find(|c| c.crtc_id == p.crtc_id).map(|c| c.name.clone()).unwrap_or_default();
        let fb = dev.fb2(p.fb_id);
        let fbs = fb
            .as_ref()
            .map(|f| {
                format!(
                    "{}x{} {} mod {:#x}{}",
                    f.width,
                    f.height,
                    fourcc_str(f.fourcc),
                    f.modifier,
                    if f.handles[0] == 0 { " (no handles: needs CAP_SYS_ADMIN)" } else { "" }
                )
            })
            .unwrap_or_else(|| "?".into());
        if let Some(f) = &fb {
            for h in dedup_handles(&f.handles) {
                dev.close_handle(h);
            }
        }
        s += &format!("  plane {:>3} {:<7} crtc {:>3} {:<10} fb {:>3}: {}\n", p.id, p.kind, p.crtc_id, out, p.fb_id, fbs);
    }
    Ok(s)
}

fn dedup_handles(h: &[u32; 4]) -> Vec<u32> {
    let mut v: Vec<u32> = h.iter().copied().filter(|&x| x != 0).collect();
    v.sort();
    v.dedup();
    v
}

impl KmsCapture {
    pub fn open(cfg: &KmsConfig) -> Result<KmsCapture, String> {
        let dev = Device::open(&cfg.device)?;
        let crtcs = dev.crtcs();
        let planes = dev.planes();
        let plane = if cfg.plane != 0 {
            planes.iter().find(|p| p.id == cfg.plane).ok_or_else(|| format!("plane {} not found", cfg.plane))?.clone()
        } else {
            let want_crtc = match &cfg.output {
                Some(o) => Some(
                    dev.connectors()
                        .into_iter()
                        .find(|c| c.name.eq_ignore_ascii_case(o))
                        .ok_or_else(|| format!("output {o} not found (see --list)"))?
                        .crtc_id,
                ),
                None => None,
            };
            planes
                .iter()
                .find(|p| p.kind == "Primary" && p.fb_id != 0 && want_crtc.map_or(true, |c| c == p.crtc_id))
                .ok_or("no active primary plane found (see --list)")?
                .clone()
        };
        let crtc = crtcs
            .iter()
            .find(|c| c.id == plane.crtc_id)
            .cloned()
            .ok_or_else(|| format!("plane {} is not on an active CRTC", plane.id))?;
        // capability check up front, with the fix in the message
        let fb = dev.fb2(plane.fb_id).ok_or("drmModeGetFB2 failed")?;
        if fb.handles[0] == 0 && !cfg.simulate {
            return Err(format!(
                "no framebuffer access on {}: the kernel hides buffer handles without CAP_SYS_ADMIN.\n  \
                 grant it to this binary: sudo setcap cap_sys_admin+ep {}",
                cfg.device,
                std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "<eyepiece-pc>".into())
            ));
        }
        for h in dedup_handles(&fb.handles) {
            dev.close_handle(h);
        }
        let fb_desc = format!(
            "{}x{} {}{} mod {:#x}",
            fb.width,
            fb.height,
            fourcc_str(fb.fourcc),
            if opaque_fourcc(fb.fourcc) != fb.fourcc { format!("→{}", fourcc_str(opaque_fourcc(fb.fourcc))) } else { String::new() },
            fb.modifier
        );
        let sync = cfg.sync;
        Ok(KmsCapture {
            pacer: Pacer::new(cfg.fps, if sync == SyncMode::Timer { None } else { Some(crtc.refresh_hz) }),
            dev,
            plane: plane.id,
            crtc,
            sync,
            last_fb: 0,
            dirty: true,
            vblank_ok: true,
            fb_desc,
        })
    }

    fn export(&self, fb_id: u32, t_us: u64) -> Result<Frame, String> {
        let fb = self.dev.fb2(fb_id).ok_or("drmModeGetFB2 failed")?;
        let handles = dedup_handles(&fb.handles);
        let mut objects = vec![];
        let mut planes = vec![];
        let mut err = None;
        for &h in &handles {
            match self.dev.prime_fd(h) {
                Ok(fd) => {
                    use std::os::fd::AsRawFd;
                    let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
                    objects.push(DmaObject { fd, size: size.max(0) as usize });
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        for i in 0..4 {
            if fb.handles[i] == 0 {
                break;
            }
            let obj = handles.iter().position(|&h| h == fb.handles[i]).unwrap();
            planes.push(DmaPlane { object: obj, offset: fb.offsets[i], pitch: fb.pitches[i] });
        }
        // the GEM handles GetFB2 created are ours: close them (the dma-buf fds keep the buffer)
        for &h in &handles {
            self.dev.close_handle(h);
        }
        if let Some(e) = err {
            return Err(e);
        }
        if handles.is_empty() {
            return Err("framebuffer without handles (CAP_SYS_ADMIN lost?)".into());
        }
        Ok(Frame {
            t_us,
            t_ready_us: mono_us(),
            data: FrameData::DmaBuf(DmaBuf {
                width: fb.width,
                height: fb.height,
                fourcc: opaque_fourcc(fb.fourcc),
                modifier: fb.modifier,
                objects,
                planes,
            }),
        })
    }

    /// Wait for the next refresh event; returns its timestamp.
    fn wait_event(&mut self) -> u64 {
        match self.sync {
            SyncMode::Vblank if self.vblank_ok => match self.dev.wait_vblank(self.crtc.index) {
                Ok(t) => t,
                Err(e) => {
                    // display off / driver without vblank: fall back to the timer
                    eprintln!("[eyepiece-pc] kms: drmWaitVBlank failed ({e}); falling back to timer pacing");
                    self.vblank_ok = false;
                    mono_us()
                }
            },
            SyncMode::Poll => {
                std::thread::sleep(std::time::Duration::from_micros(1000));
                mono_us()
            }
            _ => {
                let due = self.pacer.next_due();
                eyepiece_link::clock::sleep_until_us(due);
                mono_us()
            }
        }
    }
}

impl KmsCapture {
    /// The pacing half of `next_frame`: wait until a frame is due, return (fb_id, t_us).
    /// Usable without CAP_SYS_ADMIN (GETPLANE and WAIT_VBLANK need no privileges).
    pub fn next_event(&mut self, force: bool) -> Result<Option<(u32, u64)>, String> {
        let t_start = mono_us();
        loop {
            let t = self.wait_event();
            let (crtc, fb) = self.dev.plane_fb(self.plane).ok_or("drmModeGetPlane failed")?;
            if fb == 0 || crtc == 0 {
                // plane switched off (DPMS / output change): nothing to grab
                if t - t_start > 100_000 {
                    return Ok(None);
                }
                if self.sync != SyncMode::Timer {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                continue;
            }
            if fb != self.last_fb {
                self.dirty = true;
                self.last_fb = fb;
            }
            let timer = self.sync == SyncMode::Timer || !self.vblank_ok && self.sync == SyncMode::Vblank;
            if self.pacer.due(t) && (self.dirty || force || timer) {
                self.pacer.taken(t);
                self.dirty = false;
                return Ok(Some((fb, t)));
            }
            if t.saturating_sub(t_start) > 100_000 {
                return Ok(None);
            }
        }
    }

    /// Size and (opaque) fourcc of the plane's current framebuffer.
    pub fn fb_size(&self) -> Option<(u32, u32, u32)> {
        let (_, fb) = self.dev.plane_fb(self.plane)?;
        let f = self.dev.fb2(fb)?;
        for h in dedup_handles(&f.handles) {
            self.dev.close_handle(h);
        }
        Some((f.width, f.height, opaque_fourcc(f.fourcc)))
    }
}

impl Capture for KmsCapture {
    fn next_frame(&mut self, force: bool) -> Result<Option<Frame>, String> {
        match self.next_event(force)? {
            Some((fb, t)) => self.export(fb, t).map(Some),
            None => Ok(None),
        }
    }

    fn describe(&self) -> String {
        format!(
            "kms {} plane {} (crtc {} {}x{} @ {:.2} Hz) fb {} sync {:?}",
            self.dev.path, self.plane, self.crtc.id, self.crtc.width, self.crtc.height, self.crtc.refresh_hz, self.fb_desc, self.sync
        )
    }

    fn refresh_hz(&self) -> Option<f64> {
        Some(self.crtc.refresh_hz)
    }
}

/// Waits for vblanks of the first active CRTC of a DRM device (refresh-synced pacing for
/// backends without their own frame clock, e.g. X11).
pub struct VblankClock {
    dev: Device,
    crtc: CrtcInfo,
}
impl VblankClock {
    pub fn open(device: &str) -> Result<VblankClock, String> {
        let dev = Device::open(device)?;
        let crtc = dev.crtcs().into_iter().find(|c| c.active).ok_or("no active CRTC")?;
        dev.wait_vblank(crtc.index).map_err(|e| format!("drmWaitVBlank: {e}"))?;
        Ok(VblankClock { dev, crtc })
    }
    pub fn wait(&self) -> Option<u64> {
        self.dev.wait_vblank(self.crtc.index).ok()
    }
    pub fn refresh_hz(&self) -> f64 {
        self.crtc.refresh_hz
    }
}
