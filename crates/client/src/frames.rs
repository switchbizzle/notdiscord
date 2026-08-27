//! Live video frames for the in-app Video tab.
//!
//! LiveKit hands us raw frames on its own threads; a WebView can only show
//! what it can fetch over HTTP. So every video stream gets a slot here, a pump
//! keeps the slot's latest frame encoded as JPEG, and the `ndvideo` protocol
//! (registered in `main`) hands those bytes to an `<img>`.
//!
//! Encoding only happens while something is actually asking: the protocol
//! handler stamps the slot each time it serves it, and a pump whose slot
//! hasn't been asked for in the last second drops its frames without touching
//! them. A 1440p share that nobody is looking at costs nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use livekit::track::RemoteVideoTrack;
use livekit::webrtc::video_frame::VideoFormatType;
use livekit::webrtc::video_stream::native::NativeVideoStream;

/// Raw frames as the rest of the client passes them around: 0RGB in a u32.
pub type SharedFrame = Arc<Mutex<Option<(u32, u32, Vec<u32>)>>>;

/// Tiles are small; a screen share doesn't need to arrive pixel-perfect.
const MAX_EDGE: u32 = 960;
/// One frame every ~66ms. Smooth enough for a tile, cheap enough for six.
const MIN_INTERVAL_MS: u64 = 60;
/// How long after the last request a stream keeps encoding.
const WANTED_FOR_MS: u64 = 1000;
const QUALITY: u8 = 72;

#[derive(Clone, Default)]
struct Slot {
    jpeg: Arc<Mutex<Option<Arc<Vec<u8>>>>>,
    last_wanted: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}

fn registry() -> &'static Mutex<HashMap<String, Slot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Milliseconds since the first call, never zero — zero is the "never asked"
/// sentinel, and at startup a real timestamp of 0 would look like one.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn make_slot(key: &str) -> Slot {
    let slot = Slot {
        jpeg: Default::default(),
        last_wanted: Arc::new(AtomicU64::new(0)),
        alive: Arc::new(AtomicBool::new(true)),
    };
    // Replacing a key retires whatever was pumping into it.
    if let Some(old) = registry().lock().unwrap().insert(key.to_owned(), slot.clone()) {
        old.alive.store(false, Ordering::Relaxed);
    }
    slot
}

/// The most recent frame for `key`, and a note that someone wants it.
pub fn latest(key: &str) -> Option<Arc<Vec<u8>>> {
    let slot = registry().lock().unwrap().get(key).cloned()?;
    slot.last_wanted.store(now_ms(), Ordering::Relaxed);
    let jpeg = slot.jpeg.lock().unwrap().clone();
    jpeg
}

/// Stop pumping `key` — the stream ended or the person left.
pub fn unpublish(key: &str) {
    if let Some(slot) = registry().lock().unwrap().remove(key) {
        slot.alive.store(false, Ordering::Relaxed);
    }
}

/// Drop everything (leaving the call).
pub fn clear() {
    for (_, slot) in registry().lock().unwrap().drain() {
        slot.alive.store(false, Ordering::Relaxed);
    }
}

fn wanted(slot: &Slot, now: u64) -> bool {
    let last = slot.last_wanted.load(Ordering::Relaxed);
    // Zero means nobody has ever asked for this stream.
    last != 0 && now.saturating_sub(last) < WANTED_FOR_MS
}

/// Pump a remote participant's video into `key` until the track ends.
pub fn publish_track(key: String, track: &RemoteVideoTrack) {
    let slot = make_slot(&key);
    let rtc = track.rtc_track();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut stream = NativeVideoStream::new(rtc);
        let mut last = 0u64;
        while let Some(frame) = stream.next().await {
            if !slot.alive.load(Ordering::Relaxed) {
                break;
            }
            let now = now_ms();
            if !wanted(&slot, now) || now.saturating_sub(last) < MIN_INTERVAL_MS {
                continue;
            }
            let (width, height) = (frame.buffer.width(), frame.buffer.height());
            if width == 0 || height == 0 {
                continue;
            }
            last = now;
            // libyuv's "ABGR" writes RGBA bytes, which is what we want here.
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            frame.buffer.to_argb(
                VideoFormatType::ABGR,
                &mut rgba,
                width * 4,
                width as i32,
                height as i32,
            );
            if let Some(bytes) = encode(&rgba, width, height) {
                *slot.jpeg.lock().unwrap() = Some(Arc::new(bytes));
            }
        }
        slot.alive.store(false, Ordering::Relaxed);
    });
}

/// Pump a locally-captured preview (your own camera or screen) into `key`.
/// These arrive in a shared slot rather than off the wire, so this polls.
///
/// Returns the slot's interest stamp, so whatever fills `source` can skip the
/// work when nothing is looking — a 1440p screen share is far too expensive to
/// copy for a preview nobody has open.
pub fn publish_shared(key: String, source: SharedFrame) -> Arc<AtomicU64> {
    let slot = make_slot(&key);
    let interest = slot.last_wanted.clone();
    tokio::spawn(async move {
        loop {
            if !slot.alive.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(MIN_INTERVAL_MS)).await;
            if !wanted(&slot, now_ms()) {
                continue;
            }
            let Some((width, height, pixels)) = source.lock().unwrap().clone() else { continue };
            if width == 0 || height == 0 {
                continue;
            }
            // 0RGB in a u32 -> RGBA bytes.
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for px in &pixels {
                rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8, 255]);
            }
            if let Some(bytes) = encode(&rgba, width, height) {
                *slot.jpeg.lock().unwrap() = Some(Arc::new(bytes));
            }
        }
    });
    interest
}

/// Note that something wants `key` right now — for viewers that read a slot
/// directly instead of going through the protocol (the call window).
pub fn touch(key: &str) {
    if let Some(slot) = registry().lock().unwrap().get(key) {
        slot.last_wanted.store(now_ms(), Ordering::Relaxed);
    }
}

/// Has `key` been asked for recently enough to be worth producing?
pub fn is_wanted(stamp: &AtomicU64) -> bool {
    let last = stamp.load(Ordering::Relaxed);
    last != 0 && now_ms().saturating_sub(last) < WANTED_FOR_MS
}

/// RGBA -> JPEG, scaled down so a 1440p share doesn't cost 1440p of encoding.
fn encode(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let scale = (MAX_EDGE as f32 / width.max(height) as f32).min(1.0);
    let (out_w, out_h) = (
        ((width as f32 * scale) as u32).max(1),
        ((height as f32 * scale) as u32).max(1),
    );
    let mut rgb = Vec::with_capacity((out_w * out_h * 3) as usize);
    for y in 0..out_h {
        let src_y = (y as u64 * height as u64 / out_h as u64) as u32;
        let row = (src_y * width) as usize * 4;
        for x in 0..out_w {
            let src_x = (x as u64 * width as u64 / out_w as u64) as u32;
            let i = row + src_x as usize * 4;
            rgb.extend_from_slice(rgba.get(i..i + 3)?);
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, QUALITY)
        .encode(&rgb, out_w, out_h, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}
