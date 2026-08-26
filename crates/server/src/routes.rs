use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use sqlx::Row;

use shared::{
    AuthResponse, Channel, ClientVersionInfo, CreateChannelRequest, CreateStickerRequest,
    GifResult, LoginRequest, Message, Profile, RegisterRequest, ServerEvent, Sticker,
    UpdateProfileRequest, UploadResponse, User, UserStatus, VoiceTokenResponse,
};

use crate::auth::{self, err, internal, ApiResult, AuthUser};
use crate::{now_ms, SharedState};

pub async fn register(
    State(state): State<SharedState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<AuthResponse>> {
    if let Ok(required) = std::env::var("NOTDISCORD_INVITE") {
        let supplied = req.invite.as_deref().map(str::trim).unwrap_or_default();
        if !required.is_empty() && supplied != required {
            return Err(err(StatusCode::FORBIDDEN, "invalid invite code"));
        }
    }

    let username = req.username.trim().to_owned();
    if username.len() < 2 || username.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "username must be 2-32 characters"));
    }
    if req.password.len() < 8 {
        return Err(err(StatusCode::BAD_REQUEST, "password must be at least 8 characters"));
    }

    let hash = auth::hash_password(req.password).await.map_err(internal)?;
    let result = sqlx::query("INSERT INTO users (username, password_hash, created_at) VALUES (?, ?, ?)")
        .bind(&username)
        .bind(&hash)
        .bind(now_ms())
        .execute(&state.db)
        .await;

    let user_id = match result {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "username already taken"));
        }
        Err(e) => return Err(internal(e)),
    };

    let token = create_session(&state, user_id).await?;
    Ok(Json(AuthResponse { token, user: User { id: user_id, username, avatar: None } }))
}

