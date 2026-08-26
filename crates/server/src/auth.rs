use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use axum::Json;
use sqlx::Row;

use shared::{ApiError, User};

use crate::SharedState;

pub type ApiResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

pub fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: msg.into() }))
}

pub fn internal(e: impl std::fmt::Display) -> (StatusCode, Json<ApiError>) {
    tracing::error!("internal error: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut rand_core::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!(e))
    })
    .await?
}

pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("os rng");
    hex::encode(bytes)
}

/// Extractor: resolves the `Authorization: Bearer <token>` header (or
/// `?token=` query param, used by the WebSocket route) to a logged-in user.
pub struct AuthUser(pub User);

impl FromRequestParts<SharedState> for AuthUser {
    type Rejection = (StatusCode, Json<ApiError>);

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned);

        let token = bearer.or_else(|| {
            parts.uri.query().and_then(|q| {
                q.split('&')
                    .find_map(|pair| pair.strip_prefix("token=").map(str::to_owned))
            })
        });

        let Some(token) = token else {
            return Err(err(StatusCode::UNAUTHORIZED, "missing auth token"));
        };

        let row = sqlx::query(
            "SELECT users.id, users.username, users.avatar, users.role, users.banned FROM sessions \
             JOIN users ON users.id = sessions.user_id WHERE sessions.token = ?",
        )
        .bind(&token)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;

        match row {
            Some(row) => {
                let banned: i64 = row.get(4);
                if banned != 0 {
                    return Err(err(StatusCode::FORBIDDEN, "you are banned from this server"));
                }
                Ok(AuthUser(User {
                    id: row.get(0),
                    username: row.get(1),
                    avatar: row.get(2),
                    role: row.get(3),
                }))
            }
            None => Err(err(StatusCode::UNAUTHORIZED, "invalid or expired token")),
        }
    }
}
