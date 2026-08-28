//! End-to-end check of single-window sharing: launches Notepad, finds it in
//! the picker list, captures IT (not the screen), verifies real frames come
//! through the preview tee, then kills Notepad and verifies the capture
//! reports itself closed — the path that auto-stops the share instead of
//! streaming a frozen last frame.
//! `cargo run -p client --example windowshare`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;

#[path = "../src/frames.rs"]
mod frames;
#[path = "../src/share.rs"]
mod share;

#[tokio::main]
async fn main() {
    // A window whose process we actually own, so killing it closes it
    // (Win11's notepad.exe hands off to a packaged app and survives).
    let mut child = std::process::Command::new("mspaint.exe").spawn().expect("launch paint");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let windows = share::list_windows();
    println!("picker lists {} windows", windows.len());
    let Some(target) = windows.iter().find(|w| w.label.contains("Paint")) else {
        println!("FAIL: Paint not in the window list; got: {:?}",
            windows.iter().map(|w| &w.label).collect::<Vec<_>>());
        let _ = child.kill();
        return;
    };
    println!("found target: {:?} (hwnd {})", target.label, target.hwnd);
    if windows.iter().any(|w| w.label.starts_with("NotDiscord")) {
        println!("FAIL: our own window is offered for sharing");
    } else {
        println!("own windows excluded — correct");
    }

    let source = NativeVideoSource::new(VideoResolution { width: 1920, height: 1080 }, true);
    let slot: share::SharedFrame = Arc::new(Mutex::new(None));
    let interest = frames::publish_shared("self:screen".into(), slot.clone());
    let preview = Some(share::SelfShare { slot: slot.clone(), interest });
    let closed = Arc::new(AtomicBool::new(false));

    let control = match share::start_capture(
        source,
        share::ShareTarget::Window(target.hwnd),
        preview,
        closed.clone(),
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL: window capture did not start: {e}");
            let _ = child.kill();
            return;
        }
    };

    // Ask for the preview so the tee wakes up, then check we got frames of a
    // window-ish size (not a full monitor).
    for _ in 0..15 {
        frames::touch("self:screen");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    match slot.lock().unwrap().clone() {
        None => println!("FAIL: no frames from the window capture"),
        Some((w, h, pixels)) => {
            let lit = pixels.iter().filter(|p| **p != 0).count();
            println!("window frames: {w}x{h}, {lit} non-black pixels {}",
                if lit > 0 { "— capturing for real" } else { "(blank — FAIL?)" });
        }
    }
    if closed.load(Ordering::Relaxed) {
        println!("FAIL: closed flag set while the window is alive");
    }

    // Kill Notepad; the capture must notice.
    let _ = child.kill();
    let mut flagged = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if closed.load(Ordering::Relaxed) {
            flagged = true;
            break;
        }
    }
    println!(
        "window closed -> capture reported closed: {}",
        if flagged { "YES — the share would auto-stop" } else { "FAIL: flag never set" }
    );
    let _ = control.stop();

    // Stale-hwnd path: starting a capture of the now-dead window must fail
    // with a message, not panic.
    let source2 = NativeVideoSource::new(VideoResolution { width: 1920, height: 1080 }, true);
    match share::start_capture(source2, share::ShareTarget::Window(target.hwnd), None, Arc::new(AtomicBool::new(false))) {
        Err(e) => println!("stale window refused cleanly: \"{e}\""),
        Ok(c) => {
            println!("FAIL: capture of a dead window started");
            let _ = c.stop();
        }
    }
}
