//! Screen sharing: Windows Graphics Capture feeding a LiveKit video track,
//! plus a native viewer window (softbuffer) for watching a remote share —
//! the WebView can't composite live video, so watching gets its own window.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use livekit::track::RemoteVideoTrack;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::video_frame::{I420Buffer, VideoFormatType, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_stream::native::NativeVideoStream;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::frame::Frame;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

// ---------- Capture (the sharer) ----------

pub struct Capturer {
    source: NativeVideoSource,
    scratch: Vec<u8>,
    last: std::time::Instant,
}

type CapError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Capturer {
    type Flags = NativeVideoSource;
    type Error = CapError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            source: ctx.flags,
            scratch: Vec::new(),
            last: std::time::Instant::now() - std::time::Duration::from_secs(1),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // ~30fps cap.
        if self.last.elapsed() < std::time::Duration::from_millis(33) {
            return Ok(());
        }
        self.last = std::time::Instant::now();

        let width = frame.width();
        let height = frame.height();
        let buffer = frame.buffer()?;
        let mut scratch = std::mem::take(&mut self.scratch);
        let data = buffer.as_nopadding_buffer(&mut scratch);

        let mut i420 = I420Buffer::new(width, height);
        let (sy, su, sv) = i420.strides();
        let (dy, du, dv) = i420.data_mut();
        // windows-capture Rgba8 == libyuv "ABGR" byte order.
        yuv_helper::abgr_to_i420(data, width * 4, dy, sy, du, su, dv, sv, width as i32, height as i32);

        let mut video_frame = VideoFrame::new(VideoRotation::VideoRotation0, i420);
        video_frame.timestamp_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        self.source.capture_frame(&video_frame);

        self.scratch = scratch;
        Ok(())
    }
}

pub type ShareControl = windows_capture::capture::CaptureControl<Capturer, CapError>;

/// Start capturing the primary monitor into `source`.
pub fn start_capture(source: NativeVideoSource) -> Result<ShareControl, String> {
    let monitor = Monitor::primary().map_err(|e| format!("no primary monitor: {e}"))?;
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        source,
    );
    Capturer::start_free_threaded(settings).map_err(|e| format!("screen capture failed: {e}"))
}

// ---------- Viewer (everyone else) ----------

type SharedFrame = Arc<Mutex<Option<(u32, u32, Vec<u32>)>>>;

/// Open a native window playing `track`. Returns once the window/thread are
/// spawned; everything shuts down when the window is closed or the track ends.
pub fn open_viewer(track: RemoteVideoTrack, title: String) {
    let latest: SharedFrame = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));

    {
        let latest = latest.clone();
        let alive = alive.clone();
        let rtc = track.rtc_track();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = NativeVideoStream::new(rtc);
            while let Some(frame) = stream.next().await {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                let width = frame.buffer.width();
                let height = frame.buffer.height();
                if width == 0 || height == 0 {
                    continue;
                }
                let mut dst = vec![0u8; (width * height * 4) as usize];
                // libyuv "ARGB" writes BGRA bytes → little-endian u32 0xAARRGGBB,
                // which is exactly softbuffer's 0RGB layout.
                frame.buffer.to_argb(VideoFormatType::ARGB, &mut dst, width * 4, width as i32, height as i32);
                let pixels: Vec<u32> = dst
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                *latest.lock().unwrap() = Some((width, height, pixels));
            }
        });
    }

    std::thread::spawn(move || run_viewer_window(title, latest, alive));
}

struct ViewerApp {
    title: String,
    latest: SharedFrame,
    alive: Arc<AtomicBool>,
    window: Option<Rc<winit::window::Window>>,
    surface: Option<softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>>,
}

impl winit::application::ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = winit::window::Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 560.0));
        let window = Rc::new(event_loop.create_window(attrs).expect("viewer window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        match event {
            winit::event::WindowEvent::CloseRequested => {
                self.alive.store(false, Ordering::Relaxed);
                event_loop.exit();
            }
            winit::event::WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(33),
        ));
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ViewerApp {
    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(win_w), Some(win_h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(win_w, win_h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let frame = self.latest.lock().unwrap().clone();
        if let Some((fw, fh, pixels)) = frame {
            // Nearest-neighbour scale into the window buffer.
            let ww = win_w.get();
            let wh = win_h.get();
            for y in 0..wh {
                let sy = (y as u64 * fh as u64 / wh as u64) as u32;
                let row = (sy * fw) as usize;
                let out_row = (y * ww) as usize;
                for x in 0..ww {
                    let sx = (x as u64 * fw as u64 / ww as u64) as u32;
                    buffer[out_row + x as usize] = pixels[row + sx as usize];
                }
            }
        } else {
            buffer.fill(0x001e1f22);
        }
        let _ = buffer.present();
    }
}

fn run_viewer_window(title: String, latest: SharedFrame, alive: Arc<AtomicBool>) {
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let event_loop = match winit::event_loop::EventLoop::builder().with_any_thread(true).build() {
        Ok(el) => el,
        Err(_) => return,
    };
    let mut app = ViewerApp { title, latest, alive: alive.clone(), window: None, surface: None };
    let _ = event_loop.run_app(&mut app);
    alive.store(false, Ordering::Relaxed);
}
