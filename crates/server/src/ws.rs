use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use shared::{ClientEvent, Message, ServerEvent, User};

use crate::auth::AuthUser;
use crate::{now_ms, SharedState};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    AuthUser(user): AuthUser,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

async fn handle_socket(socket: WebSocket, state: SharedState, user: User) {
    tracing::info!("ws connected: {}", user.username);
    let (mut sink, mut stream) = socket.split();
    let mut events = state.events.subscribe();

    loop {
        tokio::select! {
            // Broadcast events fan out to every connected client.
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let text = serde_json::to_string(&event).expect("serialize event");
                        if sink.send(WsMessage::text(text)).await.is_err() {
                            break;
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
    tracing::info!("ws disconnected: {}", user.username);
}

async fn handle_event(state: &SharedState, user: &User, event: ClientEvent) -> anyhow::Result<()> {
    match event {
        ClientEvent::SendMessage { channel_id, content } => {
            let content = content.trim().to_owned();
            if content.is_empty() || content.len() > 4000 {
                return Ok(());
            }
            let created_at = now_ms();
            let result = sqlx::query(
                "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(channel_id)
            .bind(user.id)
            .bind(&content)
            .bind(created_at)
            .execute(&state.db)
            .await?;

            let message = Message {
                id: result.last_insert_rowid(),
                channel_id,
                author: user.clone(),
                content,
                created_at,
            };
            let _ = state.events.send(ServerEvent::MessageCreated { message });
        }
    }
    Ok(())
}
