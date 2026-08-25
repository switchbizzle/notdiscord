#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;

use std::collections::HashMap;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{Channel, ClientEvent, Message, ServerEvent, UserStatus};

fn main() {
    let window = WindowBuilder::new()
        .with_title("NotDiscord")
        .with_inner_size(LogicalSize::new(1100.0, 720.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window).with_menu(None))
        .launch(App);
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[component]
fn App() -> Element {
    let mut session = use_signal(|| None::<api::Session>);
    let mut restoring = use_signal(|| true);

    // Try to resume the saved session; the token is validated against /api/me.
    use_future(move || async move {
        if let Some(saved) = api::load_session() {
            if let Ok(user) = api::me(&saved).await {
                session.set(Some(api::Session { user, ..saved }));
            }
        }
        restoring.set(false);
    });

    rsx! {
        style { dangerous_inner_html: include_str!("../assets/style.css") }
        if restoring() {
            div { class: "login-wrap", div { class: "splash", "NotDiscord" } }
        } else {
            match session() {
                Some(s) => rsx! { MainView { session: s, session_slot: session } },
                None => rsx! { LoginView { session } },
            }
        }
    }
}

#[component]
fn LoginView(session: Signal<Option<api::Session>>) -> Element {
    let mut base_url = use_signal(|| {
        api::load_session()
            .map(|s| s.base_url)
            .or_else(|| std::env::var("NOTDISCORD_SERVER").ok())
            .unwrap_or_else(|| "http://127.0.0.1:3000".into())
    });
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);

    let submit = move |register: bool| {
        if busy() {
            return;
        }
        spawn(async move {
            busy.set(true);
            error.set(String::new());
            let result = if register {
                api::register(&base_url(), username(), password()).await
            } else {
                api::login(&base_url(), username(), password()).await
            };
            match result {
                Ok(s) => {
                    api::save_session(&s);
                    session.set(Some(s));
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
    };

    rsx! {
        div { class: "login-wrap",
            div { class: "login-card",
                h1 { "NotDiscord" }
                p { class: "login-sub", "Self-hosted chat for you and your friends" }
                label { "Server" }
                input {
                    value: "{base_url}",
                    oninput: move |e| base_url.set(e.value()),
                }
                label { "Username" }
                input {
                    value: "{username}",
                    oninput: move |e| username.set(e.value()),
                }
                label { "Password" }
                input {
                    r#type: "password",
                    value: "{password}",
                    oninput: move |e| password.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            submit(false);
                        }
                    },
                }
                if !error().is_empty() {
                    div { class: "login-error", "{error}" }
                }
                div { class: "login-buttons",
                    button {
                        class: "primary",
                        disabled: busy(),
                        onclick: move |_| submit(false),
                        "Log in"
                    }
                    button {
                        disabled: busy(),
                        onclick: move |_| submit(true),
                        "Register"
                    }
                }
            }
        }
    }
}

/// How long a typing indicator stays visible after the last Typing event.
const TYPING_TTL_MS: i64 = 4000;
/// Minimum interval between Typing events we send while the user types.
const TYPING_SEND_INTERVAL_MS: i64 = 2500;

#[component]
fn MainView(session: api::Session, session_slot: Signal<Option<api::Session>>) -> Element {
    let session = use_signal(move || session);
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut members = use_signal(Vec::<UserStatus>::new);
    let mut has_more = use_signal(|| false);
    let mut loading_older = use_signal(|| false);
    // user id -> (channel they are typing in, username, expiry timestamp)
    let mut typing = use_signal(HashMap::<i64, (i64, String, i64)>::new);
    let mut last_typing_sent = use_signal(|| 0i64);
    let mut draft = use_signal(String::new);
    let mut new_channel = use_signal(String::new);
    let mut status = use_signal(|| "connecting…".to_string());

    // Initial data load: channel list, then history for the first channel.
    use_future(move || async move {
        match api::channels(&session()).await {
            Ok(chs) => {
                if let Some(first) = chs.first().cloned() {
                    selected.set(Some(first.clone()));
                    match api::messages(&session(), first.id, None).await {
                        Ok(msgs) => {
                            has_more.set(msgs.len() == api::HISTORY_PAGE);
                            messages.set(msgs);
                        }
                        Err(e) => status.set(e),
                    }
                }
                channels.set(chs);
            }
            Err(e) => status.set(format!("failed to load channels: {e}")),
        }
    });

    // Expire stale typing indicators once a second.
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let now = now_ms();
            if typing.peek().values().any(|(_, _, expiry)| *expiry < now) {
                typing.write().retain(|_, (_, _, expiry)| *expiry >= now);
            }
        }
    });

    // WebSocket task: forwards outgoing events, applies incoming ones.
    // Reconnects with a delay if the connection drops.
    let ws = use_coroutine(move |mut rx: UnboundedReceiver<ClientEvent>| async move {
        loop {
            status.set("connecting…".into());
            let Ok((mut socket, _)) = connect_async(api::ws_url(&session())).await else {
                status.set("server unreachable, retrying…".into());
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            };
            status.set("online".into());

            // Refresh state that may have drifted while disconnected.
            if let Ok(users) = api::users(&session()).await {
                members.set(users);
            }
            if let Some(channel) = selected() {
                if let Ok(msgs) = api::messages(&session(), channel.id, None).await {
                    has_more.set(msgs.len() == api::HISTORY_PAGE);
                    messages.set(msgs);
                }
            }

            loop {
                tokio::select! {
                    cmd = rx.next() => {
                        let Some(cmd) = cmd else { return };
                        let text = serde_json::to_string(&cmd).expect("serialize event");
                        if socket.send(WsMsg::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    incoming = socket.next() => {
                        let Some(Ok(msg)) = incoming else { break };
                        let WsMsg::Text(text) = msg else { continue };
                        let Ok(event) = serde_json::from_str::<ServerEvent>(&text) else { continue };
                        match event {
                            ServerEvent::MessageCreated { message } => {
                                // A real message replaces the author's typing indicator.
                                typing.write().remove(&message.author.id);
                                if selected().map(|c| c.id) == Some(message.channel_id) {
                                    messages.write().push(message);
                                }
                            }
                            ServerEvent::ChannelCreated { channel } => {
                                if !channels().iter().any(|c| c.id == channel.id) {
                                    channels.write().push(channel);
                                }
                            }
                            ServerEvent::PresenceChanged { user, online } => {
                                let mut list = members.write();
                                match list.iter_mut().find(|m| m.user.id == user.id) {
                                    Some(entry) => entry.online = online,
                                    None => {
                                        list.push(UserStatus { user, online });
                                        list.sort_by(|a, b| a.user.username.to_lowercase().cmp(&b.user.username.to_lowercase()));
                                    }
                                }
                            }
                            ServerEvent::Typing { channel_id, user } => {
                                if user.id != session().user.id {
                                    typing.write().insert(user.id, (channel_id, user.username, now_ms() + TYPING_TTL_MS));
                                }
                            }
                            ServerEvent::Error { .. } => {}
                        }
                    }
                }
            }
            status.set("disconnected, retrying…".into());
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });

    let mut send = move || {
        let content = draft().trim().to_string();
        let Some(channel) = selected() else { return };
        if content.is_empty() {
            return;
        }
        ws.send(ClientEvent::SendMessage { channel_id: channel.id, content });
        draft.set(String::new());
    };

    let mut notify_typing = move || {
        let Some(channel) = selected() else { return };
        let now = now_ms();
        if now - last_typing_sent() >= TYPING_SEND_INTERVAL_MS {
            last_typing_sent.set(now);
            ws.send(ClientEvent::Typing { channel_id: channel.id });
        }
    };

    let load_older = move |_| {
        let Some(channel) = selected() else { return };
        let Some(oldest) = messages().first().map(|m| m.id) else { return };
        if loading_older() {
            return;
        }
        spawn(async move {
            loading_older.set(true);
            match api::messages(&session(), channel.id, Some(oldest)).await {
                Ok(older) => {
                    has_more.set(older.len() == api::HISTORY_PAGE);
                    let mut combined = older;
                    combined.extend(messages());
                    messages.set(combined);
                }
                Err(e) => status.set(e),
            }
            loading_older.set(false);
        });
    };

    let add_channel = move || {
        let name = new_channel().trim().to_string();
        if name.is_empty() {
            return;
        }
        spawn(async move {
            // Success arrives back via the ChannelCreated broadcast.
            if let Err(e) = api::create_channel(&session(), name).await {
                status.set(e);
            } else {
                new_channel.set(String::new());
            }
        });
    };

    let logout = move |_| {
        api::clear_session();
        session_slot.set(None);
    };

    let selected_id = selected().map(|c| c.id);
    let selected_name = selected().map(|c| c.name.clone()).unwrap_or_default();
    let typing_line = {
        let names: Vec<String> = typing()
            .values()
            .filter(|(channel_id, _, _)| Some(*channel_id) == selected_id)
            .map(|(_, name, _)| name.clone())
            .collect();
        match names.as_slice() {
            [] => String::new(),
            [a] => format!("{a} is typing…"),
            [a, b] => format!("{a} and {b} are typing…"),
            _ => "several people are typing…".into(),
        }
    };

    rsx! {
        div { class: "app",
            div { class: "sidebar",
                div { class: "sidebar-title", "NotDiscord" }
                div { class: "channel-list",
                    for channel in channels() {
                        button {
                            key: "{channel.id}",
                            class: if selected_id == Some(channel.id) { "channel active" } else { "channel" },
                            onclick: move |_| {
                                let channel = channel.clone();
                                selected.set(Some(channel.clone()));
                                messages.set(Vec::new());
                                has_more.set(false);
                                spawn(async move {
                                    match api::messages(&session(), channel.id, None).await {
                                        Ok(msgs) => {
                                            has_more.set(msgs.len() == api::HISTORY_PAGE);
                                            messages.set(msgs);
                                        }
                                        Err(e) => status.set(e),
                                    }
                                });
                            },
                            "# {channel.name}"
                        }
                    }
                }
                input {
                    class: "new-channel",
                    placeholder: "+ new channel",
                    value: "{new_channel}",
                    oninput: move |e| new_channel.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            add_channel();
                        }
                    },
                }
                div { class: "me",
                    div { class: "me-info",
                        span { class: "me-name", "{session().user.username}" }
                        span { class: "me-status", "{status}" }
                    }
                    button { class: "logout", title: "Log out", onclick: logout, "⏻" }
                }
            }
            div { class: "main",
                div { class: "channel-header", "# {selected_name}" }
                div { class: "messages",
                    // column-reverse container keeps the view pinned to the
                    // newest message, so render newest first.
                    for msg in messages().into_iter().rev() {
                        div { key: "{msg.id}", class: "msg",
                            div { class: "msg-head",
                                span { class: "msg-author", "{msg.author.username}" }
                                span { class: "msg-time", {format_time(msg.created_at)} }
                            }
                            div { class: "msg-body", "{msg.content}" }
                        }
                    }
                    if has_more() {
                        button {
                            class: "load-older",
                            disabled: loading_older(),
                            onclick: load_older,
                            if loading_older() { "loading…" } else { "Load older messages" }
                        }
                    }
                }
                div { class: "typing-line", "{typing_line}" }
                div { class: "compose",
                    input {
                        placeholder: "Message #{selected_name}",
                        value: "{draft}",
                        oninput: move |e| {
                            draft.set(e.value());
                            notify_typing();
                        },
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                send();
                            }
                        },
                    }
                }
            }
            div { class: "members",
                div { class: "members-title", "Members" }
                for member in members() {
                    div {
                        key: "{member.user.id}",
                        class: if member.online { "member online" } else { "member" },
                        span { class: "member-dot" }
                        "{member.user.username}"
                    }
                }
            }
        }
    }
}

fn format_time(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%b %-d %H:%M").to_string())
        .unwrap_or_default()
}