pub async fn login(
    State(state): State<SharedState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<AuthResponse>> {
    let row = sqlx::query("SELECT id, username, password_hash, avatar FROM users WHERE username = ?")
        .bind(req.username.trim())
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;

    let Some(row) = row else {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    };
    let (id, username, hash): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
    let avatar: Option<String> = row.get(3);

    if !auth::verify_password(req.password, hash).await {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    }

    let token = create_session(&state, id).await?;
    Ok(Json(AuthResponse { token, user: User { id, username, avatar } }))
}

async fn create_session(state: &SharedState, user_id: i64) -> ApiResult<String> {
    let token = auth::new_token();
    sqlx::query("INSERT INTO sessions (token, user_id, created_at) VALUES (?, ?, ?)")
        .bind(&token)
        .bind(user_id)
        .bind(now_ms())
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(token)
}

pub async fn me(AuthUser(user): AuthUser) -> Json<User> {
    Json(user)
}

pub async fn list_users(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<UserStatus>>> {
    let rows = sqlx::query("SELECT id, username, avatar FROM users ORDER BY username COLLATE NOCASE")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    let online: std::collections::HashSet<i64> =
        state.presence.lock().unwrap().keys().copied().collect();
    let users = rows
        .into_iter()
        .map(|r| {
            let user = User { id: r.get(0), username: r.get(1), avatar: r.get(2) };
            let is_online = online.contains(&user.id);
            UserStatus { user, online: is_online }
        })
        .collect();
    Ok(Json(users))
}

pub async fn get_profile(
    State(state): State<SharedState>,
    _user: AuthUser,
    Path(user_id): Path<i64>,
) -> ApiResult<Json<Profile>> {
    let row = sqlx::query("SELECT id, username, avatar, bio, created_at FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    let Some(row) = row else {
        return Err(err(StatusCode::NOT_FOUND, "no such user"));
    };
    Ok(Json(Profile {
        user: User { id: row.get(0), username: row.get(1), avatar: row.get(2) },
        bio: row.get(3),
        created_at: row.get(4),
    }))
}

pub async fn update_profile(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<UpdateProfileRequest>,
) -> ApiResult<Json<Profile>> {
    if let Some(avatar) = &req.avatar {
        let ok = avatar.len() < 500
            && (avatar.starts_with("http://") || avatar.starts_with("https://") || avatar.starts_with("/files/"));
        if !ok {
            return Err(err(StatusCode::BAD_REQUEST, "invalid avatar url"));
        }
        sqlx::query("UPDATE users SET avatar = ? WHERE id = ?")
            .bind(avatar)
            .bind(user.id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }
    if let Some(bio) = &req.bio {
        if bio.len() > 500 {
            return Err(err(StatusCode::BAD_REQUEST, "bio too long (max 500 chars)"));
        }
        sqlx::query("UPDATE users SET bio = ? WHERE id = ?")
            .bind(bio.trim())
            .bind(user.id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }

    let updated = get_profile(State(state.clone()), AuthUser(user.clone()), Path(user.id)).await?;
    let _ = state.events.send(ServerEvent::UserUpdated { user: updated.0.user.clone() });
    Ok(updated)
}

pub async fn list_channels(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<Channel>>> {
    let rows = sqlx::query("SELECT id, name, kind FROM channels ORDER BY id")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    let channels = rows
        .into_iter()
        .map(|r| Channel { id: r.get(0), name: r.get(1), kind: r.get(2) })
        .collect();
    Ok(Json(channels))
}

#[derive(Deserialize)]
pub struct VoiceTokenQuery {
    pub channel_id: i64,
}

#[derive(serde::Serialize)]
struct LiveKitVideoGrant {
    room: String,
    #[serde(rename = "roomJoin")]
    room_join: bool,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
}

#[derive(serde::Serialize)]
struct LiveKitClaims {
    iss: String,
    sub: String,
    name: String,
    nbf: i64,
    exp: i64,
    video: LiveKitVideoGrant,
}

/// Mint a LiveKit access token for a voice channel.
pub async fn voice_token(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Query(q): Query<VoiceTokenQuery>,
) -> ApiResult<Json<VoiceTokenResponse>> {
    let (Ok(api_key), Ok(api_secret), Ok(url)) = (
        std::env::var("LIVEKIT_API_KEY"),
        std::env::var("LIVEKIT_API_SECRET"),
        std::env::var("LIVEKIT_URL"),
    ) else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "voice is not configured on the server"));
    };

    let row = sqlx::query("SELECT kind FROM channels WHERE id = ?")
        .bind(q.channel_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    match row {
        Some(r) if r.get::<String, _>(0) == "voice" => {}
        Some(_) => return Err(err(StatusCode::BAD_REQUEST, "not a voice channel")),
        None => return Err(err(StatusCode::NOT_FOUND, "no such channel")),
    }

    let room = format!("channel-{}", q.channel_id);
    let now = now_ms() / 1000;
    let claims = LiveKitClaims {
        iss: api_key,
        sub: format!("user-{}", user.id),
        name: user.username.clone(),
        nbf: now - 10,
        exp: now + 6 * 3600,
        video: LiveKitVideoGrant {
            room: room.clone(),
            room_join: true,
            can_publish: true,
            can_subscribe: true,
        },
    };
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(api_secret.as_bytes()),
    )
    .map_err(internal)?;

    Ok(Json(VoiceTokenResponse { url, token, room }))
}

pub async fn create_channel(
    State(state): State<SharedState>,
    _user: AuthUser,
    Json(req): Json<CreateChannelRequest>,
) -> ApiResult<Json<Channel>> {
    let name = req.name.trim().trim_start_matches('#').to_lowercase();
    if name.is_empty() || name.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "channel name must be 1-32 characters"));
    }

    let result = sqlx::query("INSERT INTO channels (name, created_at) VALUES (?, ?)")
        .bind(&name)
        .bind(now_ms())
        .execute(&state.db)
        .await;

    let id = match result {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "channel already exists"));
        }
        Err(e) => return Err(internal(e)),
    };

    let channel = Channel { id, name, kind: "text".into() };
    let _ = state.events.send(ServerEvent::ChannelCreated { channel: channel.clone() });
    Ok(Json(channel))
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_owned();
    let mut out: String = cleaned.chars().take(64).collect();
    if out.is_empty() {
        out = "file".into();
    }
    out
}

