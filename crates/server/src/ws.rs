use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use sqlx::Row;

use shared::{ClientEvent, Message, ServerEvent, User, VoiceStateEntry};

use crate::auth::AuthUser;
use crate::{dm_recipients, now_ms, SharedState};

/// Broadcast to a DM's participants when `recipients` is Some, else to all.
/// Fan a new message out to subscribed devices whose owner isn't connected.
/// Runs detached: a slow push service must never hold up chat.
fn push_notify(state: SharedState, message: Message, dm_members: Option<Vec<i64>>) {
    tokio::spawn(async move {
        // Who this message pings, by the same rule the clients use.
        let lower = message.content.to_lowercase();
        let everyone = lower.contains("@everyone");
        let mentioned: Vec<i64> = match sqlx::query("SELECT id, username FROM users").fetch_all(&state.db).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| {
                    let id: i64 = r.get(0);
                    let name: String = r.get(1);
                    let hit = everyone || lower.contains(&format!("@{}", name.to_lowercase()));
                    hit.then_some(id)
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        let targets = match crate::push::recipients(&state, &message, &mentioned, dm_members.as_deref()).await {
            Ok(targets) if !targets.is_empty() => targets,
            _ => return,
        };
        let Ok(vapid) = crate::push::vapid(&state).await else { return };

        // Where the notification says it came from.
        let place: Option<String> = sqlx::query_scalar("SELECT name FROM channels WHERE id = ?")
            .bind(message.channel_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
        let title = match (&dm_members, place) {
            (Some(_), _) => message.author.username.clone(),
            (None, Some(channel)) => format!("{} · #{channel}", message.author.username),
            (None, None) => message.author.username.clone(),
        };
        let body: String = message.content.chars().take(140).collect();
        let subject = std::env::var("NOTDISCORD_MAIL_FROM")
            .map(|from| format!("mailto:{from}"))
            .unwrap_or_else(|_| "mailto:admin@notdiscord.invalid".into());
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "channel_id": message.channel_id,
            "message_id": message.id,
        });

        for (user_id, sub) in targets {
            match crate::push::send(&vapid, &sub, &payload, &subject).await {
                // Dead subscription: the browser is gone. Forget it, or we'd
                // retry this forever on every message.
                Ok(false) => {
                    let _ = sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ?")
                        .bind(&sub.endpoint)
                        .execute(&state.db)
                        .await;
                }
                Ok(true) => {}
                Err(e) => tracing::warn!("push to user {user_id} failed: {e}"),
            }
        }
    });
}

fn send_scoped(state: &SharedState, recipients: &Option<Vec<i64>>, event: ServerEvent) {
    match recipients {
        Some(ids) => state.broadcast_only(ids.clone(), event),
        None => state.broadcast(event),
    }
}

