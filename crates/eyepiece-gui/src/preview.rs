//! In-process preview: the exact encoded stream (what the goggles get) decoded by libavcodec
//! on a low-priority thread and published as a small RGBA image.
//!
//! Isolation from the stream: the session's AU tap ([`Preview::tap`]) runs on the
//! encode/send thread right after the socket write and only copies the AU into a bounded
//! channel with `try_send`. When the preview is off it returns before copying; when the
//! decoder falls behind, the AU is dropped (never waited for) and the preview skips to the
//! next keyframe. Decoding is software, at most 2 threads, loop filter skipped, niced on Linux;
//! frames are decimated to `max_fps` and downscaled to `width` before the UI sees them.

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
    let mut last_pub: Option<Instant> = None;
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
        let fps = knobs.max_fps.load(Ordering::Relaxed).max(1);
        let due = last_pub.is_none_or(|t| t.elapsed().as_secs_f64() >= 1.0 / fps as f64);
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
                        last_pub = Some(Instant::now());
                        on_image();
                    }
                }
                gap = false;
            }
            Ok(None) => {}
            Err(_) => gap = true,
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
    unsafe {
        let fr = &*f.0;
        let (y_p, y_s) = (fr.data[0], fr.linesize[0] as isize);
        let (u_p, u_s) = (fr.data[1], fr.linesize[1] as isize);
        let (v_p, v_s) = (fr.data[2], fr.linesize[2] as isize);
        for oy in 0..out_h {
            let sy = (oy * h / out_h) as isize;
            let yrow = y_p.offset(sy * y_s);
            let crow = sy / 2;
            for ox in 0..out_w {
                let sx = (ox * w / out_w) as isize;
                let yv = *yrow.offset(sx) as f32;
                let (u, v) = if nv12 {
                    let c = u_p.offset(crow * u_s + (sx & !1));
                    (*c as f32, *c.offset(1) as f32)
                } else {
                    (*u_p.offset(crow * u_s + sx / 2) as f32, *v_p.offset(crow * v_s + sx / 2) as f32)
                };
                let (yy, cb, cr) = if full {
                    (yv, u - 128.0, v - 128.0)
                } else {
                    ((yv - 16.0) * 1.164_383, (u - 128.0) * 1.138_393, (v - 128.0) * 1.138_393)
                };
                let r = yy + 1.5748 * cr;
                let g = yy - 0.187_324 * cb - 0.468_124 * cr;
                let b = yy + 1.8556 * cb;
                let o = (oy * out_w + ox) * 4;
                rgba[o] = r.clamp(0.0, 255.0) as u8;
                rgba[o + 1] = g.clamp(0.0, 255.0) as u8;
                rgba[o + 2] = b.clamp(0.0, 255.0) as u8;
                rgba[o + 3] = 255;
            }
        }
    }
    Some(Image { w: out_w, h: out_h, rgba, seq: 0 })
}
