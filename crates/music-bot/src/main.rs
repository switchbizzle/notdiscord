//! The NotDiscord music sidecar: a LiveKit participant that streams
//! SoundCloud audio into a voice room. Driven by the main server over
//! localhost HTTP — it never talks to clients directly.
//!
//! Pipeline per track: yt-dlp resolves the page URL to a stream URL, ffmpeg
//! decodes it to 48kHz stereo s16le on stdout, and 10ms frames are pumped
//! into a NativeAudioSource (capture_frame's bounded buffer paces playback).

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, TrackSource};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::{Room, RoomOptions};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// 10ms of stereo 48kHz s16le.
const FRAME_SAMPLES_PER_CH: usize = 480;
const FRAME_BYTES: usize = FRAME_SAMPLES_PER_CH * CHANNELS as usize * 2;

#[derive(Clone, Debug, Serialize)]
struct Track {
    url: String,
    title: String,
}

/// Control flags shared with the playback worker.
struct Controls {
    skip: AtomicBool,
    stop: AtomicBool,
}

struct Session {
    room_name: String,
    queue: VecDeque<Track>,
    now_playing: Option<Track>,
    controls: Arc<Controls>,
}

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Session>>,
}

type Shared = Arc<AppState>;

fn ytdlp_cmd() -> Vec<String> {
    std::env::var("YTDLP_CMD")
        .unwrap_or_else(|_| "yt-dlp".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn ffmpeg_cmd() -> String {
    std::env::var("FFMPEG_CMD").unwrap_or_else(|_| "ffmpeg".into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "music_bot=info".into()),
        )
        .init();

    let state: Shared = Arc::new(AppState::default());
    let app = Router::new()
        .route("/play", post(play))
        .route("/skip", post(skip))
        .route("/stop", post(stop))
        .route("/status", get(status))
        .with_state(state);

    let addr = std::env::var("MUSIC_ADDR").unwrap_or_else(|_| "127.0.0.1:3001".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("music sidecar listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------- HTTP API ----------

#[derive(Deserialize)]
struct PlayRequest {
    /// LiveKit room name (e.g. channel-7).
    room: String,
    /// LiveKit websocket URL.
    lk_url: String,
    /// Room-scoped access token minted by the main server.
    token: String,
    /// SoundCloud (or anything yt-dlp handles) track or playlist page URL.
    url: String,
}

#[derive(Serialize)]
struct PlayResponse {
    queued: usize,
    started: bool,
}

async fn play(
    State(state): State<Shared>,
    Json(req): Json<PlayRequest>,
) -> Result<Json<PlayResponse>, (StatusCode, String)> {
    let tracks = resolve_tracks(&req.url)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("could not resolve that link: {e}")))?;
    if tracks.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no playable tracks in that link".into()));
    }
    let queued = tracks.len();

    let mut guard = state.session.lock().unwrap();
    match guard.as_mut() {
        Some(session) if session.room_name == req.room => {
            session.queue.extend(tracks);
            Ok(Json(PlayResponse { queued, started: false }))
        }
        Some(session) => Err((
            StatusCode::CONFLICT,
            format!("already playing in another channel ({})", session.room_name),
        )),
        None => {
            let controls = Arc::new(Controls { skip: AtomicBool::new(false), stop: AtomicBool::new(false) });
            *guard = Some(Session {
                room_name: req.room.clone(),
                queue: tracks.into(),
                now_playing: None,
                controls: controls.clone(),
            });
            drop(guard);
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = run_session(state.clone(), req, controls).await {
                    tracing::warn!("session ended with error: {e}");
                }
                *state.session.lock().unwrap() = None;
            });
            Ok(Json(PlayResponse { queued, started: true }))
        }
    }
}

async fn skip(State(state): State<Shared>) -> StatusCode {
    if let Some(session) = state.session.lock().unwrap().as_ref() {
        session.controls.skip.store(true, Ordering::Relaxed);
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn stop(State(state): State<Shared>) -> StatusCode {
    if let Some(session) = state.session.lock().unwrap().as_ref() {
        session.controls.stop.store(true, Ordering::Relaxed);
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

#[derive(Serialize)]
struct StatusResponse {
    active: bool,
    room: Option<String>,
    now_playing: Option<Track>,
    queue_len: usize,
    up_next: Vec<String>,
}

async fn status(State(state): State<Shared>) -> Json<StatusResponse> {
    let guard = state.session.lock().unwrap();
    Json(match guard.as_ref() {
        Some(s) => StatusResponse {
            active: true,
            room: Some(s.room_name.clone()),
            now_playing: s.now_playing.clone(),
            queue_len: s.queue.len(),
            up_next: s.queue.iter().take(5).map(|t| t.title.clone()).collect(),
        },
        None => StatusResponse {
            active: false,
            room: None,
            now_playing: None,
            queue_len: 0,
            up_next: Vec::new(),
        },
    })
}

// ---------- Track resolution (yt-dlp) ----------

/// Expand a page URL into one or more playable tracks (flat, fast).
async fn resolve_tracks(url: &str) -> anyhow::Result<Vec<Track>> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("not a URL");
    }
    let cmd = ytdlp_cmd();
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(["-J", "--flat-playlist", "--no-warnings", url])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("yt-dlp failed"));
    }
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    let mut tracks = Vec::new();
    if let Some(entries) = info["entries"].as_array() {
        for entry in entries {
            let Some(track_url) = entry["url"].as_str().or(entry["webpage_url"].as_str()) else { continue };
            let title = entry["title"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| slug_title(track_url));
            tracks.push(Track { url: track_url.to_owned(), title });
        }
    } else {
        tracks.push(Track {
            url: info["webpage_url"].as_str().unwrap_or(url).to_owned(),
            title: info["title"].as_str().unwrap_or(url).to_owned(),
        });
    }
    Ok(tracks)
}

