//! Types shared between the NotDiscord server and client: REST DTOs and the
//! WebSocket event protocol. Timestamps are unix milliseconds (UTC).

pub mod highlight;

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
    /// When the newest message here was posted, if there is one. The DM list
    /// is ordered by it, so the conversation you're actually having is at the
    /// top.
    #[serde(default)]
    pub last_at: Option<i64>,
    /// The sidebar group this sits in, if an admin has filed it.
    #[serde(default)]
    pub category_id: Option<i64>,
}

/// A collapsible group in the channel list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCategory {
    pub id: i64,
    pub name: String,
    pub position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRequest {
    pub name: String,
}

/// Body for POST /api/channels/{id}/category — None takes it out of any group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetCategoryRequest {
    #[serde(default)]
    pub category_id: Option<i64>,
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

/// How many reactors a pill names before it starts counting the rest.
const REACTOR_NAME_CAP: usize = 8;

/// Who is behind one reaction pill, in the order they reacted, resolved
/// against the roster the client already holds — never a fetch per pill.
/// Your own reaction reads "you". A reaction outlives the person who left it,
/// so an id the roster doesn't know falls back to `user 12` rather than to
/// nothing at all. The order matches the entries themselves, so a caller
/// rendering avatars can pair the two up.
pub fn reactor_names(
    reactions: &[ReactionEntry],
    emoji: &str,
    roster: &[UserStatus],
    me_id: i64,
) -> Vec<String> {
    reactions
        .iter()
        .filter(|r| r.emoji == emoji)
        .map(|entry| {
            if entry.user_id == me_id {
                return "you".to_string();
            }
            match roster.iter().find(|m| m.user.id == entry.user_id) {
                Some(member) => member.user.username.clone(),
                None => format!("user {}", entry.user_id),
            }
        })
        .collect()
}

/// "you", "you and Jon", "you, Jon and Ada", and past the cap
/// "you, Jon, Ada … and 4 more" — a tooltip, not a census.
pub fn join_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        n if n > REACTOR_NAME_CAP => format!(
            "{} and {} more",
            names[..REACTOR_NAME_CAP].join(", "),
            n - REACTOR_NAME_CAP
        ),
        n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

/// The whole line a reaction pill shows on hover or in the reactor sheet:
/// "jon and you reacted with 👍". Empty when nobody did, which shouldn't
/// happen — a pill with no entries isn't rendered.
pub fn reaction_tooltip(
    reactions: &[ReactionEntry],
    emoji: &str,
    roster: &[UserStatus],
    me_id: i64,
) -> String {
    let names = reactor_names(reactions, emoji, roster, me_id);
    if names.is_empty() {
        return String::new();
    }
    format!("{} reacted with {}", join_names(&names), emoji)
}

// ---------- message permalinks ----------

/// The path a permalink puts after the server: the web app is served at
/// `/app`, and this is the one route under it that isn't a file on disk.
pub const MESSAGE_LINK_PATH: &str = "/app/channels/";

/// A link that points at one message — `https://chat.example/app/channels/7/1204`.
/// `base` is the server's public URL when it has one, otherwise whatever
/// address the client reached it on: a link that only works on the LAN still
/// beats no link at all.
pub fn message_link(base: &str, channel_id: i64, message_id: i64) -> String {
    format!("{}{MESSAGE_LINK_PATH}{channel_id}/{message_id}", base.trim_end_matches('/'))
}

/// The ids out of `/app/channels/{channel}/{message}`, tolerating the
/// trailing slash, query and fragment a browser or a chat client may have
/// added. Anything else — including `/app` itself — is None.
pub fn parse_message_path(path: &str) -> Option<(i64, i64)> {
    let rest = path.strip_prefix(MESSAGE_LINK_PATH)?;
    let rest = rest.split(['?', '#']).next()?.trim_end_matches('/');
    let (channel, message) = rest.split_once('/')?;
    Some((channel.parse().ok()?, message.parse().ok()?))
}

/// The ids out of a whole link, but only when it points at an address this
/// client knows its own server by. A permalink to *someone else's*
/// NotDiscord has exactly the same shape and entirely unrelated ids, so
/// following one in place would land you on a stranger's number in a channel
/// of ours; those keep going to the browser instead.
pub fn parse_message_link(url: &str, bases: &[String]) -> Option<(i64, i64)> {
    bases
        .iter()
        .filter(|base| !base.is_empty())
        .find_map(|base| url.strip_prefix(base.trim_end_matches('/')))
        .and_then(parse_message_path)
}

