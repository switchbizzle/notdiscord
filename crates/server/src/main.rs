mod auth;
mod routes;
mod ws;

use std::sync::Arc;

use axum::routing::{any, get, post};
use axum::Router;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use shared::ServerEvent;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub events: broadcast::Sender<ServerEvent>,
}

pub type SharedState = Arc<AppState>;

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

    let (events, _) = broadcast::channel(256);
    let state = Arc::new(AppState { db, events });

    let app = Router::new()
        .route("/api/register", post(routes::register))
        .route("/api/login", post(routes::login))
        .route("/api/channels", get(routes::list_channels).post(routes::create_channel))
        .route("/api/channels/{id}/messages", get(routes::channel_messages))
        .route("/ws", any(ws::ws_handler))
        .with_state(state);

    let addr = std::env::var("NOTDISCORD_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}, db at {db_path}");
    axum::serve(listener, app).await?;
    Ok(())
}
