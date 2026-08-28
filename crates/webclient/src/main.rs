//! NotDiscord in the browser — the phone client. Milestone 1: text, images,
//! DMs, live updates. Voice/video need the browser's WebRTC path and come
//! later. Served by the server itself at /app, installable as a PWA.

mod api;

use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use gloo_storage::Storage;
use shared::{Channel, ClientEvent, Message, ServerEvent, UserStatus};

const CSS: Asset = asset!("/assets/style.css");
const SESSION_KEY: &str = "nd_session";

fn main() {
    dioxus::launch(App);
}

fn load_session() -> Option<api::Session> {
    gloo_storage::LocalStorage::get(SESSION_KEY).ok()
}

fn save_session(session: &Option<api::Session>) {
    match session {
        Some(s) => {
            let _ = gloo_storage::LocalStorage::set(SESSION_KEY, s);
        }
        None => gloo_storage::LocalStorage::delete(SESSION_KEY),
    }
}

fn format_time(ms: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"hour".into(), &"numeric".into());
    let _ = js_sys::Reflect::set(&opts, &"minute".into(), &"2-digit".into());
    date.to_locale_time_string_with_options("en-US", &opts).into()
}

/// The other side of a DM, for naming it.
fn dm_peer(channel: &Channel, me: i64) -> String {
    channel
        .dm_members
        .iter()
        .find(|u| u.id != me)
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "unknown".into())
}

/// Split message text into leading media (rendered inline) and residual text.
/// Far simpler than the desktop renderer on purpose: images and videos
/// inline, other /files/ attachments become links, everything else is text.
fn extract_media(content: &str) -> (Vec<String>, Vec<String>, Vec<(String, String)>, String) {
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut files = Vec::new();
    let mut text_parts: Vec<&str> = Vec::new();
    for token in content.split_whitespace() {
        let is_url = token.starts_with("http://") || token.starts_with("https://") || token.starts_with("/files/");
        if is_url {
            let lower = token.to_lowercase();
            let path = lower.split(['?', '#']).next().unwrap_or("");
            if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| path.ends_with(e)) {
                images.push(token.to_owned());
                continue;
            }
            if [".mp4", ".webm", ".mov"].iter().any(|e| path.ends_with(e)) {
                videos.push(token.to_owned());
                continue;
            }
            if token.starts_with("/files/") {
                let name = token.rsplit('/').next().unwrap_or("file").to_owned();
                files.push((token.to_owned(), name));
                continue;
            }
        }
        text_parts.push(token);
    }
    (images, videos, files, text_parts.join(" "))
}

#[component]
fn App() -> Element {
    let mut session = use_signal(load_session);

    rsx! {
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" }
        document::Meta { name: "theme-color", content: "#1e1f22" }
        document::Link { rel: "manifest", href: "/app/manifest.webmanifest" }
        document::Link { rel: "icon", href: "/app/icon-192.png" }
        document::Link { rel: "apple-touch-icon", href: "/app/icon-192.png" }
        document::Title { "NotDiscord" }
        document::Stylesheet { href: CSS }
        document::Script {
            "if ('serviceWorker' in navigator) {{ navigator.serviceWorker.register('/app/sw.js', {{ scope: '/app/' }}); }}"
        }
        if session().is_some() {
            Main { session }
        } else {
            Login { session }
        }
    }
}

#[component]
fn Login(session: Signal<Option<api::Session>>) -> Element {
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut invite = use_signal(String::new);
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
                api::register(username(), password(), invite()).await
            } else {
                api::login(username(), password()).await
            };
            match result {
                Ok(s) => {
                    let s = Some(s);
                    save_session(&s);
                    session.set(s);
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
                p { class: "login-sub", "the phone-sized version" }
                label { "Username" }
                input {
                    value: "{username}",
                    autocapitalize: "none",
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
                label { "Invite code (only to register)" }
                input {
                    value: "{invite}",
                    autocapitalize: "none",
                    oninput: move |e| invite.set(e.value()),
                }
                if !error().is_empty() {
                    div { class: "login-error", "{error}" }
                }
                div { class: "login-buttons",
                    button { class: "primary", disabled: busy(), onclick: move |_| submit(false), "Log in" }
                    button { disabled: busy(), onclick: move |_| submit(true), "Register" }
                }
            }
        }
    }
}

