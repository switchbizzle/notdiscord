//! Same-origin HTTP helpers: the web app is served by the NotDiscord server
//! itself, so every path is relative and the browser handles the host.

use gloo_net::http::{Request, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use shared::User;

/// Stored in localStorage under "nd_session".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub user: User,
}

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

async fn handle<T: DeserializeOwned>(resp: Response) -> Result<T, String> {
    if resp.ok() {
        resp.json().await.map_err(|e| format!("bad response: {e}"))
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err(format!("request failed ({})", resp.status())),
        }
    }
}

pub async fn login(username: String, password: String) -> Result<Session, String> {
    let resp = Request::post("/api/login")
        .json(&shared::LoginRequest { username, password })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let auth: shared::AuthResponse = handle(resp).await?;
    Ok(Session { token: auth.token, user: auth.user })
}

pub async fn register(username: String, password: String, invite: String) -> Result<Session, String> {
    let invite = Some(invite.trim().to_owned()).filter(|s| !s.is_empty());
    let resp = Request::post("/api/register")
        .json(&shared::RegisterRequest { username, password, invite })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let auth: shared::AuthResponse = handle(resp).await?;
    Ok(Session { token: auth.token, user: auth.user })
}

pub async fn get<T: DeserializeOwned>(session: &Session, path: &str) -> Result<T, String> {
    let resp = Request::get(&format!("/api/{path}"))
        .header("Authorization", &format!("Bearer {}", session.token))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle(resp).await
}

pub async fn me(session: &Session) -> Result<User, String> {
    get(session, "me").await
}

pub async fn channels(session: &Session) -> Result<Vec<shared::Channel>, String> {
    get(session, "channels").await
}

pub async fn users(session: &Session) -> Result<Vec<shared::UserStatus>, String> {
    get(session, "users").await
}

pub const HISTORY_PAGE: usize = 50;

pub async fn messages(
    session: &Session,
    channel_id: i64,
    before: Option<i64>,
) -> Result<Vec<shared::Message>, String> {
    let mut path = format!("channels/{channel_id}/messages?limit={HISTORY_PAGE}");
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    get(session, &path).await
}

pub async fn create_dm(session: &Session, user_id: i64) -> Result<shared::Channel, String> {
    let resp = Request::post("/api/dms")
        .header("Authorization", &format!("Bearer {}", session.token))
        .json(&shared::CreateDmRequest { user_id })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle(resp).await
}

/// The server's custom emojis, so `:name:` in a message can render as the
/// picture everyone else is seeing.
pub async fn emojis(session: &Session) -> Result<Vec<shared::CustomEmoji>, String> {
    get(session, "emojis").await
}

/// The channel groups an admin set up on the desktop, so the phone's channel
/// list is the same shape as everyone else's.
pub async fn categories(session: &Session) -> Vec<shared::ChannelCategory> {
    get(session, "categories").await.unwrap_or_default()
}

/// Role tags, for colouring names the way the desktop does.
pub async fn tags(session: &Session) -> Vec<shared::Tag> {
    get(session, "tags").await.unwrap_or_default()
}

/// The server's stickers. A sticker is sent as its plain URL, so the phone
/// could always SEE them — it just had no way to pick one.
pub async fn stickers(session: &Session) -> Vec<shared::Sticker> {
    get(session, "stickers").await.unwrap_or_default()
}

pub async fn unread(session: &Session) -> Result<Vec<shared::UnreadInfo>, String> {
    get(session, "unread").await
}

pub async fn mark_read(session: &Session, channel_id: i64, message_id: i64) {
    if let Ok(req) = Request::post("/api/read")
        .header("Authorization", &format!("Bearer {}", session.token))
        .json(&shared::MarkReadRequest { channel_id, message_id })
    {
        let _ = req.send().await;
    }
}

/// GIF search, proxied through the server so the GIPHY key never reaches a
/// phone. An empty query is "trending", which is what the sheet opens on.
pub async fn gifs(session: &Session, query: &str) -> Result<Vec<shared::GifResult>, String> {
    let q = js_sys::encode_uri_component(query).as_string().unwrap_or_default();
    get(session, &format!("gifs?q={q}")).await
}

/// Full-text message search. The server already scopes DM hits to rooms you
/// are in, so anything it returns is safe to show.
pub async fn search(session: &Session, query: &str) -> Result<Vec<shared::SearchResult>, String> {
    let q = js_sys::encode_uri_component(query).as_string().unwrap_or_default();
    get(session, &format!("search?q={q}")).await
}

/// Your own profile — the "You" tab reads the tags off this to show the
/// colour your name is drawn in.
pub async fn my_profile(session: &Session, user_id: i64) -> Result<shared::Profile, String> {
    get(session, &format!("users/{user_id}/profile")).await
}

