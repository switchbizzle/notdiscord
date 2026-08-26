//! Types shared between the NotDiscord server and client: REST DTOs and the
//! WebSocket event protocol. Timestamps are unix milliseconds (UTC).

use serde::{Deserialize, Serialize};

// ---------- Core entities ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
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
}

/// Events pushed from server to all connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    MessageCreated { message: Message },
    ChannelCreated { channel: Channel },
    PresenceChanged { user: User, online: bool },
    Typing { channel_id: i64, user: User },
    ReactionAdded { channel_id: i64, message_id: i64, emoji: String, user_id: i64 },
    ReactionRemoved { channel_id: i64, message_id: i64, emoji: String, user_id: i64 },
    MessageEdited { channel_id: i64, message_id: i64, content: String, edited_at: i64 },
    MessageDeleted { channel_id: i64, message_id: i64 },
    Error { message: String },
}
