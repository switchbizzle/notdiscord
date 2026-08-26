//! REST calls to the NotDiscord server.

use serde::{Deserialize, Serialize};
use shared::{ApiError, AuthResponse, Channel, CreateChannelRequest, GifResult, LoginRequest, Message, RegisterRequest, UploadResponse, User, UserStatus, VoiceTokenResponse};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub base_url: String,
    pub token: String,
    pub user: User,
}

// ---------- Local session persistence ----------

fn session_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("NotDiscord").join("session.json"))
}

pub fn save_session(session: &Session) {
    let Some(path) = session_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string(session) {
        let _ = std::fs::write(path, json);
    }
}

pub fn load_session() -> Option<Session> {
    let text = std::fs::read_to_string(session_path()?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_session() {
    if let Some(path) = session_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn normalize_base(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.starts_with("http://") || base.starts_with("https://") {
        base.to_owned()
    } else {
        format!("http://{base}")
    }
}

pub fn ws_url(session: &Session) -> String {
    let base = session
        .base_url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    format!("{base}/ws?token={}", session.token)
}

async fn handle<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
    let status = resp.status();
    if status.is_success() {
        resp.json().await.map_err(|e| format!("bad response: {e}"))
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err(format!("request failed ({status})")),
        }
    }
}

async fn auth_request(base_url: &str, path: &str, body: impl serde::Serialize) -> Result<AuthResponse, String> {
    let base = normalize_base(base_url);
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/{path}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("cannot reach server: {e}"))?;
    handle(resp).await
}

pub async fn login(base_url: &str, username: String, password: String) -> Result<Session, String> {
    let auth = auth_request(base_url, "login", LoginRequest { username, password }).await?;
    Ok(Session { base_url: normalize_base(base_url), token: auth.token, user: auth.user })
}

pub async fn register(base_url: &str, username: String, password: String, invite: String) -> Result<Session, String> {
    let invite = Some(invite.trim().to_owned()).filter(|s| !s.is_empty());
    let auth = auth_request(base_url, "register", RegisterRequest { username, password, invite }).await?;
    Ok(Session { base_url: normalize_base(base_url), token: auth.token, user: auth.user })
}

async fn get<T: serde::de::DeserializeOwned>(session: &Session, path: String) -> Result<T, String> {
    let resp = reqwest::Client::new()
        .get(format!("{}/api/{path}", session.base_url))
        .bearer_auth(&session.token)
        .send()
        .await
        .map_err(|e| format!("cannot reach server: {e}"))?;
    handle(resp).await
}

pub async fn me(session: &Session) -> Result<User, String> {
    get(session, "me".into()).await
}

pub async fn users(session: &Session) -> Result<Vec<UserStatus>, String> {
    get(session, "users".into()).await
}

pub async fn channels(session: &Session) -> Result<Vec<Channel>, String> {
    get(session, "channels".into()).await
}

pub const HISTORY_PAGE: usize = 50;

pub async fn messages(session: &Session, channel_id: i64, before: Option<i64>) -> Result<Vec<Message>, String> {
    let mut path = format!("channels/{channel_id}/messages?limit={HISTORY_PAGE}");
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    get(session, path).await
}

pub async fn voice_token(session: &Session, channel_id: i64) -> Result<VoiceTokenResponse, String> {
    get(session, format!("voice/token?channel_id={channel_id}")).await
}

pub async fn gifs(session: &Session, query: &str) -> Result<Vec<GifResult>, String> {
    let resp = reqwest::Client::new()
        .get(format!("{}/api/gifs", session.base_url))
        .query(&[("q", query)])
        .bearer_auth(&session.token)
        .send()
        .await
        .map_err(|e| format!("cannot reach server: {e}"))?;
    handle(resp).await
}

/// Upload an image; returns the absolute URL to embed in a message.
pub async fn upload(session: &Session, filename: &str, bytes: Vec<u8>) -> Result<String, String> {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/upload", session.base_url))
        .query(&[("name", filename)])
        .bearer_auth(&session.token)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("cannot reach server: {e}"))?;
    let uploaded: UploadResponse = handle(resp).await?;
    Ok(format!("{}{}", session.base_url, uploaded.url))
}

pub async fn create_channel(session: &Session, name: String) -> Result<Channel, String> {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/channels", session.base_url))
        .bearer_auth(&session.token)
        .json(&CreateChannelRequest { name })
        .send()
        .await
        .map_err(|e| format!("cannot reach server: {e}"))?;
    handle(resp).await
}
