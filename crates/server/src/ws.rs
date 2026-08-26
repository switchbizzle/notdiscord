use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use sqlx::Row;

use shared::{ClientEvent, Message, ServerEvent, User, VoiceStateEntry};

use crate::auth::AuthUser;
use crate::{dm_recipients, now_ms, SharedState};

/// Broadcast to a DM's participants when `recipients` is Some, else to all.
fn send_scoped(state: &SharedState, recipients: &Option<Vec<i64>>, event: ServerEvent) {
    match recipients {
        Some(ids) => state.broadcast_only(ids.clone(), event),
        None => state.broadcast(event),
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

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

    loop {
        tokio::select! {
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
                                        voice.insert(user.id, (ch, user.clone(), sharing, camera));
                                    }
                                    None => {
                                        voice.remove(&user.id);
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
            };
            send_scoped(state, &recipients, ServerEvent::MessageCreated { message });

            // Summoned? Music commands are deterministic and free; questions
            // (and /ask, /image) go to the LLM. Both run in the background.
            match music_cmd {
                Some(crate::music::MusicCmd::Ask) => crate::bot::maybe_answer(state.clone(), channel_id),
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
            if emoji.is_empty() || emoji.chars().count() > 8 || emoji.chars().any(char::is_whitespace) {
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
                sqlx::query("SELECT channel_id FROM messages WHERE id = ?").bind(message_id)
            } else {
                sqlx::query("SELECT channel_id FROM messages WHERE id = ? AND author_id = ?")
                    .bind(message_id)
                    .bind(user.id)
            };
            let Some(row) = query.fetch_optional(&state.db).await? else {
                return Ok(());
            };
            let channel_id: i64 = row.get(0);
            sqlx::query("DELETE FROM messages WHERE id = ?")
                .bind(message_id)
                .execute(&state.db)
                .await?;
            let recipients = dm_recipients(&state.db, channel_id).await?;
            send_scoped(state, &recipients, ServerEvent::MessageDeleted { channel_id, message_id });
        }
    }
    Ok(())
}
