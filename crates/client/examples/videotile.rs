//! End-to-end check of the Video tab's tile: a synthetic moving picture goes
//! into the frame registry, the `ndvideo` protocol serves it, and an <img>
//! whose src is driven by a tick signal shows it — the exact path that froze
//! when a JS interval used to drive the refresh.
//!
//! Run it, then drive it over CDP (port 9229) to read the <img> back.
//! `cargo run -p client --example videotile`

use std::sync::{Arc, Mutex};

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

#[path = "../src/frames.rs"]
mod frames;

const KEY: &str = "test:camera";

/// A bar that marches across the frame, so successive encodes differ.
fn animate(slot: frames::SharedFrame) {
    std::thread::spawn(move || {
        let (w, h) = (640u32, 360u32);
        let mut step = 0u32;
        loop {
            let mut px = vec![0u32; (w * h) as usize];
            let bar = (step * 7) % w;
            for y in 0..h {
                for x in 0..w {
                    let lit = x.abs_diff(bar) < 40;
                    let shade = (y * 200 / h) as u32;
                    px[(y * w + x) as usize] =
                        if lit { 0x00ff_ffff } else { (shade << 16) | (shade << 8) | 0x60 };
                }
            }
            *slot.lock().unwrap() = Some((w, h, px));
            step += 1;
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

fn main() {
    let window = WindowBuilder::new()
        .with_title("videotile")
        .with_inner_size(LogicalSize::new(700.0, 460.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_window(window).with_custom_protocol("ndvideo", |_id, request| {
                use dioxus::desktop::wry::http::Response;
                let key = request.uri().path().trim_start_matches('/').to_owned();
                match frames::latest(&key) {
                    Some(jpeg) => Response::builder()
                        .status(200)
                        .header("Content-Type", "image/jpeg")
                        .header("Cache-Control", "no-store")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(std::borrow::Cow::Owned(jpeg.to_vec()))
                        .unwrap(),
                    None => Response::builder()
                        .status(204)
                        .header("Access-Control-Allow-Origin", "*")
                        .body(std::borrow::Cow::Borrowed(&[][..]))
                        .unwrap(),
                }
            }),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
    use_hook(|| {
        let slot: frames::SharedFrame = Arc::new(Mutex::new(None));
        animate(slot.clone());
        frames::publish_shared(KEY.into(), slot);
    });

    // Exactly what VideoTab does now.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        let mut frame = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(70)).await;
            frame += 1;
            tick.set(frame);
        }
    });
    let stamp = tick();

    rsx! {
        style { "body {{ background:#1e1f22; margin:0 }} img {{ width:100%; display:block }}" }
        img { id: "tile", src: "http://ndvideo.localhost/{KEY}?t={stamp}", alt: "" }
        div { id: "stamp", style: "color:#888;font:12px sans-serif;padding:6px", "tick {stamp}" }
    }
}
