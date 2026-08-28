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
    /// True when the metadata above came from somewhere better than the
    /// audio source — a Spotify link knows the real title, and whatever
    /// SoundCloud upload we end up playing shouldn't rename it.
    #[serde(default)]
    meta_locked: bool,
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

/// A cookie file handed over by the server (admins paste one into settings),
/// written to disk once so yt-dlp can read it.
static COOKIE_FILE: std::sync::OnceLock<std::sync::Mutex<Option<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

fn cookie_slot() -> &'static std::sync::Mutex<Option<std::path::PathBuf>> {
    COOKIE_FILE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Store (or clear) the server-supplied cookie. Written beside our other
/// temp files, replaced whenever the server sends a different one.
fn set_cookies(contents: Option<&str>) {
    let mut slot = cookie_slot().lock().unwrap();
    match contents.map(str::trim).filter(|c| !c.is_empty()) {
        Some(text) => {
            let path = std::env::temp_dir().join("notdiscord-soundcloud-cookies.txt");
            if std::fs::write(&path, text).is_ok() {
                *slot = Some(path);
            }
        }
        None => {
            if let Some(path) = slot.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// The `--cookies` arguments for yt-dlp, if this server has a SoundCloud
/// login to offer: the one an admin pasted, else the file named by
/// SOUNDCLOUD_COOKIES, which is how our own deployment supplies it.
///
/// Worth having, though the mechanism isn't the obvious one — measured:
/// signing in does NOT add higher streaming renditions, since 160k AAC is the
/// public ceiling either way. What it adds, on tracks where the artist enabled
/// downloads, is the `download` format: the artist's original file. On the
/// Flume re-work of Seekae's "Test & Recognise" that's 13.2 MB against 5.8 MB
/// for the best public rendition, so roughly 320k against 160k. yt-dlp's own
/// "bestaudio" picks it without help. Tracks with downloads disabled see no
/// difference, which is why a single track is a bad way to test this.
fn cookie_args() -> Vec<String> {
    if let Some(path) = cookie_slot().lock().unwrap().clone() {
        if path.exists() {
            return vec!["--cookies".into(), path.to_string_lossy().into_owned()];
        }
    }
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
    /// Optional cookies.txt contents, so HD streaming is configured per
    /// server rather than baked into this container.
    #[serde(default)]
    cookies: Option<String>,
    /// Tracks the server already worked out (Spotify links), to queue
    /// instead of asking yt-dlp what `url` is. Each carries a search phrase;
    /// the audio is found on SoundCloud when the track comes up.
    #[serde(default)]
    plan: Vec<PlannedTrack>,
}

/// One song the server knows about but hasn't found audio for yet.
#[derive(Deserialize)]
struct PlannedTrack {
    query: String,
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    art: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
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
    // The server owns this setting; every request restates it, so clearing
    // it in settings takes effect on the next song.
    set_cookies(req.cookies.as_deref());
    let tracks = if req.plan.is_empty() {
        resolve_tracks(&req.url)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("could not resolve that link: {e}")))?
    } else {
        req.plan.iter().map(planned_to_track).collect()
    };
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
                meta_locked: false,
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
            meta_locked: false,
        });
    }
    Ok(tracks)
}

/// A song the server described becomes a queue entry whose "url" is a search
/// to run later. Resolving every track up front would make a 50-song playlist
/// take a minute to start, and most of that work would be for songs nobody
/// waits around for.
fn planned_to_track(planned: &PlannedTrack) -> Track {
    Track {
        id: next_track_id(),
        url: format!("{SEARCH_PREFIX}{}", planned.query),
        title: planned.title.clone(),
        artist: planned.artist.clone(),
        art: planned.art.clone(),
        duration: planned.duration,
        meta_locked: true,
    }
}

/// Marks a queue entry as "find this on SoundCloud when it comes up".
const SEARCH_PREFIX: &str = "ndsearch:";

/// How many search hits to consider, and how many we will try to play before
/// giving up on a song.
/// Ten, not five: the top hits for anything on a label are SoundCloud's own
/// Go+ uploads, which are DRM protected and refuse to play. The playable
/// copies of a well-known song sit further down the list. Measured on
/// "deadmau5 Strobe": positions 0-2 are all DRM, the first playable copy of
/// the right length is number six.
const SEARCH_WIDTH: usize = 10;
const SEARCH_ATTEMPTS: usize = 4;

/// How far a candidate's length may sit from the one we're looking for and
/// still be the same song. Wide enough for a fade-out or a tacked-on outro,
/// narrow enough to reject the 8-minute remix and the 30-second preview.
const DURATION_TOLERANCE: f64 = 20.0;

