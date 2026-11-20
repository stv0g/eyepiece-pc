//! Wayland capture without root: xdg-desktop-portal ScreenCast → PipeWire stream.
//!
//! Frames are timestamped on buffer arrival (the compositor produces one per repaint, so
//! this is refresh-synchronised by construction). DMA-BUF is negotiated first (LINEAR
//! modifier, imported zero-copy into VAAPI like KMS frames); if the compositor refuses,
//! shared memory is used (copied once into a CPU frame).
//!
//! The newest buffer always wins: older unconsumed frames are dropped, never queued.
//! STATUS: compiles; NOT runtime-tested (the portal needs an interactive dialog).

use crate::pacer::Pacer;
use crate::*;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use ::pipewire as pw;
use pw::spa;
use spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use eyepiece_link::clock::mono_us;

#[derive(Debug, Clone)]
pub struct PipeWireConfig {
    pub fps: f64,
    /// where to keep the portal restore token (skips the dialog next time)
    pub restore_token_file: Option<PathBuf>,
    /// cursor drawn into the stream
    pub cursor: bool,
}

#[derive(Default)]
struct Shared {
    latest: Mutex<(Option<Frame>, Option<String>)>,
    cv: Condvar,
    info: Mutex<String>,
}

pub struct PipeWireCapture {
    shared: Arc<Shared>,
    pacer: Pacer,
    node: u32,
    _quit: pw::channel::Sender<()>,
}

const DRM_MOD_LINEAR: i64 = 0;

fn spa_to_fourcc(f: spa::param::video::VideoFormat) -> Option<(u32, CpuFormat)> {
    use spa::param::video::VideoFormat as V;
    Some(match f {
        V::BGRx | V::BGRA => (DRM_FORMAT_XRGB8888, CpuFormat::Bgrx),
        V::RGBx | V::RGBA => (DRM_FORMAT_XBGR8888, CpuFormat::Rgbx),
        _ => return None,
    })
}

fn serialize(obj: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .unwrap()
        .0
        .into_inner()
}

/// EnumFormat pod: BGRx/BGRA/RGBx/RGBA, any size, framerate up to `fps`;
/// `dmabuf`: with a mandatory LINEAR modifier (→ DMA-BUF buffers).
fn format_pod(fps: u32, dmabuf: bool) -> Vec<u8> {
    use spa::param::format::{FormatProperties as FP, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat as V;
    let mut props = vec![
        pw::spa::pod::property!(FP::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FP::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FP::VideoFormat, Choice, Enum, Id, V::BGRx, V::BGRx, V::BGRA, V::RGBx, V::RGBA),
        pw::spa::pod::property!(
            FP::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            FP::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 1000, denom: 1 }
        ),
    ];
    if dmabuf {
        props.push(Property {
            key: FP::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum { default: DRM_MOD_LINEAR, alternatives: vec![DRM_MOD_LINEAR] },
            ))),
        });
    }
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: props,
    })
}

/// Buffers param: which memory types we accept for the negotiated format.
fn buffers_pod(dmabuf: bool) -> Vec<u8> {
    let mask = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(mask as i32),
        }],
    })
}

