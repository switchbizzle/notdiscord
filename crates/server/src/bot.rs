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

fn api_key() -> Option<String> {
    std::env::var("NOTDISCORD_OPENROUTER_KEY").ok().filter(|k| !k.trim().is_empty())
}

fn model() -> String {
    std::env::var("NOTDISCORD_BOT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "google/gemini-2.5-flash-lite".into())
}

/// Find or create the bot account. Its password hash is unusable garbage, so
/// nobody can ever log in as it.
pub async fn ensure_bot_user(db: &sqlx::SqlitePool) -> anyhow::Result<User> {
    if let Some(row) =
        sqlx::query("SELECT id, username, avatar, role FROM users WHERE username = ?")
            .bind(BOT_NAME)
            .fetch_optional(db)
            .await?
    {
        return Ok(User {
            id: row.get(0),
            username: row.get(1),
            avatar: row.get(2),
            role: row.get(3),
        });
    }
    let result = sqlx::query(
        "INSERT INTO users (username, password_hash, role, created_at) VALUES (?, '!bot', 'member', ?)",
    )
    .bind(BOT_NAME)
    .bind(now_ms())
    .execute(db)
    .await?;
    tracing::info!("created bot user {BOT_NAME}");
    Ok(User {
        id: result.last_insert_rowid(),
        username: BOT_NAME.into(),
        avatar: None,
        role: "member".into(),
    })
}

/// Post a message as the bot (DM-scoped when the channel is a DM). Long
/// replies are split into <=4000-char messages to fit the normal limit.
pub async fn post_message(state: &SharedState, channel_id: i64, content: &str) -> anyhow::Result<()> {
    let recipients = dm_recipients(&state.db, channel_id).await?;
    for chunk in split_chunks(content.trim(), 4000) {
        let created_at = now_ms();
        let result = sqlx::query(
            "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(state.bot.id)
        .bind(&chunk)
        .bind(created_at)
        .execute(&state.db)
        .await?;

        let message = Message {
            id: result.last_insert_rowid(),
            channel_id,
            author: state.bot.clone(),
            content: chunk,
            created_at,
            edited_at: None,
            reactions: Vec::new(),
            reply_to: None,
            reply_preview: None,
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

/// Does this message text address the bot?
pub fn is_mention(content: &str) -> bool {
    content.to_lowercase().contains(&format!("@{}", BOT_NAME.to_lowercase()))
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

        let Some(channel_id) = first_text_channel(&state).await else { continue };
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
        "SELECT m.id, u.username, m.content FROM messages m JOIN users u ON u.id = m.author_id \
         WHERE m.channel_id = ? AND m.id > ? ORDER BY m.id DESC",
    )
    .bind(channel_id)
    .bind(after)
    .fetch_all(&state.db)
    .await?;

    // Newest first from SQL; accumulate until the cap, then reverse.
    let mut picked: Vec<(i64, String)> = Vec::new();
    let mut total = 0usize;
    for row in rows {
        let (id, author, content): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
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

async fn call_openrouter(key: &str, system: String, user: String, max_tokens: u32) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "model": model(),
        "max_tokens": max_tokens,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });
    let response: serde_json::Value = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("HTTP-Referer", "https://notdiscord.switchbhost.com")
        .header("X-Title", "NotDiscord")
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await?
        .json()
        .await?;

    if let Some(err) = response["error"]["message"].as_str() {
        anyhow::bail!("openrouter: {err}");
    }
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

async fn generate_reply(state: &SharedState, channel_id: i64) -> anyhow::Result<String> {
    let Some(key) = api_key() else {
        return Ok(
            "I can hear you, but my brain isn't hooked up yet — the server needs an \
             OpenRouter key in NOTDISCORD_OPENROUTER_KEY before I can answer questions."
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
    let system = format!(
        "You are {BOT_NAME}, the resident bot of \"{server_name}\", a small self-hosted \
         chat server (NotDiscord — a from-scratch Discord clone in Rust) used by a group of \
         friends. You were summoned with an @mention; reply to the person who mentioned you.\n\
         Your personality (set by the server admins — stay in it):\n{}\n\
         Keep replies concise — a couple of sentences unless the question truly needs more. \
         Basic markdown (bold, code, lists) is supported; no headings. Never invent facts \
         about the server or its members beyond what the notes and transcript show.\n\n\
         {notes_block}\
         Recent release notes, in case anyone asks what's new:\n{}",
        persona(&state.db).await,
        recent_changelog_text().await,
    );

    let reply = call_openrouter(
        &key,
        system,
        format!(
            "Chat transcript (oldest first):\n{transcript}\n\
             Write {BOT_NAME}'s reply to the latest @{BOT_NAME} mention. \
             Output only the reply text."
        ),
        700,
    )
    .await?;

    // Housekeeping: fold older transcript into the notes once the raw part
    // is heavy. Runs after the reply is generated, off the hot path.
    maybe_compact(state.clone(), channel_id);

    Ok(reply)
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
    let Some(key) = api_key() else { return Ok(()) };
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
        format!(
            "You maintain {BOT_NAME}'s long-term memory of one chat channel. Merge the \
             existing notes and the new transcript into ONE updated set of notes, at most \
             ~{} characters. Keep: facts about the people, decisions, running jokes, \
             preferences, ongoing projects/plans, and anything someone would expect a \
             regular to remember. Drop small talk. Write dense bullet points.",
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
