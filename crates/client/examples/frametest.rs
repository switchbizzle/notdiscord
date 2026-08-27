//! Checks the Video tab's frame path without needing a call: publish a
//! synthetic preview, then read back what the `ndvideo` protocol would serve.
//! Also checks the idle rule — a stream nobody is asking for shouldn't encode.
//! `cargo run -p client --example frametest`

use std::sync::{Arc, Mutex};

#[path = "../src/frames.rs"]
mod frames;

/// A recognisable 1280x720 pattern in 0RGB, the layout camera previews use.
fn pattern() -> (u32, u32, Vec<u32>) {
    let (w, h) = (1280u32, 720u32);
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w) as u32;
            let g = (y * 255 / h) as u32;
            px[(y * w + x) as usize] = (r << 16) | (g << 8) | 0x40;
        }
    }
    (w, h, px)
}

#[tokio::main]
async fn main() {
    let slot: frames::SharedFrame = Arc::new(Mutex::new(Some(pattern())));
    frames::publish_shared("test:camera".into(), slot.clone());

    // Nothing has asked yet, so nothing should have been encoded.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let idle = frames::latest("test:camera");
    println!(
        "before anyone looks: {}",
        match &idle {
            None => "no frame encoded — correct, it stays idle".to_string(),
            Some(bytes) => format!("FAIL: encoded {} bytes while idle", bytes.len()),
        }
    );

    // That call registered interest; frames should start arriving.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    match frames::latest("test:camera") {
        None => println!("FAIL: still no frame after asking"),
        Some(jpeg) => {
            let magic = jpeg.starts_with(&[0xFF, 0xD8, 0xFF]);
            let decoded = image::load_from_memory(&jpeg).ok();
            let size = decoded.as_ref().map(|i| (i.width(), i.height()));
            println!("after asking: {} bytes, jpeg magic: {magic}", jpeg.len());
            match size {
                // 1280x720 scaled to fit a 960 edge is 960x540.
                Some((960, 540)) => println!("decoded 960x540 — scaled down as intended"),
                Some((w, h)) => println!("FAIL: decoded {w}x{h}, expected 960x540"),
                None => println!("FAIL: bytes aren't a decodable image"),
            }
        }
    }

    // An unknown key is what the protocol answers 204 to.
    println!(
        "unknown key: {}",
        match frames::latest("nobody:camera") {
            None => "None — the handler will 204",
            Some(_) => "FAIL: returned bytes for a key that was never published",
        }
    );

    frames::unpublish("test:camera");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!("after unpublish: {:?}", frames::latest("test:camera").map(|b| b.len()));
}