#[component]
fn Main(session: Signal<Option<api::Session>>) -> Element {
    let sess = use_memo(move || session().expect("main renders only with a session"));
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut members = use_signal(Vec::<UserStatus>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut has_more = use_signal(|| false);
    let mut drawer = use_signal(|| true);
    let mut draft = use_signal(String::new);
    let mut status = use_signal(String::new);
    let mut uploading = use_signal(|| false);
    let me_id = sess().user.id;

    let mut open_channel = move |channel: Channel| {
        let id = channel.id;
        selected.set(Some(channel));
        drawer.set(false);
        messages.set(Vec::new());
        spawn(async move {
            match api::messages(&sess(), id, None).await {
                Ok(msgs) => {
                    has_more.set(msgs.len() == api::HISTORY_PAGE);
                    if let Some(newest) = msgs.last().map(|m| m.id) {
                        api::mark_read(&sess(), id, newest).await;
                    }
                    messages.set(msgs);
                }
                Err(e) => status.set(e),
            }
        });
    };

    // Initial load: channels + members; auto-open the first text channel.
    use_effect(move || {
        spawn(async move {
            // A dead stored token bounces back to login instead of a
            // half-broken empty shell.
            if api::me(&sess()).await.is_err() {
                save_session(&None);
                session.set(None);
                return;
            }
            match api::channels(&sess()).await {
                Ok(list) => {
                    let first = list.iter().find(|c| c.kind == "text").cloned();
                    channels.set(list);
                    if let Some(first) = first {
                        open_channel(first);
                    }
                }
                Err(e) => status.set(e),
            }
            if let Ok(list) = api::users(&sess()).await {
                members.set(list);
            }
        });
    });

    // WebSocket: outgoing ClientEvents in, ServerEvents applied to signals.
    let ws = use_coroutine(move |mut rx: UnboundedReceiver<ClientEvent>| async move {
        loop {
            let proto = if web_sys::window()
                .map(|w| w.location().protocol().unwrap_or_default())
                .as_deref()
                == Some("http:")
            {
                "ws"
            } else {
                "wss"
            };
            let host = web_sys::window()
                .map(|w| w.location().host().unwrap_or_default())
                .unwrap_or_default();
            let url = format!("{proto}://{host}/ws?token={}", sess().token);
            let Ok(socket) = gloo_net::websocket::futures::WebSocket::open(&url) else {
                gloo_timers::future::TimeoutFuture::new(3000).await;
                continue;
            };
            status.set(String::new());
            let (mut sink, stream) = socket.split();
            // select! needs fused streams; the coroutine receiver already is.
            let mut stream = stream.fuse();
            loop {
                futures_util::select! {
                    out = rx.next() => {
                        let Some(event) = out else { return };
                        if let Ok(text) = serde_json::to_string(&event) {
                            let _ = sink.send(gloo_net::websocket::Message::Text(text)).await;
                        }
                    }
                    incoming = stream.next() => {
                        let Some(Ok(gloo_net::websocket::Message::Text(text))) = incoming else {
                            break;
                        };
                        let Ok(event) = serde_json::from_str::<ServerEvent>(&text) else {
                            continue;
                        };
                        match event {
                            ServerEvent::MessageCreated { message } => {
                                if selected.peek().as_ref().map(|c| c.id) == Some(message.channel_id) {
                                    let (id, chan) = (message.id, message.channel_id);
                                    messages.write().push(message);
                                    spawn(async move { api::mark_read(&sess(), chan, id).await });
                                }
                            }
                            ServerEvent::MessageEdited { channel_id, message_id, content, edited_at } => {
                                if selected.peek().as_ref().map(|c| c.id) == Some(channel_id) {
                                    if let Some(m) = messages.write().iter_mut().find(|m| m.id == message_id) {
                                        m.content = content;
                                        m.edited_at = Some(edited_at);
                                    }
                                }
                            }
                            ServerEvent::MessageDeleted { channel_id, message_id } => {
                                if selected.peek().as_ref().map(|c| c.id) == Some(channel_id) {
                                    messages.write().retain(|m| m.id != message_id);
                                }
                            }
                            ServerEvent::ChannelCreated { channel } => {
                                if !channels.peek().iter().any(|c| c.id == channel.id) {
                                    channels.write().push(channel);
                                }
                            }
                            ServerEvent::ChannelDeleted { channel_id } => {
                                channels.write().retain(|c| c.id != channel_id);
                            }
                            ServerEvent::ChannelRenamed { channel_id, name } => {
                                if let Some(c) = channels.write().iter_mut().find(|c| c.id == channel_id) {
                                    c.name = name.clone();
                                }
                                if selected.peek().as_ref().map(|c| c.id) == Some(channel_id) {
                                    if let Some(c) = selected.write().as_mut() {
                                        c.name = name;
                                    }
                                }
                            }
                            ServerEvent::PresenceChanged { user, online } => {
                                if let Some(m) = members.write().iter_mut().find(|m| m.user.id == user.id) {
                                    m.online = online;
                                }
                            }
                            ServerEvent::StatusChanged { user_id, status: text } => {
                                if let Some(m) = members.write().iter_mut().find(|m| m.user.id == user_id) {
                                    m.status = text;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Dropped: reconnect after a beat.
            status.set("reconnecting…".into());
            gloo_timers::future::TimeoutFuture::new(2000).await;
        }
    });

    let mut send = move |_| {
        let content = draft().trim().to_owned();
        let Some(channel) = selected() else { return };
        if content.is_empty() {
            return;
        }
        draft.set(String::new());
        ws.send(ClientEvent::SendMessage { channel_id: channel.id, content, reply_to: None });
    };

    let load_older = move |_| {
        let Some(channel) = selected() else { return };
        let oldest = messages().first().map(|m| m.id);
        spawn(async move {
            if let Ok(older) = api::messages(&sess(), channel.id, oldest).await {
                has_more.set(older.len() == api::HISTORY_PAGE);
                let mut list = messages.write();
                let mut combined = older;
                combined.append(&mut list);
                *list = combined;
            }
        });
    };

    let selected_label = match selected() {
        Some(c) if c.kind == "dm" => format!("@{}", dm_peer(&c, me_id)),
        Some(c) => format!("# {}", c.name),
        None => "NotDiscord".into(),
    };

    rsx! {
        div { class: "app",
            header { class: "topbar",
                button { class: "burger", onclick: move |_| drawer.set(!drawer()), "☰" }
                div { class: "topbar-title", "{selected_label}" }
            }

            if drawer() {
                div { class: "drawer-overlay", onclick: move |_| drawer.set(false) }
                nav { class: "drawer",
                    div { class: "drawer-head", "NotDiscord" }
                    div { class: "drawer-section", "Channels" }
                    for channel in channels().into_iter().filter(|c| c.kind == "text") {
                        button {
                            key: "{channel.id}",
                            class: if selected().map(|c| c.id) == Some(channel.id) { "drawer-chan active" } else { "drawer-chan" },
                            onclick: {
                                let channel = channel.clone();
                                move |_| open_channel(channel.clone())
                            },
                            "# {channel.name}"
                        }
                    }
                    div { class: "drawer-section", "People" }
                    for member in members().into_iter().filter(|m| m.user.id != me_id) {
                        button {
                            key: "u{member.user.id}",
                            class: "drawer-chan person",
                            onclick: {
                                let user_id = member.user.id;
                                move |_| {
                                    spawn(async move {
                                        match api::create_dm(&sess(), user_id).await {
                                            Ok(channel) => {
                                                if !channels.peek().iter().any(|c| c.id == channel.id) {
                                                    channels.write().push(channel.clone());
                                                }
                                                open_channel(channel);
                                            }
                                            Err(e) => status.set(e),
                                        }
                                    });
                                }
                            },
                            span { class: if member.online { "dot online" } else { "dot" } }
                            span { class: "person-name", "{member.user.username}" }
                            if let Some(text) = member.status.clone() {
                                span { class: "person-status", "{text}" }
                            }
                        }
                    }
                    button {
                        class: "drawer-logout",
                        onclick: move |_| {
                            save_session(&None);
                            session.set(None);
                        },
                        "Log out ({sess().user.username})"
                    }
                }
            }

            main { class: "messages",
                // column-reverse pins the view to the newest message.
                for msg in messages().into_iter().rev() {
                    MessageRow { key: "{msg.id}", msg }
                }
                if has_more() {
                    button { class: "load-older", onclick: load_older, "Load older messages" }
                }
            }

            if !status().is_empty() {
                div { class: "statusbar", "{status}" }
            }

            footer { class: "composer",
                label { class: "attach",
                    input {
                        r#type: "file",
                        onchange: move |evt| {
                            spawn(async move {
                                let Some(file) = evt.files().into_iter().next() else { return };
                                let name = file.name();
                                uploading.set(true);
                                if let Ok(bytes) = file.read_bytes().await {
                                    match api::upload(&sess(), &name, bytes.to_vec()).await {
                                        Ok(url) => {
                                            if let Some(channel) = selected.peek().clone() {
                                                ws.send(ClientEvent::SendMessage {
                                                    channel_id: channel.id,
                                                    content: url,
                                                    reply_to: None,
                                                });
                                            }
                                        }
                                        Err(e) => status.set(e),
                                    }
                                }
                                uploading.set(false);
                            });
                        },
                    }
                    if uploading() { "…" } else { "+" }
                }
                input {
                    class: "draft",
                    placeholder: "Message",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            send(());
                        }
                    },
                }
                button { class: "send", onclick: move |_| send(()), "➤" }
            }
        }
    }
}

#[component]
fn MessageRow(msg: Message) -> Element {
    // The bot's music-player card is a desktop thing.
    if msg.content.starts_with(shared::PLAYER_MARKER) {
        return rsx! {
            div { class: "msg",
                div { class: "msg-body dim", "🎵 music player — open the desktop app for controls" }
            }
        };
    }
    let (images, videos, files, text) = extract_media(&msg.content);
    rsx! {
        div { class: "msg",
            div { class: "msg-head",
                span { class: "msg-author", "{msg.author.username}" }
                span { class: "msg-time", {format_time(msg.created_at)} }
            }
            if let Some(preview) = msg.reply_preview.clone() {
                div { class: "reply-ref", "↩ {preview.author}: {preview.content.chars().take(60).collect::<String>()}" }
            }
            if !text.is_empty() {
                div { class: "msg-body", "{text}" }
            }
            for (i, src) in images.into_iter().enumerate() {
                img { key: "{i}", class: "msg-img", src: "{src}", loading: "lazy" }
            }
            for (i, src) in videos.into_iter().enumerate() {
                video { key: "v{i}", class: "msg-video", src: "{src}", controls: true, preload: "metadata" }
            }
            for (i, (url, name)) in files.into_iter().enumerate() {
                a { key: "f{i}", class: "msg-file", href: "{url}", target: "_blank", "📎 {name}" }
            }
        }
    }
}