/// The one-line status everyone sees next to your name. An empty string
/// clears it, which is what the server does with None.
pub async fn set_status(session: &Session, text: &str) -> Result<(), String> {
    let text = text.trim();
    let body = shared::SetStatusRequest {
        text: if text.is_empty() { None } else { Some(text.to_owned()) },
    };
    post_ok(session, "status", &body, "could not save your status").await
}

/// How you appear to everyone else: "online", "idle", "dnd" or "invisible".
pub async fn set_presence(session: &Session, mode: &str) -> Result<(), String> {
    let body = shared::SetPresenceRequest { mode: mode.to_owned() };
    post_ok(session, "presence", &body, "could not change your presence").await
}

/// Avatar and bio. Fields left None keep their current value.
pub async fn update_profile(
    session: &Session,
    avatar: Option<String>,
    bio: Option<String>,
) -> Result<(), String> {
    post_ok(session, "profile", &shared::UpdateProfileRequest { avatar, bio }, "could not save that")
        .await
}

pub async fn music_state(session: &Session) -> Result<shared::MusicState, String> {
    get(session, "music/state").await
}

pub async fn notify_prefs(session: &Session, endpoint: &str) -> Result<shared::NotifyPrefs, String> {
    let query = if endpoint.is_empty() {
        String::new()
    } else {
        format!("?endpoint={}", js_sys::encode_uri_component(endpoint).as_string().unwrap_or_default())
    };
    get(session, &format!("notify{query}")).await
}

pub async fn set_notify_level(session: &Session, level: &str) -> Result<(), String> {
    post_ok(session, "notify", &shared::SetNotifyLevel { level: level.into() }, "could not save that")
        .await
}

pub async fn push_subscribe(session: &Session, sub: shared::PushSubscribeRequest) -> Result<(), String> {
    post_ok(session, "push/subscribe", &sub, "could not turn notifications on").await
}

pub async fn push_unsubscribe(session: &Session, sub: shared::PushSubscribeRequest) -> Result<(), String> {
    let resp = Request::delete("/api/push/subscribe")
        .header("Authorization", &format!("Bearer {}", session.token))
        .json(&sub)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err("could not turn notifications off".into())
    }
}

pub async fn voice_token(session: &Session, channel_id: i64) -> Result<shared::VoiceTokenResponse, String> {
    get(session, &format!("voice/token?channel_id={channel_id}")).await
}

async fn post_ok(session: &Session, path: &str, body: &impl serde::Serialize, fallback: &str) -> Result<(), String> {
    let resp = Request::post(&format!("/api/{path}"))
        .header("Authorization", &format!("Bearer {}", session.token))
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        match resp.json::<ApiError>().await {
            Ok(e) => Err(e.error),
            Err(_) => Err(fallback.into()),
        }
    }
}

/// Queue a link (the bot announces it in the given channel).
pub async fn music_play(session: &Session, channel_id: i64, url: String) -> Result<(), String> {
    post_ok(session, "music/play", &shared::MusicPlayRequest { channel_id, url }, "could not queue that link").await
}

/// "pause", "resume", "skip", "stop".
pub async fn music_control(session: &Session, action: &str) -> Result<(), String> {
    post_ok(session, "music/control", &shared::MusicControlRequest { action: action.into() }, "control failed").await
}

pub async fn music_queue(session: &Session, req: shared::MusicQueueRequest) -> Result<(), String> {
    post_ok(session, "music/queue", &req, "queue edit failed").await
}

/// Raw-bytes upload; returns the server-relative /files/ URL.
pub async fn upload(session: &Session, name: &str, bytes: Vec<u8>) -> Result<String, String> {
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let resp = Request::post(&format!("/api/upload?name={}", urlencode(name)))
        .header("Authorization", &format!("Bearer {}", session.token))
        .body(array)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let out: shared::UploadResponse = handle(resp).await?;
    Ok(out.url)
}

fn urlencode(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_else(|| s.to_owned())
}

/// The server's card for a link, or None when it hasn't got one. Failures are
/// silent: a missing preview just means no card, never an error in the chat.
/// Everything in the card was fetched by the server, so opening a channel
/// never tells the linked site who is reading it.
pub async fn link_preview(session: &Session, url: &str) -> Option<shared::LinkPreview> {
    let encoded = js_sys::encode_uri_component(url).as_string()?;
    let resp = Request::get(&format!("/api/preview?url={encoded}"))
        .header("Authorization", &format!("Bearer {}", session.token))
        .send()
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json().await.ok()
}

/// Public: what this server calls itself, and whether anyone has claimed it.
pub async fn server_info() -> Option<shared::ServerInfo> {
    Request::get("/api/server/info").send().await.ok()?.json().await.ok()
}

/// First run: claim a server nobody has an account on. Refused once one does.
pub async fn setup(req: shared::SetupRequest) -> Result<Session, String> {
    let resp = Request::post("/api/setup")
        .json(&req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let auth: shared::AuthResponse = handle(resp).await?;
    Ok(Session { token: auth.token, user: auth.user })
}
