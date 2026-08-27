//! Voice roster reconciliation.
//!
//! Clients announce their own voice presence over the chat WebSocket, which is
//! fast but fragile: an old client, a dropped socket, or a crashed app leaves
//! ghosts (or invisible people) in everyone's sidebar. LiveKit is the actual
//! authority on who's in a room, so this polls it and fixes up the roster —
//! including the screen-share/camera flags, which it reads from each
//! participant's published tracks.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::Row;

use shared::{ServerEvent, User};

use crate::{dm_recipients, now_ms, SharedState};

const POLL_SECS: u64 = 4;
/// After someone explicitly leaves, ignore LiveKit for this long so a
/// not-yet-propagated departure can't resurrect them.
const LEAVE_GRACE_MS: i64 = 6000;

/// LiveKit's HTTP API. Not the public wss:// URL — that only proxies /rtc.
fn api_base() -> String {
    if let Ok(url) = std::env::var("NOTDISCORD_LIVEKIT_API_URL") {
        return url.trim_end_matches('/').to_owned();
    }
    match std::env::var("LIVEKIT_URL") {
        Ok(url) => url
            .trim_end_matches('/')
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1),
        Err(_) => "http://127.0.0.1:7880".to_owned(),
    }
}

#[derive(Serialize)]
struct AdminGrant {
    room: String,
    #[serde(rename = "roomAdmin")]
    room_admin: bool,
}

#[derive(Serialize)]
struct AdminClaims {
    iss: String,
    sub: String,
    nbf: i64,
    exp: i64,
    video: AdminGrant,
}

/// Room-scoped admin token: LiveKit refuses room operations without one.
fn admin_token(room: &str) -> anyhow::Result<String> {
    let key = std::env::var("LIVEKIT_API_KEY")?;
    let secret = std::env::var("LIVEKIT_API_SECRET")?;
    let now = now_ms() / 1000;
    let claims = AdminClaims {
        iss: key.clone(),
        sub: key,
        nbf: now - 10,
        exp: now + 600,
        video: AdminGrant { room: room.to_owned(), room_admin: true },
    };
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

/// Who LiveKit says is in `room`: (user id, sharing, camera).
async fn list_participants(room: &str) -> anyhow::Result<Vec<(i64, bool, bool)>> {
    let token = admin_token(room)?;
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/twirp/livekit.RoomService/ListParticipants", api_base()))
        .bearer_auth(token)
        .json(&serde_json::json!({ "room": room }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    let mut out = Vec::new();
    for participant in response["participants"].as_array().into_iter().flatten() {
        // Identities are minted as "user-{id}" when the token is issued.
        let Some(id) = participant["identity"]
            .as_str()
            .and_then(|i| i.strip_prefix("user-"))
            .and_then(|i| i.parse::<i64>().ok())
        else {
            continue;
        };
        let sources: Vec<&str> = participant["tracks"]
            .as_array()
            .map(|tracks| tracks.iter().filter_map(|t| t["source"].as_str()).collect())
            .unwrap_or_default();
        out.push((
            id,
            sources.contains(&"SCREEN_SHARE"),
            sources.contains(&"CAMERA"),
        ));
    }
    Ok(out)
}

/// Rooms worth polling: every voice channel, plus any channel someone is
/// currently believed to be in (covers DM calls).
async fn rooms_to_check(state: &SharedState) -> Vec<i64> {
    let mut ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM channels WHERE kind = 'voice'")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    for (channel_id, ..) in state.voice.lock().unwrap().values() {
        if !ids.contains(channel_id) {
            ids.push(*channel_id);
        }
    }
    ids
}

async fn load_user(state: &SharedState, id: i64) -> Option<User> {
    let row = sqlx::query("SELECT id, username, avatar, role FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()??;
    Some(User { id: row.get(0), username: row.get(1), avatar: row.get(2), role: row.get(3) })
}

fn broadcast(state: &SharedState, recipients: &Option<Vec<i64>>, event: ServerEvent) {
    match recipients {
        Some(ids) => state.broadcast_only(ids.clone(), event),
        None => state.broadcast(event),
    }
}

pub async fn sync_loop(state: SharedState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(POLL_SECS)).await;
        if let Err(e) = sync_once(&state).await {
            tracing::debug!("voice sync skipped: {e}");
        }
    }
}

async fn sync_once(state: &SharedState) -> anyhow::Result<()> {
    // What LiveKit actually sees, keyed by user.
    let mut actual: HashMap<i64, (i64, bool, bool)> = HashMap::new();
    for channel_id in rooms_to_check(state).await {
        let room = format!("channel-{channel_id}");
        match list_participants(&room).await {
            Ok(participants) => {
                for (user_id, sharing, camera) in participants {
                    actual.insert(user_id, (channel_id, sharing, camera));
                }
            }
            // A room that has never existed 404s; that's just "empty".
            Err(e) => tracing::trace!("list {room}: {e}"),
        }
    }

    let now = now_ms();
    let known: HashMap<i64, (i64, User, bool, bool)> = state.voice.lock().unwrap().clone();
    tracing::debug!(
        "voice sync: livekit sees {} participant(s), roster has {}",
        actual.len(),
        known.len()
    );
    let mut changes: Vec<(User, Option<i64>, bool, bool)> = Vec::new();

    // Anyone LiveKit sees who we're missing (or whose state drifted).
    for (user_id, (channel_id, sharing, camera)) in &actual {
        let recently_left = state
            .voice_left
            .lock()
            .unwrap()
            .get(user_id)
            .is_some_and(|at| now - at < LEAVE_GRACE_MS);
        if recently_left {
            continue;
        }
        match known.get(user_id) {
            Some((known_channel, user, known_sharing, known_camera))
                if known_channel == channel_id
                    && known_sharing == sharing
                    && known_camera == camera =>
            {
                let _ = user;
            }
            _ => {
                let Some(user) = load_user(state, *user_id).await else { continue };
                state
                    .voice
                    .lock()
                    .unwrap()
                    .insert(*user_id, (*channel_id, user.clone(), *sharing, *camera));
                changes.push((user, Some(*channel_id), *sharing, *camera));
            }
        }
    }

    // Ghosts: we think they're in voice, LiveKit disagrees.
    for (user_id, (_, user, _, _)) in &known {
        if !actual.contains_key(user_id) {
            state.voice.lock().unwrap().remove(user_id);
            changes.push((user.clone(), None, false, false));
        }
    }

    for (user, channel_id, sharing, camera) in changes {
        // DM calls stay private to their participants.
        let scope = match channel_id.or_else(|| known.get(&user.id).map(|(c, ..)| *c)) {
            Some(channel) => dm_recipients(&state.db, channel).await.unwrap_or(None),
            None => None,
        };
        tracing::info!(
            "voice sync: {} -> {:?} (sharing {sharing}, camera {camera})",
            user.username,
            channel_id
        );
        broadcast(
            state,
            &scope,
            ServerEvent::VoiceStateChanged { user, channel_id, sharing, camera },
        );
    }
    Ok(())
}
