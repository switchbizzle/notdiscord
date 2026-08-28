use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use sqlx::Row;

use shared::{
    AuthResponse, Channel, ClientVersionInfo, CreateChannelRequest, CreateDmRequest,
    CreateStickerRequest, GifResult, LoginRequest, Message, Profile, RegisterRequest,
    AssignTagRequest, CreateTagRequest, RenameServerRequest, RetentionSetting, ServerEvent,
    SearchResult, ServerInfo, SetBanRequest, SetRoleRequest, SetServerIconRequest, Sticker, Tag,
    UpdateProfileRequest, UploadResponse, User, UserStatus, VoiceTokenResponse,
};

use crate::auth::{self, err, internal, ApiResult, AuthUser};
use crate::{now_ms, SharedState};

pub async fn register(
    State(state): State<SharedState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<AuthResponse>> {
    // The invite code lives in server_meta (seeded from NOTDISCORD_INVITE on
    // first boot); empty = open registration.
    let required = meta_value_opt(&state, "invite_code").await?.unwrap_or_default();
    if !required.is_empty() {
        let supplied = req.invite.as_deref().map(str::trim).unwrap_or_default();
        if supplied != required {
            return Err(err(StatusCode::FORBIDDEN, "invalid invite code"));
        }
    }

    let username = req.username.trim().to_owned();
    if username.len() < 2 || username.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "username must be 2-32 characters"));
    }
    if let Some(problem) = shared::password_problem(&req.password, &username) {
        return Err(err(StatusCode::BAD_REQUEST, problem));
    }

    let hash = auth::hash_password(req.password).await.map_err(internal)?;
    // The first PERSON on a fresh server becomes admin/owner. NotBot is
    // created at first boot, so counting it here left the owner of a brand
    // new server as a plain member with no way to promote themselves.
    let bot_id = state.bot_user().id;
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id != ?")
        .bind(bot_id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    let role = if existing == 0 { "admin" } else { "member" };
    let result = sqlx::query("INSERT INTO users (username, password_hash, role, created_at) VALUES (?, ?, ?, ?)")
        .bind(&username)
        .bind(&hash)
        .bind(role)
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

    // Newcomers start caught up rather than staring at a wall of unread.
    sqlx::query(
        "INSERT OR IGNORE INTO read_state (user_id, channel_id, last_read_id) \
         SELECT ?, c.id, COALESCE((SELECT MAX(m.id) FROM messages m WHERE m.channel_id = c.id), 0) \
         FROM channels c",
    )
    .bind(user_id)
    .execute(&state.db)
    .await
    .map_err(internal)?;

    let token = create_session(&state, user_id).await?;
    Ok(Json(AuthResponse {
        token,
        user: User { id: user_id, username, avatar: None, role: role.into() },
    }))
}

pub async fn login(
    State(state): State<SharedState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<AuthResponse>> {
    let row = sqlx::query("SELECT id, username, password_hash, avatar, role, banned FROM users WHERE username = ?")
        .bind(req.username.trim())
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;

    let Some(row) = row else {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    };
    let (id, username, hash): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
    let avatar: Option<String> = row.get(3);
    let role: String = row.get(4);
    let banned: i64 = row.get(5);

    if !auth::verify_password(req.password, hash).await {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    }
    if banned != 0 {
        return Err(err(StatusCode::FORBIDDEN, "you are banned from this server"));
    }

    let token = create_session(&state, id).await?;
    Ok(Json(AuthResponse { token, user: User { id, username, avatar, role } }))
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

pub async fn change_password(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    headers: axum::http::HeaderMap,
    Json(req): Json<shared::ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    if !auth::verify_password(req.current, hash).await {
        return Err(err(StatusCode::UNAUTHORIZED, "current password is incorrect"));
    }
    if let Some(problem) = shared::password_problem(&req.new, &user.username) {
        return Err(err(StatusCode::BAD_REQUEST, problem));
    }

    let new_hash = auth::hash_password(req.new).await.map_err(internal)?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&new_hash)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    // Log out every other device; the session making this request survives.
    let current_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token != ?")
        .bind(user.id)
        .bind(&current_token)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Codes expire after 15 minutes; 5 wrong guesses burns one.
const CODE_TTL_MS: i64 = 15 * 60 * 1000;
const CODE_MAX_ATTEMPTS: i64 = 5;
/// Minimum gap between sending codes to the same user+purpose.
const CODE_RESEND_MS: i64 = 60 * 1000;

pub async fn email_status(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<shared::EmailStatus>> {
    let row = sqlx::query("SELECT email, email_verified FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    let email: Option<String> = row.get(0);
    let verified: i64 = row.get(1);
    Ok(Json(shared::EmailStatus { email, verified: verified != 0 }))
}

/// Stores a fresh code for (user, purpose) and emails it, rate-limited.
async fn issue_code(
    state: &SharedState,
    user_id: i64,
    email: &str,
    purpose: &str,
    subject: &str,
    body: impl Fn(&str) -> String,
) -> ApiResult<()> {
    let last: Option<i64> =
        sqlx::query_scalar("SELECT created_at FROM mail_codes WHERE user_id = ? AND purpose = ?")
            .bind(user_id)
            .bind(purpose)
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?;
    if last.is_some_and(|t| now_ms() - t < CODE_RESEND_MS) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "wait a minute before requesting another code"));
    }
    let code = crate::mail::new_code();
    sqlx::query(
        "INSERT OR REPLACE INTO mail_codes (user_id, email, code, purpose, expires_at, attempts, created_at) \
         VALUES (?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&code)
    .bind(purpose)
    .bind(now_ms() + CODE_TTL_MS)
    .bind(now_ms())
    .execute(&state.db)
    .await
    .map_err(internal)?;

    crate::mail::send(email, subject, &body(&code)).await.map_err(|e| {
        tracing::warn!("mail send failed for user {user_id}: {e:#}");
        err(StatusCode::BAD_GATEWAY, "could not send the email — try again in a bit")
    })
}

pub async fn email_request(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::EmailRequest>,
) -> ApiResult<StatusCode> {
    if !crate::mail::configured() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "email is not set up on this server"));
    }
    let email = req.email.trim().to_lowercase();
    if !crate::mail::valid_address(&email) {
        return Err(err(StatusCode::BAD_REQUEST, "that doesn't look like an email address"));
    }
    issue_code(&state, user.id, &email, "verify", "Your NotDiscord verification code", |code| {
        format!(
            "Hey {},\n\nYour verification code is: {code}\n\nEnter it in Settings -> Account \
             within 15 minutes. If you didn't request this, ignore it.\n",
            user.username
        )
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Loads and checks the pending code for (user, purpose). On success the
/// code row is deleted and the email it was sent to is returned.
async fn consume_code(
    state: &SharedState,
    user_id: i64,
    purpose: &str,
    supplied: &str,
) -> ApiResult<String> {
    let row = sqlx::query(
        "SELECT email, code, expires_at, attempts FROM mail_codes WHERE user_id = ? AND purpose = ?",
    )
    .bind(user_id)
    .bind(purpose)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::BAD_REQUEST, "no code pending — request one first"))?;

    let email: String = row.get(0);
    let code: String = row.get(1);
    let expires_at: i64 = row.get(2);
    let attempts: i64 = row.get(3);

    let burn = |reason| async move {
        sqlx::query("DELETE FROM mail_codes WHERE user_id = ? AND purpose = ?")
            .bind(user_id)
            .bind(purpose)
            .execute(&state.db)
            .await
            .map_err(internal)?;
        Err(err(StatusCode::BAD_REQUEST, reason))
    };
    if now_ms() > expires_at {
        return burn("that code has expired — request a new one").await;
    }
    if attempts >= CODE_MAX_ATTEMPTS {
        return burn("too many wrong guesses — request a new code").await;
    }
    if supplied.trim() != code {
        sqlx::query("UPDATE mail_codes SET attempts = attempts + 1 WHERE user_id = ? AND purpose = ?")
            .bind(user_id)
            .bind(purpose)
            .execute(&state.db)
            .await
            .map_err(internal)?;
        return Err(err(StatusCode::UNAUTHORIZED, "wrong code"));
    }
    sqlx::query("DELETE FROM mail_codes WHERE user_id = ? AND purpose = ?")
        .bind(user_id)
        .bind(purpose)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(email)
}

