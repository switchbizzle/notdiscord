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
    SendMessage { channel_id: i64, content: String },
    /// Sent (throttled) while the user is typing in a channel.
    Typing { channel_id: i64 },
    /// Add the reaction if the user hasn't reacted with this emoji, else remove it.
    ToggleReaction { message_id: i64, emoji: String },
    /// Edit own message.
    EditMessage { message_id: i64, content: String },
    /// Delete own message.
    DeleteMessage { message_id: i64 },
    /// Announce which voice channel this user is in (None = left voice).
    VoiceState { channel_id: Option<i64> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStateEntry {
    pub channel_id: i64,
    pub user: User,
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
    VoiceStateChanged { user: User, channel_id: Option<i64> },
    /// Full voice occupancy, sent to a client right after it connects.
    VoiceSnapshot { entries: Vec<VoiceStateEntry> },
    Error { message: String },
}
