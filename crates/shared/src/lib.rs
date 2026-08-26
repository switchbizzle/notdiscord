//! Types shared between the NotDiscord server and client: REST DTOs and the
//! WebSocket event protocol. Timestamps are unix milliseconds (UTC).

use serde::{Deserialize, Serialize};

// ---------- Core entities ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    /// URL of the user's avatar image, if set.
    #[serde(default)]
    pub avatar: Option<String>,
    /// "admin" or "member".
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "member".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub user: User,
    pub bio: String,
    pub created_at: i64,
    #[serde(default)]
    pub banned: bool,
    #[serde(default)]
    pub tags: Vec<Tag>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoleRequest {
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetBanRequest {
    pub banned: bool,
}

/// Fields are applied only when Some; None leaves the current value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    /// "text", "voice", or "dm".
    #[serde(default = "default_channel_kind")]
    pub kind: String,
    /// For dm channels: both participants. Empty otherwise.
    #[serde(default)]
    pub dm_members: Vec<User>,
}

/// Body for POST /api/dms — open (or find) a DM with another user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDmRequest {
    pub user_id: i64,
}

fn default_channel_kind() -> String {
    "text".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: i64,
    pub author: User,
    pub content: String,
    pub created_at: i64,
    #[serde(default)]
    pub edited_at: Option<i64>,
    #[serde(default)]
    pub reactions: Vec<ReactionEntry>,
    /// Id of the message this one replies to, if any.
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// Author + content of the replied-to message, for rendering.
    #[serde(default)]
    pub reply_preview: Option<ReplyPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyPreview {
    pub author: String,
    pub content: String,
}

/// One user's reaction on a message; the client aggregates these per emoji.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionEntry {
    pub emoji: String,
    pub user_id: i64,
}

/// A user plus their current connection state, as returned by `/api/users`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStatus {
    pub user: User,
    pub online: bool,
    #[serde(default)]
    pub banned: bool,
    /// Ids of custom tags assigned to this user.
    #[serde(default)]
    pub tag_ids: Vec<i64>,
}

/// A custom cosmetic role: a named, colored badge admins assign to users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    /// "#rrggbb"
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTagRequest {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignTagRequest {
    pub user_id: i64,
    pub tag_id: i64,
    pub assigned: bool,
}

// ---------- REST request/response bodies ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    /// Required when the server sets NOTDISCORD_INVITE.
    #[serde(default)]
    pub invite: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current: String,
    pub new: String,
}

/// Why a password isn't acceptable, or None if it is. Shared so the client
/// can show the problem live and the server can enforce the same rules.
pub fn password_problem(password: &str, username: &str) -> Option<&'static str> {
    if password.len() < 8 {
        return Some("password must be at least 8 characters");
    }
    if password.len() > 128 {
        return Some("password must be at most 128 characters");
    }
    let has_letter = password.chars().any(|c| c.is_alphabetic());
    let has_other = password.chars().any(|c| !c.is_alphabetic());
    if !has_letter || !has_other {
        return Some("password needs at least one letter and one number or symbol");
    }
    let lower = password.to_lowercase();
    if username.len() >= 3 && lower.contains(&username.to_lowercase()) {
        return Some("password can't contain your username");
    }
    const COMMON: &[&str] = &[
        "password", "password1", "password123", "12345678", "123456789", "1234567890",
        "qwerty123", "qwertyuiop", "iloveyou1", "letmein1", "welcome1", "admin123",
        "abc12345", "trustno1", "sunshine1", "princess1", "football1", "baseball1",
        "dragon123", "monkey123", "master123", "shadow123", "superman1", "notdiscord",
    ];
    if COMMON.contains(&lower.as_str()) {
        return Some("that password is too common — pick something less guessable");
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    /// "text" (default) or "voice".
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// Response from POST /api/upload; `url` is server-relative (e.g. `/files/ab12….gif`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadResponse {
    pub url: String,
}

/// Response from GET /api/voice/token: credentials to join a LiveKit voice room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTokenResponse {
    /// LiveKit server URL (wss://…).
    pub url: String,
    /// Access JWT scoped to the room.
    pub token: String,
    /// Room name (derived from the channel).
    pub room: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub creator_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateStickerRequest {
    pub name: String,
    /// URL previously returned by /api/upload.
    pub url: String,
}

