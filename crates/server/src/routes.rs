use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use sqlx::Row;

use shared::{
    AuthResponse, Channel, CreateChannelRequest, LoginRequest, Message, RegisterRequest,
    ServerEvent, User,
};

use crate::auth::{self, err, internal, ApiResult, AuthUser};
use crate::{now_ms, SharedState};

pub async fn register(
    State(state): State<SharedState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<AuthResponse>> {
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
    Ok(Json(AuthResponse { token, user: User { id: user_id, username } }))
}

pub async fn login(
    State(state): State<SharedState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<AuthResponse>> {
    let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE username = ?")
        .bind(req.username.trim())
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;

    let Some(row) = row else {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    };
    let (id, username, hash): (i64, String, String) = (row.get(0), row.get(1), row.get(2));

    if !auth::verify_password(req.password, hash).await {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    }

    let token = create_session(&state, id).await?;
    Ok(Json(AuthResponse { token, user: User { id, username } }))
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

pub async fn list_channels(
    State(state): State<SharedState>,
    _user: AuthUser,
) -> ApiResult<Json<Vec<Channel>>> {
    let rows = sqlx::query("SELECT id, name FROM channels ORDER BY id")
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;
    let channels = rows
        .into_iter()
        .map(|r| Channel { id: r.get(0), name: r.get(1) })
        .collect();
    Ok(Json(channels))
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

    let channel = Channel { id, name };
    let _ = state.events.send(ServerEvent::ChannelCreated { channel: channel.clone() });
    Ok(Json(channel))
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
        "SELECT m.id, m.channel_id, m.content, m.created_at, u.id, u.username \
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
            author: User { id: r.get(4), username: r.get(5) },
        })
        .collect();
    messages.reverse();
    Ok(Json(messages))
}
