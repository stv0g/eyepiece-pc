//! In-process preview: the exact encoded stream (what the goggles get) decoded by libavcodec
//! on a low-priority thread and published as a small RGBA image.
//!
//! Isolation from the stream: the session's AU tap ([`Preview::tap`]) runs on the
//! encode/send thread right after the socket write and only copies the AU into a bounded
//! channel with `try_send`. When the preview is off it returns before copying; when the
//! decoder falls behind, the AU is dropped (never waited for) and the preview skips to the
//! next keyframe. Decoding is software, at most 2 threads, loop filter skipped, niced on Linux;
//! every n-th frame (n from the stream rate: about `max_fps`, evenly spaced) is
//! downscaled to `width` before the UI sees it.

use eyepiece_pc::av::ffi::*;
use eyepiece_pc::av::Codec;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// AUs queued for the decoder before the tap starts dropping (about 80 ms at 100 fps).
const QUEUE: usize = 8;

pub struct Image {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

/// Settings the UI may change while running.
pub struct Knobs {
    pub enabled: AtomicBool,
    /// only keyframes reach the decoder (lightest: ~0.5 fps at the default GOP)
    pub keyframes_only: AtomicBool,
    pub max_fps: AtomicU32,
    pub width: AtomicU32,
}

#[derive(Default)]
pub struct PreviewStats {
    pub decoded: AtomicU64,
    pub shown: AtomicU64,
    /// AUs not handed to the decoder because its queue was full (→ resync at next IDR)
    pub dropped: AtomicU64,
    /// decode time of the last frame, µs
    pub dec_us: AtomicU32,
}

pub struct Preview {
    pub knobs: Arc<Knobs>,
    pub stats: Arc<PreviewStats>,
    pub image: Arc<Mutex<Option<Image>>>,
    tx: SyncSender<Msg>,
    /// true: drop everything until the next keyframe (start, after a drop)
    wait_key: AtomicBool,
}

enum Msg {
    Au(Vec<u8>, bool),
    Quit,
}

impl Preview {
    /// Start the (idle) decoder thread. `on_image` is called after each published frame
    /// (UI: request a repaint).
    pub fn new(codec: Codec, knobs: Arc<Knobs>, on_image: Arc<dyn Fn() + Send + Sync>) -> Result<Arc<Preview>, String> {
        let (tx, rx) = sync_channel(QUEUE);
        let image = Arc::new(Mutex::new(None));
        let stats = Arc::new(PreviewStats::default());
        let dec = Decoder::open(codec)?;
        {
            let (image, stats, knobs) = (image.clone(), stats.clone(), knobs.clone());
            std::thread::Builder::new()
                .name("ep-gui-preview".into())
                .spawn(move || decode_loop(dec, rx, knobs, stats, image, on_image))
                .map_err(|e| e.to_string())?;
        }
        Ok(Arc::new(Preview { knobs, stats, image, tx, wait_key: AtomicBool::new(true) }))
    }