/// Portal handshake: returns (PipeWire remote fd, node id). Blocks on the user dialog.
fn portal(cfg: &PipeWireConfig) -> Result<(OwnedFd, u32), String> {
    let token = cfg.restore_token_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    pollster::block_on(async {
        let e = |x: ashpd::Error| format!("screencast portal: {x}");
        let proxy = Screencast::new().await.map_err(e)?;
        let session = proxy.create_session().await.map_err(e)?;
        proxy
            .select_sources(
                &session,
                if cfg.cursor { CursorMode::Embedded } else { CursorMode::Hidden },
                SourceType::Monitor.into(),
                false,
                token.as_deref().map(str::trim),
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .map_err(e)?;
        let resp = proxy.start(&session, None).await.map_err(e)?.response().map_err(e)?;
        let stream = resp.streams().first().ok_or("portal returned no stream")?;
        let node = stream.pipe_wire_node_id();
        if let (Some(t), Some(p)) = (resp.restore_token(), &cfg.restore_token_file) {
            let _ = std::fs::write(p, t);
        }
        let fd = proxy.open_pipe_wire_remote(&session).await.map_err(e)?;
        // the session stays open as long as the D-Bus connection lives: keep it
        std::mem::forget(session);
        std::mem::forget(proxy);
        Ok((fd, node))
    })
}

struct UserData {
    info: spa::param::video::VideoInfoRaw,
    dmabuf: bool,
}

fn run_stream(fd: OwnedFd, node: u32, fps: u32, shared: Arc<Shared>, quit_rx: pw::channel::Receiver<()>) -> Result<(), String> {
    pw::init();
    let e = |x: pw::Error| format!("pipewire: {x}");
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(e)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(e)?;
    let core = context.connect_fd_rc(fd, None).map_err(e)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "eyepiece-pc",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(e)?;
    let sh_p = shared.clone();
    let sh_f = shared.clone();
    let _listener = stream
        .add_local_listener_with_user_data(UserData { info: Default::default(), dmabuf: false })
        .param_changed(move |stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if ud.info.parse(param).is_err() {
                return;
            }
            // a modifier in the fixated format means DMA-BUF was negotiated
            ud.dmabuf = ud.info.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            let s = ud.info.size();
            *sh_p.info.lock().unwrap() = format!(
                "{}x{} {:?} {} @ {}/{}",
                s.width,
                s.height,
                ud.info.format(),
                if ud.dmabuf { format!("dmabuf mod {:#x}", ud.info.modifier()) } else { "shm".into() },
                ud.info.framerate().num,
                ud.info.framerate().denom
            );
            let b = buffers_pod(ud.dmabuf);
            let mut params = [Pod::from_bytes(&b).unwrap()];
            let _ = stream.update_params(&mut params);
        })
        .process(move |stream, ud| {
            // newest buffer wins; the others go straight back to the compositor
            let mut newest = None;
            while let Some(b) = stream.dequeue_buffer() {
                newest = Some(b);
            }
            let Some(mut buf) = newest else { return };
            let t = mono_us();
            let size = ud.info.size();
            let Some((fourcc, cpu_fmt)) = spa_to_fourcc(ud.info.format()) else { return };
            let datas = buf.datas_mut();
            if datas.is_empty() {
                return;
            }
            let d = &mut datas[0];
            let (offset, stride) = (d.chunk().offset(), d.chunk().stride());
            let data = if d.type_() == spa::buffer::DataType::DmaBuf {
                let Ok(fd) = unsafe { BorrowedFd::borrow_raw(d.fd()) }.try_clone_to_owned() else { return };
                let objsize = d.as_raw().maxsize as usize;
                FrameData::DmaBuf(DmaBuf {
                    width: size.width,
                    height: size.height,
                    fourcc,
                    modifier: ud.info.modifier(),
                    objects: vec![DmaObject { fd, size: objsize }],
                    planes: vec![DmaPlane { object: 0, offset, pitch: stride as u32 }],
                })
            } else {
                let Some(bytes) = d.data() else { return };
                let stride = if stride > 0 { stride as usize } else { size.width as usize * 4 };
                let len = stride * size.height as usize;
                let start = offset as usize;
                if bytes.len() < start + len {
                    return;
                }
                FrameData::Cpu(CpuImage {
                    width: size.width,
                    height: size.height,
                    stride,
                    format: cpu_fmt,
                    data: bytes[start..start + len].to_vec(),
                })
            };
            let mut l = sh_f.latest.lock().unwrap();
            l.0 = Some(Frame { t_us: t, t_ready_us: mono_us(), data });
            sh_f.cv.notify_one();
        })
        .register()
        .map_err(e)?;

    let dma = format_pod(fps, true);
    let shm = format_pod(fps, false);
    let mut params = [Pod::from_bytes(&dma).unwrap(), Pod::from_bytes(&shm).unwrap()];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(e)?;
    let ml = mainloop.clone();
    let _q = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
    mainloop.run();
    Ok(())
}

impl PipeWireCapture {
    pub fn open(cfg: &PipeWireConfig) -> Result<PipeWireCapture, String> {
        let (fd, node) = portal(cfg)?;
        let shared = Arc::new(Shared::default());
        let (tx, rx) = pw::channel::channel::<()>();
        let sh = shared.clone();
        let fps = cfg.fps.round() as u32;
        std::thread::Builder::new()
            .name("ep-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_stream(fd, node, fps, sh.clone(), rx) {
                    sh.latest.lock().unwrap().1 = Some(e);
                    sh.cv.notify_one();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(PipeWireCapture { shared, pacer: Pacer::new(cfg.fps, None), node, _quit: tx })
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        let _ = self._quit.send(());
    }
}

impl Capture for PipeWireCapture {
    fn next_frame(&mut self, _force: bool) -> Result<Option<Frame>, String> {
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        let mut g = self.shared.latest.lock().unwrap();
        loop {
            if let Some(e) = g.1.take() {
                return Err(e);
            }
            if let Some(f) = g.0.take() {
                if self.pacer.due(f.t_us) {
                    self.pacer.taken(f.t_us);
                    return Ok(Some(f));
                }
                // above the target rate: drop this one
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            g = self.shared.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
    fn describe(&self) -> String {
        format!("pipewire node {} ({})", self.node, self.shared.info.lock().unwrap())
    }
    fn refresh_hz(&self) -> Option<f64> {
        None
    }
}
