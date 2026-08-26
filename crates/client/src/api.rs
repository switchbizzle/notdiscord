//! REST calls to the NotDiscord server.

use serde::{Deserialize, Serialize};
use shared::{ApiError, AuthResponse, Channel, ClientVersionInfo, CreateChannelRequest, CreateDmRequest, CreateStickerRequest, GifResult, LoginRequest, Message, Profile, RegisterRequest, SetBanRequest, SetRoleRequest, Sticker, UpdateProfileRequest, UploadResponse, User, UserStatus, VoiceTokenResponse};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub base_url: String,
    pub token: String,
    pub user: User,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub server_icon: Option<String>,
}

fn default_server_name() -> String {
    "NotDiscord".into()
}

/// All saved server accounts; `active` indexes into `servers`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ServersFile {
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub servers: Vec<Session>,
}

impl ServersFile {
    pub fn active_session(&self) -> Option<&Session> {
        self.servers.get(self.active)
    }
}

fn servers_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("NotDiscord").join("servers.json"))
}

pub fn load_servers() -> ServersFile {
    if let Some(file) = servers_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<ServersFile>(&text).ok())
    {
        return file;
    }
    // Migrate the old single-session file.
    match load_session() {
        Some(session) => {
            let file = ServersFile { active: 0, servers: vec![session] };
            save_servers(&file);
            file
        }
        None => ServersFile::default(),
    }
}

pub fn save_servers(file: &ServersFile) {
    let Some(path) = servers_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string_pretty(file) {
        let _ = std::fs::write(path, json);
    }
}

/// Insert or replace (by base_url) and make active.
pub fn upsert_server(session: Session) -> ServersFile {
    let mut file = load_servers();
    match file.servers.iter().position(|s| s.base_url == session.base_url) {
        Some(i) => {
            file.servers[i] = session;
            file.active = i;
        }
        None => {
            file.servers.push(session);
            file.active = file.servers.len() - 1;
        }
    }
    save_servers(&file);
    file
}

/// Update the saved copy of this session (matched by base_url) in place.
pub fn update_saved_server(session: &Session) {
    let mut file = load_servers();
    if let Some(entry) = file.servers.iter_mut().find(|s| s.base_url == session.base_url) {
        *entry = session.clone();
        save_servers(&file);
    }
}

pub fn remove_server(index: usize) -> ServersFile {
    let mut file = load_servers();
    if index < file.servers.len() {
        file.servers.remove(index);
    }
    if file.active >= file.servers.len() {
        file.active = file.servers.len().saturating_sub(1);
    }
    save_servers(&file);
    file
}

pub async fn server_info(base_url: &str) -> Result<shared::ServerInfo, String> {
    let base = normalize_base(base_url);
    let resp = send_retry(http()
        .get(format!("{base}/api/server/info")))
        .await?;
    handle(resp).await
}

pub async fn search(session: &Session, query: &str) -> Result<Vec<shared::SearchResult>, String> {
    let resp = send_retry(http()
        .get(format!("{}/api/search", session.base_url))
        .query(&[("q", query)])
        .bearer_auth(&session.token))
        .await?;
    handle(resp).await
}

pub async fn tags(session: &Session) -> Result<Vec<shared::Tag>, String> {
    get(session, "tags".into()).await
}

pub async fn create_tag(session: &Session, name: String, color: String) -> Result<shared::Tag, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/tags", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::CreateTagRequest { name, color }))
        .await?;
    handle(resp).await
}

pub async fn delete_tag(session: &Session, tag_id: i64) -> Result<(), String> {
    let resp = send_retry(http()
        .delete(format!("{}/api/tags/{tag_id}", session.base_url))
        .bearer_auth(&session.token))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
}

pub async fn assign_tag(session: &Session, user_id: i64, tag_id: i64, assigned: bool) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/tags/assign", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::AssignTagRequest { user_id, tag_id, assigned }))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
}

pub async fn get_retention(session: &Session) -> Result<shared::RetentionSetting, String> {
    get(session, "server/retention".into()).await
}

