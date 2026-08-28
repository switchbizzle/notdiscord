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

/// Where config lives: %APPDATA%\NotDiscord, or NOTDISCORD_CONFIG_DIR when
/// set (portable installs, and running a second instance for testing).
pub fn config_root() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("NOTDISCORD_CONFIG_DIR") {
        return Some(dir.into());
    }
    dirs::config_dir().map(|d| d.join("NotDiscord"))
}

fn servers_path() -> Option<std::path::PathBuf> {
    config_root().map(|d| d.join("servers.json"))
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

pub async fn get_invite(session: &Session) -> Result<shared::InviteSetting, String> {
    get(session, "server/invite".into()).await
}

pub async fn set_invite(session: &Session, code: String) -> Result<shared::InviteSetting, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/invite", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::InviteSetting { code }))
        .await?;
    handle(resp).await
}

pub async fn get_bot_settings(session: &Session) -> Result<shared::BotSettings, String> {
    get(session, "server/bot".into()).await
}

pub async fn set_bot_settings(session: &Session, update: shared::BotSettingsUpdate) -> Result<shared::BotSettings, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/bot", session.base_url))
        .bearer_auth(&session.token)
        .json(&update))
        .await?;
    handle(resp).await
}

pub async fn get_storage(session: &Session) -> Result<shared::StorageInfo, String> {
    get(session, "server/storage".into()).await
}

pub async fn set_storage_cap(session: &Session, cap_gb: i64) -> Result<shared::StorageInfo, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/server/storage", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::StorageCapSetting { cap_gb }))
        .await?;
    handle(resp).await
}

/// The server's card for a link, or None when it hasn't got one. Failures are
/// silent: a missing preview just means no card, never an error in the UI.
pub async fn link_preview(session: &Session, url: &str) -> Option<shared::LinkPreview> {
    let resp = send_retry(http()
        .get(format!("{}/api/preview", session.base_url))
        .query(&[("url", url)])
        .bearer_auth(&session.token))
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut preview: shared::LinkPreview = resp.json().await.ok()?;
    // Thumbnails come back server-relative.
    if let Some(image) = preview.image.take() {
        preview.image = Some(format!("{}{image}", session.base_url));
    }
    Some(preview)
}

pub async fn change_password(session: &Session, current: String, new: String) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/password", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::ChangePasswordRequest { current, new }))
        .await?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err(format!("request failed ({status})")),
        }
    }
}

// ---------- Local session persistence ----------

fn session_path() -> Option<std::path::PathBuf> {
    config_root().map(|d| d.join("session.json"))
}

/// Append a line to debug.log next to the settings. Audio routing problems
/// are invisible from the UI, so the sound paths leave a breadcrumb trail.
pub fn debug_log(line: &str) {
    let Some(path) = config_root().map(|d| d.join("debug.log")) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let stamp = chrono::Local::now().format("%H:%M:%S%.3f");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{stamp}] {line}");
    }
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
    /// Automatic mic gain: slowly normalizes quiet mics toward a
    /// comfortable speech level, like every other voice app.
    #[serde(default = "yes")]
    pub auto_gain: bool,
    /// "vad" (voice activity) or "ptt" (push to talk).
    #[serde(default = "vad")]
    pub voice_mode: String,
    /// Voice-activity gate threshold, RMS of i16 samples (0 = always transmit).
    #[serde(default = "default_vad_threshold")]
    pub vad_threshold: f32,
    /// Blip when someone joins/leaves your voice channel.
    #[serde(default = "yes")]
    pub voice_join_sounds: bool,
    /// Push-to-talk key name (device_query Keycode).
    #[serde(default = "default_ptt_key")]
    pub ptt_key: String,
    /// Play the notification sound on pings.
    #[serde(default = "yes")]
    pub notification_sounds: bool,
    /// Pop a toast for @mentions and DMs you aren't currently reading.
    #[serde(default = "yes")]
    pub ping_toasts: bool,
    /// Last client version whose changelog the user has seen.
    #[serde(default)]
    pub last_seen_version: Option<String>,
    /// Sidebar DM section folded away (unread DMs still surface).
    #[serde(default)]
    pub dms_collapsed: bool,
}

fn vad() -> String {
    "vad".into()
}

fn default_ptt_key() -> String {
    "F9".into()
}

fn default_vad_threshold() -> f32 {
    350.0
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

fn settings_path() -> Option<std::path::PathBuf> {
    config_root().map(|d| d.join("settings.json"))
}

/// Where the window was last time. Kept out of settings.json because it's
/// written on resize, and settings are written on user actions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f64,
    pub height: f64,
    /// Screen position; None restores to wherever the OS puts it.
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub maximized: bool,
}

fn window_path() -> Option<std::path::PathBuf> {
    config_root().map(|d| d.join("window.json"))
}

pub fn load_window() -> Option<WindowState> {
    let state: WindowState = window_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())?;
    // A window smaller than the minimum, or absurdly large, means a corrupt
    // file or a monitor that no longer exists — start fresh instead.
    if state.width < 400.0 || state.height < 300.0 || state.width > 20000.0 || state.height > 20000.0 {
        return None;
    }
    Some(state)
}

pub fn save_window(state: &WindowState) {
    let Some(path) = window_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}

