//! End-to-end smoke test against a running server:
//! registers two users, connects both over WebSocket, sends a message as one,
//! and asserts both receive the broadcast and that history persists.
//!
//! Run the server, then: `cargo run -p server --example smoke`

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{AuthResponse, Channel, ClientEvent, Message, RegisterRequest, ServerEvent};

const BASE: &str = "http://127.0.0.1:3000";

async fn register(http: &reqwest::Client, username: &str) -> AuthResponse {
    let resp = http
        .post(format!("{BASE}/api/register"))
        .json(&RegisterRequest { username: username.into(), password: "hunter2hunter2".into() })
        .send()
        .await
        .expect("server reachable");
    assert!(resp.status().is_success(), "register failed: {}", resp.status());
    resp.json().await.expect("auth response")
}

async fn next_event(
    socket: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> ServerEvent {
    let deadline = std::time::Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(deadline, socket.next())
            .await
            .expect("timed out waiting for ws event")
            .expect("ws closed")
            .expect("ws error");
        if let WsMsg::Text(text) = msg {
            return serde_json::from_str(&text).expect("valid server event");
        }
    }
}

#[tokio::main]
async fn main() {
    let http = reqwest::Client::new();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let alice = register(&http, &format!("alice{nonce}")).await;
    let bob = register(&http, &format!("bob{nonce}")).await;
    println!("registered {} and {}", alice.user.username, bob.user.username);

    let channels: Vec<Channel> = http
        .get(format!("{BASE}/api/channels"))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let general = channels.iter().find(|c| c.name == "general").expect("seeded #general");
    println!("found #{} (id {})", general.name, general.id);

    let ws_base = BASE.replacen("http://", "ws://", 1);
    let (mut ws_alice, _) = connect_async(format!("{ws_base}/ws?token={}", alice.token)).await.unwrap();
    let (mut ws_bob, _) = connect_async(format!("{ws_base}/ws?token={}", bob.token)).await.unwrap();

    let content = format!("hello from smoke test {nonce}");
    let event = ClientEvent::SendMessage { channel_id: general.id, content: content.clone() };
    ws_alice
        .send(WsMsg::Text(serde_json::to_string(&event).unwrap().into()))
        .await
        .unwrap();

    for (name, socket) in [("alice", &mut ws_alice), ("bob", &mut ws_bob)] {
        let ServerEvent::MessageCreated { message } = next_event(socket).await else {
            panic!("{name}: expected MessageCreated");
        };
        assert_eq!(message.content, content);
        assert_eq!(message.author.id, alice.user.id);
        println!("{name} received the broadcast");
    }

    let history: Vec<Message> = http
        .get(format!("{BASE}/api/channels/{}/messages", general.id))
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