pub async fn set_retention(session: &Session, days: i64) -> Result<shared::RetentionSetting, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/retention", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::RetentionSetting { days }))
        .await?;
    handle(resp).await
}

pub async fn rename_server(session: &Session, name: String) -> Result<shared::ServerInfo, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/name", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::RenameServerRequest { name }))
        .await?;
    handle(resp).await
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

// ---------- Local app settings ----------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Audio device names; None = system default.
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
    /// Per-participant playback volume (1.0 = 100%), keyed by voice identity.
    #[serde(default)]
    pub volumes: std::collections::HashMap<String, f32>,
    /// Own microphone gain (1.0 = 100%).
    #[serde(default = "one")]
    pub input_volume: f32,
    /// Master voice output gain (1.0 = 100%).
    #[serde(default = "one")]
    pub output_volume: f32,
    /// RNNoise ML noise suppression on the microphone.
    #[serde(default = "yes")]
    pub noise_suppression: bool,
    /// "vad" (voice activity) or "ptt" (push to talk).
    #[serde(default = "vad")]
    pub voice_mode: String,
    /// Push-to-talk key name (device_query Keycode).
    #[serde(default = "default_ptt_key")]
    pub ptt_key: String,
    /// Play the notification sound on pings.
    #[serde(default = "yes")]
    pub notification_sounds: bool,
    /// Last client version whose changelog the user has seen.
    #[serde(default)]
    pub last_seen_version: Option<String>,
}

fn vad() -> String {
    "vad".into()
}

fn default_ptt_key() -> String {
    "F9".into()
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

fn settings_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("NotDiscord").join("settings.json"))
}

pub fn load_settings() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| Settings {
            input_volume: 1.0,
            output_volume: 1.0,
            noise_suppression: true,
            voice_mode: vad(),
            ptt_key: default_ptt_key(),
            notification_sounds: true,
            ..Default::default()
        })
}

pub fn save_settings(settings: &Settings) {
    let Some(path) = settings_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

/// Shared HTTP client with a connect timeout so flaky networks fail fast
/// instead of hanging.
fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("http client")
    })
}

/// Send with up to 3 attempts on connection-level failures (request never
/// reached the server), with short backoff. HTTP error statuses are not
/// retried â€” those reached the server and got an answer.
async fn send_retry(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
    let mut delay_ms = 400u64;
    for attempt in 0..3u32 {
        let Some(cloned) = builder.try_clone() else { break };
        match cloned.send().await {
            Ok(resp) => return Ok(resp),
            Err(e) if attempt < 2 && (e.is_connect() || e.is_timeout() || e.is_request()) => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }
            Err(e) => return Err(format!("cannot reach server: {e}")),
        }
    }
    builder.send().await.map_err(|e| format!("cannot reach server: {e}"))
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
    let resp = send_retry(http()
        .post(format!("{base}/api/{path}"))
        .json(&body))
        .await?;
    handle(resp).await
}

async fn session_from_auth(base_url: &str, auth: AuthResponse) -> Session {
    let info = server_info(base_url).await.ok();
    Session {
        base_url: normalize_base(base_url),
        token: auth.token,
        user: auth.user,
        server_name: info.as_ref().map(|i| i.name.clone()).unwrap_or_else(default_server_name),
        server_icon: info.as_ref().and_then(|i| i.icon.clone()),
        server_id: info.map(|i| i.id).unwrap_or_default(),
    }
}

pub async fn login(base_url: &str, username: String, password: String) -> Result<Session, String> {
    let auth = auth_request(base_url, "login", LoginRequest { username, password }).await?;
    Ok(session_from_auth(base_url, auth).await)
}

pub async fn register(base_url: &str, username: String, password: String, invite: String) -> Result<Session, String> {
    let invite = Some(invite.trim().to_owned()).filter(|s| !s.is_empty());
    let auth = auth_request(base_url, "register", RegisterRequest { username, password, invite }).await?;
    Ok(session_from_auth(base_url, auth).await)
}

