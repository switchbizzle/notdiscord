//! Music commands: "@bot play <url>" and friends. The heavy lifting (yt-dlp,
//! ffmpeg, LiveKit publishing) happens in the music sidecar — a separate
//! containerized service — driven here over localhost HTTP. This module owns
//! command parsing, permissions, chat feedback, and the voice-roster entry
//! that makes the bot show up in the sidebar.

use axum::extract::State;
use axum::http::StatusCode;
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
    Pause,
    Resume,
    Queue,
    /// "play" with no link.
    PlayUsage,
    /// Not music — hand the message to the LLM (`/ask`, `/image`).
    Ask,
}

/// Something we'd try to play: a web link, or a Spotify URI.
pub fn is_link(word: &str) -> bool {
    word.starts_with("http://") || word.starts_with("https://") || word.starts_with("spotify:")
}

/// Parse a bot command from a message: either "@bot play …" or "/play …".
/// None means it isn't a command at all.
pub fn parse_command(content: &str, bot_name: &str) -> Option<MusicCmd> {
    let trimmed = content.trim_start();
    let after = if let Some(rest) = trimmed.strip_prefix('/') {
        // Slash form: only a known verb counts, so "/shrug" stays chat.
        rest
    } else {
        let lower = content.to_lowercase();
        let mention = format!("@{}", bot_name.to_lowercase());
        let at = lower.find(&mention)?;
        &content[at + mention.len()..]
    };

    let after = after.trim();
    let mut words = after.split_whitespace();
    let verb = words.next()?.to_lowercase();
    let verb = verb.trim_matches(|c: char| c.is_ascii_punctuation());
    match verb {
        // "spotify:track:..." is what the desktop app's Copy Spotify URI gives.
        "play" | "p" => match words.find(|w| is_link(w)) {
            Some(url) => Some(MusicCmd::Play(url.to_owned())),
            None => Some(MusicCmd::PlayUsage),
        },
        "skip" | "next" => Some(MusicCmd::Skip),
        "stop" | "leave" | "dc" | "disconnect" => Some(MusicCmd::Stop),
        "pause" => Some(MusicCmd::Pause),
        "resume" | "unpause" => Some(MusicCmd::Resume),
        "queue" | "q" | "np" | "nowplaying" => Some(MusicCmd::Queue),
        "ask" | "image" | "draw" => Some(MusicCmd::Ask),
        _ => None,
    }
}

// ---------- REST API for the music tab ----------

/// The player as the tab sees it. Polled ~1s while the tab is open.
/// Queue a link straight from the Music tab.
///
/// The tab used to rewrite a pasted link into "/play <url>" and send it as a
/// message, which put the command *and* a full link preview card in the
/// channel for every track — Jon's "spam and noise". This queues it without
/// posting anything; the bot's own one-line reply still lands in the channel,
/// so people reading chat still see what turned up.
pub async fn play_endpoint(
    State(state): State<SharedState>,
    crate::auth::AuthUser(user): crate::auth::AuthUser,
    axum::Json(req): axum::Json<shared::MusicPlayRequest>,
) -> StatusCode {
    let url = req.url.trim().to_owned();
    if !is_link(&url) {
        return StatusCode::BAD_REQUEST;
    }
    handle_command(state, user, req.channel_id, MusicCmd::Play(url));
    StatusCode::ACCEPTED
}

pub async fn state_endpoint(
    State(state): State<SharedState>,
    _user: crate::auth::AuthUser,
) -> Result<axum::Json<shared::MusicState>, (StatusCode, axum::Json<shared::ApiError>)> {
    let mut music: shared::MusicState = reqwest::Client::new()
        .get(format!("{}/status", sidecar_url()))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .map_err(|_| crate::auth::err(StatusCode::SERVICE_UNAVAILABLE, "the music player is offline"))?
        .json()
        .await
        .map_err(|e| crate::auth::internal(e))?;
    // The tab's volume slider needs to know which voice participant to scale.
    music.bot_identity = format!("user-{}", state.bot_user().id);
    Ok(axum::Json(music))
}