#[derive(serde::Deserialize)]
pub struct WsQuery {
    /// Minutes to add to UTC for this client's local time, as
    /// `-new Date().getTimezoneOffset()` gives it. Absent from older clients,
    /// which is why it's optional rather than defaulted at the edge.
    #[serde(default)]
    tz: Option<i64>,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<WsQuery>,
    AuthUser(user): AuthUser,
) -> Response {
    // Remembered per person, refreshed on every connect, so a laptop that
    // crosses a timezone corrects itself the next time it reconnects.
    if let Some(tz) = q.tz.filter(|t| (-16 * 60..=16 * 60).contains(t)) {
        let db = state.db.clone();
        let id = user.id;
        tokio::spawn(async move {
            let _ = sqlx::query("UPDATE users SET tz_offset_minutes = ? WHERE id = ?")
                .bind(tz)
                .bind(id)
                .execute(&db)
                .await;
        });
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

/// How often the server pings an idle connection. Short enough to keep a NAT
/// mapping alive, long enough to be free.
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
/// Nothing heard from a client for this long and it is treated as gone. Three
/// missed heartbeats, so one slow moment isn't a disconnect.
const CLIENT_SILENT_FOR: std::time::Duration = std::time::Duration::from_secs(70);

async fn handle_socket(socket: WebSocket, state: SharedState, mut user: User) {
    tracing::info!("ws connected: {}", user.username);
    let (mut sink, mut stream) = socket.split();
    let mut events = state.events.subscribe();

    let came_online = {
        let mut presence = state.presence.lock().unwrap();
        let count = presence.entry(user.id).or_insert(0);
        *count += 1;
        *count == 1
    };
    if came_online {
        state.broadcast(ServerEvent::PresenceChanged { user: user.clone(), online: true });
    }

    // Tell the fresh connection who's already in voice — hiding DM calls
    // this user isn't part of.
    {
        let raw: Vec<VoiceStateEntry> = state
            .voice
            .lock()
            .unwrap()
            .values()
            .map(|(channel_id, user, sharing, camera)| VoiceStateEntry {
                channel_id: *channel_id,
                user: user.clone(),
                sharing: *sharing,
                camera: *camera,
            })
            .collect();
        let mut entries = Vec::with_capacity(raw.len());
        for entry in raw {
            match dm_recipients(&state.db, entry.channel_id).await {
                Ok(Some(members)) if !members.contains(&user.id) => {}
                _ => entries.push(entry),
            }
        }
        let snapshot = serde_json::to_string(&ServerEvent::VoiceSnapshot { entries }).expect("serialize");
        let _ = sink.send(WsMessage::text(snapshot)).await;
    }

    // Voice channel this connection has announced itself in.
    let mut my_voice: Option<i64> = None;
    // This connection's flood budget.
    let mut limits = crate::ratelimit::ConnectionLimits::default();

    // A silent connection is indistinguishable from a dead one, so make it
    // never be silent. The ping also keeps NAT mappings from being evicted
    // mid-conversation, which is one of the ways the path dies in the first
    // place.
    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heard = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                // Nothing at all from this client for a while: it is gone,
                // whatever the socket claims. Dropping it frees its presence
                // so everyone else stops seeing it online.
                if last_heard.elapsed() > CLIENT_SILENT_FOR {
                    tracing::info!("ws idle too long, dropping {}", user.username);
                    break;
                }
                if sink.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            // Broadcast events fan out to every connected client.
            event = events.recv() => {
                match event {
                    Ok(envelope) => {
                        // DM events are addressed to their participants only.
                        let for_me = envelope.only.as_ref().is_none_or(|ids| ids.contains(&user.id));
                        if for_me {
                            // Keep this connection's snapshot of its own user fresh,
                            // so messages sent after a profile change carry the new avatar.
                            if let ServerEvent::UserUpdated { user: updated } = &envelope.event {
                                if updated.id == user.id {
                                    user = updated.clone();
                                }
                            }
                            let text = serde_json::to_string(&envelope.event).expect("serialize event");
                            if sink.send(WsMessage::text(text)).await.is_err() {
                                break;
                            }
                        }
                    }
                    // Lagged: client fell behind the broadcast buffer; drop and
                    // let it reconnect for a clean state.
                    Err(_) => break,
                }
            }
            incoming = stream.next() => {
                let Some(Ok(msg)) = incoming else { break };
                // Pongs and pings count as much as chat does: this is about
                // whether anything is still on the other end.
                last_heard = std::time::Instant::now();
                if let WsMessage::Text(text) = msg {
                    match serde_json::from_str::<ClientEvent>(&text) {
                        // Player card buttons: deterministic, no LLM involved.
                        Ok(ClientEvent::MusicControl { action }) => {
                            crate::music::handle_control(state.clone(), action);
                        }
                        // Voice presence is connection-scoped state, handled here.
                        Ok(ClientEvent::VoiceState { channel_id, sharing, camera }) => {
                            {
                                let mut voice = state.voice.lock().unwrap();
                                match channel_id {
                                    Some(ch) => {
                                        state.voice_left.lock().unwrap().remove(&user.id);
                                        voice.insert(user.id, (ch, user.clone(), sharing, camera));
                                    }
                                    None => {
                                        voice.remove(&user.id);
                                        // Hold off the LiveKit reconciler briefly: the
                                        // participant may linger there for a moment.
                                        state.voice_left.lock().unwrap().insert(user.id, now_ms());
                                    }
                                }
                            }
                            // DM call presence stays between its two members.
                            let scope_channel = channel_id.or(my_voice);
                            let recipients = match scope_channel {
                                Some(ch) => dm_recipients(&state.db, ch).await.unwrap_or(None),
                                None => None,
                            };
                            my_voice = channel_id;
                            send_scoped(&state, &recipients, ServerEvent::VoiceStateChanged { user: user.clone(), channel_id, sharing, camera });
                        }
                        Ok(event) => {
                            // Flood guard: each kind of traffic has its own
                            // budget, so a reaction spree can't silence you
                            // mid-sentence. Refusals are told to the sender —
                            // silently dropping a message is worse than
                            // saying no — and a connection that keeps at it
                            // is dropped as not-a-person.
                            let allowed = match &event {
                                ClientEvent::SendMessage { .. } => limits.messages.take(),
                                ClientEvent::ToggleReaction { .. } => limits.reactions.take(),
                                ClientEvent::Typing { .. } => limits.typing.take(),
                                ClientEvent::EditMessage { .. } | ClientEvent::DeleteMessage { .. } => {
                                    limits.edits.take()
                                }
                                _ => true,
                            };
                            if !allowed {
                                limits.strikes += 1;
                                if limits.strikes > crate::ratelimit::MAX_STRIKES {
                                    tracing::warn!("ws flood from {}: closing", user.username);
                                    break;
                                }
                                // Typing is noise; don't nag about it.
                                if !matches!(event, ClientEvent::Typing { .. }) {
                                    let warning = serde_json::to_string(&ServerEvent::Error {
                                        message: "slow down — too many actions at once".into(),
                                    })
                                    .expect("serialize");
                                    let _ = sink.send(WsMessage::text(warning)).await;
                                }
                                continue;
                            }
                            if let Err(e) = handle_event(&state, &user, event).await {
                                tracing::warn!("ws event error from {}: {e}", user.username);
                            }
                        }
                        Err(e) => tracing::warn!("bad ws payload from {}: {e}", user.username),
                    }
                }
            }
        }
    }
    // A dropped connection clears its own voice announcement, so crashes
    // never leave ghosts in the sidebar.
    if let Some(channel) = my_voice {
        let removed = {
            let mut voice = state.voice.lock().unwrap();
            if voice.get(&user.id).map(|(c, _, _, _)| *c) == Some(channel) {
                voice.remove(&user.id);
                true
            } else {
                false
            }
        };
        if removed {
            let recipients = dm_recipients(&state.db, channel).await.unwrap_or(None);
            send_scoped(&state, &recipients, ServerEvent::VoiceStateChanged { user: user.clone(), channel_id: None, sharing: false, camera: false });
        }
    }

    let went_offline = {
        let mut presence = state.presence.lock().unwrap();
        match presence.get_mut(&user.id) {
            Some(count) if *count <= 1 => {
                presence.remove(&user.id);
                true
            }
            Some(count) => {
                *count -= 1;
                false
            }
            None => false,
        }
    };
    if went_offline {
        state.broadcast(ServerEvent::PresenceChanged { user: user.clone(), online: false });
    }
    tracing::info!("ws disconnected: {}", user.username);
}

