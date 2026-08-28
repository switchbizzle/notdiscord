//! Regression test for the viewer: opening a SECOND viewer window used to be
//! impossible (winit allows one event loop per process, and the old code built
//! a fresh loop per window — so every Watch after the first silently failed).
//! Opens two windows, feeds each a colour, and verifies both exist by title.

use std::sync::atomic::Ordering;

fn fill(latest: &client_share::SharedFrame, color: u32) {
    let (w, h) = (320u32, 180u32);
    *latest.lock().unwrap() = Some((w, h, vec![color; (w * h) as usize]));
}

// The example links the binary crate's modules directly. share.rs reaches
// for crate::frames (the capture tee), so that module rides along.
#[path = "../src/frames.rs"]
pub mod frames;
#[path = "../src/share.rs"]
mod client_share_impl;
use client_share_impl as client_share;

fn main() {
    let first = client_share::open_frame_viewer("Viewer One — NotDiscord".into());
    let (latest1, alive1) = match first {
        Ok(v) => v,
        Err(e) => {
            println!("FAIL: first viewer: {e}");
            return;
        }
    };
    fill(&latest1, 0x00ff5500);
    std::thread::sleep(std::time::Duration::from_secs(2));

    // The moment of truth: a second window on the same loop.
    let second = client_share::open_frame_viewer("Viewer Two — NotDiscord".into());
    let (latest2, alive2) = match second {
        Ok(v) => v,
        Err(e) => {
            println!("FAIL: second viewer: {e}");
            return;
        }
    };
    fill(&latest2, 0x000088ff);
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Verify both windows really exist, by title.
    use windows_capture::window::Window;
    let one = Window::from_contains_name("Viewer One").is_ok();
    let two = Window::from_contains_name("Viewer Two").is_ok();
    println!("window 'Viewer One' exists: {one}");
    println!("window 'Viewer Two' exists: {two}");
    println!("alive flags: {} / {}", alive1.load(Ordering::Relaxed), alive2.load(Ordering::Relaxed));
    println!(
        "RESULT: {}",
        if one && two { "PASS — two simultaneous viewers" } else { "FAIL" }
    );
}
