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
use livekit::options::{AudioEncoding, TrackPublishOptions};
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
    /// Stable id, so reordering and removing are unambiguous.
    id: u64,
    url: String,
    title: String,
    #[serde(default)]
    artist: String,
    /// Cover art URL, shown in the music tab.
    #[serde(default)]
    art: Option<String>,
    /// Seconds, when yt-dlp knows.
    #[serde(default)]
    duration: Option<f64>,
}

fn next_track_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Control flags shared with the playback worker.
struct Controls {
    skip: AtomicBool,
    stop: AtomicBool,
    paused: AtomicBool,
    /// Milliseconds into the current track.
    position_ms: std::sync::atomic::AtomicU64,
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

/// Which rendition to pull.
///
/// The crew's old Discord bot forced progressive http_mp3 here, because HLS
/// made its ffmpeg reconnect between segments. That doesn't reproduce on this
/// pipeline — a 160k HLS rendition decodes to 30 seconds of unbroken PCM with
/// an empty stderr — and forcing progressive actively costs quality: on a
/// SoundCloud track offering hls_aac_160k and http_mp3_1_0, plain "bestaudio"
/// takes the 160k AAC while the progressive-first string settles for 128k mp3.
/// So: highest bitrate wins, whatever the protocol. MUSIC_FORMAT overrides it
/// if a track ever misbehaves.
fn audio_format() -> String {
    std::env::var("MUSIC_FORMAT").unwrap_or_else(|_| "bestaudio/best".into())
}

/// Opus bitrate for the published music track, in bits per second. 128k
/// stereo is transparent enough for a listening room; LiveKit's own default
/// for an unspecified audio track is 48k.
fn music_bitrate() -> u64 {
    std::env::var("MUSIC_BITRATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128_000)
}

/// A SoundCloud cookie file (Netscape format), which is what the crew's old
/// bot used and worth having.
///
/// Measured, because the mechanism isn't the obvious one: signing in does NOT
/// add higher streaming renditions — 160k AAC is the public ceiling either
/// way. What it adds, on tracks where the artist enabled downloads, is the
/// `download` format: the artist's original file. On the Flume re-work of
/// Seekae's "Test & Recognise" that's 13.2 MB against 5.8 MB for the best
/// public rendition, so roughly 320k against 160k. yt-dlp's own "bestaudio"
/// picks it without help. Tracks with downloads disabled see no difference,
/// which is why a single track is a bad way to test this.
fn cookie_args() -> Vec<String> {
    match std::env::var("SOUNDCLOUD_COOKIES") {
        Ok(path) if !path.is_empty() && std::path::Path::new(&path).exists() => {
            vec!["--cookies".into(), path]
        }
        _ => Vec::new(),
    }
}

/// Mastered music is far hotter than voice, so the DJ is attenuated before it
/// hits the room — otherwise everyone has to ride their own slider down and
/// nobody has headroom to turn it *up*. Override with MUSIC_GAIN.
fn music_gain() -> f32 {
    std::env::var("MUSIC_GAIN")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|g| *g > 0.0 && *g <= 2.0)
        .unwrap_or(0.35)
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
        .route("/pause", post(pause))
        .route("/resume", post(resume))
        .route("/status", get(status))
        .route("/queue/move", post(queue_move))
        .route("/queue/remove", post(queue_remove))
        .route("/queue/clear", post(queue_clear))
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
            let controls = Arc::new(Controls {
                skip: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                position_ms: std::sync::atomic::AtomicU64::new(0),
            });
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

/// Pause/resume: playback stops feeding frames, ffmpeg blocks on pipe
/// backpressure, and picks up where it left off on resume.
async fn pause(State(state): State<Shared>) -> StatusCode {
    set_paused(&state, true)
}

async fn resume(State(state): State<Shared>) -> StatusCode {
    set_paused(&state, false)
}

fn set_paused(state: &Shared, paused: bool) -> StatusCode {
    if let Some(session) = state.session.lock().unwrap().as_ref() {
        session.controls.paused.store(paused, Ordering::Relaxed);
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
    /// Seconds into the current track.
    position: f64,
    queue: Vec<Track>,
    queue_len: usize,
    up_next: Vec<String>,
    paused: bool,
}

async fn status(State(state): State<Shared>) -> Json<StatusResponse> {
    let guard = state.session.lock().unwrap();
    Json(match guard.as_ref() {
        Some(s) => StatusResponse {
            active: true,
            room: Some(s.room_name.clone()),
            now_playing: s.now_playing.clone(),
            position: s.controls.position_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            queue: s.queue.iter().cloned().collect(),
            queue_len: s.queue.len(),
            up_next: s.queue.iter().take(5).map(|t| t.title.clone()).collect(),
            paused: s.controls.paused.load(Ordering::Relaxed),
        },
        None => StatusResponse {
            active: false,
            room: None,
            now_playing: None,
            position: 0.0,
            queue: Vec::new(),
            queue_len: 0,
            up_next: Vec::new(),
            paused: false,
        },
    })
}

#[derive(Deserialize)]
struct MoveRequest {
    id: u64,
    /// Negative moves earlier in the queue, positive later.
    offset: i64,
}

/// Reorder one track. Used by the drag handles and the up/down badges.
async fn queue_move(State(state): State<Shared>, Json(req): Json<MoveRequest>) -> StatusCode {
    let mut guard = state.session.lock().unwrap();
    let Some(session) = guard.as_mut() else { return StatusCode::NOT_FOUND };
    let Some(from) = session.queue.iter().position(|t| t.id == req.id) else {
        return StatusCode::NOT_FOUND;
    };
    let to = (from as i64 + req.offset).clamp(0, session.queue.len() as i64 - 1) as usize;
    if to == from {
        return StatusCode::NO_CONTENT;
    }
    let Some(track) = session.queue.remove(from) else { return StatusCode::NOT_FOUND };
    session.queue.insert(to, track);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct RemoveRequest {
    ids: Vec<u64>,
}

/// Drop one or many tracks — the multi-select delete.
async fn queue_remove(State(state): State<Shared>, Json(req): Json<RemoveRequest>) -> StatusCode {
    let mut guard = state.session.lock().unwrap();
    let Some(session) = guard.as_mut() else { return StatusCode::NOT_FOUND };
    session.queue.retain(|t| !req.ids.contains(&t.id));
    StatusCode::NO_CONTENT
}

async fn queue_clear(State(state): State<Shared>) -> StatusCode {
    let mut guard = state.session.lock().unwrap();
    let Some(session) = guard.as_mut() else { return StatusCode::NOT_FOUND };
    session.queue.clear();
    StatusCode::NO_CONTENT
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
            tracks.push(Track {
                id: next_track_id(),
                url: track_url.to_owned(),
                title,
                artist: entry["uploader"].as_str().unwrap_or_default().to_owned(),
                art: best_thumbnail(entry),
                duration: entry["duration"].as_f64(),
            });
        }
    } else {
        tracks.push(Track {
            id: next_track_id(),
            url: info["webpage_url"].as_str().unwrap_or(url).to_owned(),
            title: info["title"].as_str().unwrap_or(url).to_owned(),
            artist: info["uploader"].as_str().unwrap_or_default().to_owned(),
            art: best_thumbnail(&info),
            duration: info["duration"].as_f64(),
        });
    }
    Ok(tracks)
}

/// The biggest thumbnail that isn't enormous — cover art for the music tab.
fn best_thumbnail(info: &serde_json::Value) -> Option<String> {
    if let Some(url) = info["thumbnail"].as_str() {
        return Some(url.to_owned());
    }
    let thumbs = info["thumbnails"].as_array()?;
    thumbs
        .iter()
        .filter(|t| t["width"].as_f64().unwrap_or(0.0) <= 800.0)
        .max_by_key(|t| t["width"].as_f64().unwrap_or(0.0) as i64)
        .or_else(|| thumbs.last())
        .and_then(|t| t["url"].as_str())
        .map(str::to_owned)
}

/// Readable stand-in title from a track URL's slug, used until the real one
/// resolves at play time ("some-track-2" → "Some Track"). SoundCloud's
/// trailing dedup digits aren't part of the name.
fn slug_title(url: &str) -> String {
    let slug = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url);
    let mut words: Vec<String> = slug
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.last().is_some_and(|w| w.chars().all(|c| c.is_ascii_digit())) && words.len() > 1 {
        words.pop();
    }
    words.join(" ")
}

/// One yt-dlp resolve, with or without the cookie file.
async fn ytdlp_resolve(page_url: &str, cookies: Vec<String>) -> anyhow::Result<std::process::Output> {
    let cmd = ytdlp_cmd();
    Ok(Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(cookies)
        .args([
            "-f", &audio_format(), "--no-warnings",
            "--print", "title",
            "--print", "uploader",
            "--print", "thumbnail",
            "--print", "duration",
            "--print", "urls",
            // Which rendition actually won, so the logs can prove it.
            "--print", "format_id",
            "--print", "abr",
            page_url,
        ])
        .stdin(Stdio::null())
        .output()
        .await?)
}

/// Resolve one track page to (title, direct stream URL).
struct Resolved {
    title: String,
    artist: String,
    art: Option<String>,
    duration: Option<f64>,
    stream: String,
}

async fn resolve_stream(page_url: &str) -> anyhow::Result<Resolved> {
    let mut output = ytdlp_resolve(page_url, cookie_args()).await?;
    // A cookie file that's gone stale must not take the music down with it:
    // fall back to anonymous, which still plays, just at the public bitrate.
    if !output.status.success() && !cookie_args().is_empty() {
        tracing::warn!("resolving with the SoundCloud cookie failed; retrying signed out");
        output = ytdlp_resolve(page_url, Vec::new()).await?;
    }
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("yt-dlp failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let title = lines.next().unwrap_or(page_url).to_owned();
    let artist = lines.next().unwrap_or_default().to_owned();
    // yt-dlp prints "NA" for fields it doesn't have.
    let na = |v: &str| (v != "NA" && !v.is_empty()).then(|| v.to_owned());
    let art = lines.next().and_then(na);
    let duration = lines.next().and_then(|d| d.parse::<f64>().ok());
    let stream = lines.next().ok_or_else(|| anyhow::anyhow!("no stream url"))?.to_owned();
    let format_id = lines.next().unwrap_or("?");
    let abr = lines.next().unwrap_or("?");
    tracing::info!("source: {format_id} @ {abr} kbps — {title}");
    Ok(Resolved { title, artist, art, duration, stream })
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
            TrackPublishOptions {
                source: TrackSource::Microphone,
                // The defaults are tuned for talking, and all three hurt music:
                // the built-in preset caps Opus at 48k, DTX stops transmitting
                // through quiet passages (intros and fades come back chopped),
                // and RED spends bitrate on packet redundancy a wired server
                // doesn't need. Override MUSIC_BITRATE to trade quality for
                // bandwidth.
                audio_encoding: Some(AudioEncoding { max_bitrate: music_bitrate() }),
                dtx: false,
                red: false,
                ..Default::default()
            },
        )
        .await?;

    loop {
        if controls.stop.load(Ordering::Relaxed) {
            break;
        }
        let next = {
            let mut guard = state.session.lock().unwrap();
            let Some(session) = guard.as_mut() else { break };
            // now_playing is set only once the real title is resolved, so
            // chat announces each track exactly once.
            session.queue.pop_front()
        };
        let Some(track) = next else { break };

        controls.skip.store(false, Ordering::Relaxed);
        // Resolve the stream (and the real title — flat playlist entries
        // often only carry URLs) before announcing.
        match resolve_stream(&track.url).await {
            Ok(resolved) => {
                let playing = Track {
                    id: track.id,
                    url: track.url.clone(),
                    title: resolved.title.clone(),
                    artist: if resolved.artist.is_empty() { track.artist.clone() } else { resolved.artist },
                    art: resolved.art.or(track.art.clone()),
                    duration: resolved.duration.or(track.duration),
                };
                controls.position_ms.store(0, Ordering::Relaxed);
                if let Some(session) = state.session.lock().unwrap().as_mut() {
                    session.now_playing = Some(playing);
                }
                tracing::info!("playing: {}", resolved.title);
                if let Err(e) = stream_pcm(&source, &resolved.stream, &controls).await {
                    tracing::warn!("track failed ({}): {e}", resolved.title);
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
            // A dropped TCP connection mid-track used to end the track. Let
            // ffmpeg pick the stream back up instead.
            "-reconnect", "1",
            "-reconnect_streamed", "1",
            "-reconnect_delay_max", "5",
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

    let gain = music_gain();
    let mut buf = vec![0u8; FRAME_BYTES];
    loop {
        if controls.skip.load(Ordering::Relaxed) || controls.stop.load(Ordering::Relaxed) {
            let _ = ffmpeg.start_kill();
            break;
        }
        // Paused: stop pulling frames. ffmpeg blocks on pipe backpressure,
        // so the track resumes exactly where it left off.
        if controls.paused.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
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
            .map(|b| {
                let scaled = i16::from_le_bytes([b[0], b[1]]) as f32 * gain;
                scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
            })
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
        // Each frame is 10ms of audio — that's the progress bar.
        controls.position_ms.fetch_add(10, Ordering::Relaxed);
    }
    let _ = ffmpeg.wait().await;
    Ok(())
}
