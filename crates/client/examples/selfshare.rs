//! Checks that sharing your screen produces a preview you can actually see.
//!
//! Your published track never comes back to you, so without the capture tee
//! you're the only person in the call who can't see your own share. This
//! starts a real capture of the primary monitor and checks the preview slot —
//! and checks the idle rule too, since a 1440p tee that runs when nobody is
//! looking would be far more expensive than the bug it fixes.
//! `cargo run -p client --example selfshare`

use std::sync::{Arc, Mutex};

use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;

#[path = "../src/frames.rs"]
mod frames;
#[path = "../src/share.rs"]
mod share;

#[tokio::main]
async fn main() {
    // true = screencast, matching how voice.rs publishes a share.
    let source = NativeVideoSource::new(VideoResolution { width: 1920, height: 1080 }, true);
    let slot: share::SharedFrame = Arc::new(Mutex::new(None));
    let interest = frames::publish_shared("self:screen".into(), slot.clone());
    let preview = Some(share::SelfShare { slot: slot.clone(), interest });

    let control = match share::start_capture(source, None, preview) {
        Ok(control) => control,
        Err(e) => {
            println!("FAIL: could not start capture: {e}");
            return;
        }
    };

    // Nobody is looking yet: the tee should stay idle.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    match slot.lock().unwrap().clone() {
        None => println!("nobody looking: no preview copied — correct"),
        Some((w, h, _)) => println!("FAIL: copied a {w}x{h} frame while idle"),
    }

    // The call window stamps interest through frames::touch; the Video tab
    // does it by fetching. Either way the tee should wake up.
    for _ in 0..12 {
        frames::touch("self:screen");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    match slot.lock().unwrap().clone() {
        None => println!("FAIL: still no preview after asking for one"),
        Some((w, h, pixels)) => {
            let expected = (w * h) as usize;
            let lit = pixels.iter().filter(|p| **p != 0).count();
            println!("preview: {w}x{h}, {} pixels", pixels.len());
            println!(
                "  buffer size: {}",
                if pixels.len() == expected { "matches w*h" } else { "FAIL: wrong length" }
            );
            println!(
                "  content    : {lit} non-black pixels {}",
                if lit > expected / 20 { "— a real screen" } else { "FAIL: looks blank" }
            );
            println!(
                "  scaled     : {}",
                if w.max(h) <= 960 { "within the 960px tile budget" } else { "FAIL: full size" }
            );
        }
    }

    // And it should reach the Video tab as a JPEG.
    match frames::latest("self:screen") {
        Some(jpeg) if jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) => {
            println!("ndvideo would serve {} bytes of JPEG", jpeg.len())
        }
        Some(_) => println!("FAIL: bytes aren't a JPEG"),
        None => println!("FAIL: nothing for the Video tab to serve"),
    }

    let _ = control.stop();
}
