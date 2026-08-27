//! Visual harness for the call window: one person on camera, two audio-only
//! people as avatars, plus the control row — then it screenshots itself, so
//! the layout can be checked without needing three people in a call. It also
//! drives a real double-click on the camera tile (through the OS, not a fake
//! event) to check the blown-up view and the way back.
//! `cargo run -p client --example callwindow`

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

// share.rs tees a copy of your own capture through the frame registry.
#[path = "../src/frames.rs"]
mod frames;
#[path = "../src/share.rs"]
mod share;

/// A recognisable test pattern so scaling and letterboxing are obvious.
fn pattern(w: u32, h: u32, base: u32) -> (u32, u32, Vec<u32>) {
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let edge = x < 3 || y < 3 || x >= w - 3 || y >= h - 3;
            let shade = ((y * 255) / h) & 0x3f;
            px[(y * w + x) as usize] = if edge { 0x00ffffff } else { base + (shade << 8) };
        }
    }
    (w, h, px)
}

fn main() {
    let state: share::SharedCall = Arc::new(Mutex::new(share::CallState {
        title: "lounge".into(),
        started_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(154)),
        muted: true,
        camera_on: false,
        sharing: false,
        ..Default::default()
    }));

    let tile = |identity: &str, label: &str, kind: share::TileKind, talking: bool| share::Tile {
        identity: identity.into(),
        label: label.into(),
        kind,
        speaking: Arc::new(AtomicBool::new(talking)),
        alive: Arc::new(AtomicBool::new(true)),
    };

    {
        let mut view = state.lock().unwrap();
        view.tiles.push(tile(
            "user-4",
            "JunkfoodJon · camera",
            share::TileKind::Video(Arc::new(Mutex::new(Some(pattern(640, 480, 0x00204060))))),
            true,
        ));
        view.tiles.push(tile(
            "user-3",
            "switchb",
            share::TileKind::Avatar { initial: "S".into(), color: 0x005865f2 },
            false,
        ));
        view.tiles.push(tile(
            "user-7",
            "space_goat",
            share::TileKind::Avatar { initial: "G".into(), color: 0x004f8a6d },
            false,
        ));
    }

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<share::CallAction>();
    if let Err(e) = share::open_call_window(state.clone(), tx) {
        println!("FAIL: {e}");
        return;
    }

    std::thread::sleep(std::time::Duration::from_secs(3));
    shoot("callwindow-grid.png");

    // Tile 0 (the camera) sits in the top-left quarter of the content area.
    match window_rect() {
        Some((left, top, width, height)) => {
            let (x, y) = (left + width / 4, top + (height as f64 * 0.35) as i32);
            double_click(x, y);
            std::thread::sleep(std::time::Duration::from_secs(1));
            shoot("callwindow-focused.png");
            double_click(x, y);
            std::thread::sleep(std::time::Duration::from_secs(1));
            shoot("callwindow-restored.png");
        }
        None => println!("FAIL: could not find the call window to click"),
    }

    let view = state.lock().unwrap();
    println!("tiles: {} · window open: {}", view.tiles.len(), view.open);
}

fn shoot(name: &str) {
    match snapshot("lounge", name) {
        Ok(()) => println!("saved {name}"),
        Err(e) => println!("screenshot failed ({name}): {e}"),
    }
}

/// Screen rect of the call window, in physical pixels.
fn window_rect() -> Option<(i32, i32, i32, i32)> {
    use winapi::um::winuser::{FindWindowW, GetWindowRect};
    let class: Vec<u16> = "NotDiscordViewer\0".encode_utf16().collect();
    unsafe {
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            return None;
        }
        let mut rect = std::mem::zeroed::<winapi::shared::windef::RECT>();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
        Some((rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top))
    }
}

/// Two real clicks, close enough together to count as a double-click.
fn double_click(x: i32, y: i32) {
    use winapi::um::winuser::{mouse_event, SetCursorPos, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP};
    unsafe {
        SetCursorPos(x, y);
        for _ in 0..2 {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }
}

fn snapshot(title: &str, out: &str) -> Result<(), String> {
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::encoder::ImageFormat;
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    use windows_capture::window::Window;

    struct Snap {
        out: String,
    }
    impl GraphicsCaptureApiHandler for Snap {
        type Flags = String;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self { out: ctx.flags })
        }
        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            frame.save_as_image(&self.out, ImageFormat::Png)?;
            control.stop();
            Ok(())
        }
    }

    let window = Window::from_contains_name(title).map_err(|e| e.to_string())?;
    let settings = Settings::new(
        window,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        out.to_string(),
    );
    let control = Snap::start_free_threaded(settings).map_err(|e| e.to_string())?;
    control.wait().map_err(|e| e.to_string())
}
