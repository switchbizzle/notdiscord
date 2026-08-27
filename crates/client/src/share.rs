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

/// Where a copy of your own share goes so you can see it too. Your published
/// track never comes back to you — you don't subscribe to yourself — so
/// without this you're the one person in the call who can't see your screen.
pub struct SelfShare {
    pub slot: SharedFrame,
    /// Stamped by whoever is looking. Copying a 1440p frame is far too
    /// expensive to do for a preview nobody has open.
    pub interest: Arc<std::sync::atomic::AtomicU64>,
}

pub struct CaptureFlags {
    pub source: NativeVideoSource,
    pub preview: Option<SelfShare>,
}

pub struct Capturer {
    source: NativeVideoSource,
    preview: Option<SelfShare>,
    scratch: Vec<u8>,
    last: std::time::Instant,
    last_preview: std::time::Instant,
}

type CapError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Capturer {
    type Flags = CaptureFlags;
    type Error = CapError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(1);
        Ok(Self {
            source: ctx.flags.source,
            preview: ctx.flags.preview,
            scratch: Vec::new(),
            last: long_ago,
            last_preview: long_ago,
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

        // Tee a scaled-down copy for your own tile, but only while something
        // is actually showing it, and at half the send rate.
        if let Some(preview) = &self.preview {
            if crate::frames::is_wanted(&preview.interest)
                && self.last_preview.elapsed() >= std::time::Duration::from_millis(66)
            {
                self.last_preview = std::time::Instant::now();
                tee_preview(data, width, height, &preview.slot);
            }
        }

        self.scratch = scratch;
        Ok(())
    }
}

/// RGBA capture buffer -> the 0RGB pixels the viewer draws, scaled to a tile.
fn tee_preview(data: &[u8], width: u32, height: u32, slot: &SharedFrame) {
    const MAX_EDGE: u32 = 960;
    if width == 0 || height == 0 || data.len() < (width * height * 4) as usize {
        return;
    }
    let scale = (MAX_EDGE as f32 / width.max(height) as f32).min(1.0);
    let out_w = ((width as f32 * scale) as u32).max(1);
    let out_h = ((height as f32 * scale) as u32).max(1);
    let mut pixels = Vec::with_capacity((out_w * out_h) as usize);
    for y in 0..out_h {
        let src_y = (y as u64 * height as u64 / out_h as u64) as u32;
        let row = (src_y * width) as usize * 4;
        for x in 0..out_w {
            let src_x = (x as u64 * width as u64 / out_w as u64) as u32;
            let i = row + src_x as usize * 4;
            let (r, g, b) = (data[i] as u32, data[i + 1] as u32, data[i + 2] as u32);
            pixels.push((r << 16) | (g << 8) | b);
        }
    }
    *slot.lock().unwrap() = Some((out_w, out_h, pixels));
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
/// `preview` also receives a scaled copy, so the sharer can see their own tile.
pub fn start_capture(
    source: NativeVideoSource,
    monitor: Option<usize>,
    preview: Option<SelfShare>,
) -> Result<ShareControl, String> {
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
        CaptureFlags { source, preview },
    );
    Capturer::start_free_threaded(settings).map_err(|e| format!("screen capture failed: {e}"))
}

// ---------- Viewer (everyone else) ----------

/// Latest decoded frame: (width, height, 0RGB pixels).
pub type SharedFrame = Arc<Mutex<Option<(u32, u32, Vec<u32>)>>>;

// ---------- Call window state ----------

/// What a tile shows: live video, or the person's avatar when they're
/// audio-only. Everyone in the call gets a tile either way.
pub enum TileKind {
    Video(SharedFrame),
    Avatar { initial: String, color: u32 },
}

/// One person (or one of their streams) in the call.
pub struct Tile {
    /// LiveKit identity, so speaking updates can find this tile.
    pub identity: String,
    pub label: String,
    pub kind: TileKind,
    pub speaking: Arc<AtomicBool>,
    /// Cleared when the stream ends, which stops its frame pump.
    pub alive: Arc<AtomicBool>,
}

impl Tile {
    fn frame(&self) -> Option<(u32, u32, Vec<u32>)> {
        match &self.kind {
            TileKind::Video(slot) => slot.lock().ok().and_then(|f| f.clone()),
            TileKind::Avatar { .. } => None,
        }
    }
}

/// Everything the call window draws. Voice owns it and mutates in place; the
/// window reads it every frame.
#[derive(Default)]
pub struct CallState {
    pub title: String,
    pub tiles: Vec<Tile>,
    /// Your own camera, shown picture-in-picture.
    pub self_preview: Option<SharedFrame>,
    /// A copy of your own screen share, so you get a tile like everyone else.
    pub self_share: Option<(SharedFrame, Arc<std::sync::atomic::AtomicU64>)>,
    pub muted: bool,
    pub deafened: bool,
    pub camera_on: bool,
    pub sharing: bool,
    pub started_at: Option<std::time::Instant>,
    /// True while the window is on screen.
    pub open: bool,
}

pub type SharedCall = Arc<Mutex<CallState>>;

/// A button press in the call window, handed back to the voice engine.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CallAction {
    Mic,
    Deafen,
    Camera,
    Screen,
    Leave,
}

