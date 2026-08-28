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
    #[serde(default)]
    pub status: Option<String>,
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
    #[serde(default)]
    pub pinned: bool,
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
    /// Custom status text ("away", "playing X"), if set.
    #[serde(default)]
    pub status: Option<String>,
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

/// Body for POST /api/push/subscribe — the browser's PushSubscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscribeRequest {
    pub endpoint: String,
    /// base64url, straight from the browser.
    pub p256dh: String,
    pub auth: String,
}

/// How much a person wants to be notified: "all" | "mentions" | "none".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyPrefs {
    pub level: String,
    /// Whether this device already has a push subscription registered.
    #[serde(default)]
    pub subscribed: bool,
    /// The server's VAPID public key, needed to subscribe.
    #[serde(default)]
    pub vapid_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetNotifyLevel {
    pub level: String,
}

/// The caller's own email state (never another user's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailStatus {
    pub email: Option<String>,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailRequest {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailVerifyRequest {
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetStatusRequest {
    /// None or empty clears the status.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgotPasswordRequest {
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetPasswordRequest {
    pub username: String,
    pub code: String,
    pub new_password: String,
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
    /// True while nobody has an account yet: the clients offer to set the
    /// server up instead of asking for a login nobody can have.
    #[serde(default)]
    pub needs_setup: bool,
}

/// POST /api/setup — only accepted while a server has no people on it.
/// Creates the owner (who is always an admin), names the server, and can
/// set the invite code the next person will need.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
    pub server_name: String,
    #[serde(default)]
    pub invite: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetServerIconRequest {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameServerRequest {
    pub name: String,
}

/// Body for PATCH /api/channels/{id} — rename a text or voice channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameChannelRequest {
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

/// The bot's identity and personality (GET /api/server/bot, admin only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotSettings {
    pub persona: String,
    pub name: String,
    #[serde(default)]
    pub avatar: Option<String>,
    /// Where release announcements go. None = the first text channel
    /// (the old behavior); Some(0) = don't announce at all.
    #[serde(default)]
    pub announce_channel: Option<i64>,
    /// The LLM the bot answers with (not a secret, so it round-trips).
    #[serde(default)]
    pub model: String,
    /// Secrets never travel back to the client — only whether they're set,
    /// and where they came from, so an admin can tell "the server was
    /// started with one" from "I typed one in here".
    #[serde(default)]
    pub credentials: Vec<CredentialStatus>,
}

/// One configurable credential, as the settings pane sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialStatus {
    /// Stable key: "openrouter" | "giphy" | "soundcloud" | "spotify".
    pub key: String,
    pub label: String,
    pub hint: String,
    pub set: bool,
    /// True when the value comes from the environment rather than settings.
    pub from_env: bool,
}

/// Partial update for POST /api/server/bot — only Some fields change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotSettingsUpdate {
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    /// Channel id for release announcements; 0 disables them.
    #[serde(default)]
    pub announce_channel: Option<i64>,
    #[serde(default)]
    pub model: Option<String>,
    /// Credential updates by key; an empty string clears one, and clearing
    /// falls back to whatever the environment provides.
    #[serde(default)]
    pub credentials: Vec<(String, String)>,
}

/// The model the bot uses when an admin hasn't picked one. Cheap, fast, and
/// good enough for chat; anything on OpenRouter can replace it in settings.
pub const DEFAULT_BOT_MODEL: &str = "google/gemini-2.5-flash-lite";

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

/// First line marker of the bot's music player message: the client renders
/// those messages as a player card with transport buttons instead of text.
pub const PLAYER_MARKER: &str = "⟦player⟧";

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

/// A server emoji, written `:name:` in messages and reactions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub creator_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEmojiRequest {
    pub name: String,
    /// URL previously returned by /api/upload.
    pub url: String,
}

/// Emoji names are `:snake_case:` — 2-32 of [a-z0-9_].
pub fn emoji_name_problem(name: &str) -> Option<&'static str> {
    if name.len() < 2 || name.len() > 32 {
        return Some("emoji name must be 2-32 characters");
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Some("emoji names use lowercase letters, numbers, and underscores");
    }
    None
}

/// One track in the music player's queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MusicTrack {
    pub id: u64,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub art: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

/// The shared player, as the music tab sees it (GET /api/music/state).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MusicState {
    pub active: bool,
    pub paused: bool,
    /// Seconds into the current track.
    pub position: f64,
    #[serde(default)]
    pub now_playing: Option<MusicTrack>,
    #[serde(default)]
    pub queue: Vec<MusicTrack>,
    /// Voice identity of the player, for the per-listener volume slider.
    #[serde(default)]
    pub bot_identity: String,
}

/// Body for POST /api/music/play — queue a link without posting it to chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicPlayRequest {
    /// Where the bot should announce what it queued.
    pub channel_id: i64,
    pub url: String,
}

/// Body for POST /api/music/control — "pause", "resume", "skip", "stop".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicControlRequest {
    pub action: String,
}

/// Body for POST /api/music/queue — "move", "remove", or "clear".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MusicQueueRequest {
    pub action: String,
    #[serde(default)]
    pub id: Option<u64>,
    /// Negative moves the track earlier in the queue, positive later.
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub ids: Vec<u64>,
}

/// One attachment found in a channel's history (GET /api/channels/{id}/files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Server-relative /files/... URL.
    pub url: String,
    pub name: String,
    /// Bytes on disk right now.
    pub size: i64,
    pub created_at: i64,
    /// The message it was posted in, for jump-to.
    pub message_id: i64,
    pub uploader: String,
}

/// Unread state for one channel (GET /api/unread).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadInfo {
    pub channel_id: i64,
    /// Messages from other people since you last read.
    pub count: i64,
    /// How many of those ping you (@you, @everyone, or any DM).
    pub mentions: i64,
    /// Id of the newest message you've seen — the NEW divider goes after it.
    pub last_read_id: i64,
}

/// Body for POST /api/read — "I've seen up to this message".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkReadRequest {
    pub channel_id: i64,
    pub message_id: i64,
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
    /// Press a button on the music player card: "pause", "resume", "skip", "stop".
    MusicControl { action: String },
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
    MessagePinChanged { channel_id: i64, message_id: i64, pinned: bool },
    /// A user set or cleared their custom status text.
    StatusChanged { user_id: i64, status: Option<String> },
    StickerCreated { sticker: Sticker },
    StickerDeleted { sticker_id: i64 },
    ChannelDeleted { channel_id: i64 },
    ChannelRenamed { channel_id: i64, name: String },
    ServerRenamed { name: String },
    ServerIconChanged { icon: String },
    /// Tags or assignments changed; clients refetch /api/tags and /api/users.
    TagsChanged,
    /// Server emojis changed; clients refetch /api/emojis.
    EmojisChanged,
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