pub async fn email_verify(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::EmailVerifyRequest>,
) -> ApiResult<StatusCode> {
    let email = consume_code(&state, user.id, "verify", &req.code).await?;
    sqlx::query("UPDATE users SET email = ?, email_verified = 1 WHERE id = ?")
        .bind(&email)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Unauthenticated: always answers 204 so it can't be used to probe which
/// usernames exist or have email set up.
pub async fn forgot_password(
    State(state): State<SharedState>,
    Json(req): Json<shared::ForgotPasswordRequest>,
) -> ApiResult<StatusCode> {
    if !crate::mail::configured() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "email is not set up on this server"));
    }
    let row = sqlx::query(
        "SELECT id, username, email FROM users WHERE username = ? AND email_verified = 1 AND banned = 0",
    )
    .bind(req.username.trim())
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;
    if let Some(row) = row {
        let (user_id, username): (i64, String) = (row.get(0), row.get(1));
        let email: String = row.get(2);
        // Swallow rate-limit/send errors too — same reason.
        let _ = issue_code(&state, user_id, &email, "reset", "Your NotDiscord password reset code", |code| {
            format!(
                "Hey {username},\n\nYour password reset code is: {code}\n\nEnter it on the \
                 login screen within 15 minutes. If you didn't ask to reset your password, \
                 you can ignore this — your account is fine.\n"
            )
        })
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reset_password(
    State(state): State<SharedState>,
    Json(req): Json<shared::ResetPasswordRequest>,
) -> ApiResult<StatusCode> {
    let username = req.username.trim().to_owned();
    // Password rules first: a rejected password must not burn the code.
    if let Some(problem) = shared::password_problem(&req.new_password, &username) {
        return Err(err(StatusCode::BAD_REQUEST, problem));
    }
    let user_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = ? AND email_verified = 1 AND banned = 0")
            .bind(&username)
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?;
    // Same message as a wrong code, so this can't probe usernames either.
    let Some(user_id) = user_id else {
        return Err(err(StatusCode::UNAUTHORIZED, "wrong code"));
    };
    consume_code(&state, user_id, "reset", &req.code).await?;
    let hash = auth::hash_password(req.new_password).await.map_err(internal)?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hash)
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    // Whoever held the old password loses every session.
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_users(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<UserStatus>>> {
    let rows = sqlx::query(
        "SELECT id, username, avatar, role, banned, status_text FROM users ORDER BY username COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    let online: std::collections::HashSet<i64> =
        state.presence.lock().unwrap().keys().copied().collect();
    let mut tag_map: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for row in sqlx::query("SELECT user_id, tag_id FROM user_tags")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?
    {
        tag_map.entry(row.get(0)).or_default().push(row.get(1));
    }
    let users = rows
        .into_iter()
        .map(|r| {
            let user = User { id: r.get(0), username: r.get(1), avatar: r.get(2), role: r.get(3) };
            let banned: i64 = r.get(4);
            let status: Option<String> = r.get(5);
            // The bot never sleeps.
            let is_online = online.contains(&user.id) || user.id == state.bot_user().id;
            let tag_ids = tag_map.remove(&user.id).unwrap_or_default();
            UserStatus { user, online: is_online, banned: banned != 0, tag_ids, status }
        })
        .collect();
    Ok(Json(users))
}

/// Set (or clear) your own custom status line.
pub async fn set_status(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::SetStatusRequest>,
) -> ApiResult<StatusCode> {
    let status = req
        .text
        .map(|t| t.trim().chars().take(100).collect::<String>())
        .filter(|t| !t.is_empty());
    sqlx::query("UPDATE users SET status_text = ? WHERE id = ?")
        .bind(&status)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::StatusChanged { user_id: user.id, status });
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_tags(State(state): State<SharedState>, _user: AuthUser) -> ApiResult<Json<Vec<Tag>>> {
    let rows = sqlx::query("SELECT id, name, color FROM tags ORDER BY name COLLATE NOCASE")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(rows.into_iter().map(|r| Tag { id: r.get(0), name: r.get(1), color: r.get(2) }).collect()))
}

fn valid_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub async fn create_tag(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateTagRequest>,
) -> ApiResult<Json<Tag>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let name = req.name.trim().to_owned();
    if name.is_empty() || name.len() > 24 {
        return Err(err(StatusCode::BAD_REQUEST, "tag name must be 1-24 characters"));
    }
    if !valid_color(&req.color) {
        return Err(err(StatusCode::BAD_REQUEST, "color must be #rrggbb"));
    }
    let result = sqlx::query("INSERT INTO tags (name, color, created_at) VALUES (?, ?, ?)")
        .bind(&name)
        .bind(&req.color)
        .bind(now_ms())
        .execute(&state.db)
        .await;
    let id = match result {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "tag already exists"));
        }
        Err(e) => return Err(internal(e)),
    };
    state.broadcast(ServerEvent::TagsChanged);
    Ok(Json(Tag { id, name, color: req.color }))
}

