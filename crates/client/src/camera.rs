//! Webcam capture feeding a LiveKit camera track (video calls).
//!
//! The camera is confined to its own thread: MediaFoundation objects are
//! COM-based and not Send, so the thread owns the `Camera` end-to-end and a
//! stop flag is the only cross-thread handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

pub struct CameraHandle {
    stop: Arc<AtomicBool>,
    /// Cleared to close the self-preview window along with the camera.
    preview_alive: Option<Arc<AtomicBool>>,
}

impl CameraHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(alive) = &self.preview_alive {
            alive.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for CameraHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the default webcam and pump frames into `source` until stopped.
/// Blocks until the camera is actually open, so failures surface to the caller.
/// `preview` also mirrors frames into a viewer window so you can see yourself.
pub fn start_camera(
    source: NativeVideoSource,
    preview: Option<(crate::share::SharedFrame, Arc<AtomicBool>)>,
) -> Result<CameraHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let preview_alive = preview.as_ref().map(|(_, alive)| alive.clone());
    let preview_slot = preview.map(|(slot, _)| slot);
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::spawn(move || {
        let format = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::MJPEG, 30),
        ));
        let mut cam = match Camera::new(CameraIndex::Index(0), format) {
            Ok(cam) => cam,
            Err(e) => {
                let _ = tx.send(Err(format!("couldn't open webcam: {e}")));
                return;
            }
        };
        if let Err(e) = cam.open_stream() {
            let _ = tx.send(Err(format!("couldn't start webcam stream: {e}")));
            return;
        }
        let _ = tx.send(Ok(()));

        let mut last_preview = std::time::Instant::now() - std::time::Duration::from_secs(1);
        while !stop_thread.load(Ordering::Relaxed) {
            let buffer = match cam.frame() {
                Ok(buffer) => buffer,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    continue;
                }
            };
            let Ok(img) = buffer.decode_image::<RgbAFormat>() else {
                continue;
            };
            let (width, height) = (img.width(), img.height());
            if width == 0 || height == 0 {
                continue;
            }
            let mut i420 = I420Buffer::new(width, height);
            let (sy, su, sv) = i420.strides();
            let (dy, du, dv) = i420.data_mut();
            // RGBA bytes == libyuv "ABGR" word order.
            yuv_helper::abgr_to_i420(
                img.as_raw(),
                width * 4,
                dy,
                sy,
                du,
                su,
                dv,
                sv,
                width as i32,
                height as i32,
            );
            let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, i420);
            frame.timestamp_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            source.capture_frame(&frame);

            // Self-preview, throttled to ~15fps: seeing yourself doesn't need
            // every frame, and each one is a full-resolution copy.
            if let Some(slot) = &preview_slot {
                if last_preview.elapsed() >= std::time::Duration::from_millis(66) {
                    last_preview = std::time::Instant::now();
                    let pixels: Vec<u32> = img
                        .as_raw()
                        .chunks_exact(4)
                        .map(|p| ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32)
                        .collect();
                    *slot.lock().unwrap() = Some((width, height, pixels));
                }
            }
        }
        let _ = cam.stop_stream();
    });

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(CameraHandle { stop, preview_alive }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("webcam open timed out".into()),
    }
}