async fn handle_event(state: &SharedState, user: &User, event: ClientEvent) -> anyhow::Result<()> {
    match event {
        ClientEvent::SendMessage { channel_id, content, reply_to } => {
            let content = content.trim().to_owned();
            if content.is_empty() || content.len() > 4000 {
                return Ok(());
            }
            let recipients = dm_recipients(&state.db, channel_id).await?;
            if recipients.as_ref().is_some_and(|ids| !ids.contains(&user.id)) {
                return Ok(());
            }

            // Resolve the reply target (same channel only) and its preview.
            let mut reply_preview = None;
            let mut valid_reply = None;
            if let Some(target_id) = reply_to {
                if let Some(row) = sqlx::query(
                    "SELECT u.username, m.content FROM messages m JOIN users u ON u.id = m.author_id \
                     WHERE m.id = ? AND m.channel_id = ?",
                )
                .bind(target_id)
                .bind(channel_id)
                .fetch_optional(&state.db)
                .await?
                {
                    valid_reply = Some(target_id);
                    reply_preview = Some(shared::ReplyPreview { author: row.get(0), content: row.get(1) });
                }
            }

            let created_at = now_ms();
            let result = sqlx::query(
                "INSERT INTO messages (channel_id, author_id, content, created_at, reply_to) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(channel_id)
            .bind(user.id)
            .bind(&content)
            .bind(created_at)
            .bind(valid_reply)
            .execute(&state.db)
            .await?;

            let bot = state.bot_user();
            let is_slash = content.trim_start().starts_with('/');
            let mentioned_bot = user.id != bot.id && crate::bot::is_mention(&content, &bot.username);
            let music_cmd = if (mentioned_bot || is_slash) && user.id != bot.id {
                crate::music::parse_command(&content, &bot.username)
            } else {
                None
            };
            // /remindme sits outside the music commands: it is the only one
            // that answers later rather than now.
            let remind_arg = (user.id != bot.id)
                .then(|| content.trim_start())
                .and_then(|t| t.strip_prefix("/remindme").or_else(|| t.strip_prefix("/remind")))
                .map(|rest| rest.to_owned());
            let message = Message {
                id: result.last_insert_rowid(),
                channel_id,
                author: user.clone(),
                content,
                created_at,
                edited_at: None,
                reactions: Vec::new(),
                reply_to: valid_reply,
                reply_preview,
                pinned: false,
            };
            send_scoped(state, &recipients, ServerEvent::MessageCreated { message: message.clone() });

            // Phones that aren't connected get a push notification instead.
            push_notify(state.clone(), message, recipients.clone());

            // Summoned? Music commands are deterministic and free; questions
            // (and /ask, /image) go to the LLM. Both run in the background.
            if let Some(rest) = remind_arg {
                let state = state.clone();
                let user_id = user.id;
                tokio::spawn(async move {
                    let offset = crate::reminders::offset_for(&state, user_id).await;
                    let reply = match crate::reminders::parse(&rest, crate::now_ms(), offset) {
                        Ok(reminder) => match crate::reminders::schedule(&state, user_id, &reminder, offset).await {
                            Ok(ok) => ok,
                            Err(e) => {
                                tracing::warn!("could not store reminder: {e}");
                                "couldn't save that one, sorry".to_owned()
                            }
                        },
                        Err(why) => why.to_owned(),
                    };
                    let _ = crate::bot::post_message(&state, channel_id, &reply).await;
                });
                return Ok(());
            }

            match music_cmd {
                Some(crate::music::MusicCmd::Ask) => crate::bot::maybe_answer(state.clone(), channel_id),
                Some(crate::music::MusicCmd::Draw(prompt)) => {
                    crate::bot::draw_now(state.clone(), channel_id, prompt)
                }
                Some(cmd) => crate::music::handle_command(state.clone(), user.clone(), channel_id, cmd),
                None if mentioned_bot => crate::bot::maybe_answer(state.clone(), channel_id),
                None => {}
            }
        }
        // Handled at the connection level in handle_socket.
        ClientEvent::VoiceState { .. } | ClientEvent::MusicControl { .. } => {}
        ClientEvent::Typing { channel_id } => {
            let recipients = dm_recipients(&state.db, channel_id).await?;
            if recipients.as_ref().is_some_and(|ids| !ids.contains(&user.id)) {
                return Ok(());
            }
            send_scoped(state, &recipients, ServerEvent::Typing { channel_id, user: user.clone() });
        }
        ClientEvent::ToggleReaction { message_id, emoji } => {
            // Either a unicode emoji, or :name: naming a real server emoji.
            let valid = if let Some(name) = emoji.strip_prefix(':').and_then(|e| e.strip_suffix(':')) {
                shared::emoji_name_problem(name).is_none()
                    && sqlx::query_scalar::<_, i64>("SELECT 1 FROM custom_emojis WHERE name = ?")
                        .bind(name)
                        .fetch_optional(&state.db)
                        .await?
                        .is_some()
            } else {
                !emoji.is_empty()
                    && emoji.chars().count() <= 8
                    && !emoji.chars().any(char::is_whitespace)
            };
            if !valid {
                return Ok(());
            }
            let Some(row) = sqlx::query("SELECT channel_id FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&state.db)
                .await?
            else {
                return Ok(());
            };
            let channel_id: i64 = row.get(0);
            let recipients = dm_recipients(&state.db, channel_id).await?;
            if recipients.as_ref().is_some_and(|ids| !ids.contains(&user.id)) {
                return Ok(());
            }

            let removed = sqlx::query(
                "DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
            )
            .bind(message_id)
            .bind(user.id)
            .bind(&emoji)
            .execute(&state.db)
            .await?
            .rows_affected();

            if removed > 0 {
                send_scoped(state, &recipients, ServerEvent::ReactionRemoved {
                    channel_id,
                    message_id,
                    emoji,
                    user_id: user.id,
                });
            } else {
                sqlx::query(
                    "INSERT OR IGNORE INTO reactions (message_id, user_id, emoji, created_at) VALUES (?, ?, ?, ?)",
                )
                .bind(message_id)
                .bind(user.id)
                .bind(&emoji)
                .bind(now_ms())
                .execute(&state.db)
                .await?;
                send_scoped(state, &recipients, ServerEvent::ReactionAdded {
                    channel_id,
                    message_id,
                    emoji,
                    user_id: user.id,
                });
            }
        }
        ClientEvent::EditMessage { message_id, content } => {
            let content = content.trim().to_owned();
            if content.is_empty() || content.len() > 4000 {
                return Ok(());
            }
            let edited_at = now_ms();
            let Some(row) = sqlx::query("SELECT channel_id FROM messages WHERE id = ? AND author_id = ?")
                .bind(message_id)
                .bind(user.id)
                .fetch_optional(&state.db)
                .await?
            else {
                return Ok(());
            };
            let channel_id: i64 = row.get(0);
            sqlx::query("UPDATE messages SET content = ?, edited_at = ? WHERE id = ?")
                .bind(&content)
                .bind(edited_at)
                .bind(message_id)
                .execute(&state.db)
                .await?;
            let recipients = dm_recipients(&state.db, channel_id).await?;
            send_scoped(state, &recipients, ServerEvent::MessageEdited { channel_id, message_id, content, edited_at });
        }
        ClientEvent::DeleteMessage { message_id } => {
            // Authors can delete their own messages; admins can delete any.
            let query = if user.role == "admin" {
                sqlx::query("SELECT channel_id, content FROM messages WHERE id = ?").bind(message_id)
            } else {
                sqlx::query("SELECT channel_id, content FROM messages WHERE id = ? AND author_id = ?")
                    .bind(message_id)
                    .bind(user.id)
            };
            let Some(row) = query.fetch_optional(&state.db).await? else {
                return Ok(());
            };
            let channel_id: i64 = row.get(0);
            let content: String = row.get(1);
            sqlx::query("DELETE FROM messages WHERE id = ?")
                .bind(message_id)
                .execute(&state.db)
                .await?;
            let recipients = dm_recipients(&state.db, channel_id).await?;
            send_scoped(state, &recipients, ServerEvent::MessageDeleted { channel_id, message_id });
            // Take the attachments with it, unless something else — another
            // message, an avatar, a sticker, an emoji — still points at them.
            // Off the reply path: deleting is done either way.
            let state = state.clone();
            tokio::spawn(async move {
                crate::attachments::drop_orphans(&state, &content, message_id).await;
            });
        }
    }
    Ok(())
}
