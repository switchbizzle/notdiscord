//! Music commands: "@bot play <url>" and friends. The heavy lifting (yt-dlp,
//! ffmpeg, LiveKit publishing) happens in the music sidecar — a separate
//! containerized service — driven here over localhost HTTP. This module owns
//! command parsing, permissions, chat feedback, and the voice-roster entry
//! that makes the bot show up in the sidebar.

use serde::Deserialize;

use shared::ServerEvent;

use crate::{now_ms, SharedState};

fn sidecar_url() -> String {
    std::env::var("NOTDISCORD_MUSIC_URL").unwrap_or_else(|_| "http://127.0.0.1:3001".into())
}

#[derive(Debug, PartialEq)]
pub enum MusicCmd {
    Play(String),
    Skip,
    Stop,
    Queue,
    /// "play" with no link.
    PlayUsage,
}

/// Parse a music command out of a message that mentions the bot. None means
/// it's not a music command (fall through to the LLM).
pub fn parse_command(content: &str, bot_name: &str) -> Option<MusicCmd> {
    let lower = content.to_lowercase();
    let mention = format!("@{}", bot_name.to_lowercase());
    let at = lower.find(&mention)?;
    let after = content[at + mention.len()..].trim();
    let mut words = after.split_whitespace();
    let verb = words.next()?.to_lowercase();
    match verb.trim_matches(|c: char| c.is_ascii_punctuation() && c != '!').as_ref() {
        "play" | "p" => match words.find(|w| w.starts_with("http://") || w.starts_with("https://")) {
            Some(url) => Some(MusicCmd::Play(url.to_owned())),
            None => Some(MusicCmd::PlayUsage),
        },
        "skip" | "next" => Some(MusicCmd::Skip),
        "stop" | "leave" | "dc" | "disconnect" => Some(MusicCmd::Stop),
        "queue" | "q" | "np" | "nowplaying" => Some(MusicCmd::Queue),
        _ => None,
    }
}

#[derive(Deserialize)]
struct SidecarStatus {
    active: bool,
    #[serde(default)]
    now_playing: Option<SidecarTrack>,
    #[serde(default)]
    queue_len: usize,
    #[serde(default)]
    up_next: Vec<String>,
}

#[derive(Deserialize)]
struct SidecarTrack {
    title: String,
}

/// Handle a parsed command; posts feedback into `text_channel` as the bot.
pub fn handle_command(state: SharedState, user: shared::User, text_channel: i64, cmd: MusicCmd) {
    tokio::spawn(async move {
        let reply = match run_command(&state, &user, text_channel, cmd).await {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!("music command failed: {e}");
                "the music engine hiccuped 😖 — check the server logs".to_owned()
            }
        };
        if !reply.is_empty() {
            let _ = crate::bot::post_message(&state, text_channel, &reply).await;
        }
    });
}

async fn run_command(
    state: &SharedState,
    user: &shared::User,
    text_channel: i64,
    cmd: MusicCmd,
) -> anyhow::Result<String> {
    let http = reqwest::Client::new();
    match cmd {
        MusicCmd::PlayUsage => Ok("give me a link, senpai: `play <soundcloud track or playlist url>`".into()),
        MusicCmd::Play(url) => {
            // The requester must be sitting in a (non-DM) voice channel.
            let voice_channel = state.voice.lock().unwrap().get(&user.id).map(|(ch, ..)| *ch);
            let Some(vc) = voice_channel else {
                return Ok("join a voice channel first, then ask me to play 🎧".into());
            };
            if crate::dm_recipients(&state.db, vc).await?.is_some() {
                return Ok("I can't DJ private calls — use a voice channel".into());
            }

            let (lk_url, token) = mint_bot_token(state, vc)?;
            let resp = http
                .post(format!("{}/play", sidecar_url()))
                .json(&serde_json::json!({
                    "room": format!("channel-{vc}"),
                    "lk_url": lk_url,
                    "token": token,
                    "url": url,
                }))
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("music sidecar unreachable: {e}"))?;

            if !resp.status().is_success() {
                let msg = resp.text().await.unwrap_or_default();
                return Ok(format!("can't play that: {msg}"));
            }
            let body: serde_json::Value = resp.json().await?;
            let queued = body["queued"].as_u64().unwrap_or(0);
            let started = body["started"].as_bool().unwrap_or(false);

            if started {
                join_roster(state, vc);
                spawn_status_watch(state.clone(), text_channel, vc);
                Ok(if queued > 1 {
                    format!("🎶 coming right up — {queued} tracks queued!")
                } else {
                    String::new() // the status watch posts "now playing" momentarily
                })
            } else {
                Ok(format!("added to the queue (+{queued}) 🎵"))
            }
        }
        MusicCmd::Skip => {
            let resp = http.post(format!("{}/skip", sidecar_url())).send().await?;
            Ok(if resp.status().is_success() { "⏭ skipped".into() } else { "nothing is playing".into() })
        }
        MusicCmd::Stop => {
            let resp = http.post(format!("{}/stop", sidecar_url())).send().await?;
            Ok(if resp.status().is_success() {
                String::new() // the status watch announces the departure
            } else {
                "nothing is playing".into()
            })
        }
        MusicCmd::Queue => {
            let status: SidecarStatus = http
                .get(format!("{}/status", sidecar_url()))
                .send()
                .await?
                .json()
                .await?;
            if !status.active {
                return Ok("nothing is playing — `play <url>` to start 🎵".into());
            }
            let mut out = match &status.now_playing {
                Some(track) => format!("▶ **{}**", track.title),
                None => "▶ (loading…)".into(),
            };
            if status.queue_len > 0 {
                out.push_str(&format!("\nup next ({} queued):", status.queue_len));
                for title in &status.up_next {
                    out.push_str(&format!("\n• {title}"));
                }
            }
            Ok(out)
        }
    }
}