fn image_content_type(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next().unwrap_or_default().to_lowercase().as_str() {
        "gif" => Some("image/gif"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    pub name: String,
}

pub async fn upload(
    State(_state): State<SharedState>,
    _user: AuthUser,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<Json<UploadResponse>> {
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty upload"));
    }

    let name = sanitize_filename(&q.name);
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("os rng");
    let id = hex::encode(id);

    let dir = crate::uploads_dir().join(&id);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    tokio::fs::write(dir.join(&name), &body).await.map_err(internal)?;

    Ok(Json(UploadResponse { url: format!("/files/{id}/{name}") }))
}

#[derive(Deserialize)]
pub struct FileQuery {
    /// When present, force a download even for inline-renderable images.
    pub dl: Option<String>,
}

fn file_response(
    path: std::path::PathBuf,
    name: &str,
    force_download: bool,
) -> impl std::future::Future<Output = Response> + Send + 'static {
    let name = name.to_owned();
    async move {
        let Ok(bytes) = tokio::fs::read(path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let mut headers = vec![
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ];
        let inline_image = image_content_type(&name).filter(|_| !force_download);
        match inline_image {
            Some(ct) => headers.push((header::CONTENT_TYPE, ct.to_owned())),
            None => {
                headers.push((header::CONTENT_TYPE, "application/octet-stream".to_owned()));
                headers.push((
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ));
            }
        }
        let mut resp = bytes.into_response();
        for (key, value) in headers {
            if let Ok(value) = value.parse() {
                resp.headers_mut().insert(key, value);
            }
        }
        resp
    }
}

/// Current format: /files/{32-hex id}/{sanitized original filename}.
pub async fn serve_file(
    Path((id, name)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let id_ok = id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !id_ok || name != sanitize_filename(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    file_response(crate::uploads_dir().join(&id).join(&name), &name, q.dl.is_some()).await
}

/// Legacy format from the first uploads release: /files/{32-hex}.{ext}.
pub async fn serve_file_legacy(Path(name): Path<String>, Query(q): Query<FileQuery>) -> Response {
    let valid = name.len() < 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
        && name.matches('.').count() == 1;
    if !valid {
        return StatusCode::NOT_FOUND.into_response();
    }
    file_response(crate::uploads_dir().join(&name), &name, q.dl.is_some()).await
}

pub async fn list_stickers(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<Sticker>>> {
    let rows = sqlx::query("SELECT id, name, url, creator_id FROM stickers ORDER BY name COLLATE NOCASE")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    let stickers = rows
        .into_iter()
        .map(|r| Sticker { id: r.get(0), name: r.get(1), url: r.get(2), creator_id: r.get(3) })
        .collect();
    Ok(Json(stickers))
}

pub async fn create_sticker(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateStickerRequest>,
) -> ApiResult<Json<Sticker>> {
    let name = req.name.trim().to_owned();
    if name.is_empty() || name.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "sticker name must be 1-32 characters"));
    }
    let ok = req.url.len() < 500
        && (req.url.starts_with("http://") || req.url.starts_with("https://") || req.url.starts_with("/files/"));
    if !ok {
        return Err(err(StatusCode::BAD_REQUEST, "invalid sticker url"));
    }

    let result = sqlx::query("INSERT INTO stickers (name, url, creator_id, created_at) VALUES (?, ?, ?, ?)")
        .bind(&name)
        .bind(&req.url)
        .bind(user.id)
        .bind(now_ms())
        .execute(&state.db)
        .await
        .map_err(internal)?;

    let sticker = Sticker { id: result.last_insert_rowid(), name, url: req.url, creator_id: user.id };
    let _ = state.events.send(ServerEvent::StickerCreated { sticker: sticker.clone() });
    Ok(Json(sticker))
}

pub async fn delete_sticker(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(sticker_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let affected = sqlx::query("DELETE FROM stickers WHERE id = ? AND creator_id = ?")
        .bind(sticker_id)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?
        .rows_affected();
    if affected == 0 {
        return Err(err(StatusCode::FORBIDDEN, "you can only delete your own stickers"));
    }
    let _ = state.events.send(ServerEvent::StickerDeleted { sticker_id });
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn client_dir() -> std::path::PathBuf {
    std::env::var("NOTDISCORD_CLIENT_DIR")
        .unwrap_or_else(|_| "client".into())
        .into()
}

/// Public: current client version (used by the auto-updater).
pub async fn client_version() -> Result<Json<ClientVersionInfo>, StatusCode> {
    let version = tokio::fs::read_to_string(client_dir().join("version.txt"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(ClientVersionInfo { version: version.trim().to_owned(), url: "/download".into() }))
}

/// Public: download the current client build.
pub async fn download_client() -> Response {
    match tokio::fs::read(client_dir().join("NotDiscord.exe")).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"NotDiscord.exe\"".to_owned()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
pub struct GifQuery {
    pub q: Option<String>,
}

/// Proxy GIF search to GIPHY so the API key stays on the server.
pub async fn gifs(
    State(_state): State<SharedState>,
    _user: AuthUser,
    Query(q): Query<GifQuery>,
) -> ApiResult<Json<Vec<GifResult>>> {
    let Ok(key) = std::env::var("NOTDISCORD_GIPHY_KEY") else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "GIF search not configured on the server yet",
        ));
    };

    let query = q.q.unwrap_or_default();
    let endpoint = if query.trim().is_empty() {
        "https://api.giphy.com/v1/gifs/trending"
    } else {
        "https://api.giphy.com/v1/gifs/search"
    };
    let response: serde_json::Value = reqwest::Client::new()
        .get(endpoint)
        .query(&[
            ("api_key", key.as_str()),
            ("q", query.trim()),
            ("limit", "24"),
            ("rating", "pg-13"),
        ])
        .send()
        .await
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;

    let results = response["data"]
        .as_array()
        .map(|gifs| {
            gifs.iter()
                .filter_map(|gif| {
                    let images = &gif["images"];
                    let preview = images["fixed_width"]["url"].as_str()?;
                    let url = images["downsized"]["url"]
                        .as_str()
                        .filter(|u| !u.is_empty())
                        .or_else(|| images["original"]["url"].as_str())?;
                    Some(GifResult { preview: preview.to_owned(), url: url.to_owned() })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(results))
}

#[derive(Deserialize)]
pub struct MessagesQuery {
    /// Return messages with id < before (for paging back through history).
    pub before: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn channel_messages(
    State(state): State<SharedState>,
    _user: AuthUser,
    Path(channel_id): Path<i64>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Json<Vec<Message>>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let before = q.before.unwrap_or(i64::MAX);

    let rows = sqlx::query(
        "SELECT m.id, m.channel_id, m.content, m.created_at, m.edited_at, u.id, u.username, u.avatar \
         FROM messages m JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = ? AND m.id < ? ORDER BY m.id DESC LIMIT ?",
    )
    .bind(channel_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    // Rows come back newest-first; flip to chronological order for the client.
    let mut messages: Vec<Message> = rows
        .into_iter()
        .map(|r| Message {
            id: r.get(0),
            channel_id: r.get(1),
            content: r.get(2),
            created_at: r.get(3),
            edited_at: r.get(4),
            author: User { id: r.get(5), username: r.get(6), avatar: r.get(7) },
            reactions: Vec::new(),
        })
        .collect();
    messages.reverse();

    if !messages.is_empty() {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT message_id, emoji, user_id FROM reactions WHERE message_id IN (",
        );
        let mut ids = qb.separated(", ");
        for msg in &messages {
            ids.push_bind(msg.id);
        }
        qb.push(") ORDER BY id");
        let reaction_rows = qb.build().fetch_all(&state.db).await.map_err(internal)?;
        for row in reaction_rows {
            let message_id: i64 = row.get(0);
            if let Some(msg) = messages.iter_mut().find(|m| m.id == message_id) {
                msg.reactions.push(shared::ReactionEntry { emoji: row.get(1), user_id: row.get(2) });
            }
        }
    }
    Ok(Json(messages))
}