pub fn load_settings() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| Settings {
            input_volume: 1.0,
            output_volume: 1.0,
            noise_suppression: true,
            auto_gain: true,
            voice_mode: vad(),
            ptt_key: default_ptt_key(),
            vad_threshold: default_vad_threshold(),
            notification_sounds: true,
            ping_toasts: true,
            voice_join_sounds: true,
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

/// First run: claim a server nobody has an account on. The server refuses
/// once one does, so this is only ever offered when it says it needs setting
/// up.
pub async fn setup(
    base_url: &str,
    username: String,
    password: String,
    server_name: String,
    invite: String,
) -> Result<Session, String> {
    let req = shared::SetupRequest {
        username,
        password,
        server_name,
        invite: Some(invite.trim().to_owned()).filter(|s| !s.is_empty()),
    };
    let auth = auth_request(base_url, "setup", req).await?;
    Ok(session_from_auth(base_url, auth).await)
}

/// Success is 204 with no body; failures carry a JSON error message.
async fn expect_no_content(resp: reqwest::Response, fallback: &str) -> Result<(), String> {
    if resp.status().is_success() {
        Ok(())
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err(fallback.into()),
        }
    }
}

pub async fn set_status(session: &Session, text: Option<String>) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/status", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::SetStatusRequest { text }))
        .await?;
    expect_no_content(resp, "could not update your status").await
}

pub async fn email_status(session: &Session) -> Result<shared::EmailStatus, String> {
    get(session, "email".into()).await
}

pub async fn email_request(session: &Session, email: String) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/email", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::EmailRequest { email }))
        .await?;
    expect_no_content(resp, "could not send the code").await
}

pub async fn email_verify(session: &Session, code: String) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/email/verify", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::EmailVerifyRequest { code }))
        .await?;
    expect_no_content(resp, "could not verify the code").await
}

pub async fn forgot_password(base_url: &str, username: String) -> Result<(), String> {
    let base = normalize_base(base_url);
    let resp = send_retry(http()
        .post(format!("{base}/api/password/forgot"))
        .json(&shared::ForgotPasswordRequest { username }))
        .await?;
    expect_no_content(resp, "could not request a reset code").await
}

pub async fn reset_password(
    base_url: &str,
    username: String,
    code: String,
    new_password: String,
) -> Result<(), String> {
    let base = normalize_base(base_url);
    let resp = send_retry(http()
        .post(format!("{base}/api/password/reset"))
        .json(&shared::ResetPasswordRequest { username, code, new_password }))
        .await?;
    expect_no_content(resp, "could not reset the password").await
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

pub async fn set_pinned(session: &Session, message_id: i64, pinned: bool) -> Result<(), String> {
    let url = format!("{}/api/messages/{message_id}/pin", session.base_url);
    let req = if pinned { http().post(url) } else { http().delete(url) };
    let resp = send_retry(req.bearer_auth(&session.token)).await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err("could not update the pin".into()),
        }
    }
}

pub async fn channel_pins(session: &Session, channel_id: i64) -> Result<Vec<Message>, String> {
    get(session, format!("channels/{channel_id}/pins")).await
}

pub async fn channel_files(session: &Session, channel_id: i64) -> Result<Vec<shared::FileEntry>, String> {
    get(session, format!("channels/{channel_id}/files")).await
}

/// Queue a link without posting it to chat (the Music tab's composer).
pub async fn music_play(session: &Session, channel_id: i64, url: String) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/music/play", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::MusicPlayRequest { channel_id, url }))
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err("could not queue that link".into())
    }
}

pub async fn music_state(session: &Session) -> Result<shared::MusicState, String> {
    get(session, "music/state".into()).await
}

/// Transport buttons: "pause", "resume", "skip", "stop".
pub async fn music_control(session: &Session, action: &str) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/music/control", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::MusicControlRequest { action: action.to_owned() }))
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err("nothing is playing".into())
    }
}

pub async fn music_queue(session: &Session, req: shared::MusicQueueRequest) -> Result<(), String> {
    let resp = send_retry(http()
        .post(format!("{}/api/music/queue", session.base_url))
        .bearer_auth(&session.token)
        .json(&req))
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err("couldn't update the queue".into())
    }
}

pub async fn unread(session: &Session) -> Result<Vec<shared::UnreadInfo>, String> {
    get(session, "unread".into()).await
}

/// Tell the server we've read `channel_id` up to `message_id`. Fire and
/// forget — a missed mark just means the badge lingers until next time.
pub async fn mark_read(session: &Session, channel_id: i64, message_id: i64) {
    let _ = send_retry(http()
        .post(format!("{}/api/read", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::MarkReadRequest { channel_id, message_id }))
        .await;
}

pub async fn emojis(session: &Session) -> Result<Vec<shared::CustomEmoji>, String> {
    let mut list: Vec<shared::CustomEmoji> = get(session, "emojis".into()).await?;
    // Server-relative paths would resolve against the webview's own origin,
    // so anchor them to this server.
    for emoji in &mut list {
        if emoji.url.starts_with('/') {
            emoji.url = format!("{}{}", session.base_url, emoji.url);
        }
    }
    Ok(list)
}

pub async fn create_emoji(session: &Session, name: String, url: String) -> Result<shared::CustomEmoji, String> {
    let resp = send_retry(http()
        .post(format!("{}/api/emojis", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::CreateEmojiRequest { name, url }))
        .await?;
    handle(resp).await
}

pub async fn delete_emoji(session: &Session, emoji_id: i64) -> Result<(), String> {
    let resp = send_retry(http()
        .delete(format!("{}/api/emojis/{emoji_id}", session.base_url))
        .bearer_auth(&session.token))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
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

pub async fn rename_channel(session: &Session, channel_id: i64, name: String) -> Result<(), String> {
    let resp = send_retry(http()
        .patch(format!("{}/api/channels/{channel_id}", session.base_url))
        .bearer_auth(&session.token)
        .json(&shared::RenameChannelRequest { name }))
        .await?;
    let _: serde_json::Value = handle(resp).await?;
    Ok(())
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