/// A request for the viewer loop to open a window.
enum ViewerRequest {
    /// A plain single-stream window (used by the dev harness).
    Plain {
        title: String,
        latest: SharedFrame,
        alive: Arc<AtomicBool>,
    },
    /// The call window: tiles, self-preview, and controls.
    Call {
        state: SharedCall,
        actions: tokio::sync::mpsc::UnboundedSender<CallAction>,
    },
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
        .send_event(ViewerRequest::Plain { title, latest: latest.clone(), alive: alive.clone() })
        .map_err(|_| "the video viewer stopped responding".to_string())?;
    Ok((latest, alive))
}

/// Show the call window (no-op when it's already up).
pub fn open_call_window(
    state: SharedCall,
    actions: tokio::sync::mpsc::UnboundedSender<CallAction>,
) -> Result<(), String> {
    if state.lock().unwrap().open {
        return Ok(());
    }
    let Some(proxy) = viewer_proxy() else {
        return Err("could not start the call window".into());
    };
    state.lock().unwrap().open = true;
    proxy
        .send_event(ViewerRequest::Call { state, actions })
        .map_err(|_| "the call window stopped responding".to_string())?;
    Ok(())
}

/// Pump a remote track's frames into a slot until the slot's window closes.
pub fn pump_track(track: &RemoteVideoTrack, latest: SharedFrame, alive: Arc<AtomicBool>) {
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

/// One open plain viewer window (single stream, no chrome).
struct ViewerWindow {
    window: Rc<winit::window::Window>,
    surface: softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
    latest: SharedFrame,
    alive: Arc<AtomicBool>,
}

/// Where a tile landed in the last frame, so clicks can find it.
struct TileHit {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Labels are unique per tile and survive the 200ms rebuilds, so they're
    /// what "which tile is focused" is remembered by.
    label: String,
    video: bool,
}

/// The call window: tiles, self-preview and controls.
struct CallWindow {
    window: Rc<winit::window::Window>,
    surface: softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
    state: SharedCall,
    actions: tokio::sync::mpsc::UnboundedSender<CallAction>,
    cursor: (f64, f64),
    /// Hit boxes recomputed on each draw: (x, y, w, h, action).
    buttons: Vec<(i32, i32, i32, i32, CallAction)>,
    /// The window's own controls, which never reach the voice engine.
    chrome: Vec<(i32, i32, i32, i32, ChromeAction)>,
    tile_hits: Vec<TileHit>,
    /// Label of the tile blown up to fill the window, if any.
    focused: Option<String>,
    fullscreen: bool,
    /// Last left-press, for spotting double-clicks ourselves — winit doesn't
    /// report click counts on Windows.
    last_click: Option<(std::time::Instant, i32, i32)>,
}

/// A click on the window's own chrome (as opposed to a call control).
#[derive(Clone, Copy, PartialEq)]
enum ChromeAction {
    Fullscreen,
}

enum WindowKind {
    Plain(ViewerWindow),
    Call(CallWindow),
}

#[derive(Default)]
struct ViewerApp {
    windows: std::collections::HashMap<winit::window::WindowId, WindowKind>,
}

/// Create a window + softbuffer surface, sharing the class-name workaround.
fn make_window(
    event_loop: &winit::event_loop::ActiveEventLoop,
    title: &str,
    size: (f64, f64),
) -> Option<(
    Rc<winit::window::Window>,
    softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>,
)> {
    // dioxus's tao already registered the Win32 class "Window Class" (both
    // libraries' default!) with ITS window procedure. Without a distinct
    // class name here, winit's registration silently no-ops and this window
    // would be created with tao's wndproc on the wrong thread — freezing
    // the whole app and then crashing it.
    use winit::platform::windows::WindowAttributesExtWindows;
    let attrs = winit::window::Window::default_attributes()
        .with_title(title)
        .with_class_name("NotDiscordViewer")
        .with_inner_size(winit::dpi::LogicalSize::new(size.0, size.1));
    let window = Rc::new(event_loop.create_window(attrs).ok()?);
    let surface = softbuffer::Context::new(window.clone())
        .and_then(|context| softbuffer::Surface::new(&context, window.clone()))
        .ok()?;
    Some((window, surface))
}

impl winit::application::ApplicationHandler<ViewerRequest> for ViewerApp {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {}

    /// A Watch click: open another window on this same loop.
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, request: ViewerRequest) {
        match request {
            ViewerRequest::Plain { title, latest, alive } => {
                match make_window(event_loop, &title, (960.0, 560.0)) {
                    Some((window, surface)) => {
                        self.windows.insert(
                            window.id(),
                            WindowKind::Plain(ViewerWindow { window, surface, latest, alive }),
                        );
                    }
                    None => alive.store(false, Ordering::Relaxed),
                }
            }
            ViewerRequest::Call { state, actions } => {
                let title = {
                    let call = state.lock().unwrap();
                    format!("{} — NotDiscord", call.title)
                };
                match make_window(event_loop, &title, (1000.0, 660.0)) {
                    Some((window, surface)) => {
                        self.windows.insert(
                            window.id(),
                            WindowKind::Call(CallWindow {
                                window,
                                surface,
                                state,
                                actions,
                                cursor: (0.0, 0.0),
                                buttons: Vec::new(),
                                chrome: Vec::new(),
                                tile_hits: Vec::new(),
                                focused: None,
                                fullscreen: false,
                                last_click: None,
                            }),
                        );
                    }
                    None => state.lock().unwrap().open = false,
                }
            }
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
                match self.windows.remove(&id) {
                    Some(WindowKind::Plain(viewer)) => viewer.alive.store(false, Ordering::Relaxed),
                    Some(WindowKind::Call(call)) => {
                        let mut state = call.state.lock().unwrap();
                        state.open = false;
                        // Watching stops; the call itself keeps running.
                        for tile in &state.tiles {
                            tile.alive.store(false, Ordering::Relaxed);
                        }
                        state.tiles.clear();
                    }
                    None => {}
                }
            }
            winit::event::WindowEvent::RedrawRequested => match self.windows.get_mut(&id) {
                Some(WindowKind::Plain(viewer)) => viewer.draw(),
                Some(WindowKind::Call(call)) => call.draw(),
                None => {}
            },
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                if let Some(WindowKind::Call(call)) = self.windows.get_mut(&id) {
                    call.cursor = (position.x, position.y);
                }
            }
            winit::event::WindowEvent::MouseInput { state: pressed, button, .. } => {
                if pressed != winit::event::ElementState::Pressed
                    || button != winit::event::MouseButton::Left
                {
                    return;
                }
                if let Some(WindowKind::Call(call)) = self.windows.get_mut(&id) {
                    call.click();
                }
            }
            winit::event::WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{Key, NamedKey};
                if event.state != winit::event::ElementState::Pressed {
                    return;
                }
                let Some(WindowKind::Call(call)) = self.windows.get_mut(&id) else { return };
                match event.logical_key {
                    Key::Named(NamedKey::F11) => call.set_fullscreen(!call.fullscreen),
                    // Escape backs out one level: the blown-up tile first,
                    // then fullscreen. It never hangs up the call.
                    Key::Named(NamedKey::Escape) => {
                        if call.focused.take().is_none() && call.fullscreen {
                            call.set_fullscreen(false);
                        }
                    }
                    _ => {}
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
        // A plain viewer whose track ended closes itself.
        self.windows.retain(|_, kind| match kind {
            WindowKind::Plain(viewer) => viewer.alive.load(Ordering::Relaxed),
            WindowKind::Call(call) => call.state.lock().unwrap().open,
        });
        for kind in self.windows.values() {
            match kind {
                WindowKind::Plain(viewer) => viewer.window.request_redraw(),
                WindowKind::Call(call) => call.window.request_redraw(),
            }
        }
    }
}

