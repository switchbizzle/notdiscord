//! REST calls to the NotDiscord server.

use shared::{ApiError, AuthResponse, Channel, CreateChannelRequest, LoginRequest, Message, RegisterRequest, User};

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub base_url: String,
    pub token: String,
    pub user: User,
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

pub async fn register(base_url: &str, username: String, password: String) -> Result<Session, String> {
    let auth = auth_request(base_url, "register", RegisterRequest { username, password }).await?;
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

pub async fn channels(session: &Session) -> Result<Vec<Channel>, String> {
    get(session, "channels".into()).await
}

pub async fn messages(session: &Session, channel_id: i64) -> Result<Vec<Message>, String> {
    get(session, format!("channels/{channel_id}/messages")).await
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