pub async fn delete_tag(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(tag_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    sqlx::query("DELETE FROM tags WHERE id = ?")
        .bind(tag_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::TagsChanged);
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn assign_tag(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<AssignTagRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    load_user(&state, req.user_id).await?;
    if req.assigned {
        sqlx::query("INSERT OR IGNORE INTO user_tags (user_id, tag_id) VALUES (?, ?)")
            .bind(req.user_id)
            .bind(req.tag_id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    } else {
        sqlx::query("DELETE FROM user_tags WHERE user_id = ? AND tag_id = ?")
            .bind(req.user_id)
            .bind(req.tag_id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }
    state.broadcast(ServerEvent::TagsChanged);
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn owner_id(state: &SharedState) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT MIN(id) FROM users")
        .fetch_one(&state.db)
        .await
        .map_err(internal)
}

async fn load_user(state: &SharedState, user_id: i64) -> ApiResult<User> {
    let row = sqlx::query("SELECT id, username, avatar, role FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    match row {
        Some(r) => Ok(User { id: r.get(0), username: r.get(1), avatar: r.get(2), role: r.get(3) }),
        None => Err(err(StatusCode::NOT_FOUND, "no such user")),
    }
}

pub async fn set_role(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(target_id): Path<i64>,
    Json(req): Json<SetRoleRequest>,
) -> ApiResult<Json<User>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    if !matches!(req.role.as_str(), "admin" | "member") {
        return Err(err(StatusCode::BAD_REQUEST, "role must be admin or member"));
    }
    if target_id == owner_id(&state).await? {
        return Err(err(StatusCode::FORBIDDEN, "the server owner's role cannot be changed"));
    }
    load_user(&state, target_id).await?;
    sqlx::query("UPDATE users SET role = ? WHERE id = ?")
        .bind(&req.role)
        .bind(target_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    let updated = load_user(&state, target_id).await?;
    state.broadcast(ServerEvent::UserUpdated { user: updated.clone() });
    Ok(Json(updated))
}

pub async fn set_ban(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(target_id): Path<i64>,
    Json(req): Json<SetBanRequest>,
) -> ApiResult<Json<User>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    if target_id == owner_id(&state).await? {
        return Err(err(StatusCode::FORBIDDEN, "the server owner cannot be banned"));
    }
    if target_id == user.id {
        return Err(err(StatusCode::BAD_REQUEST, "you cannot ban yourself"));
    }
    load_user(&state, target_id).await?;
    sqlx::query("UPDATE users SET banned = ? WHERE id = ?")
        .bind(req.banned as i64)
        .bind(target_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    if req.banned {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(target_id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }
    let updated = load_user(&state, target_id).await?;
    state.broadcast(ServerEvent::UserUpdated { user: updated.clone() });
    Ok(Json(updated))
}

pub async fn delete_channel(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(channel_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM channels WHERE id = ?")
        .bind(channel_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    match kind.as_deref() {
        None => return Err(err(StatusCode::NOT_FOUND, "no such channel")),
        Some("dm") => return Err(err(StatusCode::BAD_REQUEST, "DMs cannot be deleted")),
        Some(_) => {}
    }
    // Reactions cascade from message deletion.
    sqlx::query("DELETE FROM messages WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    sqlx::query("DELETE FROM channels WHERE id = ?")
        .bind(channel_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::ChannelDeleted { channel_id });
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn get_profile(
    State(state): State<SharedState>,
    _user: AuthUser,
    Path(user_id): Path<i64>,
) -> ApiResult<Json<Profile>> {
    let row = sqlx::query("SELECT id, username, avatar, bio, created_at, role, banned, status_text FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    let Some(row) = row else {
        return Err(err(StatusCode::NOT_FOUND, "no such user"));
    };
    let banned: i64 = row.get(6);
    let status: Option<String> = row.get(7);
    let tags = sqlx::query(
        "SELECT t.id, t.name, t.color FROM user_tags ut JOIN tags t ON t.id = ut.tag_id \
         WHERE ut.user_id = ? ORDER BY t.name COLLATE NOCASE",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?
    .into_iter()
    .map(|r| Tag { id: r.get(0), name: r.get(1), color: r.get(2) })
    .collect();
    Ok(Json(Profile {
        user: User { id: row.get(0), username: row.get(1), avatar: row.get(2), role: row.get(5) },
        bio: row.get(3),
        created_at: row.get(4),
        banned: banned != 0,
        tags,
        status,
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
    state.broadcast(ServerEvent::UserUpdated { user: updated.0.user.clone() });
    Ok(updated)
}

pub async fn list_channels(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Channel>>> {
    let rows = sqlx::query(
        "SELECT id, name, kind FROM channels WHERE kind != 'dm' \
         UNION ALL \
         SELECT c.id, c.name, c.kind FROM channels c \
         JOIN dm_members m ON m.channel_id = c.id \
         WHERE c.kind = 'dm' AND m.user_id = ? \
         ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let mut channels: Vec<Channel> = rows
        .into_iter()
        .map(|r| Channel { id: r.get(0), name: r.get(1), kind: r.get(2), dm_members: Vec::new() })
        .collect();

    for channel in channels.iter_mut().filter(|c| c.kind == "dm") {
        channel.dm_members = dm_member_users(&state, channel.id).await?;
    }
    Ok(Json(channels))
}

async fn dm_member_users(state: &SharedState, channel_id: i64) -> ApiResult<Vec<User>> {
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.avatar, u.role FROM dm_members m \
         JOIN users u ON u.id = m.user_id WHERE m.channel_id = ?",
    )
    .bind(channel_id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    Ok(rows
        .into_iter()
        .map(|r| User { id: r.get(0), username: r.get(1), avatar: r.get(2), role: r.get(3) })
        .collect())
}

/// Open (or return the existing) DM channel with another user.
pub async fn create_dm(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateDmRequest>,
) -> ApiResult<Json<Channel>> {
    if req.user_id == user.id {
        return Err(err(StatusCode::BAD_REQUEST, "that's you"));
    }
    load_user(&state, req.user_id).await?;

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT c.id FROM channels c \
         JOIN dm_members a ON a.channel_id = c.id AND a.user_id = ? \
         JOIN dm_members b ON b.channel_id = c.id AND b.user_id = ? \
         WHERE c.kind = 'dm' LIMIT 1",
    )
    .bind(user.id)
    .bind(req.user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;

    let channel_id = match existing {
        Some(id) => id,
        None => {
            // channels.name is UNIQUE, so every DM cannot be called "dm" —
            // the first pair to talk would own the name and everyone else's
            // DM would fail to insert. The pair's ids make it unique, and
            // nothing displays it: DMs are labelled by who's in them.
            let (low, high) = if user.id < req.user_id {
                (user.id, req.user_id)
            } else {
                (req.user_id, user.id)
            };
            let id = sqlx::query("INSERT INTO channels (name, kind, created_at) VALUES (?, 'dm', ?)")
                .bind(format!("dm:{low}:{high}"))
                .bind(now_ms())
                .execute(&state.db)
                .await
                .map_err(internal)?
                .last_insert_rowid();
            for member in [user.id, req.user_id] {
                sqlx::query("INSERT INTO dm_members (channel_id, user_id) VALUES (?, ?)")
                    .bind(id)
                    .bind(member)
                    .execute(&state.db)
                    .await
                    .map_err(internal)?;
            }
            id
        }
    };

    let channel = Channel {
        id: channel_id,
        name: "dm".into(),
        kind: "dm".into(),
        dm_members: dm_member_users(&state, channel_id).await?,
    };
    if existing.is_none() {
        state.broadcast_only(vec![user.id, req.user_id], ServerEvent::ChannelCreated { channel: channel.clone() });
    }
    Ok(Json(channel))
}

#[derive(Deserialize)]
pub struct VoiceTokenQuery {
    pub channel_id: i64,
}

#[derive(serde::Serialize)]
pub(crate) struct LiveKitVideoGrant {
    pub(crate) room: String,
    #[serde(rename = "roomJoin")]
    pub(crate) room_join: bool,
    #[serde(rename = "canPublish")]
    pub(crate) can_publish: bool,
    #[serde(rename = "canSubscribe")]
    pub(crate) can_subscribe: bool,
}

#[derive(serde::Serialize)]
pub(crate) struct LiveKitClaims {
    pub(crate) iss: String,
    pub(crate) sub: String,
    pub(crate) name: String,
    pub(crate) nbf: i64,
    pub(crate) exp: i64,
    pub(crate) video: LiveKitVideoGrant,
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
    match row.map(|r| r.get::<String, _>(0)) {
        Some(kind) if kind == "voice" => {}
        // Private calls: a DM doubles as a voice room for its two members.
        Some(kind) if kind == "dm" => {
            let members = crate::dm_recipients(&state.db, q.channel_id).await.map_err(internal)?;
            if !members.is_some_and(|ids| ids.contains(&user.id)) {
                return Err(err(StatusCode::FORBIDDEN, "not your conversation"));
            }
        }
        Some(_) => return Err(err(StatusCode::BAD_REQUEST, "not a voice channel")),
        None => return Err(err(StatusCode::NOT_FOUND, "no such channel")),
    }

    let room = crate::livekit_room(q.channel_id);
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

/// Rename a text or voice channel. DMs are named after their members, so
/// they're left alone.
pub async fn rename_channel(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(channel_id): Path<i64>,
    Json(req): Json<shared::RenameChannelRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let name = req.name.trim().trim_start_matches('#').to_lowercase();
    if name.is_empty() || name.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "channel name must be 1-32 characters"));
    }
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM channels WHERE id = ?")
        .bind(channel_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    match kind.as_deref() {
        None => return Err(err(StatusCode::NOT_FOUND, "no such channel")),
        Some("dm") => return Err(err(StatusCode::BAD_REQUEST, "DMs can't be renamed")),
        Some(_) => {}
    }
    let result = sqlx::query("UPDATE channels SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(channel_id)
        .execute(&state.db)
        .await;
    match result {
        Ok(_) => {}
        // channels.name is UNIQUE, so say so rather than returning a 500.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "a channel with that name already exists"));
        }
        Err(e) => return Err(internal(e)),
    }
    state.broadcast(ServerEvent::ChannelRenamed { channel_id, name: name.clone() });
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
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
    let kind = match req.kind.as_deref() {
        Some("voice") => "voice",
        _ => "text",
    };

    let result = sqlx::query("INSERT INTO channels (name, kind, created_at) VALUES (?, ?, ?)")
        .bind(&name)
        .bind(kind)
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

    let channel = Channel { id, name, kind: kind.into(), dm_members: Vec::new() };
    state.broadcast(ServerEvent::ChannelCreated { channel: channel.clone() });
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

/// Media types safe to serve inline (rendered by the client / a browser).
fn inline_content_type(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next().unwrap_or_default().to_lowercase().as_str() {
        "gif" => Some("image/gif"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "webm" => Some("video/webm"),
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    pub name: String,
}

/// Total bytes in the uploads directory (both storage layouts).
pub async fn uploads_size() -> i64 {
    let mut total: i64 = 0;
    let Ok(mut dir) = tokio::fs::read_dir(crate::uploads_dir()).await else {
        return 0;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if meta.is_dir() {
            if let Ok(mut sub) = tokio::fs::read_dir(entry.path()).await {
                while let Ok(Some(file)) = sub.next_entry().await {
                    if let Ok(m) = file.metadata().await {
                        total += m.len() as i64;
                    }
                }
            }
        } else {
            total += meta.len() as i64;
        }
    }
    total
}

pub(crate) async fn storage_cap_bytes(state: &SharedState) -> i64 {
    let gb: i64 = meta_value_opt(state, "storage_cap_gb")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    gb.saturating_mul(1024 * 1024 * 1024)
}

pub async fn upload(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<Json<UploadResponse>> {
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty upload"));
    }
    // Uploads cost disk, storage cap, and thumbnail CPU; the budget is per
    // user, so opening more connections doesn't buy more of it.
    if !state.uploads.take(user.id) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many uploads at once — give it a moment"));
    }
    if uploads_size().await + body.len() as i64 > storage_cap_bytes(&state).await {
        return Err(err(
            StatusCode::INSUFFICIENT_STORAGE,
            "server storage is full — an admin can raise the cap in Settings → Server, or old files will expire with retention",
        ));
    }

    let url = save_bytes_to_uploads(&q.name, &body).await.map_err(internal)?;
    Ok(Json(UploadResponse { url }))
}

/// Store bytes in the uploads area; returns the server-relative /files/ URL.
pub(crate) async fn save_bytes_to_uploads(name: &str, bytes: &[u8]) -> anyhow::Result<String> {
    let name = sanitize_filename(name);
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|e| anyhow::anyhow!("os rng: {e}"))?;
    let id = hex::encode(id);

    let dir = crate::uploads_dir().join(&id);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(&name);
    tokio::fs::write(&path, bytes).await?;
    // Build the thumbnail now so the first person to see the message isn't
    // the one who pays for it. Failure is fine: readers fall back to the
    // original, and ensure() will try again on demand.
    crate::thumbs::ensure(path).await;
    Ok(format!("/files/{id}/{name}"))
}

#[derive(Deserialize)]
pub struct FileQuery {
    /// When present, force a download even for inline-renderable images.
    pub dl: Option<String>,
    /// When present, serve the small preview instead of the original.
    pub thumb: Option<String>,
}

/// Outcome of applying a Range header to a file of known length.
#[derive(Debug, PartialEq, Eq)]
enum ByteRange {
    /// No usable range — serve the whole file. Malformed headers land here
    /// too: RFC 9110 says an unparseable Range is ignored, not an error.
    Full,
    /// Serve bytes start..=end (both already clamped inside the file).
    Slice(u64, u64),
    /// Syntactically a range, but nothing in it exists — 416.
    Unsatisfiable,
}

/// Single ranges only ("bytes=a-b", "bytes=a-", "bytes=-n") — that's all a
/// video element ever sends. Multi-range requests get the whole file.
fn parse_range(header: Option<&str>, len: u64) -> ByteRange {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return ByteRange::Full;
    };
    if spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    let (start, end) = (start.trim(), end.trim());
    match (start.is_empty(), end.is_empty()) {
        (true, true) => ByteRange::Full,
        // "-n": the last n bytes.
        (true, false) => match end.parse::<u64>() {
            Ok(0) => ByteRange::Unsatisfiable,
            Ok(n) if len > 0 => ByteRange::Slice(len.saturating_sub(n), len - 1),
            Ok(_) => ByteRange::Unsatisfiable,
            Err(_) => ByteRange::Full,
        },
        // "a-": from a to the end.
        (false, true) => match start.parse::<u64>() {
            Ok(s) if s < len => ByteRange::Slice(s, len - 1),
            Ok(_) => ByteRange::Unsatisfiable,
            Err(_) => ByteRange::Full,
        },
        // "a-b", end clamped to the file.
        (false, false) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e && s < len => ByteRange::Slice(s, e.min(len - 1)),
            (Ok(_), Ok(_)) => ByteRange::Unsatisfiable,
            _ => ByteRange::Full,
        },
    }
}

/// Stream a file from disk with backpressure (no whole-file buffering).
/// Honors single-byte-range requests so video elements can seek.
async fn stream_file(
    path: std::path::PathBuf,
    headers: Vec<(header::HeaderName, String)>,
    range: Option<String>,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = file.metadata().await.ok().map(|m| m.len());
    let verdict = match len {
        Some(len) => parse_range(range.as_deref(), len),
        // No length means no way to validate a range; serve in full.
        None => ByteRange::Full,
    };

    let mut resp = match verdict {
        ByteRange::Unsatisfiable => {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Some(len) = len {
                if let Ok(v) = format!("bytes */{len}").parse() {
                    resp.headers_mut().insert(header::CONTENT_RANGE, v);
                }
            }
            resp
        }
        ByteRange::Slice(start, end) => {
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let window = end - start + 1;
            let stream =
                tokio_util::io::ReaderStream::with_capacity(file.take(window), 64 * 1024);
            let mut resp = Response::new(axum::body::Body::from_stream(stream));
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            let total = len.unwrap_or(0);
            if let Ok(v) = format!("bytes {start}-{end}/{total}").parse() {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            if let Ok(v) = window.to_string().parse() {
                resp.headers_mut().insert(header::CONTENT_LENGTH, v);
            }
            resp
        }
        ByteRange::Full => {
            let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
            let mut resp = Response::new(axum::body::Body::from_stream(stream));
            if let Some(len) = len {
                if let Ok(v) = len.to_string().parse() {
                    resp.headers_mut().insert(header::CONTENT_LENGTH, v);
                }
            }
            resp
        }
    };
    resp.headers_mut().insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    for (key, value) in headers {
        if let Ok(value) = value.parse() {
            resp.headers_mut().insert(key, value);
        }
    }
    resp
}

#[cfg(test)]
mod range_tests {
    use super::{parse_range, ByteRange::*};

    #[test]
    fn range_parsing() {
        let len = 1000;
        // The forms a video element actually sends.
        assert_eq!(parse_range(Some("bytes=0-499"), len), Slice(0, 499));
        assert_eq!(parse_range(Some("bytes=500-"), len), Slice(500, 999));
        assert_eq!(parse_range(Some("bytes=-100"), len), Slice(900, 999));
        // Clamping and bounds.
        assert_eq!(parse_range(Some("bytes=900-5000"), len), Slice(900, 999));
        assert_eq!(parse_range(Some("bytes=-5000"), len), Slice(0, 999));
        assert_eq!(parse_range(Some("bytes=1000-"), len), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=1200-1300"), len), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), len), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-2"), len), Unsatisfiable);
        // Ignored (full response), per RFC: malformed or unsupported.
        assert_eq!(parse_range(None, len), Full);
        assert_eq!(parse_range(Some("bytes=abc-"), len), Full);
        assert_eq!(parse_range(Some("bytes=-"), len), Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-9"), len), Full);
        assert_eq!(parse_range(Some("items=0-5"), len), Full);
        // Empty file: any concrete range is unsatisfiable.
        assert_eq!(parse_range(Some("bytes=0-"), 0), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-5"), 0), Unsatisfiable);
    }
}

fn file_response(
    path: std::path::PathBuf,
    name: &str,
    force_download: bool,
    range: Option<String>,
) -> impl std::future::Future<Output = Response> + Send + 'static {
    let name = name.to_owned();
    async move {
        let mut headers = vec![
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ];
        let inline_media = inline_content_type(&name).filter(|_| !force_download);
        match inline_media {
            Some(ct) => headers.push((header::CONTENT_TYPE, ct.to_owned())),
            None => {
                headers.push((header::CONTENT_TYPE, "application/octet-stream".to_owned()));
                headers.push((
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ));
            }
        }
        stream_file(path, headers, range).await
    }
}

/// The Range header as a string, if the request carried one.
fn range_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// Current format: /files/{32-hex id}/{sanitized original filename}.
pub async fn serve_file(
    Path((id, name)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let id_ok = id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !id_ok || name != sanitize_filename(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let original = crate::uploads_dir().join(&id).join(&name);
    // ?thumb wants the small version; anything without one (small images,
    // videos, files that predate this and fail to decode) serves as usual.
    if q.thumb.is_some() && q.dl.is_none() {
        if let Some(thumb) = crate::thumbs::ensure(original.clone()).await {
            return stream_file(
                thumb,
                vec![
                    (header::CONTENT_TYPE, "image/jpeg".to_owned()),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_owned()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                ],
                None,
            )
            .await;
        }
    }
    file_response(original, &name, q.dl.is_some(), range_of(&headers)).await
}

/// Legacy format from the first uploads release: /files/{32-hex}.{ext}.
pub async fn serve_file_legacy(
    Path(name): Path<String>,
    Query(q): Query<FileQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let valid = name.len() < 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
        && name.matches('.').count() == 1;
    if !valid {
        return StatusCode::NOT_FOUND.into_response();
    }
    file_response(crate::uploads_dir().join(&name), &name, q.dl.is_some(), range_of(&headers)).await
}

pub async fn list_emojis(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<shared::CustomEmoji>>> {
    let rows = sqlx::query(
        "SELECT id, name, url, creator_id FROM custom_emojis ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| shared::CustomEmoji {
                id: r.get(0),
                name: r.get(1),
                url: r.get(2),
                creator_id: r.get(3),
            })
            .collect(),
    ))
}

pub async fn create_emoji(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::CreateEmojiRequest>,
) -> ApiResult<Json<shared::CustomEmoji>> {
    let name = req.name.trim().to_lowercase();
    if let Some(problem) = shared::emoji_name_problem(&name) {
        return Err(err(StatusCode::BAD_REQUEST, problem));
    }
    let ok = req.url.len() < 500
        && (req.url.starts_with("http://") || req.url.starts_with("https://") || req.url.starts_with("/files/"));
    if !ok {
        return Err(err(StatusCode::BAD_REQUEST, "invalid emoji url"));
    }

    let result = sqlx::query(
        "INSERT INTO custom_emojis (name, url, creator_id, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&name)
    .bind(&req.url)
    .bind(user.id)
    .bind(now_ms())
    .execute(&state.db)
    .await;

    let id = match result {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "an emoji with that name already exists"));
        }
        Err(e) => return Err(internal(e)),
    };

    let emoji = shared::CustomEmoji { id, name, url: req.url, creator_id: user.id };
    state.broadcast(ServerEvent::EmojisChanged);
    Ok(Json(emoji))
}

pub async fn delete_emoji(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator: Option<i64> = sqlx::query_scalar("SELECT creator_id FROM custom_emojis WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
    let Some(creator) = creator else {
        return Err(err(StatusCode::NOT_FOUND, "no such emoji"));
    };
    if creator != user.id && user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "only the uploader or an admin can delete that"));
    }
    sqlx::query("DELETE FROM custom_emojis WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::EmojisChanged);
    Ok(Json(serde_json::json!({ "ok": true })))
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
    state.broadcast(ServerEvent::StickerCreated { sticker: sticker.clone() });
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
    state.broadcast(ServerEvent::StickerDeleted { sticker_id });
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) fn client_dir() -> std::path::PathBuf {
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

async fn meta_value(state: &SharedState, key: &str) -> ApiResult<String> {
    sqlx::query_scalar("SELECT value FROM server_meta WHERE key = ?")
        .bind(key)
        .fetch_one(&state.db)
        .await
        .map_err(internal)
}

async fn meta_value_opt(state: &SharedState, key: &str) -> ApiResult<Option<String>> {
    sqlx::query_scalar("SELECT value FROM server_meta WHERE key = ?")
        .bind(key)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)
}

/// Public: this instance's identity.
pub async fn server_info(State(state): State<SharedState>) -> ApiResult<Json<ServerInfo>> {
    Ok(Json(ServerInfo {
        id: meta_value(&state, "id").await?,
        name: meta_value(&state, "name").await?,
        icon: meta_value_opt(&state, "icon").await?,
    }))
}

pub async fn set_server_icon(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<SetServerIconRequest>,
) -> ApiResult<Json<ServerInfo>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let ok = req.url.len() < 500
        && (req.url.starts_with("http://") || req.url.starts_with("https://") || req.url.starts_with("/files/"));
    if !ok {
        return Err(err(StatusCode::BAD_REQUEST, "invalid icon url"));
    }
    sqlx::query("INSERT OR REPLACE INTO server_meta (key, value) VALUES ('icon', ?)")
        .bind(&req.url)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::ServerIconChanged { icon: req.url });
    server_info(State(state)).await
}

pub async fn rename_server(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<RenameServerRequest>,
) -> ApiResult<Json<ServerInfo>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let name = req.name.trim().to_owned();
    if name.is_empty() || name.len() > 40 {
        return Err(err(StatusCode::BAD_REQUEST, "server name must be 1-40 characters"));
    }
    sqlx::query("UPDATE server_meta SET value = ? WHERE key = 'name'")
        .bind(&name)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.broadcast(ServerEvent::ServerRenamed { name: name.clone() });
    Ok(Json(ServerInfo {
        id: meta_value(&state, "id").await?,
        icon: meta_value_opt(&state, "icon").await?,
        name,
    }))
}

pub async fn get_retention(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<RetentionSetting>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let days = meta_value(&state, "upload_retention_days").await?.parse().unwrap_or(21);
    Ok(Json(RetentionSetting { days }))
}

pub async fn set_retention(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<RetentionSetting>,
) -> ApiResult<Json<RetentionSetting>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    if !(7..=365).contains(&req.days) {
        return Err(err(StatusCode::BAD_REQUEST, "retention must be between 7 and 365 days"));
    }
    sqlx::query("UPDATE server_meta SET value = ? WHERE key = 'upload_retention_days'")
        .bind(req.days.to_string())
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(req))
}

pub async fn get_storage(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<shared::StorageInfo>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let cap_gb = meta_value(&state, "storage_cap_gb").await?.parse().unwrap_or(30);
    Ok(Json(shared::StorageInfo { used_bytes: uploads_size().await, cap_gb }))
}

pub async fn set_storage_cap(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::StorageCapSetting>,
) -> ApiResult<Json<shared::StorageInfo>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    if !(1..=500).contains(&req.cap_gb) {
        return Err(err(StatusCode::BAD_REQUEST, "storage cap must be between 1 and 500 GB"));
    }
    sqlx::query("UPDATE server_meta SET value = ? WHERE key = 'storage_cap_gb'")
        .bind(req.cap_gb.to_string())
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(Json(shared::StorageInfo { used_bytes: uploads_size().await, cap_gb: req.cap_gb }))
}

pub async fn get_invite(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<shared::InviteSetting>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let code = meta_value_opt(&state, "invite_code").await?.unwrap_or_default();
    Ok(Json(shared::InviteSetting { code }))
}

pub async fn set_invite(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::InviteSetting>,
) -> ApiResult<Json<shared::InviteSetting>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let code = req.code.trim().to_owned();
    if code.len() > 64 {
        return Err(err(StatusCode::BAD_REQUEST, "invite code must be at most 64 characters"));
    }
    if code.chars().any(char::is_whitespace) {
        return Err(err(StatusCode::BAD_REQUEST, "invite code can't contain spaces"));
    }
    sqlx::query(
        "INSERT INTO server_meta (key, value) VALUES ('invite_code', ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(&code)
    .execute(&state.db)
    .await
    .map_err(internal)?;
    Ok(Json(shared::InviteSetting { code }))
}

/// Unread counts per visible channel. DMs only count for their participants.
pub async fn unread(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<shared::UnreadInfo>>> {
    let mention = format!("%@{}%", user.username.to_lowercase());
    let rows = sqlx::query(
        "SELECT c.id, \
                COUNT(m.id) AS unread, \
                COALESCE(SUM(CASE WHEN c.kind = 'dm' \
                                    OR LOWER(m.content) LIKE '%@everyone%' \
                                    OR LOWER(m.content) LIKE ? \
                                  THEN 1 ELSE 0 END), 0) AS mentions, \
                COALESCE(r.last_read_id, 0) AS last_read \
         FROM channels c \
         LEFT JOIN read_state r ON r.channel_id = c.id AND r.user_id = ? \
         LEFT JOIN messages m ON m.channel_id = c.id \
              AND m.id > COALESCE(r.last_read_id, 0) \
              AND m.author_id != ? \
         WHERE c.kind != 'voice' \
           AND (c.kind != 'dm' OR EXISTS ( \
                 SELECT 1 FROM dm_members d WHERE d.channel_id = c.id AND d.user_id = ?)) \
         GROUP BY c.id",
    )
    .bind(&mention)
    .bind(user.id)
    .bind(user.id)
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(
        rows.into_iter()
            .map(|r| shared::UnreadInfo {
                channel_id: r.get(0),
                count: r.get(1),
                mentions: r.get(2),
                last_read_id: r.get(3),
            })
            .collect(),
    ))
}

/// Mark a channel read up to a message. Never moves backwards.
pub async fn mark_read(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::MarkReadRequest>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "INSERT INTO read_state (user_id, channel_id, last_read_id) VALUES (?, ?, ?) \
         ON CONFLICT(user_id, channel_id) \
         DO UPDATE SET last_read_id = MAX(last_read_id, excluded.last_read_id)",
    )
    .bind(user.id)
    .bind(req.channel_id)
    .bind(req.message_id)
    .execute(&state.db)
    .await
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_bot_settings(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<shared::BotSettings>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }
    let bot = state.bot_user();
    let announce_channel: Option<i64> =
        sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'announce_channel'")
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?
            .and_then(|v: String| v.parse().ok());
    Ok(Json(shared::BotSettings {
        persona: crate::bot::persona(&state.db).await,
        name: bot.username,
        avatar: bot.avatar,
        announce_channel,
    }))
}

pub async fn set_bot_settings(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::BotSettingsUpdate>,
) -> ApiResult<Json<shared::BotSettings>> {
    if user.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "admins only"));
    }

    if let Some(persona) = &req.persona {
        let persona = persona.trim();
        if persona.len() > 4000 {
            return Err(err(StatusCode::BAD_REQUEST, "personality must be at most 4000 characters"));
        }
        // Empty resets to the built-in default.
        sqlx::query(
            "INSERT INTO server_meta (key, value) VALUES ('bot_persona', ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(persona)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    }

    let mut identity_changed = false;
    let bot_id = state.bot_user().id;

    if let Some(name) = &req.name {
        let name = name.trim();
        if name.len() < 2 || name.len() > 32 {
            return Err(err(StatusCode::BAD_REQUEST, "bot name must be 2-32 characters"));
        }
        let result = sqlx::query("UPDATE users SET username = ? WHERE id = ?")
            .bind(name)
            .bind(bot_id)
            .execute(&state.db)
            .await;
        match result {
            Ok(_) => identity_changed = true,
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(err(StatusCode::CONFLICT, "someone already has that name"));
            }
            Err(e) => return Err(internal(e)),
        }
    }

    if let Some(avatar) = &req.avatar {
        let ok = avatar.len() < 500
            && (avatar.starts_with("http://") || avatar.starts_with("https://") || avatar.starts_with("/files/"));
        if !ok {
            return Err(err(StatusCode::BAD_REQUEST, "invalid avatar url"));
        }
        sqlx::query("UPDATE users SET avatar = ? WHERE id = ?")
            .bind(avatar)
            .bind(bot_id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
        identity_changed = true;
    }

    if let Some(channel) = req.announce_channel {
        // 0 means "don't announce"; anything else must be a real text channel.
        if channel != 0 {
            let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM channels WHERE id = ?")
                .bind(channel)
                .fetch_optional(&state.db)
                .await
                .map_err(internal)?;
            if kind.as_deref() != Some("text") {
                return Err(err(StatusCode::BAD_REQUEST, "pick a text channel"));
            }
        }
        sqlx::query(
            "INSERT INTO server_meta (key, value) VALUES ('announce_channel', ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(channel.to_string())
        .execute(&state.db)
        .await
        .map_err(internal)?;
    }

    if identity_changed {
        let updated = load_user(&state, bot_id).await?;
        *state.bot.lock().unwrap() = updated.clone();
        state.broadcast(ServerEvent::UserUpdated { user: updated });
    }

    get_bot_settings(State(state), AuthUser(user)).await
}

/// Public: release notes, newest first (uploaded by the release script).
pub async fn changelog() -> Response {
    match tokio::fs::read(client_dir().join("changelog.json")).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Where the built web app (dx output + PWA files) lives on disk.
fn webapp_dir() -> std::path::PathBuf {
    std::env::var("NOTDISCORD_WEBAPP_DIR").unwrap_or_else(|_| "./webapp".into()).into()
}

fn webapp_content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "text/javascript",
        // The right type matters: browsers only streaming-compile wasm
        // served as application/wasm.
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        "json" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// The web app shell. No caching: it references content-hashed assets, so a
/// fresh index is what makes deploys take effect.
pub async fn webapp_index() -> Response {
    stream_file(
        webapp_dir().join("index.html"),
        vec![
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        None,
    )
    .await
}

pub async fn webapp_asset(
    Path(path): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Flat allowlist of path characters; no traversal, no hidden files.
    let ok = !path.is_empty()
        && path.len() < 200
        && !path.starts_with('.')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
        && !path.contains("..");
    if !ok {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = path.rsplit('/').next().unwrap_or("");
    // dx content-hashes everything under assets/ — those can cache forever.
    // The mutable files (sw.js, manifest, icons) revalidate.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    stream_file(
        webapp_dir().join(&path),
        vec![
            (header::CONTENT_TYPE, webapp_content_type(name).to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        range_of(&headers),
    )
    .await
}

/// Public: download the current client build (streamed). Range support means
/// a dropped update download can resume where it left off.
pub async fn download_client(headers: axum::http::HeaderMap) -> Response {
    stream_file(
        client_dir().join("NotDiscord.exe"),
        vec![
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"NotDiscord.exe\"".to_owned()),
        ],
        range_of(&headers),
    )
    .await
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// Full-text message search. DM messages only surface for participants.
pub async fn search(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<SearchResult>>> {
    // Build a safe FTS5 query: quoted prefix tokens, implicit AND.
    let fts: String = q
        .q
        .split_whitespace()
        .take(8)
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    if fts.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let rows = sqlx::query(
        "SELECT m.id, m.channel_id, m.content, m.created_at, m.edited_at, \
                u.id, u.username, u.avatar, u.role, c.name, c.kind \
         FROM messages_fts f \
         JOIN messages m ON m.id = f.rowid \
         JOIN users u ON u.id = m.author_id \
         JOIN channels c ON c.id = m.channel_id \
         WHERE messages_fts MATCH ? \
           AND (c.kind != 'dm' OR EXISTS ( \
                SELECT 1 FROM dm_members dm WHERE dm.channel_id = c.id AND dm.user_id = ?)) \
         ORDER BY m.id DESC LIMIT 30",
    )
    .bind(&fts)
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let results = rows
        .into_iter()
        .map(|r| SearchResult {
            message: Message {
                id: r.get(0),
                channel_id: r.get(1),
                content: r.get(2),
                created_at: r.get(3),
                edited_at: r.get(4),
                author: User { id: r.get(5), username: r.get(6), avatar: r.get(7), role: r.get(8) },
                reactions: Vec::new(),
                reply_to: None,
                reply_preview: None,
                pinned: false,
            },
            channel_name: r.get(9),
            channel_kind: r.get(10),
        })
        .collect();
    Ok(Json(results))
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
    AuthUser(user): AuthUser,
    Path(channel_id): Path<i64>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Json<Vec<Message>>> {
    // DM history is participants-only.
    let recipients = crate::dm_recipients(&state.db, channel_id).await.map_err(internal)?;
    if recipients.is_some_and(|ids| !ids.contains(&user.id)) {
        return Err(err(StatusCode::FORBIDDEN, "not your conversation"));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let before = q.before.unwrap_or(i64::MAX);

    let rows = sqlx::query(
        "SELECT m.id, m.channel_id, m.content, m.created_at, m.edited_at, u.id, u.username, u.avatar, u.role, \
                m.reply_to, ru.username, r.content, m.pinned_at IS NOT NULL \
         FROM messages m JOIN users u ON u.id = m.author_id \
         LEFT JOIN messages r ON r.id = m.reply_to \
         LEFT JOIN users ru ON ru.id = r.author_id \
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
            author: User { id: r.get(5), username: r.get(6), avatar: r.get(7), role: r.get(8) },
            reactions: Vec::new(),
            reply_to: r.get(9),
            reply_preview: {
                let author: Option<String> = r.get(10);
                let content: Option<String> = r.get(11);
                match (author, content) {
                    (Some(author), Some(content)) => Some(shared::ReplyPreview { author, content }),
                    _ => None,
                }
            },
            pinned: r.get(12),
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

/// What this person wants pushed, plus the key their browser needs to
/// subscribe. `endpoint` identifies the calling device, if it has one.
pub async fn get_notify_prefs(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Query(q): Query<NotifyQuery>,
) -> ApiResult<Json<shared::NotifyPrefs>> {
    let level: Option<String> = sqlx::query_scalar("SELECT notify_level FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .flatten();
    let subscribed = match q.endpoint {
        Some(endpoint) => sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM push_subscriptions WHERE endpoint = ? AND user_id = ?",
        )
        .bind(&endpoint)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .is_some(),
        None => false,
    };
    let vapid_key = crate::push::vapid(&state).await.map_err(internal)?.public_b64;
    Ok(Json(shared::NotifyPrefs {
        level: level.unwrap_or_else(|| "mentions".into()),
        subscribed,
        vapid_key,
    }))
}

#[derive(Deserialize)]
pub struct NotifyQuery {
    pub endpoint: Option<String>,
}

pub async fn set_notify_level(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::SetNotifyLevel>,
) -> ApiResult<StatusCode> {
    if !matches!(req.level.as_str(), "all" | "mentions" | "none") {
        return Err(err(StatusCode::BAD_REQUEST, "unknown notification level"));
    }
    sqlx::query("UPDATE users SET notify_level = ? WHERE id = ?")
        .bind(&req.level)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn push_subscribe(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::PushSubscribeRequest>,
) -> ApiResult<StatusCode> {
    if req.endpoint.len() > 800 || !req.endpoint.starts_with("https://") {
        return Err(err(StatusCode::BAD_REQUEST, "invalid push endpoint"));
    }
    // The endpoint is the primary key: re-subscribing the same device (or a
    // different account on it) replaces rather than duplicates.
    sqlx::query(
        "INSERT INTO push_subscriptions (endpoint, user_id, p256dh, auth, created_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(endpoint) DO UPDATE SET user_id = excluded.user_id, \
             p256dh = excluded.p256dh, auth = excluded.auth",
    )
    .bind(&req.endpoint)
    .bind(user.id)
    .bind(&req.p256dh)
    .bind(&req.auth)
    .bind(now_ms())
    .execute(&state.db)
    .await
    .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn push_unsubscribe(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Json(req): Json<shared::PushSubscribeRequest>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ? AND user_id = ?")
        .bind(&req.endpoint)
        .bind(user.id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Every attachment ever posted in a channel, newest first — the Files panel.
/// Files are found by scanning message content for /files/ links (that's the
/// only way attachments exist), and anything retention already deleted from
/// disk is silently skipped.
pub async fn channel_files(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(channel_id): Path<i64>,
) -> ApiResult<Json<Vec<shared::FileEntry>>> {
    let recipients = crate::dm_recipients(&state.db, channel_id).await.map_err(internal)?;
    if recipients.is_some_and(|ids| !ids.contains(&user.id)) {
        return Err(err(StatusCode::FORBIDDEN, "not your conversation"));
    }
    let rows = sqlx::query(
        "SELECT m.id, m.content, m.created_at, u.username \
         FROM messages m JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = ? AND m.content LIKE '%/files/%' \
         ORDER BY m.id DESC LIMIT 400",
    )
    .bind(channel_id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    // Stickers and custom emojis live in /files/ too, but they're decoration,
    // not shared files — using one shouldn't put it in the Files panel.
    let mut decoration = std::collections::HashSet::new();
    for table in ["SELECT url FROM stickers", "SELECT url FROM custom_emojis"] {
        for row in sqlx::query(table).fetch_all(&state.db).await.map_err(internal)? {
            let url: String = row.get(0);
            if let Some(idx) = url.find("/files/") {
                decoration.insert(url[idx..].to_owned());
            }
        }
    }

    let uploads = crate::uploads_dir();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let (message_id, content, created_at, uploader): (i64, String, i64, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        for token in content.split_whitespace() {
            let Some(idx) = token.find("/files/") else { continue };
            // A bare /files/ path or one inside an http(s) URL; anything else
            // is prose that happens to contain the string.
            if idx != 0 && !(token.starts_with("http://") || token.starts_with("https://")) {
                continue;
            }
            let rel = &token[idx..];
            if decoration.contains(rel) || !seen.insert(rel.to_owned()) {
                continue;
            }
            // Message content is user text: re-validate exactly like the
            // serving endpoints before touching the filesystem.
            let segments: Vec<&str> = rel.trim_start_matches("/files/").split('/').collect();
            let (disk, name) = match segments[..] {
                [id, name] => {
                    let id_ok = id.len() == 32
                        && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
                    if !id_ok || name.is_empty() || name != sanitize_filename(name) {
                        continue;
                    }
                    (uploads.join(id).join(name), name)
                }
                [single] => {
                    let valid = single.len() < 40
                        && single.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
                        && single.matches('.').count() == 1;
                    if !valid {
                        continue;
                    }
                    (uploads.join(single), single)
                }
                _ => continue,
            };
            let Ok(meta) = std::fs::metadata(&disk) else { continue };
            out.push(shared::FileEntry {
                url: rel.to_owned(),
                name: name.to_owned(),
                size: meta.len() as i64,
                created_at,
                message_id,
                uploader: uploader.clone(),
            });
        }
    }
    Ok(Json(out))
}

/// Pin rights: admins anywhere; in a DM, either participant (a DM has no
/// admin). Returns the message's channel and the event audience.
async fn pin_target(
    state: &SharedState,
    user: &User,
    message_id: i64,
) -> ApiResult<(i64, Option<Vec<i64>>)> {
    let channel_id: i64 = sqlx::query_scalar("SELECT channel_id FROM messages WHERE id = ?")
        .bind(message_id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such message"))?;
    let recipients = crate::dm_recipients(&state.db, channel_id).await.map_err(internal)?;
    let allowed = match &recipients {
        Some(ids) => ids.contains(&user.id),
        None => user.role == "admin",
    };
    if !allowed {
        return Err(err(StatusCode::FORBIDDEN, "only admins can pin messages"));
    }
    Ok((channel_id, recipients))
}

pub async fn pin_message(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(message_id): Path<i64>,
) -> ApiResult<StatusCode> {
    let (channel_id, recipients) = pin_target(&state, &user, message_id).await?;
    sqlx::query("UPDATE messages SET pinned_at = ?, pinned_by = ? WHERE id = ?")
        .bind(now_ms())
        .bind(user.id)
        .bind(message_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    let event = ServerEvent::MessagePinChanged { channel_id, message_id, pinned: true };
    match recipients {
        Some(ids) => state.broadcast_only(ids, event),
        None => state.broadcast(event),
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unpin_message(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(message_id): Path<i64>,
) -> ApiResult<StatusCode> {
    let (channel_id, recipients) = pin_target(&state, &user, message_id).await?;
    sqlx::query("UPDATE messages SET pinned_at = NULL, pinned_by = NULL WHERE id = ?")
        .bind(message_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    let event = ServerEvent::MessagePinChanged { channel_id, message_id, pinned: false };
    match recipients {
        Some(ids) => state.broadcast_only(ids, event),
        None => state.broadcast(event),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The pin list for a channel, newest pin first.
pub async fn channel_pins(
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
    Path(channel_id): Path<i64>,
) -> ApiResult<Json<Vec<Message>>> {
    let recipients = crate::dm_recipients(&state.db, channel_id).await.map_err(internal)?;
    if recipients.is_some_and(|ids| !ids.contains(&user.id)) {
        return Err(err(StatusCode::FORBIDDEN, "not your conversation"));
    }
    let rows = sqlx::query(
        "SELECT m.id, m.channel_id, m.content, m.created_at, m.edited_at, \
                u.id, u.username, u.avatar, u.role \
         FROM messages m JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = ? AND m.pinned_at IS NOT NULL \
         ORDER BY m.pinned_at DESC",
    )
    .bind(channel_id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    let pins = rows
        .into_iter()
        .map(|r| Message {
            id: r.get(0),
            channel_id: r.get(1),
            content: r.get(2),
            created_at: r.get(3),
            edited_at: r.get(4),
            author: User { id: r.get(5), username: r.get(6), avatar: r.get(7), role: r.get(8) },
            reactions: Vec::new(),
            reply_to: None,
            reply_preview: None,
            pinned: true,
        })
        .collect();
    Ok(Json(pins))
}