/// Find a SoundCloud page for a song we only know by name. Length is the
/// tiebreaker: the top text match is often a cover, a sped-up edit, or a
/// mix that happens to mention the title, and all of those miss badly.
async fn search_soundcloud(query: &str, want: Option<f64>) -> anyhow::Result<Vec<String>> {
    let cmd = ytdlp_cmd();
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(cookie_args())
        .args(["-J", "--flat-playlist", "--no-warnings", &format!("scsearch{SEARCH_WIDTH}:{query}")])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("search failed"));
    }
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let ranked = rank_candidates(&search_candidates(&info), want);
    if ranked.is_empty() {
        anyhow::bail!("nothing on SoundCloud for that");
    }
    Ok(ranked)
}

/// The (url, seconds) pairs a flat search returns.
fn search_candidates(info: &serde_json::Value) -> Vec<(String, Option<f64>)> {
    info["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let url = e["url"].as_str().or(e["webpage_url"].as_str())?;
                    Some((url.to_owned(), e["duration"].as_f64()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Order the search hits by how likely each is to be the song we asked for.
/// When the length is known, uploads that match it lead — the top text hit is
/// so often a sped-up edit or an hour-long mix that mentions the title. The
/// rest follow in the search engine's own order, because a candidate can
/// still fall over at play time: SoundCloud's Go+ catalogue is DRM protected,
/// and the caller works down this list until something actually plays.
fn rank_candidates(candidates: &[(String, Option<f64>)], want: Option<f64>) -> Vec<String> {
    let Some(want) = want else {
        return candidates.iter().map(|(url, _)| url.clone()).collect();
    };
    let delta = |d: Option<f64>| (d.unwrap_or(f64::INFINITY) - want).abs();
    let mut close: Vec<&(String, Option<f64>)> =
        candidates.iter().filter(|(_, d)| delta(*d) <= DURATION_TOLERANCE).collect();
    close.sort_by(|a, b| delta(a.1).total_cmp(&delta(b.1)));
    let mut ranked: Vec<String> = close.iter().map(|(url, _)| url.clone()).collect();
    let rest: Vec<String> = candidates
        .iter()
        .filter(|(url, _)| !ranked.contains(url))
        .map(|(url, _)| url.clone())
        .collect();
    ranked.extend(rest);
    ranked
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

/// Work down a candidate list until one actually plays. The best guess can
/// still be a DRM protected Go+ upload or a dead link, and the next one
/// usually is not.
async fn first_playable(candidates: &[String]) -> Option<(String, Resolved)> {
    for page_url in candidates.iter().take(SEARCH_ATTEMPTS) {
        match resolve_stream(page_url).await {
            Ok(resolved) => return Some((page_url.clone(), resolved)),
            Err(e) => tracing::warn!("candidate unplayable ({page_url}): {e}"),
        }
    }
    None
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
        // A planned track only knows what to search for; find pages for it.
        let candidates = match track.url.strip_prefix(SEARCH_PREFIX) {
            None => Ok(vec![track.url.clone()]),
            Some(query) => search_soundcloud(query, track.duration).await,
        };
        let candidates = match candidates {
            Ok(urls) => urls,
            Err(e) => {
                tracing::warn!("could not find {}: {e}", track.title);
                continue;
            }
        };
        match first_playable(&candidates).await {
            Some((page_url, resolved)) => {
                // A Spotify link already knew the title; whatever upload we
                // found shouldn't rename the song mid-queue.
                let playing = Track {
                    id: track.id,
                    url: page_url.clone(),
                    title: if track.meta_locked { track.title.clone() } else { resolved.title.clone() },
                    artist: if track.meta_locked || !track.artist.is_empty() && resolved.artist.is_empty() {
                        track.artist.clone()
                    } else {
                        resolved.artist
                    },
                    art: if track.meta_locked { track.art.clone().or(resolved.art) } else { resolved.art.or(track.art.clone()) },
                    duration: resolved.duration.or(track.duration),
                    meta_locked: track.meta_locked,
                };
                let announce = playing.title.clone();
                controls.position_ms.store(0, Ordering::Relaxed);
                if let Some(session) = state.session.lock().unwrap().as_mut() {
                    session.now_playing = Some(playing);
                }
                tracing::info!("playing: {announce}");
                if let Err(e) = stream_pcm(&source, &resolved.stream, &controls).await {
                    tracing::warn!("track failed ({announce}): {e}");
                }
            }
            None => tracing::warn!("gave up on {}", track.title),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the whole cookie lifecycle: the slot is process-global and
    /// the file path is fixed, so splitting this up would just race with itself.
    #[test]
    fn server_supplied_cookies_win_and_can_be_taken_back() {
        assert!(cookie_args().is_empty(), "nothing configured, nothing passed");

        set_cookies(Some("# Netscape HTTP Cookie File\n.soundcloud.com\tTRUE\t/\tTRUE\t0\toauth_token\tX\n"));
        let args = cookie_args();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--cookies");
        let written = std::fs::read_to_string(&args[1]).expect("cookie file exists");
        assert!(written.contains("oauth_token\tX"));

        // Whitespace-only is how the server says "an admin cleared this".
        set_cookies(Some("   "));
        assert!(cookie_args().is_empty());
        assert!(!std::path::Path::new(&args[1]).exists(), "the file goes too");

        set_cookies(Some("something"));
        set_cookies(None);
        assert!(cookie_args().is_empty());
    }

    fn candidates() -> Vec<(String, Option<f64>)> {
        vec![
            // What SoundCloud's text ranking loves: a sped-up edit and an hour
            // long mix that both mention the song by name.
            ("https://soundcloud.com/x/sped-up".into(), Some(140.0)),
            ("https://soundcloud.com/x/dj-mix-2024".into(), Some(3600.0)),
            ("https://soundcloud.com/x/the-actual-song".into(), Some(238.0)),
            ("https://soundcloud.com/x/extended".into(), Some(255.0)),
        ]
    }

    #[test]
    fn length_decides_which_upload_is_the_song() {
        let c = candidates();
        // 240s from Spotify: the real thing leads, not the edit at the top.
        assert_eq!(rank_candidates(&c, Some(240.0))[0], "https://soundcloud.com/x/the-actual-song");
        // Knowing nothing, defer to the search ranking.
        assert_eq!(rank_candidates(&c, None)[0], "https://soundcloud.com/x/sped-up");
        // Nothing close enough: still offer something rather than nothing.
        assert_eq!(rank_candidates(&c, Some(30.0))[0], "https://soundcloud.com/x/sped-up");
        // Ties within tolerance go to the nearer length.
        assert_eq!(rank_candidates(&c, Some(250.0))[0], "https://soundcloud.com/x/extended");
        // Durationless entries cannot outrank one that matches.
        let mut unknown = c.clone();
        unknown.insert(0, ("https://soundcloud.com/x/mystery".into(), None));
        assert_eq!(rank_candidates(&unknown, Some(240.0))[0], "https://soundcloud.com/x/the-actual-song");
        assert!(rank_candidates(&[], Some(240.0)).is_empty());
    }

    #[test]
    fn every_hit_stays_on_the_list_as_a_fallback() {
        // The best guess can be DRM protected or dead, so nothing may be
        // dropped — just reordered, once each.
        let c = candidates();
        let ranked = rank_candidates(&c, Some(240.0));
        assert_eq!(ranked.len(), c.len());
        let unique: std::collections::HashSet<_> = ranked.iter().collect();
        assert_eq!(unique.len(), c.len(), "a candidate was listed twice");
        for (url, _) in &c {
            assert!(ranked.contains(url), "{url} was dropped");
        }
    }

    #[test]
    fn a_planned_track_queues_as_a_search() {
        let track = planned_to_track(&PlannedTrack {
            query: "deadmau5 Strobe".into(),
            title: "Strobe".into(),
            artist: "deadmau5".into(),
            art: Some("https://i.example/cover.jpg".into()),
            duration: Some(637.0),
        });
        assert_eq!(track.url, "ndsearch:deadmau5 Strobe");
        // Spotify knew the title; whatever upload wins must not rename it.
        assert!(track.meta_locked);
        assert_eq!(track.title, "Strobe");
    }

    /// Hits SoundCloud, so it stays out of the normal run:
    ///   YTDLP_CMD="python -m yt_dlp" cargo test -p music-bot -- --ignored
    /// It checks the assumption the rule above rests on — that a flat search
    /// really does come back with durations to compare.
    #[tokio::test]
    #[ignore]
    async fn a_real_search_returns_comparable_durations() {
        let urls = search_soundcloud("deadmau5 Strobe", Some(637.0)).await.expect("search");
        assert!(urls.len() > 1, "need fallbacks, got {urls:?}");
        // What actually matters, through the same call the player makes:
        // one of these plays. (SoundCloud's own copies of anything on a label
        // are DRM protected, so the first few usually don't.)
        let (url, resolved) = first_playable(&urls).await.expect("no candidate resolved");
        println!("played {url} -> {} ({:?}s)", resolved.title, resolved.duration);
        assert!(!resolved.stream.is_empty());
    }
}
