mod auth;
mod bot;
mod music;
mod preview;
mod routes;
mod ws;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::routing::{any, get, post};
use axum::Router;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use shared::ServerEvent;

/// A broadcast event, optionally restricted to specific user ids (DM privacy).
#[derive(Clone)]
pub struct Envelope {
    pub event: ServerEvent,
    pub only: Option<Vec<i64>>,
}

pub struct AppState {
    pub db: SqlitePool,
    pub events: broadcast::Sender<Envelope>,
    /// user id -> number of live WebSocket connections.
    pub presence: Mutex<HashMap<i64, u32>>,
    /// user id -> (voice channel id, user, sharing screen, camera on) for everyone in voice.
    pub voice: Mutex<HashMap<i64, (i64, shared::User, bool, bool)>>,
    /// The resident bot's user account (admins can rename it / set an avatar).
    pub bot: Mutex<shared::User>,
    /// Whether a music status watcher task is currently running.
    pub music_watch: Mutex<bool>,
    /// The live player card message: (channel id, message id).
    pub music_player: Mutex<Option<(i64, i64)>>,
}

impl AppState {
    pub fn bot_user(&self) -> shared::User {
        self.bot.lock().unwrap().clone()
    }
}

impl AppState {
    /// Broadcast to everyone.
    pub fn broadcast(&self, event: ServerEvent) {
        let _ = self.events.send(Envelope { event, only: None });
    }

    /// Broadcast only to the given user ids' connections.
    pub fn broadcast_only(&self, only: Vec<i64>, event: ServerEvent) {
        let _ = self.events.send(Envelope { event, only: Some(only) });
    }
}

