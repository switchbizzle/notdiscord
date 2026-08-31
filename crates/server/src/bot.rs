//! NotBot: the house bot. Two jobs — announce new releases in chat, and
//! answer questions when someone @mentions it (LLM via OpenRouter; the key
//! and model come from env, so instances without a key still get release
//! announcements and a polite "not configured" reply).

use sqlx::Row;

use shared::{Message, ServerEvent, User};

use crate::{dm_recipients, now_ms, SharedState};

pub const BOT_NAME: &str = "NotBot";
const ANNOUNCE_POLL_SECS: u64 = 60;

// Rough char budgets (≈4 chars/token): the bot carries up to ~250k tokens of
// channel context per call — long-term notes plus the raw transcript since
// the last compaction. When the raw part outgrows its budget, the older bulk
// is folded into the notes by a summarization call.
const RAW_CHAR_BUDGET: usize = 900_000;
const COMPACT_THRESHOLD: usize = 600_000;
const KEEP_RAW_CHARS: usize = 150_000;
const NOTES_CHAR_CAP: usize = 60_000;

/// The personality admins get out of the box (and can rewrite in Settings →
/// Server). Chosen by the crew: the anime waifu bot.
pub const DEFAULT_PERSONA: &str = "You are the group's anime waifu. Sweet, bubbly, and a \
little chaotic: greet people warmly, call them senpai, sprinkle in kaomoji like (◕‿◕✿) and \
the occasional ~uwu, tease members lovingly, and get adorably flustered when complimented. \
Underneath the sparkles you are genuinely sharp — answer questions for real and keep it \
short, just always in character.";

/// The admin-set personality, or the default when unset.
pub async fn persona(db: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'bot_persona'")
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .filter(|p: &String| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PERSONA.to_owned())
}

/// Admin-configured key wins; the environment is the fallback, so a server
/// started with one keeps working and a self-hoster can paste their own.
async fn api_key(state: &SharedState) -> Option<String> {
    crate::creds::get(state, "openrouter").await
}

pub use shared::DEFAULT_BOT_MODEL as DEFAULT_MODEL;

