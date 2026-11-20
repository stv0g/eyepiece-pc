//! One streaming session: source → convert → encode → bridge, plus stats, preview, dump.
//!
//! Single pipeline thread, no queues: a frame is captured when due (display refresh),
//! converted, encoded and its AU written to the socket before the next one is taken.
//! If anything is slow, the next refresh is simply missed (drop, never queue).

use crate::preview::Preview;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use usbvr_av::encoder;
use usbvr_av::{ConvertConfig, Converter, Encoder, EncoderConfig, Packet, Source};
use usbvr_capture::SyncMode;
use usbvr_link::clock::mono_us;
use usbvr_link::stats::{Interval, Snapshot};
use usbvr_link::{Link, LinkOptions, LogFn};

#[derive(Debug, Clone)]
pub enum SourceKind {
    #[cfg(target_os = "linux")]
    Kms { device: String, plane: u32, output: Option<String>, simulate: bool },
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11 { display: Option<String>, region: Option<(u32, u32, i32, i32)>, vblank_device: String },
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    PipeWire { restore_token_file: Option<PathBuf>, cursor: bool },
    #[cfg(windows)]
    Dxgi { adapter: u32, output: u32 },
    Test { lavfi: Option<String> },
    File { path: String },
}

#[derive(Clone)]
pub struct Config {
    pub source: SourceKind,
    pub sync: SyncMode,
    /// DRM node that owns captured DMA-BUFs (KMS device / PipeWire's GPU)
    pub drm_device: String,
    /// encoder settings; `fps` and `gop` may be lowered to the source refresh
    pub enc: EncoderConfig,
    pub letterbox: bool,
    pub gpu_scale_cpu_input: bool,
    pub vf: Option<String>,
    pub scale_mode: String,
    /// false: head tracking only
    pub video: bool,
    /// None: no bridge (encode only, e.g. with `dump`)
    pub link: Option<LinkOptions>,
    pub reconnect: bool,
    pub stop_bridge: bool,
    pub dump: Option<PathBuf>,
    pub preview: Option<(i32, i32)>,
    pub stats_interval: f64,
    pub max_frames: Option<u64>,
    pub duration_s: Option<f64>,
}

/// Remote control for a running session (CLI stdin, a UI, signals).
#[derive(Default)]
pub struct Control {
    pub stop: AtomicBool,
    pub recenter: AtomicBool,
    pub idr: AtomicBool,
}

pub struct Events {
    pub log: LogFn,
    pub stats: Arc<dyn Fn(&Snapshot) + Send + Sync>,
}

fn make_source(cfg: &Config) -> Result<Box<dyn Source>, String> {
    let fps = cfg.enc.fps as f64;
    Ok(match &cfg.source {
        #[cfg(target_os = "linux")]
        SourceKind::Kms { device, plane, output, simulate } => {
            let k = usbvr_capture::kms::KmsCapture::open(&usbvr_capture::kms::KmsConfig {
                device: device.clone(),
                plane: *plane,
                output: output.clone(),
                fps,
                sync: cfg.sync,
                simulate: *simulate,
            })?;
            if *simulate {
                Box::new(usbvr_av::simkms::SimKmsSource::new(k, device, &cfg.enc.vaapi_device)?)
            } else {
                Box::new(usbvr_av::CaptureSource::new(Box::new(k), device))
            }
        }
        #[cfg(all(target_os = "linux", feature = "x11"))]
        SourceKind::X11 { display, region, vblank_device } => {
            let x = usbvr_capture::x11::X11Capture::open(&usbvr_capture::x11::X11Config {
                display: display.clone(),
                region: *region,
                fps,
                sync: cfg.sync,
                vblank_device: vblank_device.clone(),
            })?;
            Box::new(usbvr_av::CaptureSource::new(Box::new(x), &cfg.drm_device))
        }
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        SourceKind::PipeWire { restore_token_file, cursor } => {
            let p = usbvr_capture::portal::PipeWireCapture::open(&usbvr_capture::portal::PipeWireConfig {
                fps,
                restore_token_file: restore_token_file.clone(),
                cursor: *cursor,
            })?;
            Box::new(usbvr_av::CaptureSource::new(Box::new(p), &cfg.drm_device))
        }
        #[cfg(windows)]
        SourceKind::Dxgi { adapter, output } => {
            let d = usbvr_capture::dxgi::DxgiCapture::open(*adapter, *output, fps)?;
            Box::new(usbvr_av::CaptureSource::new(Box::new(d), ""))
        }
        SourceKind::Test { lavfi } => {
            Box::new(usbvr_av::TestSource::new(cfg.enc.width, cfg.enc.height, cfg.enc.fps, lavfi.as_deref())?)
        }
        SourceKind::File { path } => Box::new(usbvr_av::FileSource::new(path, cfg.enc.fps)?),
    })
}