// ---------- Drawing helpers ----------

/// System UI font, loaded at runtime (never embedded — no redistribution).
fn ui_font() -> Option<&'static fontdue::Font> {
    static FONT: std::sync::OnceLock<Option<fontdue::Font>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        for path in ["C:\\Windows\\Fonts\\segoeui.ttf", "C:\\Windows\\Fonts\\arial.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(font);
                }
            }
        }
        None
    })
    .as_ref()
}

struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for row in y.max(0)..(y + h).min(self.h) {
            let start = (row * self.w) as usize;
            for col in x.max(0)..(x + w).min(self.w) {
                self.buf[start + col as usize] = color;
            }
        }
    }

    /// Blend `color` over the pixel at `coverage`/255 opacity.
    fn blend(&mut self, x: i32, y: i32, color: u32, coverage: u8) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || coverage == 0 {
            return;
        }
        let i = (y * self.w + x) as usize;
        let dst = self.buf[i];
        let a = coverage as u32;
        let mix = |shift: u32| {
            let s = (color >> shift) & 0xff;
            let d = (dst >> shift) & 0xff;
            ((s * a + d * (255 - a)) / 255) << shift
        };
        self.buf[i] = mix(16) | mix(8) | mix(0);
    }

    fn text(&mut self, x: i32, y: i32, size: f32, color: u32, text: &str) -> i32 {
        let Some(font) = ui_font() else { return x };
        let mut pen = x as f32;
        for ch in text.chars() {
            let (metrics, bitmap) = font.rasterize(ch, size);
            for (i, coverage) in bitmap.iter().enumerate() {
                let gx = pen as i32 + metrics.xmin + (i % metrics.width.max(1)) as i32;
                let gy = y + size as i32 - metrics.ymin - metrics.height as i32
                    + (i / metrics.width.max(1)) as i32;
                self.blend(gx, gy, color, *coverage);
            }
            pen += metrics.advance_width;
        }
        pen as i32
    }

    fn text_width(&self, size: f32, text: &str) -> i32 {
        let Some(font) = ui_font() else { return 0 };
        text.chars()
            .map(|ch| font.metrics(ch, size).advance_width)
            .sum::<f32>() as i32
    }

    fn circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        for y in (cy - r).max(0)..(cy + r).min(self.h) {
            let dy = y - cy;
            let span = ((r * r - dy * dy) as f64).sqrt() as i32;
            for x in (cx - span).max(0)..(cx + span).min(self.w) {
                self.buf[(y * self.w + x) as usize] = color;
            }
        }
    }

    /// Draw a frame letterboxed inside a rect.
    fn video(&mut self, x: i32, y: i32, w: i32, h: i32, frame: &Option<(u32, u32, Vec<u32>)>) {
        self.rect(x, y, w, h, 0x00101113);
        let Some((fw, fh, pixels)) = frame else { return };
        if *fw == 0 || *fh == 0 || w <= 0 || h <= 0 {
            return;
        }
        // Fit while preserving aspect.
        let scale = (w as f64 / *fw as f64).min(h as f64 / *fh as f64);
        let dw = (*fw as f64 * scale) as i32;
        let dh = (*fh as f64 * scale) as i32;
        let ox = x + (w - dw) / 2;
        let oy = y + (h - dh) / 2;
        for row in 0..dh {
            let sy = (row as u64 * *fh as u64 / dh.max(1) as u64) as u32;
            let src_row = (sy * fw) as usize;
            let dst_y = oy + row;
            if dst_y < 0 || dst_y >= self.h {
                continue;
            }
            let dst_row = (dst_y * self.w) as usize;
            for col in 0..dw {
                let dst_x = ox + col;
                if dst_x < 0 || dst_x >= self.w {
                    continue;
                }
                let sx = (col as u64 * *fw as u64 / dw.max(1) as u64) as u32;
                self.buf[dst_row + dst_x as usize] = pixels[src_row + sx as usize];
            }
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

impl CallWindow {
    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        self.window.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
    }

    fn click(&mut self) {
        let (cx, cy) = (self.cursor.0 as i32, self.cursor.1 as i32);
        let hit = |x: &i32, y: &i32, w: &i32, h: &i32| cx >= *x && cx < x + w && cy >= *y && cy < y + h;

        // Second press in the same spot, quickly: a double-click. (Windows'
        // own threshold is 500ms; winit doesn't surface click counts.)
        let now = std::time::Instant::now();
        let double = self.last_click.is_some_and(|(at, x, y)| {
            now.duration_since(at) < std::time::Duration::from_millis(450)
                && (cx - x).abs() < 6
                && (cy - y).abs() < 6
        });
        // A double-click's second press starts a fresh count, so a triple
        // click doesn't toggle twice.
        self.last_click = if double { None } else { Some((now, cx, cy)) };

        for (x, y, w, h, action) in &self.buttons {
            if hit(x, y, w, h) {
                let _ = self.actions.send(*action);
                return;
            }
        }
        for (x, y, w, h, action) in &self.chrome {
            if hit(x, y, w, h) {
                match action {
                    ChromeAction::Fullscreen => {
                        let on = !self.fullscreen;
                        self.set_fullscreen(on);
                    }
                }
                return;
            }
        }
        if !double {
            return;
        }
        // Double-click a stream to fill the window with it; double-click the
        // filled window to get everyone back.
        for tile in &self.tile_hits {
            if hit(&tile.x, &tile.y, &tile.w, &tile.h) {
                if self.focused.as_deref() == Some(tile.label.as_str()) {
                    self.focused = None;
                } else if tile.video {
                    self.focused = Some(tile.label.clone());
                }
                return;
            }
        }
    }

    fn draw(&mut self) {
        const BG: u32 = 0x001e1f22;
        const BAR: u32 = 0x00232428;
        const TEXT: u32 = 0x00dbdee1;
        const BRIGHT: u32 = 0x00f2f3f5;
        const MUTED: u32 = 0x00949ba4;
        const GREEN: u32 = 0x0023a55a;
        const RED: u32 = 0x00f23f43;
        const CHIP: u32 = 0x003a3c42;
        const HEADER_H: i32 = 40;
        const FOOTER_H: i32 = 62;

        let size = self.window.inner_size();
        let (Some(win_w), Some(win_h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if self.surface.resize(win_w, win_h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };

        let (w, h) = (win_w.get() as i32, win_h.get() as i32);
        let state = self.state.lock().unwrap();
        let mut canvas = Canvas { buf: &mut buffer, w, h };
        canvas.rect(0, 0, w, h, BG);

        // ---- header: who you're with, and for how long ----
        canvas.rect(0, 0, w, HEADER_H, BAR);
        let mut pen = canvas.text(16, 11, 15.0, BRIGHT, &state.title);
        // Count people, not tiles — someone sharing camera *and* screen has two.
        let mut seen: Vec<&str> = Vec::new();
        for tile in &state.tiles {
            if !seen.contains(&tile.identity.as_str()) {
                seen.push(&tile.identity);
            }
        }
        pen = canvas.text(
            pen + 12,
            13,
            13.0,
            MUTED,
            &match seen.len() {
                0 => "connecting…".to_string(),
                1 => "just you".to_string(),
                n => format!("{n} in the call"),
            },
        );
        if self.focused.is_some() {
            canvas.text(pen + 12, 13, 13.0, MUTED, "· double-click to go back");
        }

        // Fullscreen toggle, right-aligned — and the only way back out once
        // the title bar is gone (Escape and F11 work too).
        let fs_label = if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" };
        let fs_w = canvas.text_width(12.0, fs_label) + 20;
        let fs_x = w - fs_w - 12;
        let fs_y = 7;
        let fs_h = 26;
        let fs_hovered = self.cursor.0 as i32 >= fs_x
            && (self.cursor.0 as i32) < fs_x + fs_w
            && self.cursor.1 as i32 >= fs_y
            && (self.cursor.1 as i32) < fs_y + fs_h;
        canvas.rect(fs_x, fs_y, fs_w, fs_h, if fs_hovered { 0x004e5058 } else { CHIP });
        canvas.text(fs_x + 10, fs_y + 6, 12.0, BRIGHT, fs_label);
        self.chrome.clear();
        self.chrome.push((fs_x, fs_y, fs_w, fs_h, ChromeAction::Fullscreen));

        if let Some(started) = state.started_at {
            let secs = started.elapsed().as_secs();
            let clock = format!("{:02}:{:02}", secs / 60, secs % 60);
            let tw = canvas.text_width(13.0, &clock);
            canvas.text(fs_x - tw - 14, 13, 13.0, MUTED, &clock);
        }

        // ---- tiles ----
        let content_y = HEADER_H;
        let content_h = (h - HEADER_H - FOOTER_H).max(0);
        self.tile_hits.clear();
        // A focused stream that ended (or a person who left) quietly drops
        // back to the grid rather than showing an empty box.
        let focused_idx = self.focused.as_ref().and_then(|label| {
            state
                .tiles
                .iter()
                .position(|t| t.label == *label && matches!(t.kind, TileKind::Video(_)))
        });
        if self.focused.is_some() && focused_idx.is_none() {
            self.focused = None;
        }
        if state.tiles.is_empty() {
            let msg = "Nobody's sharing video yet";
            let tw = canvas.text_width(15.0, msg);
            canvas.text((w - tw) / 2, content_y + content_h / 2 - 8, 15.0, MUTED, msg);
        } else {
            let pad = 8;
            // One tile filling the window, or the grid.
            let places: Vec<(usize, i32, i32, i32, i32)> = match focused_idx {
                Some(idx) => {
                    vec![(idx, pad, content_y + pad, w - pad * 2, content_h - pad * 2)]
                }
                None => {
                    let n = state.tiles.len() as i32;
                    let cols = (n as f64).sqrt().ceil() as i32;
                    let rows = (n + cols - 1) / cols;
                    let cell_w = (w - pad * (cols + 1)) / cols;
                    let cell_h = (content_h - pad * (rows + 1)) / rows.max(1);
                    (0..n)
                        .map(|i| {
                            let (cx, cy) = (i % cols, i / cols);
                            (
                                i as usize,
                                pad + cx * (cell_w + pad),
                                content_y + pad + cy * (cell_h + pad),
                                cell_w,
                                cell_h,
                            )
                        })
                        .collect()
                }
            };
            for (idx, x, y, cell_w, cell_h) in places {
                let tile = &state.tiles[idx];
                self.tile_hits.push(TileHit {
                    x,
                    y,
                    w: cell_w,
                    h: cell_h,
                    label: tile.label.clone(),
                    video: matches!(tile.kind, TileKind::Video(_)),
                });
                match &tile.kind {
                    TileKind::Video(_) => {
                        let frame = tile.frame();
                        canvas.video(x, y, cell_w, cell_h, &frame);
                    }
                    TileKind::Avatar { initial, color } => {
                        // Audio-only: their avatar, so the room still looks
                        // like a room.
                        canvas.rect(x, y, cell_w, cell_h, 0x00272930);
                        let radius = (cell_w.min(cell_h) / 5).clamp(22, 74);
                        canvas.circle(x + cell_w / 2, y + cell_h / 2 - 6, radius, *color);
                        let size = radius as f32 * 1.1;
                        let tw = canvas.text_width(size, initial);
                        canvas.text(
                            x + cell_w / 2 - tw / 2,
                            y + cell_h / 2 - 6 - (size * 0.62) as i32,
                            size,
                            0x00ffffff,
                            initial,
                        );
                    }
                }
                // Speaking gets a green frame, like the ring in the app.
                if tile.speaking.load(Ordering::Relaxed) {
                    canvas.rect(x, y, cell_w, 2, GREEN);
                    canvas.rect(x, y + cell_h - 2, cell_w, 2, GREEN);
                    canvas.rect(x, y, 2, cell_h, GREEN);
                    canvas.rect(x + cell_w - 2, y, 2, cell_h, GREEN);
                }
                // Name plate.
                let label_w = canvas.text_width(13.0, &tile.label) + 16;
                canvas.rect(x + 8, y + cell_h - 30, label_w, 22, BAR);
                canvas.text(x + 16, y + cell_h - 26, 13.0, TEXT, &tile.label);
            }
        }

        // ---- your own camera, picture-in-picture ----
        if let Some(preview) = &state.self_preview {
            let frame = preview.lock().unwrap().clone();
            if frame.is_some() {
                let pip_w = (w / 5).clamp(140, 260);
                let pip_h = pip_w * 9 / 16;
                let x = w - pip_w - 14;
                let y = h - FOOTER_H - pip_h - 14;
                canvas.rect(x - 2, y - 2, pip_w + 4, pip_h + 4, BAR);
                canvas.video(x, y, pip_w, pip_h, &frame);
                canvas.text(x + 8, y + pip_h - 20, 12.0, TEXT, "You");
            }
        }

        // ---- footer controls ----
        canvas.rect(0, h - FOOTER_H, w, FOOTER_H, BAR);
        let controls: [(CallAction, &str, bool, bool); 5] = [
            (CallAction::Mic, if state.muted { "Unmute" } else { "Mute" }, state.muted, false),
            (
                CallAction::Deafen,
                if state.deafened { "Undeafen" } else { "Deafen" },
                state.deafened,
                false,
            ),
            (
                CallAction::Camera,
                if state.camera_on { "Camera off" } else { "Camera on" },
                false,
                state.camera_on,
            ),
            (
                CallAction::Screen,
                if state.sharing { "Stop sharing" } else { "Share screen" },
                false,
                state.sharing,
            ),
            (CallAction::Leave, "Leave", true, false),
        ];

        let gap = 10;
        let widths: Vec<i32> = controls
            .iter()
            .map(|(_, label, _, _)| canvas.text_width(13.0, label) + 28)
            .collect();
        let total: i32 = widths.iter().sum::<i32>() + gap * (controls.len() as i32 - 1);
        let mut bx = (w - total) / 2;
        let by = h - FOOTER_H + 13;
        let bh = 36;

        self.buttons.clear();
        for ((action, label, danger, active), bw) in controls.iter().zip(widths) {
            let hovered = self.cursor.0 as i32 >= bx
                && (self.cursor.0 as i32) < bx + bw
                && self.cursor.1 as i32 >= by
                && (self.cursor.1 as i32) < by + bh;
            let bg = match (danger, active, hovered) {
                (true, _, true) => RED,
                (true, _, false) => 0x006b2a2c,
                (_, true, _) => GREEN,
                (_, _, true) => 0x004e5058,
                _ => CHIP,
            };
            canvas.rect(bx, by, bw, bh, bg);
            let tw = canvas.text_width(13.0, label);
            canvas.text(bx + (bw - tw) / 2, by + 10, 13.0, BRIGHT, label);
            self.buttons.push((bx, by, bw, bh, *action));
            bx += bw + gap;
        }

        drop(state);
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