/// A custom cosmetic role: a named, colored badge admins assign to users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    /// "#rrggbb"
    pub color: String,
}

/// The length of the `http://` or `https://` at the start of `s`, if it has
/// one. Schemes are case-insensitive (RFC 3986 §3.1) and a phone keyboard
/// autocapitalises the first letter of a line, so `Https://…` is both a
/// perfectly valid URL and the one people actually send — it used to render
/// as plain grey text because every check here compared bytes.
pub fn web_scheme_len(s: &str) -> Option<usize> {
    // Longest first: "http://" is not a prefix of "https://", but keeping the
    // order explicit means adding a scheme later can't silently shadow one.
    ["https://", "http://"]
        .into_iter()
        // get(), not slicing: `s` is user text and may not have a char
        // boundary at byte 7 or 8.
        .find(|scheme| s.get(..scheme.len()).is_some_and(|head| head.eq_ignore_ascii_case(scheme)))
        .map(|scheme| scheme.len())
}

/// Does `s` begin with a web URL scheme, in any capitalisation?
pub fn is_web_url(s: &str) -> bool {
    web_scheme_len(s).is_some()
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
    /// `NOTDISCORD_PUBLIC_URL`, when the instance has one. The address a
    /// client reached the server on is not necessarily one anybody else can
    /// open — a LAN address, or `localhost` — so a link meant to be shared
    /// prefers this. Optional: a self-hoster who hasn't set it still gets
    /// links, they just point at whatever address the client is using.
    #[serde(default)]
    pub public_url: Option<String>,
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
    /// The model `/image` draws with. A separate setting because the two are
    /// separate models: changing the chat one leaves drawing untouched.
    #[serde(default)]
    pub image_model: String,
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
    #[serde(default)]
    pub image_model: Option<String>,
    /// Credential updates by key; an empty string clears one, and clearing
    /// falls back to whatever the environment provides.
    #[serde(default)]
    pub credentials: Vec<(String, String)>,
}

/// The model the bot uses when an admin hasn't picked one. Cheap, fast, and
/// good enough for chat; anything on OpenRouter can replace it in settings.
pub const DEFAULT_BOT_MODEL: &str = "google/gemini-2.5-flash-lite";

/// What `/image` draws with when an admin hasn't picked one. Its refusals are
/// its own — a different model is one box away in Settings → Bot.
pub const DEFAULT_BOT_IMAGE_MODEL: &str = "google/gemini-2.5-flash-image";

/// Upload storage usage and cap (GET /api/server/storage).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageInfo {
    pub used_bytes: i64,
    pub cap_gb: i64,
    /// Biggest single file anyone may upload, in MB.
    #[serde(default = "default_upload_max_mb")]
    pub upload_max_mb: i64,
}

fn default_upload_max_mb() -> i64 {
    64
}

/// Body for POST /api/server/upload-limit (admin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadLimitSetting {
    pub upload_max_mb: i64,
}

/// What GET /api/server/stats reports (admin). Everything an owner would
/// otherwise SSH in to find out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStats {
    pub people: i64,
    pub online: i64,
    pub admins: i64,
    pub messages: i64,
    pub text_channels: i64,
    pub voice_channels: i64,
    pub dms: i64,
    pub uploads_bytes: i64,
    pub uploads_cap_bytes: i64,
    pub upload_count: i64,
    pub database_bytes: i64,
    /// How long the server process has been running.
    pub uptime_secs: i64,
    /// When the first person registered — the server's own birthday.
    pub founded_at: Option<i64>,
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

