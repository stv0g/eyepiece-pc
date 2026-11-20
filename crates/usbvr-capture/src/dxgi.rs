//! Windows: DXGI Desktop Duplication. Frames arrive when DWM presents (synchronised to
//! the display by construction) and are stamped with DXGI's LastPresentTime (QPC, same
//! clock as `usbvr_link::clock::mono_us` on Windows).
//!
//! STATUS: written against the documented API and type-checked for
//! x86_64-pc-windows-gnu, NEVER RUN. This first version copies through a staging texture
//! into a CPU BGRA frame (~1–2 ms at 1080p); the zero-copy path (shared D3D11 texture →
//! AV_PIX_FMT_D3D11 → h264_nvenc/h264_amf/h264_qsv) is the obvious next step.

use crate::pacer::Pacer;
use crate::*;
use usbvr_link::clock::{mono_us, qpc_to_us};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct DxgiCapture {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: IDXGIOutputDuplication,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pacer: Pacer,
    qpc_freq: i64,
    desc: String,
}

// COM pointers are only used from the capture thread
unsafe impl Send for DxgiCapture {}

fn qpc_freq() -> i64 {
    extern "system" {
        fn QueryPerformanceFrequency(f: *mut i64) -> i32;
    }
    let mut f = 1i64;
    unsafe { QueryPerformanceFrequency(&mut f) };
    f
}

impl DxgiCapture {
    pub fn open(adapter: u32, output_idx: u32, fps: f64) -> Result<DxgiCapture, String> {
        unsafe {
            let e = |x: windows::core::Error| format!("DXGI: {x}");
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(e)?;
            let ad: IDXGIAdapter1 = factory.EnumAdapters1(adapter).map_err(e)?;
            let out: IDXGIOutput = ad.EnumOutputs(output_idx).map_err(e)?;
            let output: IDXGIOutput1 = out.cast().map_err(e)?;
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &ad,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(e)?;
            let device = device.ok_or("no D3D11 device")?;
            let ctx = ctx.ok_or("no D3D11 context")?;
            let dup = output.DuplicateOutput(&device).map_err(e)?;
            let od = out.GetDesc().map_err(e)?;
            let r = od.DesktopCoordinates;
            let desc = format!("dxgi adapter {adapter} output {output_idx} {}x{}", r.right - r.left, r.bottom - r.top);
            Ok(DxgiCapture { device, ctx, output, dup, staging: None, pacer: Pacer::new(fps, None), qpc_freq: qpc_freq(), desc })
        }
    }

    fn staging(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        if let Some((t, sw, sh)) = &self.staging {
            if (*sw, *sh) == (w, h) {
                return Ok(t.clone());
            }
        }
        let d = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&d, None, Some(&mut t)) }.map_err(|e| format!("staging texture: {e}"))?;
        let t = t.ok_or("staging texture")?;
        self.staging = Some((t.clone(), w, h));
        Ok(t)
    }
}

impl Capture for DxgiCapture {
    fn next_frame(&mut self, force: bool) -> Result<Option<Frame>, String> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(100, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // mode change / secure desktop: recreate the duplication
                    self.dup = self.output.DuplicateOutput(&self.device).map_err(|e| format!("DXGI: {e}"))?;
                    return Ok(None);
                }
                Err(e) => return Err(format!("AcquireNextFrame: {e}")),
            }
            let t = if info.LastPresentTime != 0 { qpc_to_us(info.LastPresentTime, self.qpc_freq) } else { mono_us() };
            // LastPresentTime == 0: only the mouse moved
            let take = (info.LastPresentTime != 0 || force) && self.pacer.due(t);
            let result = if take {
                self.pacer.taken(t);
                let tex: ID3D11Texture2D = res.ok_or("no frame resource")?.cast().map_err(|e| e.to_string())?;
                let mut d = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut d);
                let st = self.staging(d.Width, d.Height)?;
                self.ctx.CopyResource(&st, &tex);
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| format!("Map: {e}"))?;
                let stride = d.Width as usize * 4;
                let mut data = vec![0u8; stride * d.Height as usize];
                for y in 0..d.Height as usize {
                    let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), stride);
                    data[y * stride..(y + 1) * stride].copy_from_slice(src);
                }
                self.ctx.Unmap(&st, 0);
                Some(Frame {
                    t_us: t,
                    t_ready_us: mono_us(),
                    data: FrameData::Cpu(CpuImage { width: d.Width, height: d.Height, stride, format: CpuFormat::Bgrx, data }),
                })
            } else {
                None
            };
            let _ = self.dup.ReleaseFrame();
            Ok(result)
        }
    }
    fn describe(&self) -> String {
        self.desc.clone()
    }
    fn refresh_hz(&self) -> Option<f64> {
        None
    }
}
