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

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorChoice {
    /// 1-based index for `Monitor::from_index`.
    pub index: usize,
    pub label: String,
}

/// All monitors, primary first, labeled for the share picker.
pub fn list_monitors() -> Vec<MonitorChoice> {
    let primary_name = Monitor::primary().and_then(|m| m.device_name()).ok();
    let mut choices: Vec<(bool, MonitorChoice)> = Monitor::enumerate()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|monitor| {
            let index = monitor.index().ok()?;
            let is_primary = monitor.device_name().ok() == primary_name && primary_name.is_some();
            let size = match (monitor.width(), monitor.height()) {
                (Ok(w), Ok(h)) => format!(" — {w}×{h}"),
                _ => String::new(),
            };
            let primary_tag = if is_primary { " (primary)" } else { "" };
            Some((is_primary, MonitorChoice { index, label: format!("Monitor {index}{size}{primary_tag}") }))
        })
        .collect();
    choices.sort_by_key(|(is_primary, c)| (!is_primary, c.index));
    choices.into_iter().map(|(_, c)| c).collect()
}

/// Start capturing a monitor into `source` (primary when `monitor` is None).
pub fn start_capture(source: NativeVideoSource, monitor: Option<usize>) -> Result<ShareControl, String> {
    let monitor = match monitor {
        Some(index) => Monitor::from_index(index).map_err(|e| format!("monitor {index} not found: {e}"))?,
        None => Monitor::primary().map_err(|e| format!("no primary monitor: {e}"))?,
    };
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

/// Latest decoded frame: (width, height, 0RGB pixels).
pub type SharedFrame = Arc<Mutex<Option<(u32, u32, Vec<u32>)>>>;

/// A request for the viewer loop to open one more window.
struct ViewerRequest {
    title: String,
    latest: SharedFrame,
    alive: Arc<AtomicBool>,
}

/// winit allows exactly ONE event loop per process for the whole run — the
/// "already created" flag is never cleared outside web builds — so the viewer
/// is a single long-lived loop that opens windows on demand. (Creating a
/// second loop returned an error, which is why every Watch after the first
/// used to do nothing at all.)
static VIEWER: std::sync::OnceLock<Option<winit::event_loop::EventLoopProxy<ViewerRequest>>> =
    std::sync::OnceLock::new();

fn viewer_proxy() -> Option<&'static winit::event_loop::EventLoopProxy<ViewerRequest>> {
    VIEWER
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || run_viewer_loop(tx));
            // The loop hands its proxy back once it exists.
            rx.recv_timeout(std::time::Duration::from_secs(5)).ok().flatten()
        })
        .as_ref()
}

/// Open a viewer window fed by frames the caller writes into the returned
/// slot. `alive` goes false when the window closes.
pub fn open_frame_viewer(title: String) -> Result<(SharedFrame, Arc<AtomicBool>), String> {
    let latest: SharedFrame = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));
    let Some(proxy) = viewer_proxy() else {
        return Err("could not start the video viewer".into());
    };
    proxy
        .send_event(ViewerRequest { title, latest: latest.clone(), alive: alive.clone() })
        .map_err(|_| "the video viewer stopped responding".to_string())?;
    Ok((latest, alive))
}

/// Open a native window playing `track`. Everything for that window shuts down
/// when it's closed or the track ends; the shared loop keeps running.
pub fn open_viewer(track: RemoteVideoTrack, title: String) -> Result<(), String> {
    let (latest, alive) = open_frame_viewer(title)?;
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
    Ok(())
}

/// One open viewer window.
struct ViewerWindow {
    window: Rc<winit::window::Window>,
    surface: softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
    latest: SharedFrame,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct ViewerApp {
    windows: std::collections::HashMap<winit::window::WindowId, ViewerWindow>,
}

impl winit::application::ApplicationHandler<ViewerRequest> for ViewerApp {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {}

    /// A Watch click: open another window on this same loop.
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, request: ViewerRequest) {
        // dioxus's tao already registered the Win32 class "Window Class" (both
        // libraries' default!) with ITS window procedure. Without a distinct
        // class name here, winit's registration silently no-ops and this window
        // would be created with tao's wndproc on the wrong thread — freezing
        // the whole app and then crashing it.
        use winit::platform::windows::WindowAttributesExtWindows;
        let attrs = winit::window::Window::default_attributes()
            .with_title(request.title)
            .with_class_name("NotDiscordViewer")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 560.0));
        let Ok(window) = event_loop.create_window(attrs) else {
            request.alive.store(false, Ordering::Relaxed);
            return;
        };
        let window = Rc::new(window);
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|context| softbuffer::Surface::new(&context, window.clone()));
        match surface {
            Ok(surface) => {
                self.windows.insert(
                    window.id(),
                    ViewerWindow { window, surface, latest: request.latest, alive: request.alive },
                );
            }
            Err(_) => request.alive.store(false, Ordering::Relaxed),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        match event {
            winit::event::WindowEvent::CloseRequested => {
                // Close just this window; the loop lives on for the next Watch.
                if let Some(viewer) = self.windows.remove(&id) {
                    viewer.alive.store(false, Ordering::Relaxed);
                }
            }
            winit::event::WindowEvent::RedrawRequested => {
                if let Some(viewer) = self.windows.get_mut(&id) {
                    viewer.draw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        // Idle cheaply when nothing is being watched.
        if self.windows.is_empty() {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
            return;
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(33),
        ));
        // A track that ended closes its own window.
        self.windows.retain(|_, viewer| viewer.alive.load(Ordering::Relaxed));
        for viewer in self.windows.values() {
            viewer.window.request_redraw();
        }
    }
}

impl ViewerWindow {
    fn draw(&mut self) {
        let (window, surface) = (&self.window, &mut self.surface);
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

/// The process's one and only viewer event loop. Hands its proxy back through
/// `ready`, then runs until the app exits.
fn run_viewer_loop(ready: std::sync::mpsc::Sender<Option<winit::event_loop::EventLoopProxy<ViewerRequest>>>) {
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let event_loop = match winit::event_loop::EventLoop::<ViewerRequest>::with_user_event()
        .with_any_thread(true)
        .build()
    {
        Ok(event_loop) => event_loop,
        Err(_) => {
            let _ = ready.send(None);
            return;
        }
    };
    let _ = ready.send(Some(event_loop.create_proxy()));
    let mut app = ViewerApp::default();
    let _ = event_loop.run_app(&mut app);
}