/// Body for POST /api/channels/{id}/mute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuteRequest {
    pub muted: bool,
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
    /// Categories or channel filing changed; clients refetch both lists.
    /// Coarse on purpose — it happens while an admin edits settings, never
    /// on the hot path.
    CategoriesChanged,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: i64, name: &str) -> UserStatus {
        UserStatus {
            user: User {
                id,
                username: name.into(),
                avatar: None,
                role: "member".into(),
            },
            online: true,
            banned: false,
            tag_ids: Vec::new(),
            status: None,
        }
    }

    fn entry(emoji: &str, user_id: i64) -> ReactionEntry {
        ReactionEntry { emoji: emoji.into(), user_id }
    }

    #[test]
    fn names_only_for_the_pill_asked_about() {
        let roster = vec![member(1, "jon"), member(2, "ada")];
        let reactions = vec![entry("👍", 1), entry("🔥", 2), entry("👍", 2)];
        assert_eq!(
            reactor_names(&reactions, "👍", &roster, 99),
            vec!["jon".to_string(), "ada".to_string()]
        );
    }

    #[test]
    fn your_own_reaction_reads_as_you() {
        let roster = vec![member(1, "jon"), member(2, "ada")];
        let reactions = vec![entry("👍", 1), entry("👍", 2)];
        assert_eq!(
            reaction_tooltip(&reactions, "👍", &roster, 2),
            "jon and you reacted with 👍"
        );
    }

    #[test]
    fn a_reactor_the_roster_has_never_heard_of_still_gets_a_name() {
        let reactions = vec![entry("👍", 7)];
        assert_eq!(
            reaction_tooltip(&reactions, "👍", &[], 1),
            "user 7 reacted with 👍"
        );
    }

    #[test]
    fn joining_reads_like_a_sentence_until_the_cap() {
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(join_names(&names[..1]), "a");
        assert_eq!(join_names(&names[..2]), "a and b");
        assert_eq!(join_names(&names), "a, b and c");
        let many: Vec<String> = (0..12).map(|i| format!("u{i}")).collect();
        assert!(join_names(&many).ends_with("and 4 more"));
        assert_eq!(join_names(&[]), "");
    }

    #[test]
    fn a_permalink_round_trips() {
        let link = message_link("https://chat.example", 7, 1204);
        assert_eq!(link, "https://chat.example/app/channels/7/1204");
        let bases = vec!["https://chat.example".to_string()];
        assert_eq!(parse_message_link(&link, &bases), Some((7, 1204)));
    }

    #[test]
    fn a_trailing_slash_on_the_base_doesnt_double_up() {
        assert_eq!(
            message_link("https://chat.example/", 7, 1204),
            "https://chat.example/app/channels/7/1204"
        );
        let bases = vec!["https://chat.example/".to_string()];
        assert_eq!(
            parse_message_link("https://chat.example/app/channels/7/1204", &bases),
            Some((7, 1204))
        );
    }

    #[test]
    fn what_a_browser_adds_is_tolerated() {
        assert_eq!(parse_message_path("/app/channels/7/1204/"), Some((7, 1204)));
        assert_eq!(parse_message_path("/app/channels/7/1204?x=1"), Some((7, 1204)));
        assert_eq!(parse_message_path("/app/channels/7/1204#top"), Some((7, 1204)));
    }

    #[test]
    fn anything_that_isnt_a_permalink_is_left_alone() {
        assert_eq!(parse_message_path("/app/"), None);
        assert_eq!(parse_message_path("/app/channels/7"), None);
        assert_eq!(parse_message_path("/app/channels/seven/1204"), None);
        assert_eq!(parse_message_path("/files/3/cat.png"), None);
        // An empty base would match every URL in the world.
        assert_eq!(
            parse_message_link("https://elsewhere/app/channels/7/1204", &[String::new()]),
            None
        );
    }

    #[test]
    fn another_servers_permalink_is_not_ours_to_follow() {
        let bases = vec!["https://chat.example".to_string()];
        assert_eq!(
            parse_message_link("https://someone-else.example/app/channels/7/1204", &bases),
            None
        );
    }

    #[test]
    fn web_urls_are_recognised_in_any_capitalisation() {
        // The bug this guards: a phone keyboard capitalises the first letter
        // of a line, so the link somebody actually sends is "Https://…" and
        // it used to render as grey text nobody could tap.
        assert_eq!(web_scheme_len("Https://notdiscord.example/app"), Some(8));
        assert_eq!(web_scheme_len("HTTPS://SHOUTY.example"), Some(8));
        assert_eq!(web_scheme_len("http://plain.example"), Some(7));
        assert_eq!(web_scheme_len("HtTp://mixed.example"), Some(7));
        // Not URLs.
        assert_eq!(web_scheme_len("Hello there"), None);
        assert_eq!(web_scheme_len("ftp://files.example"), None);
        assert_eq!(web_scheme_len("https:/typo.example"), None);
        assert_eq!(web_scheme_len(""), None);
        // Multi-byte text must not panic on the byte slice.
        assert_eq!(web_scheme_len("héllo—there"), None);
        assert_eq!(web_scheme_len("日本語のテキスト"), None);
    }
}