    /// The session's AU tap (encode/send thread): copy + `try_send`, never blocks.
    pub fn tap(&self, au: &[u8], key: bool) {
        if !self.knobs.enabled.load(Ordering::Relaxed) {
            self.wait_key.store(true, Ordering::Relaxed);
            return;
        }
        if !key && (self.wait_key.load(Ordering::Relaxed) || self.knobs.keyframes_only.load(Ordering::Relaxed)) {
            return;
        }
        match self.tx.try_send(Msg::Au(au.to_vec(), key)) {
            Ok(()) => {
                if key {
                    self.wait_key.store(false, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                self.wait_key.store(true, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // the queue may be full: the decoder also exits when every sender is gone
        let _ = self.tx.try_send(Msg::Quit);
    }
}

fn lower_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // per-thread nice value on Linux (PRIO_PROCESS with a TID)
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

fn decode_loop(
    mut dec: Decoder,
    rx: Receiver<Msg>,
    knobs: Arc<Knobs>,
    stats: Arc<PreviewStats>,
    image: Arc<Mutex<Option<Image>>>,
    on_image: Arc<dyn Fn() + Send + Sync>,
) {
    lower_priority();
    // decimation by frame count (every n-th decoded frame, n from the measured AU rate),
    // not by wall time: a time threshold after a variable decode + convert lands on the 4th
    // or 5th frame (39–54 ms at 100 fps, max 30) — an uneven, visibly juddering cadence
    let mut since_pub: Option<u32> = None;
    let mut last_au: Option<Instant> = None;
    let mut au_dt = 0.0f64; // EMA of the AU interval, s
    let mut seq = 0u64;
    let mut gap = false;
    let mut dropped_seen = 0;
    while let Ok(Msg::Au(au, key)) = rx.recv() {
        // the tap dropped AUs (queue full): it resumes at a keyframe; flush the decoder there
        let d = stats.dropped.load(Ordering::Relaxed);
        if d != dropped_seen {
            dropped_seen = d;
            gap = true;
        }
        if key && gap {
            dec.flush();
        }
        let t0 = Instant::now();
        if let Some(l) = last_au {
            let dt = t0.duration_since(l).as_secs_f64().min(1.0);
            au_dt = if au_dt == 0.0 { dt } else { au_dt + (dt - au_dt) * 0.05 };
        }
        last_au = Some(t0);
        let max_fps = knobs.max_fps.load(Ordering::Relaxed).max(1) as f64;
        let every = if au_dt > 0.0 { (1.0 / au_dt / max_fps).round().max(1.0) as u32 } else { 1 };
        let due = since_pub.is_none_or(|n| n + 1 >= every);
        match dec.decode(&au) {
            Ok(Some(f)) => {
                stats.decoded.fetch_add(1, Ordering::Relaxed);
                stats.dec_us.store(t0.elapsed().as_micros() as u32, Ordering::Relaxed);
                if due {
                    let w = knobs.width.load(Ordering::Relaxed).clamp(160, 1920) as usize;
                    if let Some(mut img) = to_rgba(f, w) {
                        seq += 1;
                        img.seq = seq;
                        *image.lock().unwrap() = Some(img);
                        stats.shown.fetch_add(1, Ordering::Relaxed);
                        since_pub = Some(0);
                        on_image();
                    }
                } else if let Some(n) = since_pub.as_mut() {
                    *n += 1;
                }
                gap = false;
            }
            Ok(None) => {}
            Err(_) => {
                gap = true;
                since_pub = None; // show the first good frame after the resync at once
            }
        }
    }
}

struct Decoder {
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    frame: Frame,
}
unsafe impl Send for Decoder {}

impl Decoder {
    fn open(codec: Codec) -> Result<Decoder, String> {
        let id = match codec {
            Codec::H264 => AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => AVCodecID::AV_CODEC_ID_HEVC,
        };
        unsafe {
            let c = avcodec_find_decoder(id);
            if c.is_null() {
                return Err(format!("preview: no {codec:?} decoder in this FFmpeg build"));
            }
            let ctx = avcodec_alloc_context3(c);
            if ctx.is_null() {
                return Err("preview: avcodec_alloc_context3".into());
            }
            // light and bounded: 2 slice threads (the stream has several slices), no
            // frame-thread delay, loop filter skipped (preview only)
            (*ctx).thread_count = 2;
            (*ctx).thread_type = FF_THREAD_SLICE as i32;
            (*ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;
            (*ctx).skip_loop_filter = AVDiscard::AVDISCARD_ALL;
            let mut ctx = ctx;
            if let Err(e) = check(avcodec_open2(ctx, c, std::ptr::null_mut()), "preview: avcodec_open2") {
                avcodec_free_context(&mut ctx);
                return Err(e);
            }
            Ok(Decoder { ctx, pkt: av_packet_alloc(), frame: Frame::alloc() })
        }
    }

    fn flush(&mut self) {
        unsafe { avcodec_flush_buffers(self.ctx) };
    }

    /// One AU in, at most the newest decoded frame out (low delay: normally that AU).
    fn decode(&mut self, au: &[u8]) -> Result<Option<&Frame>, String> {
        unsafe {
            check(av_new_packet(self.pkt, au.len() as i32), "av_new_packet")?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), (*self.pkt).data, au.len());
            let r = avcodec_send_packet(self.ctx, self.pkt);
            av_packet_unref(self.pkt);
            check(r, "avcodec_send_packet")?;
            // low delay, no reordering: one AU in → at most one frame out
            av_frame_unref(self.frame.0);
            let r = avcodec_receive_frame(self.ctx, self.frame.0);
            if r == EAGAIN_ || r == AVERROR_EOF_ {
                return Ok(None);
            }
            check(r, "avcodec_receive_frame")?;
            Ok(Some(&self.frame))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.pkt);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

/// Nearest-neighbour downscale + BT.709 limited-range YUV → RGBA. 8-bit 4:2:0 only
/// (yuv420p / yuvj420p / nv12: what the encoders here produce).
fn to_rgba(f: &Frame, out_w: usize) -> Option<Image> {
    let (w, h, fmt) = (f.width() as usize, f.height() as usize, f.format());
    if w == 0 || h == 0 {
        return None;
    }
    let planar = fmt == AVPixelFormat::AV_PIX_FMT_YUV420P as i32 || fmt == AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
    let nv12 = fmt == AVPixelFormat::AV_PIX_FMT_NV12 as i32;
    if !planar && !nv12 {
        return None;
    }
    let full = fmt == AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
    let out_w = out_w.min(w);
    let out_h = (h * out_w / w).max(1);
    let mut rgba = vec![0u8; out_w * out_h * 4];
    // 16.16 fixed point; source columns once per frame
    let k = |x: f32| (x * 65536.0).round() as i32;
    let (ky, kc) = if full { (k(1.0), k(1.0)) } else { (k(1.164_383), k(1.138_393)) };
    let y0 = if full { 0 } else { 16 };
    let (kr_v, kg_u, kg_v, kb_u) = (k(1.5748), k(0.187_324), k(0.468_124), k(1.8556));
    let xs: Vec<(usize, usize)> = (0..out_w)
        .map(|ox| {
            let sx = ox * w / out_w;
            (sx, if nv12 { sx & !1 } else { sx / 2 })
        })
        .collect();
    let px = |v: i32| (v >> 16).clamp(0, 255) as u8;
    unsafe {
        let fr = &*f.0;
        let (y_p, y_s) = (fr.data[0], fr.linesize[0] as isize);
        let (u_p, u_s) = (fr.data[1], fr.linesize[1] as isize);
        let (v_p, v_s) = (fr.data[2], fr.linesize[2] as isize);
        for (oy, out) in rgba.chunks_exact_mut(out_w * 4).enumerate() {
            let sy = (oy * h / out_h) as isize;
            let yrow = y_p.offset(sy * y_s);
            let urow = u_p.offset(sy / 2 * u_s);
            let vrow = v_p.offset(sy / 2 * v_s);
            for (o, &(sx, cx)) in out.chunks_exact_mut(4).zip(&xs) {
                let yv = *yrow.add(sx) as i32;
                let (u, v) = if nv12 { (*urow.add(cx), *urow.add(cx + 1)) } else { (*urow.add(cx), *vrow.add(cx)) };
                let yy = (yv - y0) * ky;
                let cb = (u as i32 - 128) * kc >> 16;
                let cr = (v as i32 - 128) * kc >> 16;
                o[0] = px(yy + kr_v * cr);
                o[1] = px(yy - kg_u * cb - kg_v * cr);
                o[2] = px(yy + kb_u * cb);
                o[3] = 255;
            }
        }
    }
    Some(Image { w: out_w, h: out_h, rgba, seq: 0 })
}