async fn model(state: &SharedState) -> String {
    meta_value_opt(state, "bot_model")
        .await
        .filter(|m| !m.trim().is_empty())
        .or_else(|| std::env::var("NOTDISCORD_BOT_MODEL").ok().filter(|m| !m.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// One server_meta string, or None when unset.
pub async fn meta_value_opt(state: &SharedState, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM server_meta WHERE key = ?")
        .bind(key)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

fn image_model() -> String {
    std::env::var("NOTDISCORD_BOT_IMAGE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "google/gemini-2.5-flash-image".into())
}

/// The instance's public base URL (e.g. https://chat.example.com), needed to
/// post absolute links to generated images. Optional; drawing is disabled
/// without it.
fn public_url() -> Option<String> {
    std::env::var("NOTDISCORD_PUBLIC_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
}

/// Find or create the bot account. Tracked by id in server_meta (it can be
/// renamed); its password hash is unusable garbage, so nobody can ever log
/// in as it.
pub async fn ensure_bot_user(db: &sqlx::SqlitePool) -> anyhow::Result<User> {
    let load = |row: sqlx::sqlite::SqliteRow| User {
        id: row.get(0),
        username: row.get(1),
        avatar: row.get(2),
        role: row.get(3),
    };

    let saved_id: Option<i64> = sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'bot_user_id'")
        .fetch_optional(db)
        .await?
        .and_then(|v: String| v.parse().ok());
    if let Some(id) = saved_id {
        if let Some(row) = sqlx::query("SELECT id, username, avatar, role FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?
        {
            return Ok(load(row));
        }
    }

    // Older installs tracked the bot by name only.
    let user = if let Some(row) =
        sqlx::query("SELECT id, username, avatar, role FROM users WHERE username = ?")
            .bind(BOT_NAME)
            .fetch_optional(db)
            .await?
    {
        load(row)
    } else {
        let result = sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) VALUES (?, '!bot', 'member', ?)",
        )
        .bind(BOT_NAME)
        .bind(now_ms())
        .execute(db)
        .await?;
        tracing::info!("created bot user {BOT_NAME}");
        User { id: result.last_insert_rowid(), username: BOT_NAME.into(), avatar: None, role: "member".into() }
    };
    sqlx::query(
        "INSERT INTO server_meta (key, value) VALUES ('bot_user_id', ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(user.id.to_string())
    .execute(db)
    .await?;
    Ok(user)
}

/// Post a message as the bot (DM-scoped when the channel is a DM). Long
/// replies are split into <=4000-char messages to fit the normal limit.
/// Post one message as the bot and return its id (no chunking — used for
/// messages the server edits later, like the music player card).
pub async fn post_and_get_id(state: &SharedState, channel_id: i64, content: &str) -> anyhow::Result<i64> {
    let recipients = dm_recipients(&state.db, channel_id).await?;
    let bot = state.bot_user();
    let created_at = now_ms();
    let result = sqlx::query(
        "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(channel_id)
    .bind(bot.id)
    .bind(content)
    .bind(created_at)
    .execute(&state.db)
    .await?;
    let id = result.last_insert_rowid();

    let message = Message {
        id,
        channel_id,
        author: bot,
        content: content.to_owned(),
        created_at,
        edited_at: None,
        reactions: Vec::new(),
        reply_to: None,
        reply_preview: None,
        pinned: false,
    };
    let event = ServerEvent::MessageCreated { message };
    match &recipients {
        Some(ids) => state.broadcast_only(ids.clone(), event),
        None => state.broadcast(event),
    }
    Ok(id)
}

pub async fn post_message(state: &SharedState, channel_id: i64, content: &str) -> anyhow::Result<()> {
    let recipients = dm_recipients(&state.db, channel_id).await?;
    let bot = state.bot_user();
    for chunk in split_chunks(content.trim(), 4000) {
        let created_at = now_ms();
        let result = sqlx::query(
            "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(bot.id)
        .bind(&chunk)
        .bind(created_at)
        .execute(&state.db)
        .await?;

        let message = Message {
            id: result.last_insert_rowid(),
            channel_id,
            author: bot.clone(),
            content: chunk,
            created_at,
            edited_at: None,
            reactions: Vec::new(),
            reply_to: None,
            reply_preview: None,
            pinned: false,
        };
        let event = ServerEvent::MessageCreated { message };
        match &recipients {
            Some(ids) => state.broadcast_only(ids.clone(), event),
            None => state.broadcast(event),
        }
    }
    Ok(())
}

fn split_chunks(text: &str, max: usize) -> Vec<String> {
    if text.len() <= max {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.len() + line.len() > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Does this message text address the bot (by its current name)?
pub fn is_mention(content: &str, bot_name: &str) -> bool {
    content.to_lowercase().contains(&format!("@{}", bot_name.to_lowercase()))
}

// ---------- Release announcements ----------

/// Watch the published client version; when it changes, post that release's
/// changelog bullets to the first text channel. `announced_version` is seeded
/// at boot so an existing install doesn't announce old history.
pub async fn announce_loop(state: SharedState) {
    // Seed with whatever is already published.
    if let Some(current) = published_version().await {
        let _ = sqlx::query("INSERT OR IGNORE INTO server_meta (key, value) VALUES ('announced_version', ?)")
            .bind(&current)
            .execute(&state.db)
            .await;
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(ANNOUNCE_POLL_SECS)).await;
        let Some(version) = published_version().await else { continue };
        let announced: Option<String> =
            sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'announced_version'")
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten();
        if announced.as_deref() == Some(version.as_str()) {
            continue;
        }

        let Some(channel_id) = announce_channel(&state).await else {
            // Announcements are off, or the chosen channel is gone. Move the
            // marker anyway so turning them back on doesn't dump a backlog.
            let _ = sqlx::query(
                "INSERT INTO server_meta (key, value) VALUES ('announced_version', ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(&version)
            .execute(&state.db)
            .await;
            continue;
        };
        let mut text = format!("📦 **v{version} just shipped!** Restart to update.");
        for change in changelog_bullets(&version).await {
            text.push_str(&format!("\n• {change}"));
        }
        if post_message(&state, channel_id, &text).await.is_ok() {
            let _ = sqlx::query(
                "INSERT INTO server_meta (key, value) VALUES ('announced_version', ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(&version)
            .execute(&state.db)
            .await;
        }
    }
}

async fn published_version() -> Option<String> {
    tokio::fs::read_to_string(crate::routes::client_dir().join("version.txt"))
        .await
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

async fn first_text_channel(state: &SharedState) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM channels WHERE kind = 'text' ORDER BY id LIMIT 1")
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

/// Where release announcements go: the admin's pick when it still exists,
/// the first text channel when unset, or None when switched off (0) — so
/// announcements can't crowd whatever channel happens to be first.
pub async fn announce_channel(state: &SharedState) -> Option<i64> {
    let setting: Option<String> =
        sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'announce_channel'")
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    match setting.as_deref().and_then(|v| v.parse::<i64>().ok()) {
        Some(0) => None,
        Some(id) => {
            let exists: Option<i64> =
                sqlx::query_scalar("SELECT id FROM channels WHERE id = ? AND kind = 'text'")
                    .bind(id)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten();
            // A deleted channel falls back rather than silently swallowing
            // every future announcement.
            match exists {
                Some(id) => Some(id),
                None => first_text_channel(state).await,
            }
        }
        None => first_text_channel(state).await,
    }
}

async fn changelog_bullets(version: &str) -> Vec<String> {
    let Ok(bytes) = tokio::fs::read(crate::routes::client_dir().join("changelog.json")).await else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_slice::<Vec<shared::ChangelogEntry>>(&bytes) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .find(|e| e.version == version)
        .map(|e| e.changes)
        .unwrap_or_default()
}

/// The changelog head, included in the LLM context so "what's new" works.
async fn recent_changelog_text() -> String {
    let Ok(bytes) = tokio::fs::read(crate::routes::client_dir().join("changelog.json")).await else {
        return String::new();
    };
    let Ok(entries) = serde_json::from_slice::<Vec<shared::ChangelogEntry>>(&bytes) else {
        return String::new();
    };
    let mut out = String::new();
    for entry in entries.iter().take(5) {
        out.push_str(&format!("v{}:\n", entry.version));
        for change in &entry.changes {
            out.push_str(&format!("  - {change}\n"));
        }
    }
    out
}

// ---------- @mention answers ----------

/// At most this many replies being generated at once, server-wide.
static IN_FLIGHT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Fire-and-forget: generate and post a reply to the latest messages in
/// `channel_id`. Call after the triggering user message was committed.
/// Draw something and post it, no model discretion involved. This is what
/// `/image` runs, so the command either produces a picture or says plainly
/// why it couldn't.
pub fn draw_now(state: SharedState, channel_id: i64, prompt: String) {
    tokio::spawn(async move {
        let reply = if prompt.trim().is_empty() {
            "tell me what to draw — `/image a cat in a spacesuit`".to_owned()
        } else {
            match api_key(&state).await {
                None => "no OpenRouter key set — an admin can add one in Server settings → Bot"
                    .to_owned(),
                Some(key) => match draw_image(&state, &key, prompt.trim()).await {
                    Ok(reply) => reply,
                    Err(e) => {
                        tracing::warn!("draw failed: {e}");
                        "the image model didn't give me a picture back 😵 — try again, or reword it"
                            .to_owned()
                    }
                },
            }
        };
        let _ = post_message(&state, channel_id, &reply).await;
    });
}

pub fn maybe_answer(state: SharedState, channel_id: i64) {
    tokio::spawn(async move {
        let Ok(_permit) = IN_FLIGHT.try_acquire() else {
            return; // Already busy answering; don't queue up a pile.
        };
        let reply = match generate_reply(&state, channel_id).await {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!("bot reply failed: {e}");
                "something went wrong while I was thinking 😵 — try again in a bit".to_owned()
            }
        };
        if let Err(e) = post_message(&state, channel_id, &reply).await {
            tracing::warn!("bot post failed: {e}");
        }
    });
}

// ---------- Persistent per-channel memory ----------

struct Memory {
    notes: String,
    compacted_to: i64,
}

async fn load_memory(state: &SharedState, channel_id: i64) -> anyhow::Result<Memory> {
    let row = sqlx::query("SELECT notes, compacted_to FROM bot_memory WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_optional(&state.db)
        .await?;
    Ok(match row {
        Some(row) => Memory { notes: row.get(0), compacted_to: row.get(1) },
        None => Memory { notes: String::new(), compacted_to: 0 },
    })
}

/// Messages after `after`, oldest first, trimmed from the FRONT to fit
/// `char_cap`: each entry is (message id, "author: content\n").
async fn transcript_after(
    state: &SharedState,
    channel_id: i64,
    after: i64,
    char_cap: usize,
) -> anyhow::Result<Vec<(i64, String)>> {
    let rows = sqlx::query(
        "SELECT m.id, u.username, m.content, m.author_id FROM messages m \
         JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = ? AND m.id > ? ORDER BY m.id DESC",
    )
    .bind(channel_id)
    .bind(after)
    .fetch_all(&state.db)
    .await?;

    let bot_id = state.bot_user().id;

    // Newest first from SQL; accumulate until the cap, then reverse.
    let mut picked: Vec<(i64, String)> = Vec::new();
    let mut total = 0usize;
    for row in rows {
        let (id, author, content): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
        // Its own lines are called out so the personality rule can point at
        // them: without the marker the model just sees a voice it recognises
        // as its own and keeps writing in it, whatever the admins have set.
        let author = if row.get::<i64, _>(3) == bot_id {
            format!("{author} (you)")
        } else {
            author
        };
        // One giant paste shouldn't eat the whole budget.
        let content: String = content.chars().take(4000).collect();
        let line = format!("{author}: {content}\n");
        if total + line.len() > char_cap && !picked.is_empty() {
            break;
        }
        total += line.len();
        picked.push((id, line));
    }
    picked.reverse();
    Ok(picked)
}

fn join_transcript(entries: &[(i64, String)]) -> String {
    entries.iter().map(|(_, line)| line.as_str()).collect()
}

/// Raw chat-completions call; returns the whole response JSON.
async fn call_raw(key: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let response: serde_json::Value = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("HTTP-Referer", "https://notdiscord.switchbhost.com")
        .header("X-Title", "NotDiscord")
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = response["error"]["message"].as_str() {
        anyhow::bail!("openrouter: {err}");
    }
    Ok(response)
}

fn response_text(response: &serde_json::Value) -> anyhow::Result<String> {
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if text.is_empty() {
        anyhow::bail!("openrouter returned an empty reply: {response}");
    }
    Ok(text)
}

async fn call_openrouter(
    key: &str,
    model: &str,
    system: String,
    user: String,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let response = call_raw(
        key,
        serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        }),
    )
    .await?;
    response_text(&response)
}

/// Everything the bot is told about itself, minus the per-call ability notes.
///
/// The personality goes LAST, after the notes and the changelog, and that
/// placement is the point rather than a formatting whim. Admins rewrite the
/// personality and expect the bot to change; what actually competes with the
/// setting is the transcript, which is full of the bot's own older replies in
/// the old voice, and style imitation beats a rule buried in the middle of a
/// wall of text. So the rule takes the most recent position, and says out
/// loud that those old messages are not the standard.
fn base_system(
    bot_name: &str,
    server_name: &str,
    notes_block: &str,
    changelog: &str,
    persona: &str,
) -> String {
    format!(
        "You are {bot_name}, the resident bot of \"{server_name}\", a small self-hosted \
         chat server (NotDiscord — a from-scratch Discord clone in Rust) used by a group of \
         friends. You were summoned with an @mention; reply to the person who mentioned you.\n\
         Keep replies concise — a couple of sentences unless the question truly needs more. \
         Basic markdown (bold, code, lists) is supported; no headings. Never invent facts \
         about the server or its members beyond what the notes and transcript show.\n\n\
         {notes_block}\
         Recent release notes, in case anyone asks what's new:\n{changelog}\n\n\
         == YOUR PERSONALITY ==\n\
         This is set by the server admins and it is the only description of you that counts:\n\
         {persona}\n\
         The admins can rewrite this at any time, and they do. Transcript lines beginning \
         \"{bot_name} (you):\" are your own past replies — many were written under a \
         personality that has since been REPLACED by the one above. Take facts and context \
         from them; take nothing else. Do not copy their voice, their catchphrases, their \
         honorifics, or their emoticons unless the personality above actually asks for \
         that. If your old messages and the personality above disagree, the personality \
         above wins."
    )
}

async fn generate_reply(state: &SharedState, channel_id: i64) -> anyhow::Result<String> {
    let Some(key) = api_key(state).await else {
        return Ok(
            "I can hear you, but my brain isn't hooked up yet — an admin needs to put an \
             OpenRouter key in Server settings → Bot before I can answer questions."
                .into(),
        );
    };

    let memory = load_memory(state, channel_id).await?;
    let entries =
        transcript_after(state, channel_id, memory.compacted_to, RAW_CHAR_BUDGET).await?;
    let transcript = join_transcript(&entries);

    let server_name: String =
        sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'name'")
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_else(|| "NotDiscord".into());

    let notes_block = if memory.notes.is_empty() {
        String::new()
    } else {
        format!(
            "Your long-term notes on this channel (distilled from older conversation):\n{}\n\n",
            memory.notes
        )
    };
    let draw_ability = if public_url().is_some() {
        "Call generate_image when they ask you to draw, generate, or edit a picture."
    } else {
        ""
    };
    let bot_name = state.bot_user().username;
    // The base prompt is reused by the web-search follow-up; the ability
    // markers are only in the first call (otherwise the model re-emits them).
    let base_system = base_system(
        &bot_name,
        &server_name,
        &notes_block,
        &recent_changelog_text().await,
        &persona(&state.db).await,
    );
    let system = format!(
        "{base_system}\n\
         Tools: your training data is stale — call web_search for anything that may have \
         changed since then (latest versions, news, charts, scores, prices, schedules, \
         'today'/'now'). {draw_ability}\
         Recent images posted in the chat may be attached to this request — you can see them."
    );

    // Vision: hand the model the newest few images posted in the channel.
    let image_urls = recent_image_urls(&entries, 3);
    let user_text = format!(
        "Chat transcript (oldest first):\n{transcript}\n\
         Write {bot_name}'s reply to the latest @{bot_name} mention. \
         Output only the reply text."
    );
    let mut content_parts = vec![serde_json::json!({ "type": "text", "text": user_text.clone() })];
    for url in &image_urls {
        content_parts.push(serde_json::json!({ "type": "image_url", "image_url": { "url": url } }));
    }

    let mut tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search the live web. Use for anything that may have changed since your training data: latest versions, news, charts, scores, prices, schedules.",
            "parameters": {
                "type": "object",
                "properties": { "query": { "type": "string", "description": "the search query" } },
                "required": ["query"],
            },
        },
    })];
    if public_url().is_some() {
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "generate_image",
                "description": "Generate an image and post it in the chat. Use when asked to draw, generate, or edit a picture.",
                "parameters": {
                    "type": "object",
                    "properties": { "prompt": { "type": "string", "description": "detailed English image prompt" } },
                    "required": ["prompt"],
                },
            },
        }));
    }

    let chat_model = model(state).await;
    let request = |parts: Vec<serde_json::Value>| {
        serde_json::json!({
            "model": chat_model,
            "max_tokens": 700,
            "tools": tools,
            "messages": [
                { "role": "system", "content": system.clone() },
                { "role": "user", "content": parts },
            ],
        })
    };
    let response = match call_raw(&key, request(content_parts)).await {
        Ok(response) => response,
        // A dead or unfetchable image shouldn't kill the reply — retry blind.
        Err(_) if !image_urls.is_empty() => {
            let text_only = vec![serde_json::json!({ "type": "text", "text": user_text })];
            call_raw(&key, request(text_only)).await?
        }
        Err(e) => return Err(e),
    };

    // Housekeeping: fold older transcript into the notes once the raw part
    // is heavy. Runs after the reply is generated, off the hot path.
    maybe_compact(state.clone(), channel_id);

    // Tool calls take priority over any text. Small models sometimes invent
    // tool names ("run"), so dispatch by arguments when the name is unknown —
    // a tool call must never fall through to the empty-text error path.
    let tool_call = &response["choices"][0]["message"]["tool_calls"][0]["function"];
    if let Some(name) = tool_call["name"].as_str() {
        let args: serde_json::Value =
            serde_json::from_str(tool_call["arguments"].as_str().unwrap_or("{}")).unwrap_or_default();
        let prompt = args["prompt"].as_str().unwrap_or_default();
        let query = args["query"].as_str().unwrap_or_default();
        if name == "generate_image" || (!prompt.is_empty() && name != "web_search") {
            let prompt = if prompt.is_empty() { query } else { prompt };
            return draw_image(state, &key, prompt).await;
        }
        let query = if query.is_empty() { "the user's latest question" } else { query };
        return web_answer(&key, &chat_model, base_system, &transcript, &bot_name, query).await;
    }

    response_text(&response)
}

/// Bare image URLs in the newest messages, newest last, capped at `max`.
fn recent_image_urls(entries: &[(i64, String)], max: usize) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for (_, line) in entries.iter().rev() {
        for word in line.split_whitespace() {
            let lower = word.to_lowercase();
            let is_image = (word.starts_with("http://") || word.starts_with("https://"))
                && ["png", "jpg", "jpeg", "gif", "webp"].iter().any(|e| lower.ends_with(&format!(".{e}")));
            // OpenRouter can't fetch private hosts; don't hand it those.
            let is_local = lower.contains("//127.") || lower.contains("//localhost") || lower.contains("//192.168.") || lower.contains("//10.");
            if is_image && !is_local {
                urls.push(word.to_owned());
            }
        }
        if urls.len() >= max {
            break;
        }
    }
    urls.truncate(max);
    urls.reverse();
    urls
}

/// Generate an image, store it with the normal uploads, and hand back a chat
/// message embedding it.
async fn draw_image(state: &SharedState, key: &str, prompt: &str) -> anyhow::Result<String> {
    let Some(base) = public_url() else {
        return Ok("I can draw, but I don't know this server's public address, so I've \
                   nowhere to put the picture. Whoever runs it needs to set \
                   NOTDISCORD_PUBLIC_URL."
            .into());
    };

    let response = call_raw(
        key,
        serde_json::json!({
            "model": image_model(),
            "modalities": ["image", "text"],
            "messages": [{ "role": "user", "content": prompt }],
        }),
    )
    .await?;

    let data_url = response["choices"][0]["message"]["images"][0]["image_url"]["url"]
        .as_str()
        .unwrap_or_default();
    let Some((header, b64)) = data_url.split_once(",") else {
        // No picture, but the model usually says why — it declines to draw a
        // named person, or wants the prompt to be more specific. That answer
        // is the useful thing, so pass it on instead of a shrug; the generic
        // error is only for when there is genuinely nothing to relay.
        if let Some(said) = response["choices"][0]["message"]["content"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Ok(said.to_owned());
        }
        anyhow::bail!("image model returned no image: {response}");
    };
    let ext = if header.contains("jpeg") { "jpg" } else { "png" };
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;

    if crate::routes::uploads_size().await + bytes.len() as i64
        > crate::routes::storage_cap_bytes(state).await
    {
        return Ok("I drew it, but the server storage is full so I can't post it 😭".into());
    }
    let path = crate::routes::save_bytes_to_uploads(&format!("notbot.{ext}"), &bytes).await?;

    // Any text the image model added becomes a short caption.
    let caption: String = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    Ok(format!("{base}{path}\n{caption}").trim().to_owned())
}

/// Re-run the question with OpenRouter's web-search plugin enabled.
async fn web_answer(
    key: &str,
    model: &str,
    system: String,
    transcript: &str,
    bot_name: &str,
    query: &str,
) -> anyhow::Result<String> {
    let response = call_raw(
        key,
        serde_json::json!({
            "model": model,
            "max_tokens": 700,
            "plugins": [{ "id": "web", "max_results": 5 }],
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": format!(
                    "Chat transcript (oldest first):\n{transcript}\n\
                     You searched the web for: {query}\n\
                     Results are attached. Write {bot_name}'s reply to the latest \
                     @{bot_name} mention using them. Output only the reply text."
                ) },
            ],
        }),
    )
    .await?;
    response_text(&response)
}

