//! Visual harness for the call window: two fake participant tiles, a
//! self-preview, and the control row — then it screenshots itself so the
//! layout can be checked without needing two people in a call.
//! `cargo run -p client --example callwindow`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[path = "../src/share.rs"]
mod share;

/// A recognisable test pattern so scaling and letterboxing are obvious.
fn pattern(w: u32, h: u32, base: u32, bars: bool) -> (u32, u32, Vec<u32>) {
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let edge = x < 3 || y < 3 || x >= w - 3 || y >= h - 3;
            let stripe = bars && ((x / 40) % 2 == 0);
            let shade = ((y * 255) / h) & 0x3f;
            px[(y * w + x) as usize] = if edge {
                0x00ffffff
            } else if stripe {
                base.saturating_add(shade << 8)
            } else {
                base
            };
        }
    }
    (w, h, px)
}

fn main() {
    let state: share::SharedCall = Arc::new(Mutex::new(share::CallState {
        title: "lounge".into(),
        started_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(154)),
        muted: true,
        sharing: true,
        camera_on: true,
        ..Default::default()
    }));

    let make_tile = |identity: &str, label: &str, frame: (u32, u32, Vec<u32>), talking: bool| {
        let slot: share::SharedFrame = Arc::new(Mutex::new(Some(frame)));
        share::Tile {
            identity: identity.into(),
            label: label.into(),
            frame: slot,
            speaking: Arc::new(AtomicBool::new(talking)),
            alive: Arc::new(AtomicBool::new(true)),
        }
    };

    {
        let mut view = state.lock().unwrap();
        view.tiles.push(make_tile(
            "user-4",
            "JunkfoodJon · camera",
            pattern(640, 480, 0x00204060, false),
            true,
        ));
        view.tiles.push(make_tile(
            "user-3",
            "switchb · screen",
            pattern(1280, 720, 0x00203020, true),
            false,
        ));
        view.self_preview = Some(Arc::new(Mutex::new(Some(pattern(320, 240, 0x00402038, false)))));
    }

    let (tx, _rx) = tokio_unbounded();
    if let Err(e) = share::open_call_window(state.clone(), tx) {
        println!("FAIL: {e}");
        return;
    }

    std::thread::sleep(std::time::Duration::from_secs(4));

    use windows_capture::encoder::ImageFormat;
    use windows_capture::window::Window;
    match Window::from_contains_name("lounge") {
        Ok(_) => println!("call window is open"),
        Err(e) => {
            println!("FAIL: no call window ({e})");
            return;
        }
    }
    // Snapshot it for inspection.
    match snapshot("lounge") {
        Ok(()) => println!("saved callwindow-shot.png"),
        Err(e) => println!("screenshot failed: {e}"),
    }
    println!(
        "tiles: {} · window open: {}",
        state.lock().unwrap().tiles.len(),
        state.lock().unwrap().open
    );
    let _ = ImageFormat::Png;
}

/// The window wants a tokio sender; the harness has no runtime, so make one.
fn tokio_unbounded() -> (
    tokio::sync::mpsc::UnboundedSender<share::CallAction>,
    tokio::sync::mpsc::UnboundedReceiver<share::CallAction>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

fn snapshot(title: &str) -> Result<(), String> {
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::encoder::ImageFormat;
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    use windows_capture::window::Window;

    struct Snap;
    impl GraphicsCaptureApiHandler for Snap {
        type Flags = ();
        type Error = Box<dyn std::error::Error + Send + Sync>;
        fn new(_: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            frame.save_as_image("callwindow-shot.png", ImageFormat::Png)?;
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
        (),
    );
    let control = Snap::start_free_threaded(settings).map_err(|e| e.to_string())?;
    control.wait().map_err(|e| e.to_string())
}

fn _unused(_: Ordering) {}