/// Transport buttons: pause, resume, skip, stop.
pub async fn control_endpoint(
    State(_state): State<SharedState>,
    _user: crate::auth::AuthUser,
    axum::Json(req): axum::Json<shared::MusicControlRequest>,
) -> StatusCode {
    let endpoint = match req.action.as_str() {
        "pause" => "pause",
        "resume" => "resume",
        "skip" => "skip",
        "stop" => "stop",
        _ => return StatusCode::BAD_REQUEST,
    };
    match reqwest::Client::new()
        .post(format!("{}/{endpoint}", sidecar_url()))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => StatusCode::NO_CONTENT,
        Ok(_) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Queue editing: reorder, remove a selection, or clear.
pub async fn queue_endpoint(
    State(_state): State<SharedState>,
    _user: crate::auth::AuthUser,
    axum::Json(req): axum::Json<shared::MusicQueueRequest>,
) -> StatusCode {
    let http = reqwest::Client::new();
    let call = match req.action.as_str() {
        "move" => {
            let (Some(id), Some(offset)) = (req.id, req.offset) else {
                return StatusCode::BAD_REQUEST;
            };
            http.post(format!("{}/queue/move", sidecar_url()))
                .json(&serde_json::json!({ "id": id, "offset": offset }))
        }
        "remove" => http
            .post(format!("{}/queue/remove", sidecar_url()))
            .json(&serde_json::json!({ "ids": req.ids })),
        "clear" => http.post(format!("{}/queue/clear", sidecar_url())),
        _ => return StatusCode::BAD_REQUEST,
    };
    match call.timeout(std::time::Duration::from_secs(8)).send().await {
        Ok(resp) if resp.status().is_success() => StatusCode::NO_CONTENT,
        Ok(_) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// A button press on the player card.
pub fn handle_control(_state: SharedState, action: String) {
    tokio::spawn(async move {
        let cmd = match action.as_str() {
            "pause" => MusicCmd::Pause,
            "resume" => MusicCmd::Resume,
            "skip" => MusicCmd::Skip,
            "stop" => MusicCmd::Stop,
            _ => return,
        };
        let http = reqwest::Client::new();
        let endpoint = match cmd {
            MusicCmd::Pause => "pause",
            MusicCmd::Resume => "resume",
            MusicCmd::Skip => "skip",
            _ => "stop",
        };
        let _ = http.post(format!("{}/{endpoint}", sidecar_url())).send().await;
        // The status watcher repaints the card within a beat.
    });
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
    #[serde(default)]
    paused: bool,
}

/// The player card the client renders with transport buttons:
/// `⟦player⟧playing` / title / up-next.
fn player_card(status: &SidecarStatus) -> String {
    let state = if status.paused { "paused" } else { "playing" };
    let title = status
        .now_playing
        .as_ref()
        .map(|t| t.title.as_str())
        .unwrap_or("loading…");
    let mut out = format!("{}{state}\n{title}\n", shared::PLAYER_MARKER);
    if status.queue_len > 0 {
        let names: Vec<&str> = status.up_next.iter().map(String::as_str).collect();
        out.push_str(&format!("up next: {}", names.join(" · ")));
        if status.queue_len > names.len() {
            out.push_str(&format!(" (+{} more)", status.queue_len - names.len()));
        }
    } else {
        out.push_str("up next: nothing — queue's empty");
    }
    out
}

/// Post the player card, or edit the existing one in place so the channel
/// gets one live-updating card instead of a wall of "now playing" lines.
async fn paint_player(state: &SharedState, channel_id: i64, content: &str) -> anyhow::Result<()> {
    let existing = *state.music_player.lock().unwrap();
    match existing {
        Some((ch, message_id)) if ch == channel_id => {
            let edited_at = now_ms();
            sqlx::query("UPDATE messages SET content = ?, edited_at = ? WHERE id = ?")
                .bind(content)
                .bind(edited_at)
                .bind(message_id)
                .execute(&state.db)
                .await?;
            state.broadcast(ServerEvent::MessageEdited {
                channel_id,
                message_id,
                content: content.to_owned(),
                edited_at,
            });
        }
        _ => {
            let id = crate::bot::post_and_get_id(state, channel_id, content).await?;
            *state.music_player.lock().unwrap() = Some((channel_id, id));
        }
    }
    Ok(())
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
        MusicCmd::PlayUsage => Ok("give me a link, senpai: `play <soundcloud or spotify url>` 🎧".into()),
        MusicCmd::Play(url) => {
            // The requester must be sitting in a (non-DM) voice channel.
            let voice_channel = state.voice.lock().unwrap().get(&user.id).map(|(ch, ..)| *ch);
            let Some(vc) = voice_channel else {
                return Ok("join a voice channel first, then ask me to play 🎧".into());
            };
            if crate::dm_recipients(&state.db, vc).await?.is_some() {
                return Ok("I can't DJ private calls — use a voice channel".into());
            }

            // A Spotify link is a list of songs, not something playable:
            // resolve it to search phrases the sidecar can find audio for.
            let mut plan = Vec::new();
            // The queue is on screen in the rail, so confirming an add in chat
            // just puts the bot in a channel it was never invited to. The one
            // thing the queue can't tell you is that we took only part of a
            // long playlist, so that is the only thing left to say.
            let mut truncated_note = String::new();
            if crate::spotify::is_spotify_url(&url) {
                if crate::spotify::credentials(state).await.is_none() {
                    return Ok("I can read Spotify links once an admin adds Spotify credentials in \
                               Server settings → Bot 🎧 (SoundCloud links work either way)"
                        .into());
                }
                match crate::spotify::resolve(state, &url).await {
                    Ok(resolved) if resolved.tracks.is_empty() => {
                        return Ok(format!("\"{}\" is empty — nothing to queue 🫥", resolved.name));
                    }
                    Ok(resolved) => {
                        if resolved.truncated {
                            truncated_note = format!(
                                "🎧 {} is long — queued the first {} tracks",
                                resolved.name,
                                resolved.tracks.len()
                            );
                        }
                        plan = resolved.tracks;
                    }
                    Err(e) => {
                        tracing::warn!("spotify resolve failed: {e}");
                        return Ok(format!("couldn't read that Spotify link — {e}"));
                    }
                }
            }

            let (lk_url, token) = mint_bot_token(state, vc)?;
            let resp = http
                .post(format!("{}/play", sidecar_url()))
                .json(&serde_json::json!({
                    "room": crate::livekit_room(vc),
                    "lk_url": lk_url,
                    "token": token,
                    "url": url,
                    // Per-server HD credentials, if an admin configured any.
                    "cookies": crate::creds::get(state, "soundcloud").await,
                    // Non-empty for Spotify: the sidecar queues these instead
                    // of asking yt-dlp what the link is.
                    "plan": plan,
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
            let started = body["started"].as_bool().unwrap_or(false);

            if started {
                join_roster(state, vc);
                spawn_status_watch(state.clone(), text_channel);
            }
            // Otherwise silence: the player card and the queue both say more
            // than a line of chat could.
            Ok(truncated_note)
        }
        MusicCmd::Ask => Ok(String::new()), // handled by the LLM path
        MusicCmd::Skip => {
            let resp = http.post(format!("{}/skip", sidecar_url())).send().await?;
            Ok(if resp.status().is_success() { String::new() } else { "nothing is playing".into() })
        }
        MusicCmd::Pause | MusicCmd::Resume => {
            let endpoint = if cmd == MusicCmd::Pause { "pause" } else { "resume" };
            let resp = http.post(format!("{}/{endpoint}", sidecar_url())).send().await?;
            // The card repaints itself; only speak up when there's nothing to control.
            Ok(if resp.status().is_success() { String::new() } else { "nothing is playing".into() })
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
            room: crate::livekit_room(channel_id),
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

/// Poll the sidecar while a session runs: keep the player card up to date and
/// clean up when the music ends. One watcher at a time.
fn spawn_status_watch(state: SharedState, text_channel: i64) {
    {
        let mut music = state.music_watch.lock().unwrap();
        if *music {
            return;
        }
        *music = true;
    }
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let mut painted = String::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
                // Retire the card: no buttons once there's nothing to control.
                let _ = paint_player(
                    &state,
                    text_channel,
                    "🎶 that's the end of the queue — thanks for listening! leaving voice~",
                )
                .await;
                *state.music_player.lock().unwrap() = None;
                break;
            }

            let card = player_card(&status);
            if card != painted {
                if paint_player(&state, text_channel, &card).await.is_ok() {
                    painted = card;
                }
            }
        }
        *state.music_watch.lock().unwrap() = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spotify_link_is_a_play_command_like_any_other() {
        let play = |s: &str| parse_command(s, "NotBot");
        assert_eq!(
            play("/play https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=x"),
            Some(MusicCmd::Play("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=x".into()))
        );
        // "Copy Spotify URI" in the desktop app gives this instead of a link.
        assert_eq!(
            play("@notbot play spotify:album:1ATL5GLyefJaxhQzSPVrLX please"),
            Some(MusicCmd::Play("spotify:album:1ATL5GLyefJaxhQzSPVrLX".into()))
        );
        assert_eq!(play("/play"), Some(MusicCmd::PlayUsage));
        // Talking about Spotify isn't asking for anything.
        assert_eq!(play("spotify is down again"), None);
    }
}