/// Mint a LiveKit token for the bot in `channel`'s room.
fn mint_bot_token(state: &SharedState, channel_id: i64) -> anyhow::Result<(String, String)> {
    let url = std::env::var("LIVEKIT_URL").map_err(|_| anyhow::anyhow!("LIVEKIT_URL not set"))?;
    let api_key = std::env::var("LIVEKIT_API_KEY")?;
    let api_secret = std::env::var("LIVEKIT_API_SECRET")?;
    let bot = state.bot_user();
    let now = now_ms() / 1000;
    let claims = crate::routes::LiveKitClaims {
        iss: api_key,
        sub: format!("user-{}", bot.id),
        name: bot.username,
        nbf: now - 10,
        exp: now + 12 * 3600,
        video: crate::routes::LiveKitVideoGrant {
            room: format!("channel-{channel_id}"),
            room_join: true,
            can_publish: true,
            can_subscribe: false,
        },
    };
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(api_secret.as_bytes()),
    )?;
    Ok((url, token))
}

/// Show the bot in the voice roster.
fn join_roster(state: &SharedState, channel_id: i64) {
    let bot = state.bot_user();
    state.voice.lock().unwrap().insert(bot.id, (channel_id, bot.clone(), false, false));
    state.broadcast(ServerEvent::VoiceStateChanged {
        user: bot,
        channel_id: Some(channel_id),
        sharing: false,
        camera: false,
    });
}

fn leave_roster(state: &SharedState) {
    let bot = state.bot_user();
    state.voice.lock().unwrap().remove(&bot.id);
    state.broadcast(ServerEvent::VoiceStateChanged {
        user: bot,
        channel_id: None,
        sharing: false,
        camera: false,
    });
}

/// Poll the sidecar while a session runs: announce track changes in chat,
/// and clean up the roster when the music ends. One watcher at a time.
fn spawn_status_watch(state: SharedState, text_channel: i64, voice_channel: i64) {
    {
        let mut music = state.music_watch.lock().unwrap();
        if *music {
            return;
        }
        *music = true;
    }
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let mut last_title: Option<String> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let status: Option<SidecarStatus> = match http
                .get(format!("{}/status", sidecar_url()))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => resp.json().await.ok(),
                Err(_) => None,
            };
            let Some(status) = status else { continue };

            if !status.active {
                leave_roster(&state);
                let _ = crate::bot::post_message(&state, text_channel, "🎶 that's the end of the queue — thanks for listening! leaving voice~").await;
                break;
            }
            if let Some(track) = &status.now_playing {
                if last_title.as_deref() != Some(track.title.as_str()) {
                    last_title = Some(track.title.clone());
                    let _ = crate::bot::post_message(
                        &state,
                        text_channel,
                        &format!("▶ now playing: **{}**", track.title),
                    )
                    .await;
                }
            }
        }
        *state.music_watch.lock().unwrap() = false;
        let _ = voice_channel;
    });
}