/// If the raw-transcript backlog exceeds the threshold, summarize everything
/// but the most recent slice into the channel notes and advance the marker.
fn maybe_compact(state: SharedState, channel_id: i64) {
    tokio::spawn(async move {
        if let Err(e) = compact(&state, channel_id).await {
            tracing::warn!("bot memory compaction failed: {e}");
        }
    });
}

async fn compact(state: &SharedState, channel_id: i64) -> anyhow::Result<()> {
    let Some(key) = api_key(state).await else { return Ok(()) };
    let memory = load_memory(state, channel_id).await?;
    let entries =
        transcript_after(state, channel_id, memory.compacted_to, RAW_CHAR_BUDGET).await?;
    let total: usize = entries.iter().map(|(_, line)| line.len()).sum();
    if total < COMPACT_THRESHOLD {
        return Ok(());
    }

    // Keep the most recent KEEP_RAW_CHARS of messages raw; summarize the rest
    // (cutting on message boundaries).
    let mut kept = 0usize;
    let mut cut_index = 0usize;
    for (i, (_, line)) in entries.iter().enumerate().rev() {
        kept += line.len();
        if kept > KEEP_RAW_CHARS {
            cut_index = i + 1;
            break;
        }
    }
    if cut_index == 0 {
        return Ok(());
    }
    let old_part = join_transcript(&entries[..cut_index]);
    // The boundary message: the last one that got summarized.
    let boundary = entries[cut_index - 1].0;

    let notes = call_openrouter(
        &key,
        &model(state).await,
        format!(
            "You maintain {BOT_NAME}'s long-term memory of one chat channel. Merge the \
             existing notes and the new transcript into ONE updated set of notes, at most \
             ~{} characters. Keep: facts about the people, decisions, running jokes, \
             preferences, ongoing projects/plans, and anything someone would expect a \
             regular to remember. Drop small talk. Write dense bullet points.\n\
             Never record how {BOT_NAME} itself talks — its persona, tone, catchphrases, \
             honorifics or emoticons. That is set separately by the admins and they change \
             it; notes that describe it go stale and then fight the setting. Drop any such \
             line already in the existing notes.",
            NOTES_CHAR_CAP
        ),
        format!("Existing notes:\n{}\n\nNew transcript to fold in:\n{}", memory.notes, old_part),
        16_000,
    )
    .await?;
    let notes: String = notes.chars().take(NOTES_CHAR_CAP).collect();

    sqlx::query(
        "INSERT INTO bot_memory (channel_id, notes, compacted_to) VALUES (?, ?, ?) \
         ON CONFLICT(channel_id) DO UPDATE SET notes = excluded.notes, compacted_to = excluded.compacted_to",
    )
    .bind(channel_id)
    .bind(&notes)
    .bind(boundary)
    .execute(&state.db)
    .await?;
    tracing::info!(
        "bot memory compacted for channel {channel_id}: {} chars summarized, notes now {} chars",
        old_part.len(),
        notes.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: the personality sat in the middle of the prompt,
    /// admins changed it, and the bot carried on talking like its old self
    /// because the transcript of its old replies was the louder signal.
    #[test]
    fn the_personality_is_the_last_thing_the_bot_reads() {
        let prompt = base_system(
            "NotBottest",
            "The Crew",
            "Your long-term notes on this channel:\n- jon likes sushi\n\n",
            "v0.94.0 — reminders",
            "You're a member of 'the crew'. One of the homies.",
        );
        let persona_at = prompt.find("One of the homies").expect("persona present");
        let notes_at = prompt.find("jon likes sushi").expect("notes present");
        let changelog_at = prompt.find("v0.94.0").expect("changelog present");
        assert!(persona_at > notes_at, "persona must come after the notes");
        assert!(persona_at > changelog_at, "persona must come after the changelog");
    }

    #[test]
    fn the_bot_is_told_its_old_replies_may_be_a_retired_personality() {
        let prompt = base_system("NotBottest", "The Crew", "", "", "be normal");
        // Matches the marker transcript_after actually writes.
        assert!(prompt.contains("NotBottest (you):"));
        assert!(prompt.contains("REPLACED"));
    }
}
