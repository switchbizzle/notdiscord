//! Dev harness: join a LiveKit room, subscribe to audio, and count frames —
//! proves a publisher (the music sidecar) is actually streaming sound.
//! Usage: listen <ws-url> <token> [seconds]

use futures_util::StreamExt;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::{Room, RoomEvent, RoomOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("ws url");
    let token = args.next().expect("token");
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let (room, mut events) = Room::connect(&url, &token, RoomOptions::default()).await?;
    println!("connected to {}", room.name());

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    RoomEvent::TrackSubscribed { track, participant, .. } => {
                        println!("subscribed to {} from {} ({})",
                            track.sid(), participant.identity(), participant.name());
                        if let livekit::track::RemoteTrack::Audio(audio) = track {
                            let counter = counter.clone();
                            tokio::spawn(async move {
                                let mut stream = NativeAudioStream::new(audio.rtc_track(), 48000, 2);
                                let mut peak: u16 = 0;
                                while let Some(frame) = stream.next().await {
                                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    for s in frame.data.iter() {
                                        peak = peak.max(s.unsigned_abs());
                                    }
                                    if counter.load(std::sync::atomic::Ordering::Relaxed) % 300 == 0 {
                                        println!("  ...peak amplitude so far: {peak}");
                                    }
                                }
                            });
                        }
                    }
                    RoomEvent::ParticipantConnected(p) => {
                        println!("participant joined: {} ({})", p.identity(), p.name());
                    }
                    _ => {}
                }
            }
        }
    }

    let frames = counter.load(std::sync::atomic::Ordering::Relaxed);
    println!("RESULT: {frames} audio frames received in {seconds}s");
    room.close().await.ok();
    Ok(())
}