/// Public identity of a NotDiscord instance (GET /api/server/info).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Stable unique id generated at the instance's first boot.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetServerIconRequest {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameServerRequest {
    pub name: String,
}

/// How long chat uploads are kept before expiry (avatars/stickers exempt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionSetting {
    pub days: i64,
}

/// The invite code newcomers need to register (GET/POST /api/server/invite,
/// admin only). Empty = registration is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteSetting {
    pub code: String,
}

/// The bot's personality — the editable part of its system prompt
/// (GET/POST /api/server/bot, admin only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotPersonaSetting {
    pub persona: String,
}

/// Upload storage usage and cap (GET /api/server/storage).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageInfo {
    pub used_bytes: i64,
    pub cap_gb: i64,
}

/// Body for POST /api/server/storage (admin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCapSetting {
    pub cap_gb: i64,
}

/// A server-fetched card for a link posted in chat (GET /api/preview?url=…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPreview {
    pub url: String,
    pub title: String,
    pub description: String,
    /// Server-relative path to the cached thumbnail (`/files/…`). The client
    /// never fetches the remote image itself, so nobody's IP leaks to the site.
    #[serde(default)]
    pub image: Option<String>,
    pub site_name: String,
    /// Inline player URL for hosts we know how to embed (SoundCloud, YouTube,
    /// Spotify, Vimeo), if any.
    #[serde(default)]
    pub embed: Option<String>,
    /// Player height in pixels; only meaningful when `embed` is set.
    #[serde(default)]
    pub embed_height: i64,
}

/// One release's entry in GET /api/changelog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub version: String,
    #[serde(default)]
    pub date: String,
    pub changes: Vec<String>,
}

/// Response from GET /api/client/version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientVersionInfo {
    pub version: String,
    /// Server-relative download path for the current client build.
    pub url: String,
}

/// One message-search hit from GET /api/search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub message: Message,
    pub channel_name: String,
    pub channel_kind: String,
}

/// One GIF search result from GET /api/gifs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifResult {
    /// Small animated preview for the picker grid.
    pub preview: String,
    /// Full-size URL to embed in the message.
    pub url: String,
}

// ---------- WebSocket protocol ----------

/// Events sent from client to server over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    SendMessage {
        channel_id: i64,
        content: String,
        #[serde(default)]
        reply_to: Option<i64>,
    },
    /// Sent (throttled) while the user is typing in a channel.
    Typing { channel_id: i64 },
    /// Add the reaction if the user hasn't reacted with this emoji, else remove it.
    ToggleReaction { message_id: i64, emoji: String },
    /// Edit own message.
    EditMessage { message_id: i64, content: String },
    /// Delete own message.
    DeleteMessage { message_id: i64 },
    /// Announce which voice channel this user is in (None = left voice).
    VoiceState {
        channel_id: Option<i64>,
        #[serde(default)]
        sharing: bool,
        #[serde(default)]
        camera: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStateEntry {
    pub channel_id: i64,
    pub user: User,
    #[serde(default)]
    pub sharing: bool,
    #[serde(default)]
    pub camera: bool,
}

/// Events pushed from server to all connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    MessageCreated { message: Message },
    ChannelCreated { channel: Channel },
    PresenceChanged { user: User, online: bool },
    /// A user's profile (avatar/name) changed.
    UserUpdated { user: User },
    Typing { channel_id: i64, user: User },
    ReactionAdded { channel_id: i64, message_id: i64, emoji: String, user_id: i64 },
    ReactionRemoved { channel_id: i64, message_id: i64, emoji: String, user_id: i64 },
    MessageEdited { channel_id: i64, message_id: i64, content: String, edited_at: i64 },
    MessageDeleted { channel_id: i64, message_id: i64 },
    StickerCreated { sticker: Sticker },
    StickerDeleted { sticker_id: i64 },
    ChannelDeleted { channel_id: i64 },
    ServerRenamed { name: String },
    ServerIconChanged { icon: String },
    /// Tags or assignments changed; clients refetch /api/tags and /api/users.
    TagsChanged,
    /// Someone joined (Some) or left (None) a voice channel.
    VoiceStateChanged {
        user: User,
        channel_id: Option<i64>,
        #[serde(default)]
        sharing: bool,
        #[serde(default)]
        camera: bool,
    },
    /// Full voice occupancy, sent to a client right after it connects.
    VoiceSnapshot { entries: Vec<VoiceStateEntry> },
    Error { message: String },
}
