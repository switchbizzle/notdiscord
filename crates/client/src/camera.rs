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
}

impl CameraHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CameraHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the default webcam and pump frames into `source` until stopped.
/// Blocks until the camera is actually open, so failures surface to the caller.
pub fn start_camera(source: NativeVideoSource) -> Result<CameraHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
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
        }
        let _ = cam.stop_stream();
    });

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(CameraHandle { stop }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("webcam open timed out".into()),
    }
}
