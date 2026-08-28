//! End-to-end smoke test against a running server:
//! registers two users, connects both over WebSocket, sends a message as one,
//! and asserts both receive the broadcast and that history persists.
//!
//! Run the server, then: `cargo run -p server --example smoke`

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{AuthResponse, Channel, ClientEvent, Message, RegisterRequest, ServerEvent};

fn base_url() -> String {
    std::env::var("NOTDISCORD_BASE").unwrap_or_else(|_| "http://127.0.0.1:3000".into())
}

async fn register(http: &reqwest::Client, username: &str) -> AuthResponse {
    let base = base_url();
    let resp = http
        .post(format!("{base}/api/register"))
        .json(&RegisterRequest {
            username: username.into(),
            password: "hunter2hunter2".into(),
            invite: std::env::var("NOTDISCORD_INVITE").ok(),
        })
        .send()
        .await
        .expect("server reachable");
    assert!(resp.status().is_success(), "register failed: {}", resp.status());
    resp.json().await.expect("auth response")
}

/// Read events until one matches `pred` (presence/typing broadcasts from other
/// connections can interleave with what a test is waiting for).
async fn wait_for(
    socket: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    pred: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    let deadline = std::time::Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(deadline, socket.next())
            .await
            .expect("timed out waiting for ws event")
            .expect("ws closed")
            .expect("ws error");
        if let WsMsg::Text(text) = msg {
            let event: ServerEvent = serde_json::from_str(&text).expect("valid server event");
            if pred(&event) {
                return event;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let http = reqwest::Client::new();
    let base = base_url();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let alice = register(&http, &format!("alice{nonce}")).await;
    let bob = register(&http, &format!("bob{nonce}")).await;
    println!("registered {} and {}", alice.user.username, bob.user.username);

    let channels: Vec<Channel> = http
        .get(format!("{base}/api/channels"))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let general = channels.iter().find(|c| c.name == "general").expect("seeded #general");
    println!("found #{} (id {})", general.name, general.id);

    let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let (mut ws_alice, _) = connect_async(format!("{ws_base}/ws?token={}", alice.token)).await.unwrap();
    let (mut ws_bob, _) = connect_async(format!("{ws_base}/ws?token={}", bob.token)).await.unwrap();

    // Alice should see bob come online.
    let bob_id = bob.user.id;
    wait_for(&mut ws_alice, |e| {
        matches!(e, ServerEvent::PresenceChanged { user, online: true } if user.id == bob_id)
    })
    .await;
    println!("alice saw bob come online");

    // Bob types; alice should see the typing indicator.
    let typing = ClientEvent::Typing { channel_id: general.id };
    ws_bob
        .send(WsMsg::Text(serde_json::to_string(&typing).unwrap().into()))
        .await
        .unwrap();
    wait_for(&mut ws_alice, |e| {
        matches!(e, ServerEvent::Typing { user, .. } if user.id == bob_id)
    })
    .await;
    println!("alice saw bob typing");

    let content = format!("hello from smoke test {nonce}");
    let event =
        ClientEvent::SendMessage { channel_id: general.id, content: content.clone(), reply_to: None };
    ws_alice
        .send(WsMsg::Text(serde_json::to_string(&event).unwrap().into()))
        .await
        .unwrap();

    for (name, socket) in [("alice", &mut ws_alice), ("bob", &mut ws_bob)] {
        let event = wait_for(socket, |e| matches!(e, ServerEvent::MessageCreated { .. })).await;
        let ServerEvent::MessageCreated { message } = event else { unreachable!() };
        assert_eq!(message.content, content);
        assert_eq!(message.author.id, alice.user.id);
        println!("{name} received the broadcast");
    }

    let history: Vec<Message> = http
        .get(format!("{base}/api/channels/{}/messages", general.id))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(history.iter().any(|m| m.content == content), "message persisted in history");
    println!("message persisted in history ({} total)", history.len());

    println!("SMOKE TEST PASSED");
}