async fn get<T: serde::de::DeserializeOwned>(session: &Session, path: String) -> Result<T, String> {
    let resp = send_retry(http()
        .get(format!("{}/api/{path}", session.base_url))
        .bearer_auth(&session.token))
        .await?;
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

pub async fn stickers(session: &Session) -> Result<Vec<Sticker>, String> {
    get(session, "stickers".into()).await
}

pub async fn create_sticker(session: &Session, name: String, url: String) -> Result<Sticker, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/stickers", session.base_url))
        .bearer_auth(&session.token)
        .json(&CreateStickerRequest { name, url }))
        .await?;
    handle(resp).await
}

pub async fn delete_sticker(session: &Session, sticker_id: i64) -> Result<(), String> {
    let resp = send_retry(http()
        .delete(format!("{}/api/stickers/{sticker_id}", session.base_url))
        .bearer_auth(&session.token))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
}

pub async fn create_dm(session: &Session, user_id: i64) -> Result<Channel, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/dms", session.base_url))
        .bearer_auth(&session.token)
        .json(&CreateDmRequest { user_id }))
        .await?;
    handle(resp).await
}

pub async fn set_role(session: &Session, user_id: i64, role: &str) -> Result<User, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/users/{user_id}/role", session.base_url))
        .bearer_auth(&session.token)
        .json(&SetRoleRequest { role: role.into() }))
        .await?;
    handle(resp).await
}

pub async fn set_ban(session: &Session, user_id: i64, banned: bool) -> Result<User, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/users/{user_id}/ban", session.base_url))
        .bearer_auth(&session.token)
        .json(&SetBanRequest { banned }))
        .await?;
    handle(resp).await
}

pub async fn delete_channel(session: &Session, channel_id: i64) -> Result<(), String> {
    let resp = send_retry(http()
        .delete(format!("{}/api/channels/{channel_id}", session.base_url))
        .bearer_auth(&session.token))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
}

pub async fn changelog(session: &Session) -> Result<Vec<shared::ChangelogEntry>, String> {
    let resp = send_retry(http()
        .get(format!("{}/api/changelog", session.base_url)))
        .await?;
    handle(resp).await
}

pub async fn client_version(session: &Session) -> Result<ClientVersionInfo, String> {
    let resp = send_retry(http()
        .get(format!("{}/api/client/version", session.base_url)))
        .await?;
    handle(resp).await
}

pub async fn profile(session: &Session, user_id: i64) -> Result<Profile, String> {
    get(session, format!("users/{user_id}/profile")).await
}

pub async fn update_profile(session: &Session, req: UpdateProfileRequest) -> Result<Profile, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/profile", session.base_url))
        .bearer_auth(&session.token)
        .json(&req))
        .await?;
    handle(resp).await
}

pub async fn voice_token(session: &Session, channel_id: i64) -> Result<VoiceTokenResponse, String> {
    get(session, format!("voice/token?channel_id={channel_id}")).await
}

pub async fn gifs(session: &Session, query: &str) -> Result<Vec<GifResult>, String> {
    let resp = send_retry(http()
        .get(format!("{}/api/gifs", session.base_url))
        .query(&[("q", query)])
        .bearer_auth(&session.token))
        .await?;
    handle(resp).await
}

/// Upload an image; returns the absolute URL to embed in a message.
pub async fn upload(session: &Session, filename: &str, bytes: Vec<u8>) -> Result<String, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/upload", session.base_url))
        .query(&[("name", filename)])
        .bearer_auth(&session.token)
        .body(bytes))
        .await?;
    let uploaded: UploadResponse = handle(resp).await?;
    Ok(format!("{}{}", session.base_url, uploaded.url))
}

pub async fn create_channel(session: &Session, name: String, kind: &str) -> Result<Channel, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/channels", session.base_url))
        .bearer_auth(&session.token)
        .json(&CreateChannelRequest { name, kind: Some(kind.into()) }))
        .await?;
    handle(resp).await
}

pub async fn set_server_icon(session: &Session, url: String) -> Result<shared::ServerInfo, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/icon", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::SetServerIconRequest { url }))
        .await?;
    handle(resp).await
}
