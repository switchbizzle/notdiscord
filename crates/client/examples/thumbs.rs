//! Checks the share picker's thumbnails: one still per target, small enough
//! to sit in the DOM, and actual picture rather than a black rectangle.
//! `cargo run -p client --example thumbs`

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[path = "../src/frames.rs"]
mod frames;
#[path = "../src/share.rs"]
mod share;

fn main() {
    // Capture threads need the multithreaded COM apartment (v0.47.1).
    unsafe {
        use winapi::um::combaseapi::CoInitializeEx;
        use winapi::um::objbase::COINIT_MULTITHREADED;
        let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED);
    }
    let _ = Arc::new(AtomicBool::new(false));

    let started = std::time::Instant::now();
    let monitors = share::list_monitors();
    let windows = share::list_windows();
    println!("targets: {} monitors, {} windows", monitors.len(), windows.len());

    let mut ok = 0;
    let mut failed = 0;
    let mut total_bytes = 0usize;

    for m in &monitors {
        let t0 = std::time::Instant::now();
        match share::thumbnail(share::ShareTarget::Monitor(m.index)) {
            Some(jpeg) => {
                let jpeg_ok = jpeg.starts_with(&[0xFF, 0xD8, 0xFF]);
                total_bytes += jpeg.len();
                ok += 1;
                println!(
                    "  [monitor {}] {} bytes in {:?} {}",
                    m.index,
                    jpeg.len(),
                    t0.elapsed(),
                    if jpeg_ok { "— JPEG" } else { "— NOT A JPEG (FAIL)" }
                );
            }
            None => {
                failed += 1;
                println!("  [monitor {}] no frame (FAIL)", m.index);
            }
        }
    }

    for w in windows.iter().take(6) {
        let t0 = std::time::Instant::now();
        match share::thumbnail(share::ShareTarget::Window(w.hwnd)) {
            Some(jpeg) => {
                total_bytes += jpeg.len();
                ok += 1;
                println!("  [{}] {} bytes in {:?}", w.label, jpeg.len(), t0.elapsed());
            }
            None => {
                // Minimized windows legitimately never paint.
                failed += 1;
                println!("  [{}] no frame (minimized?)", w.label);
            }
        }
    }

    println!(
        "\n{ok} thumbnails, {failed} without frames, {} KB total, {:?} elapsed",
        total_bytes / 1024,
        started.elapsed()
    );
    println!(
        "size sanity: {}",
        if total_bytes / ok.max(1) < 40_000 { "each well under 40KB — fine as data URIs" } else { "TOO BIG" }
    );

    // A dead window must not hang or panic.
    let t0 = std::time::Instant::now();
    match share::thumbnail(share::ShareTarget::Window(0x1)) {
        None => println!("bogus window: refused in {:?} — no hang", t0.elapsed()),
        Some(_) => println!("FAIL: bogus window produced a thumbnail"),
    }
}