struct Pending {
    pts: i64,
    t_cap: u64,
    t_conv: u64,
    t_conv_done: u64,
    t_enc_in: u64,
}

/// Connect and announce the stream (VR_STREAM must precede video).
fn connect(cfg: &Config, enc: &EncoderConfig, ev: &Events) -> Result<Option<Arc<Link>>, String> {
    let Some(lo) = &cfg.link else { return Ok(None) };
    let l = Link::connect(lo, ev.log.clone())?;
    (ev.log)(format!("connected to vr_bridge via {}", lo.addr));
    if cfg.video {
        l.send_stream(enc.width as u16, enc.height as u16, enc.codec.wire(), enc.fps).map_err(|e| e.to_string())?;
    }
    Ok(Some(l))
}

pub fn run(mut cfg: Config, ctl: Arc<Control>, ev: Events) -> Result<(), String> {
    let log = ev.log.clone();
    let pc_stats = Arc::new(Mutex::new(Interval::default()));

    // ---- source first: its refresh rate decides the real frame rate ----
    let mut src = if cfg.video { Some(make_source(&cfg)?) } else { None };
    if let Some(s) = &src {
        log(format!("source: {}", s.describe()));
        if let Some(r) = s.refresh_hz() {
            let r_fps = r.round() as u32;
            if cfg.sync != SyncMode::Timer && r_fps < cfg.enc.fps {
                log(format!(
                    "source refreshes at {r:.2} Hz < --fps {}: encoding at {r_fps} fps (rate control needs the real rate)",
                    cfg.enc.fps
                ));
                let gop_s = cfg.enc.gop as f64 / cfg.enc.fps as f64;
                cfg.enc.fps = r_fps;
                cfg.enc.gop = (gop_s * r_fps as f64).round().max(1.0) as u32;
            }
        }
    }
    let enc_name = if cfg.video { encoder::resolve(&cfg.enc.encoder, cfg.enc.codec, &cfg.enc.vaapi_device)? } else { String::new() };
    let family = encoder::family(&enc_name);

    let link: Arc<Mutex<Option<Arc<Link>>>> = Arc::new(Mutex::new(connect(&cfg, &cfg.enc, &ev)?));

    // ---- stats thread ----
    {
        let link = link.clone();
        let pc_stats = pc_stats.clone();
        let ctl = ctl.clone();
        let interval = cfg.stats_interval;
        let on_stats = ev.stats.clone();
        std::thread::Builder::new()
            .name("vr-stats".into())
            .spawn(move || {
                let mut t_last = mono_us();
                while !ctl.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    let now = mono_us();
                    if (now - t_last) as f64 / 1e6 < interval {
                        continue;
                    }
                    let dt = (now - t_last) as f64 / 1e6;
                    t_last = now;
                    let mut iv = link
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|l| l.shared.stats.lock().unwrap().reset_keep_totals())
                        .unwrap_or_default();
                    let pc = pc_stats.lock().unwrap().reset_keep_totals();
                    iv.sent = pc.sent;
                    iv.bytes = pc.bytes;
                    iv.captured = pc.captured;
                    iv.skipped = pc.skipped;
                    iv.idr = pc.idr;
                    iv.cap_send = pc.cap_send;
                    iv.cap_enc = pc.cap_enc;
                    iv.enc = pc.enc;
                    let l = link.lock().unwrap().clone();
                    let rtt = l.as_ref().and_then(|l| l.shared.rtt_us());
                    let pose = l.as_ref().and_then(|l| l.shared.last_pose.lock().unwrap().map(|p| (p.yaw, p.pitch)));
                    on_stats(&Snapshot::from_interval(&iv, dt, rtt, pose));
                }
            })
            .unwrap();
    }

    let mut dump = match &cfg.dump {
        Some(p) => Some(File::create(p).map_err(|e| format!("--dump {}: {e}", p.display()))?),
        None => None,
    };
    let mut preview = match cfg.preview {
        Some(pos) if cfg.video => Some(Preview::spawn(pos, cfg.enc.codec, cfg.enc.fps, &log)?),
        _ => None,
    };

    let convert_cfg = ConvertConfig {
        width: cfg.enc.width,
        height: cfg.enc.height,
        letterbox: cfg.letterbox,
        target: encoder::target_for(family),
        vaapi_device: cfg.enc.vaapi_device.clone(),
        gpu_scale_cpu_input: cfg.gpu_scale_cpu_input,
        extra_vf: cfg.vf.clone(),
        scale_mode: cfg.scale_mode.clone(),
    };

    // ---- stage A (own thread): capture when due + convert on the GPU ----
    // Hands frames to stage B through a single slot: a newer frame replaces one that B
    // hasn't picked up yet (counted as skipped). Two stages let capture/convert of frame
    // N+1 overlap encode/send of frame N without ever holding more than one frame.
    let slot = Arc::new(Slot::default());
    let need_idr = Arc::new(AtomicBool::new(true));
    let conv_fps = cfg.enc.fps;
    let stage_a = src.take().map(|mut src| {
        let slot = slot.clone();
        let ctl = ctl.clone();
        let need_idr = need_idr.clone();
        let pc_stats = pc_stats.clone();
        let log = log.clone();
        std::thread::Builder::new()
            .name("vr-capture".into())
            .spawn(move || {
                let mut conv = Converter::new(convert_cfg, conv_fps);
                let mut described = false;
                let r: Result<(), String> = (|| {
                    while !ctl.stop.load(Ordering::Relaxed) {
                        // force a frame of a static screen when an IDR is pending
                        let Some(sf) = src.next(need_idr.load(Ordering::Relaxed))? else { continue };
                        pc_stats.lock().unwrap().captured += 1;
                        let t_conv = mono_us();
                        let Some(frame) = conv.convert(sf.frame)? else { continue };
                        if !described {
                            log(format!("convert: {}", conv.description));
                            described = true;
                        }
                        let item = Item { frame, t_cap: sf.t_us, t_conv, t_conv_done: mono_us() };
                        if slot.put(item) {
                            pc_stats.lock().unwrap().skipped += 1;
                        }
                    }
                    Ok(())
                })();
                slot.close(r);
            })
            .unwrap()
    });

    // ---- stage B (this thread): encode + send, connection management ----
    let mut enc: Option<Encoder> = None;
    let codec_wire = cfg.enc.codec.wire();
    let fps = cfg.enc.fps as f64;
    let mut t0: Option<u64> = None;
    let mut last_pts = -1i64;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let mut pkts: Vec<Packet> = vec![];
    let mut frames_sent = 0u64;
    let t_start = mono_us();
    let mut last_reconnect = 0u64;

    let result: Result<(), String> = (|| {
        loop {
            if ctl.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(d) = cfg.duration_s {
                if (mono_us() - t_start) as f64 / 1e6 >= d {
                    return Ok(());
                }
            }
            if cfg.max_frames.is_some_and(|m| frames_sent >= m) {
                return Ok(());
            }
            // ---- connection state ----
            let cur = link.lock().unwrap().clone();
            if let Some(l) = &cur {
                if !l.alive() {
                    *link.lock().unwrap() = None;
                    if !cfg.reconnect {
                        return Ok(());
                    }
                    log("bridge connection lost, reconnecting…".into());
                    continue;
                }
                if ctl.recenter.swap(false, Ordering::Relaxed) {
                    let _ = l.recenter();
                    log("recenter sent".into());
                }
            } else if cfg.link.is_some() {
                // reconnect loop (only reached with --reconnect); stage A keeps running
                // and its frames are dropped meanwhile
                let _ = slot.take(Duration::from_millis(0));
                if mono_us() - last_reconnect < 1_000_000 {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                last_reconnect = mono_us();
                let mut c2 = cfg.clone();
                if let Some(lo) = c2.link.as_mut() {
                    lo.connect_timeout = Duration::from_millis(500);
                }
                if let Ok(Some(l)) = connect(&c2, &cfg.enc, &ev) {
                    *link.lock().unwrap() = Some(l);
                    need_idr.store(true, Ordering::Relaxed); // the bridge waits for an IDR
                }
                continue;
            }
            if stage_a.is_none() {
                // head tracking only
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let key_req = cur.as_ref().is_some_and(|l| l.take_key_request()) || ctl.idr.swap(false, Ordering::Relaxed);
            if key_req && !need_idr.swap(true, Ordering::Relaxed) {
                log("IDR requested (bridge resync)".into());
            }

            // ---- next converted frame ----
            let it = match slot.take(Duration::from_millis(100)) {
                Taken::Item(it) => it,
                Taken::Empty => continue,
                Taken::Closed(r) => return r,
            };
            let t_enc_in = mono_us();
            let frame = it.frame;
            if enc.is_none() {
                let e = Encoder::open(&cfg.enc, &enc_name, &frame, unsafe { (*frame.0).hw_frames_ctx })?;
                log(format!("encoder: {}", e.description));
                enc = Some(e);
            }
            let base = *t0.get_or_insert(it.t_cap);
            let mut pts = ((it.t_cap.saturating_sub(base)) as f64 * fps / 1e6).round() as i64;
            if pts <= last_pts {
                pts = last_pts + 1;
            }
            last_pts = pts;
            pending.push_back(Pending { pts, t_cap: it.t_cap, t_conv: it.t_conv, t_conv_done: it.t_conv_done, t_enc_in });
            while pending.len() > 32 {
                pending.pop_front();
            }
            let idr_now = need_idr.load(Ordering::Relaxed);
            enc.as_mut().unwrap().encode(frame, pts, idr_now, &mut pkts)?;
            let t_enc_out = mono_us();

            for p in pkts.drain(..) {
                let Some(i) = pending.iter().position(|x| x.pts == p.pts) else { continue };
                let meta = pending.remove(i).unwrap();
                if idr_now && p.key {
                    need_idr.store(false, Ordering::Relaxed);
                    pc_stats.lock().unwrap().idr += 1;
                }
                if let Some(l) = &cur {
                    if let Err(e) = l.send_video(meta.t_cap, codec_wire, p.key, &p.data) {
                        if !cfg.reconnect {
                            return Err(format!("send: {e}"));
                        }
                        continue;
                    }
                }
                let t_sent = mono_us();
                {
                    let mut s = pc_stats.lock().unwrap();
                    s.sent += 1;
                    s.bytes += p.data.len() as u64;
                    s.cap_send.push((t_sent - meta.t_cap) as u32);
                    // capture→encoder input: capture wait + GPU convert (+ slot wait)
                    s.cap_enc.push((meta.t_enc_in - meta.t_cap) as u32);
                    s.enc.push((t_enc_out - meta.t_enc_in) as u32);
                    let _ = (meta.t_conv, meta.t_conv_done);
                }
                frames_sent += 1;
                if let Some(d) = dump.as_mut() {
                    d.write_all(&p.data).map_err(|e| format!("dump: {e}"))?;
                }
                if let Some(pv) = preview.as_mut() {
                    pv.feed(&p.data);
                }
            }
        }
    })();

    ctl.stop.store(true, Ordering::Relaxed);
    if let Some(a) = stage_a {
        let _ = a.join();
    }
    if let Some(l) = link.lock().unwrap().take() {
        if cfg.stop_bridge {
            let _ = l.quit_bridge();
        }
        l.close();
    }
    drop(preview);
    drop(enc);
    result
}

struct Item {
    frame: usbvr_av::ffi::Frame,
    t_cap: u64,
    t_conv: u64,
    t_conv_done: u64,
}

enum Taken {
    Item(Item),
    Empty,
    Closed(Result<(), String>),
}

/// One-frame mailbox between the stages: `put` replaces an unclaimed frame.
#[derive(Default)]
struct Slot {
    m: Mutex<(Option<Item>, Option<Result<(), String>>)>,
    cv: std::sync::Condvar,
}
impl Slot {
    /// true if an older frame was replaced (dropped)
    fn put(&self, it: Item) -> bool {
        let mut g = self.m.lock().unwrap();
        let dropped = g.0.replace(it).is_some();
        self.cv.notify_one();
        dropped
    }
    fn close(&self, r: Result<(), String>) {
        self.m.lock().unwrap().1 = Some(r);
        self.cv.notify_one();
    }
    fn take(&self, timeout: Duration) -> Taken {
        let mut g = self.m.lock().unwrap();
        if g.0.is_none() && g.1.is_none() && !timeout.is_zero() {
            g = self.cv.wait_timeout_while(g, timeout, |g| g.0.is_none() && g.1.is_none()).unwrap().0;
        }
        if let Some(it) = g.0.take() {
            return Taken::Item(it);
        }
        match g.1.take() {
            Some(r) => Taken::Closed(r),
            None => Taken::Empty,
        }
    }
}
