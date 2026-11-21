use crate::{Capture, CpuFormat, CpuImage, Frame, FrameData};
use ::screencapturekit::cm::{CMSampleBufferExt, CMSampleBufferSCExt, SCFrameStatus};
use ::screencapturekit::cv::CVPixelBufferLockFlags;
use ::screencapturekit::prelude::*;
use ::screencapturekit::stream::delegate_trait::ErrorHandler;
use eyepiece_link::clock::mono_us;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    latest: Option<(CMSampleBuffer, u64)>,
    error: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct Handler(Arc<Shared>);

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        if kind != SCStreamOutputType::Screen
            || sample.frame_status() != Some(SCFrameStatus::Complete)
            || sample.pixel_buffer().is_none()
        {
            return;
        }
        let arrived = mono_us();
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.latest = Some((sample, arrived));
        self.0.changed.notify_one();
    }
}

pub struct ScreenCapture {
    stream: SCStream,
    shared: Arc<Shared>,
    last: Option<CMSampleBuffer>,
    description: String,
}

fn permission_error(error: impl std::fmt::Display) -> String {
    format!("ScreenCaptureKit: {error}. Allow Screen Recording for your terminal or app in System Settings > Privacy & Security, then restart it.")
}

pub fn list() -> Result<String, String> {
    let content = SCShareableContent::get().map_err(permission_error)?;
    let mut output = String::from("ScreenCaptureKit displays (--display-id ID):\n");
    for display in content.displays() {
        output.push_str(&format!(
            "  {}: {}x{}\n",
            display.display_id(),
            display.width(),
            display.height()
        ));
    }
    Ok(output)
}

impl ScreenCapture {
    pub fn open(display_id: Option<u32>, fps: u32, cursor: bool) -> Result<Self, String> {
        if fps == 0 || fps > i32::MAX as u32 {
            return Err("ScreenCaptureKit needs a positive frame rate below 2^31".into());
        }
        let content = SCShareableContent::get().map_err(permission_error)?;
        let display = content
            .displays()
            .into_iter()
            .find(|display| display_id.is_none_or(|id| display.display_id() == id))
            .ok_or("No matching display; run --list to see available display IDs")?;
        let width = display.width();
        let height = display.height();
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()
            .map_err(permission_error)?;
        let configuration = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(cursor)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&CMTime::new(1, fps as i32));
        let shared = Arc::new(Shared::default());
        let errors = shared.clone();
        let delegate = ErrorHandler::new(move |error| {
            let mut state = errors
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.error = Some(format!("ScreenCaptureKit stopped: {error}"));
            errors.changed.notify_one();
        });
        let mut stream = SCStream::new_with_delegate(&filter, &configuration, delegate)
            .map_err(permission_error)?;
        stream
            .add_output_handler(Handler(shared.clone()), SCStreamOutputType::Screen)
            .map_err(permission_error)?;
        stream.start_capture().map_err(permission_error)?;
        Ok(Self {
            stream,
            shared,
            last: None,
            description: format!(
                "ScreenCaptureKit display {}: {width}x{height} BGRA, up to {fps} fps (CPU copy)",
                display.display_id()
            ),
        })
    }

    fn copy_sample(sample: &CMSampleBuffer, t_us: u64) -> Result<Frame, String> {
        let pixels = sample
            .pixel_buffer()
            .ok_or("ScreenCaptureKit sample has no pixel buffer")?;
        let width = pixels.width();
        let height = pixels.height();
        let stride = pixels.bytes_per_row();
        let length = stride
            .checked_mul(height)
            .ok_or("ScreenCaptureKit pixel buffer size overflow")?;
        if width == 0
            || height == 0
            || stride
                < width
                    .checked_mul(4)
                    .ok_or("ScreenCaptureKit row size overflow")?
        {
            return Err("ScreenCaptureKit returned an invalid BGRA layout".into());
        }
        let guard = pixels
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|error| format!("ScreenCaptureKit pixel buffer lock failed: {error}"))?;
        let bytes = unsafe { guard.as_slice() }
            .ok_or("ScreenCaptureKit pixel buffer is not CPU-readable")?;
        let data = bytes
            .get(..length)
            .ok_or("ScreenCaptureKit pixel buffer is truncated")?
            .to_vec();
        Ok(Frame {
            t_us,
            t_ready_us: mono_us(),
            data: FrameData::Cpu(CpuImage {
                width: u32::try_from(width).map_err(|_| "ScreenCaptureKit width overflow")?,
                height: u32::try_from(height).map_err(|_| "ScreenCaptureKit height overflow")?,
                stride,
                format: CpuFormat::Bgrx,
                data,
            }),
        })
    }
}

impl Capture for ScreenCapture {
    fn next_frame(&mut self, force: bool) -> Result<Option<Frame>, String> {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (mut state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, Duration::from_millis(100), |state| {
                state.latest.is_none() && state.error.is_none() && !(force && self.last.is_some())
            })
            .unwrap_or_else(|error| error.into_inner());
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let latest = state.latest.take();
        drop(state);
        if let Some((sample, timestamp)) = latest {
            self.last = Some(sample.clone());
            return Self::copy_sample(&sample, timestamp).map(Some);
        }
        if force {
            if let Some(sample) = &self.last {
                return Self::copy_sample(sample, mono_us()).map(Some);
            }
        }
        Ok(None)
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn refresh_hz(&self) -> Option<f64> {
        None
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}
