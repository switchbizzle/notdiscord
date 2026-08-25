#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{Channel, ClientEvent, Message, ServerEvent};

fn main() {
    let window = WindowBuilder::new()
        .with_title("NotDiscord")
        .with_inner_size(LogicalSize::new(1100.0, 720.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window).with_menu(None))
        .launch(App);
}

#[component]
fn App() -> Element {
    let session = use_signal(|| None::<api::Session>);
    rsx! {
        style { dangerous_inner_html: include_str!("../assets/style.css") }
        match session() {
            Some(s) => rsx! { MainView { session: s } },
            None => rsx! { LoginView { session } },
        }
    }
}

#[component]
fn LoginView(session: Signal<Option<api::Session>>) -> Element {
    let mut base_url = use_signal(|| {
        std::env::var("NOTDISCORD_SERVER").unwrap_or_else(|_| "http://127.0.0.1:3000".into())
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
                Ok(s) => session.set(Some(s)),
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

#[component]
fn MainView(session: api::Session) -> Element {
    let session = use_signal(move || session);
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut draft = use_signal(String::new);
    let mut new_channel = use_signal(String::new);
    let mut status = use_signal(|| "connecting…".to_string());

    // Initial data load: channel list, then history for the first channel.
    use_future(move || async move {
        match api::channels(&session()).await {
            Ok(chs) => {
                if let Some(first) = chs.first().cloned() {
                    selected.set(Some(first.clone()));
                    match api::messages(&session(), first.id).await {
                        Ok(msgs) => messages.set(msgs),
                        Err(e) => status.set(e),
                    }
                }
                channels.set(chs);
            }
            Err(e) => status.set(format!("failed to load channels: {e}")),
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
                                if selected().map(|c| c.id) == Some(message.channel_id) {
                                    messages.write().push(message);
                                }
                            }
                            ServerEvent::ChannelCreated { channel } => {
                                if !channels().iter().any(|c| c.id == channel.id) {
                                    channels.write().push(channel);
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

    let selected_name = selected().map(|c| c.name.clone()).unwrap_or_default();

    rsx! {
        div { class: "app",
            div { class: "sidebar",
                div { class: "sidebar-title", "NotDiscord" }
                div { class: "channel-list",
                    for channel in channels() {
                        button {
                            key: "{channel.id}",
                            class: if selected().map(|c| c.id) == Some(channel.id) { "channel active" } else { "channel" },
                            onclick: move |_| {
                                let channel = channel.clone();
                                selected.set(Some(channel.clone()));
                                messages.set(Vec::new());
                                spawn(async move {
                                    match api::messages(&session(), channel.id).await {
                                        Ok(msgs) => messages.set(msgs),
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
                    span { class: "me-name", "{session().user.username}" }
                    span { class: "me-status", "{status}" }
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
                }
                div { class: "compose",
                    input {
                        placeholder: "Message #{selected_name}",
                        value: "{draft}",
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                send();
                            }
                        },
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