/// For dm channels, the participant ids (the event audience); None otherwise.
pub async fn dm_recipients(db: &SqlitePool, channel_id: i64) -> Result<Option<Vec<i64>>, sqlx::Error> {
    use sqlx::Row;
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM channels WHERE id = ?")
        .bind(channel_id)
        .fetch_optional(db)
        .await?;
    if kind.as_deref() != Some("dm") {
        return Ok(None);
    }
    let rows = sqlx::query("SELECT user_id FROM dm_members WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_all(db)
        .await?;
    Ok(Some(rows.into_iter().map(|r| r.get(0)).collect()))
}

pub type SharedState = Arc<AppState>;

pub fn uploads_dir() -> std::path::PathBuf {
    std::env::var("NOTDISCORD_UPLOADS")
        .unwrap_or_else(|_| "uploads".into())
        .into()
}

/// Delete uploads older than the retention window, except files still
/// referenced as an avatar or sticker. Both storage layouts are handled:
/// uploads/{32hex}/{name} directories and legacy flat uploads/{32hex}.{ext}.
async fn cleanup_uploads(state: &SharedState) -> anyhow::Result<usize> {
    use sqlx::Row;

    let days: i64 = sqlx::query_scalar::<_, String>(
        "SELECT value FROM server_meta WHERE key = 'upload_retention_days'",
    )
    .fetch_optional(&state.db)
    .await?
    .and_then(|v| v.parse().ok())
    .unwrap_or(21);
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days as u64 * 86400);

    // Protected: the first /files/ path segment of every avatar and sticker.
    let mut keep = std::collections::HashSet::<String>::new();
    let referenced = sqlx::query(
        "SELECT avatar AS url FROM users WHERE avatar IS NOT NULL \
         UNION ALL SELECT url FROM stickers \
         UNION ALL SELECT value FROM server_meta WHERE key = 'icon'",
    )
    .fetch_all(&state.db)
    .await?;
    for row in referenced {
        let url: String = row.get(0);
        if let Some(pos) = url.find("/files/") {
            if let Some(first) = url[pos + 7..].split('/').next() {
                keep.insert(first.to_owned());
            }
        }
    }

    let mut removed = 0usize;
    let mut dir = match tokio::fs::read_dir(uploads_dir()).await {
        Ok(dir) => dir,
        Err(_) => return Ok(0),
    };
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if modified >= cutoff {
            continue;
        }
        let ok = if meta.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await.is_ok()
        } else {
            tokio::fs::remove_file(entry.path()).await.is_ok()
        };
        if ok {
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "server=info,axum=info".into()),
        )
        .init();

    let db_path = std::env::var("NOTDISCORD_DB").unwrap_or_else(|_| "notdiscord.db".into());
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true);
    let db = SqlitePoolOptions::new().connect_with(options).await?;
    sqlx::migrate!().run(&db).await?;

    // Instance identity: generate a stable id on first boot, default the name.
    let meta_defaults: &[(&str, fn() -> String)] = &[
        ("id", || {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).expect("os rng");
            hex::encode(bytes)
        }),
        ("name", || "NotDiscord".to_string()),
        ("upload_retention_days", || "21".to_string()),
        ("storage_cap_gb", || "30".to_string()),
        // Seeded from the env var once; after that admins manage it in-app.
        ("invite_code", || std::env::var("NOTDISCORD_INVITE").unwrap_or_default()),
    ];
    for (key, default) in meta_defaults {
        sqlx::query("INSERT OR IGNORE INTO server_meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(default())
            .execute(&db)
            .await?;
    }

    let bot_user = bot::ensure_bot_user(&db).await?;

    let (events, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        db,
        events,
        presence: Mutex::new(HashMap::new()),
        voice: Mutex::new(HashMap::new()),
        bot: Mutex::new(bot_user),
        music_watch: Mutex::new(false),
        music_player: Mutex::new(None),
    });

    // NotBot announces new client releases in chat.
    tokio::spawn(bot::announce_loop(state.clone()));

    // Hourly sweep of expired uploads (avatars and stickers are protected).
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match cleanup_uploads(&state).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("upload cleanup removed {n} expired item(s)"),
                    Err(e) => tracing::warn!("upload cleanup failed: {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    let app = Router::new()
        .route("/api/register", post(routes::register))
        .route("/api/login", post(routes::login))
        .route("/api/me", get(routes::me))
        .route("/api/password", post(routes::change_password))
        .route("/api/users", get(routes::list_users))
        .route("/api/users/{id}/profile", get(routes::get_profile))
        .route("/api/users/{id}/role", post(routes::set_role))
        .route("/api/users/{id}/ban", post(routes::set_ban))
        .route("/api/channels/{id}", axum::routing::delete(routes::delete_channel))
        .route("/api/profile", post(routes::update_profile))
        .route("/api/channels", get(routes::list_channels).post(routes::create_channel))
        .route("/api/channels/{id}/messages", get(routes::channel_messages))
        .route("/api/dms", post(routes::create_dm))
        .route(
            "/api/upload",
            post(routes::upload).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/gifs", get(routes::gifs))
        .route("/api/preview", get(preview::preview))
        .route("/api/voice/token", get(routes::voice_token))
        .route("/api/stickers", get(routes::list_stickers).post(routes::create_sticker))
        .route("/api/stickers/{id}", axum::routing::delete(routes::delete_sticker))
        .route("/api/client/version", get(routes::client_version))
        .route("/api/changelog", get(routes::changelog))
        .route("/api/server/info", get(routes::server_info))
        .route("/api/server/name", post(routes::rename_server))
        .route("/api/server/retention", get(routes::get_retention).post(routes::set_retention))
        .route("/api/server/storage", get(routes::get_storage).post(routes::set_storage_cap))
        .route("/api/server/invite", get(routes::get_invite).post(routes::set_invite))
        .route("/api/server/bot", get(routes::get_bot_settings).post(routes::set_bot_settings))
        .route("/api/server/icon", post(routes::set_server_icon))
        .route("/api/tags", get(routes::list_tags).post(routes::create_tag))
        .route("/api/tags/{id}", axum::routing::delete(routes::delete_tag))
        .route("/api/tags/assign", post(routes::assign_tag))
        .route("/api/search", get(routes::search))
        .route("/api/unread", get(routes::unread))
        .route("/api/read", post(routes::mark_read))
        .route("/download", get(routes::download_client))
        .route("/files/{name}", get(routes::serve_file_legacy))
        .route("/files/{id}/{name}", get(routes::serve_file))
        .route("/ws", any(ws::ws_handler))
        .with_state(state);

    std::fs::create_dir_all(uploads_dir())?;

    let addr = std::env::var("NOTDISCORD_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}, db at {db_path}");
    axum::serve(listener, app).await?;
    Ok(())
}