/// Readable stand-in title from a track URL's slug ("some-track-1" → "some track 1").
fn slug_title(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url)
        .replace('-', " ")
}

/// Resolve one track page to (title, direct stream URL).
async fn resolve_stream(page_url: &str) -> anyhow::Result<(String, String)> {
    let cmd = ytdlp_cmd();
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(["-f", "bestaudio/best", "--no-warnings", "--print", "title", "--print", "urls", page_url])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("yt-dlp failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let title = lines.next().unwrap_or(page_url).to_owned();
    let stream = lines.next().ok_or_else(|| anyhow::anyhow!("no stream url"))?.to_owned();
    Ok((title, stream))
}

// ---------- Playback ----------

async fn run_session(state: Shared, req: PlayRequest, controls: Arc<Controls>) -> anyhow::Result<()> {
    tracing::info!("joining {} at {}", req.room, req.lk_url);
    let (room, mut events) = Room::connect(&req.lk_url, &req.token, RoomOptions::default()).await?;
    // Drain room events so the connection stays healthy.
    tokio::spawn(async move { while events.recv().await.is_some() {} });

    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        RATE,
        CHANNELS,
        1000,
    );
    let track = LocalAudioTrack::create_audio_track("music", RtcAudioSource::Native(source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
        )
        .await?;

    loop {
        if controls.stop.load(Ordering::Relaxed) {
            break;
        }
        let next = {
            let mut guard = state.session.lock().unwrap();
            let Some(session) = guard.as_mut() else { break };
            let next = session.queue.pop_front();
            session.now_playing = next.clone();
            next
        };
        let Some(track) = next else { break };

        controls.skip.store(false, Ordering::Relaxed);
        // Resolve the stream (and the real title — flat playlist entries
        // often only carry URLs) before announcing.
        match resolve_stream(&track.url).await {
            Ok((title, stream_url)) => {
                if let Some(session) = state.session.lock().unwrap().as_mut() {
                    session.now_playing = Some(Track { url: track.url.clone(), title: title.clone() });
                }
                tracing::info!("playing: {title}");
                if let Err(e) = stream_pcm(&source, &stream_url, &controls).await {
                    tracing::warn!("track failed ({title}): {e}");
                }
            }
            Err(e) => tracing::warn!("could not resolve {}: {e}", track.url),
        }
    }

    tracing::info!("leaving {}", req.room);
    room.close().await.ok();
    Ok(())
}

/// Stream one resolved URL into the source until it ends or skip/stop is set.
async fn stream_pcm(source: &NativeAudioSource, stream_url: &str, controls: &Controls) -> anyhow::Result<()> {
    let mut ffmpeg = Command::new(ffmpeg_cmd())
        .args([
            "-loglevel", "error",
            "-i", stream_url,
            "-vn",
            "-f", "s16le",
            "-ar", "48000",
            "-ac", "2",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = ffmpeg.stdout.take().unwrap();

    let mut buf = vec![0u8; FRAME_BYTES];
    loop {
        if controls.skip.load(Ordering::Relaxed) || controls.stop.load(Ordering::Relaxed) {
            let _ = ffmpeg.start_kill();
            break;
        }
        // Fill a whole 10ms frame (read_exact over the pipe).
        let mut filled = 0;
        while filled < FRAME_BYTES {
            match stdout.read(&mut buf[filled..]).await? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break; // track over
        }
        buf[filled..].fill(0);

        let samples: Vec<i16> = buf
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let frame = AudioFrame {
            data: samples.into(),
            sample_rate: RATE,
            num_channels: CHANNELS,
            samples_per_channel: FRAME_SAMPLES_PER_CH as u32,
        };
        if source.capture_frame(&frame).await.is_err() {
            break;
        }
    }
    let _ = ffmpeg.wait().await;
    Ok(())
}
