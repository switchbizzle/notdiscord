#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod md;

use std::collections::{HashMap, HashSet};

use dioxus::desktop::tao::window::UserAttentionType;
use dioxus::html::HasFileData;
use dioxus::desktop::{use_window, Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{Channel, ClientEvent, GifResult, Message, ServerEvent, UserStatus};

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
            .unwrap_or_else(|| "https://notdiscord.switchbhost.com".into())
    });
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
                api::register(&base_url(), username(), password(), invite()).await
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
                label { "Invite code (only needed to register)" }
                input {
                    value: "{invite}",
                    oninput: move |e| invite.set(e.value()),
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
    use_context_provider(|| session);
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut members = use_signal(Vec::<UserStatus>::new);
    let mut has_more = use_signal(|| false);
    let mut loading_older = use_signal(|| false);
    let mut unread = use_signal(HashSet::<i64>::new);
    let window = use_window();
    // user id -> (channel they are typing in, username, expiry timestamp)
    let mut typing = use_signal(HashMap::<i64, (i64, String, i64)>::new);
    let mut last_typing_sent = use_signal(|| 0i64);
    let mut draft = use_signal(String::new);
    let mut new_channel = use_signal(String::new);
    let mut uploading = use_signal(|| false);
    let mut drag_over = use_signal(|| false);
    let mut gif_open = use_signal(|| false);
    let mut gif_query = use_signal(String::new);
    let mut gif_results = use_signal(Vec::<GifResult>::new);
    let mut gif_status = use_signal(String::new);
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
    let notif_window = window.clone();
    let ws = use_coroutine(move |mut rx: UnboundedReceiver<ClientEvent>| {
        let window = notif_window.clone();
        async move {
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
                                let me = session().user;
                                let mentioned = message.author.id != me.id
                                    && message
                                        .content
                                        .to_lowercase()
                                        .contains(&format!("@{}", me.username.to_lowercase()));
                                if mentioned && !window.window.is_focused() {
                                    window.window.request_user_attention(Some(UserAttentionType::Informational));
                                    play_notification_sound();
                                }
                                if selected().map(|c| c.id) == Some(message.channel_id) {
                                    messages.write().push(message);
                                } else {
                                    unread.write().insert(message.channel_id);
                                }
                            }
                            ServerEvent::ReactionAdded { channel_id, message_id, emoji, user_id } => {
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    let mut list = messages.write();
                                    if let Some(m) = list.iter_mut().find(|m| m.id == message_id) {
                                        if !m.reactions.iter().any(|r| r.emoji == emoji && r.user_id == user_id) {
                                            m.reactions.push(shared::ReactionEntry { emoji, user_id });
                                        }
                                    }
                                }
                            }
                            ServerEvent::ReactionRemoved { channel_id, message_id, emoji, user_id } => {
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    let mut list = messages.write();
                                    if let Some(m) = list.iter_mut().find(|m| m.id == message_id) {
                                        m.reactions.retain(|r| !(r.emoji == emoji && r.user_id == user_id));
                                    }
                                }
                            }
                            ServerEvent::MessageEdited { channel_id, message_id, content, edited_at } => {
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    let mut list = messages.write();
                                    if let Some(m) = list.iter_mut().find(|m| m.id == message_id) {
                                        m.content = content;
                                        m.edited_at = Some(edited_at);
                                    }
                                }
                            }
                            ServerEvent::MessageDeleted { channel_id, message_id } => {
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    messages.write().retain(|m| m.id != message_id);
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
    }});

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

    let upload_files = move |files: Vec<dioxus::html::FileData>| {
        let Some(channel) = selected() else { return };
        spawn(async move {
            for file in files.into_iter().take(5) {
                if file.size() > 50 * 1024 * 1024 {
                    status.set("file too large (max 50 MB)".into());
                    continue;
                }
                let Ok(bytes) = file.read_bytes().await else {
                    status.set("could not read dropped file".into());
                    continue;
                };
                uploading.set(true);
                match api::upload(&session(), &file.name(), bytes.to_vec()).await {
                    Ok(url) => ws.send(ClientEvent::SendMessage { channel_id: channel.id, content: url }),
                    Err(e) => status.set(e),
                }
                uploading.set(false);
            }
        });
    };

    let search_gifs = move |query: String| {
        spawn(async move {
            gif_status.set("searching…".into());
            match api::gifs(&session(), &query).await {
                Ok(results) => {
                    gif_status.set(if results.is_empty() { "no results".into() } else { String::new() });
                    gif_results.set(results);
                }
                Err(e) => gif_status.set(e),
            }
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
        div {
            class: "app",
            ondragover: move |e| {
                e.prevent_default();
                drag_over.set(true);
            },
            ondragleave: move |_| drag_over.set(false),
            ondrop: move |e| {
                e.prevent_default();
                drag_over.set(false);
                upload_files(e.files());
            },
            if drag_over() {
                div { class: "drop-overlay", "Drop to upload to #{selected_name}" }
            }
            div { class: "sidebar",
                div { class: "sidebar-title", "NotDiscord" }
                div { class: "channel-list",
                    for channel in channels() {
                        button {
                            key: "{channel.id}",
                            class: if selected_id == Some(channel.id) {
                                "channel active"
                            } else if unread().contains(&channel.id) {
                                "channel unread"
                            } else {
                                "channel"
                            },
                            onclick: move |_| {
                                let channel = channel.clone();
                                unread.write().remove(&channel.id);
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
                    for (msg, compact) in group_messages(&messages()).into_iter().rev() {
                        MessageRow { key: "{msg.id}", msg, compact }
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
                if gif_open() {
                    div { class: "gif-panel",
                        input {
                            class: "gif-search",
                            placeholder: "Search GIPHY… (Enter)",
                            value: "{gif_query}",
                            oninput: move |e| gif_query.set(e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Enter {
                                    search_gifs(gif_query());
                                }
                            },
                        }
                        if !gif_status().is_empty() {
                            div { class: "gif-status", "{gif_status}" }
                        }
                        div { class: "gif-grid",
                            for (i, gif) in gif_results().into_iter().enumerate() {
                                img {
                                    key: "{i}",
                                    class: "gif-cell",
                                    src: "{gif.preview}",
                                    loading: "lazy",
                                    onclick: move |_| {
                                        if let Some(channel) = selected() {
                                            ws.send(ClientEvent::SendMessage {
                                                channel_id: channel.id,
                                                content: gif.url.clone(),
                                            });
                                        }
                                        gif_open.set(false);
                                    },
                                }
                            }
                        }
                    }
                }
                div { class: "typing-line", "{typing_line}" }
                div { class: "compose",
                    button {
                        class: "attach gif-btn",
                        title: "Send a GIF",
                        onclick: move |_| {
                            let opening = !gif_open();
                            gif_open.set(opening);
                            if opening && gif_results().is_empty() {
                                search_gifs(String::new());
                            }
                        },
                        "GIF"
                    }
                    button {
                        class: "attach",
                        title: "Upload a file, image, or GIF",
                        disabled: uploading(),
                        onclick: move |_| {
                            let Some(channel) = selected() else { return };
                            spawn(async move {
                                let Some(file) = rfd::AsyncFileDialog::new()
                                    .add_filter("All files", &["*"])
                                    .add_filter("Images", &["gif", "png", "jpg", "jpeg", "webp"])
                                    .pick_file()
                                    .await
                                else {
                                    return;
                                };
                                let name = file.file_name();
                                let bytes = file.read().await;
                                if bytes.len() > 50 * 1024 * 1024 {
                                    status.set("file too large (max 50 MB)".into());
                                    return;
                                }
                                uploading.set(true);
                                match api::upload(&session(), &name, bytes).await {
                                    Ok(url) => ws.send(ClientEvent::SendMessage { channel_id: channel.id, content: url }),
                                    Err(e) => status.set(e),
                                }
                                uploading.set(false);
                            });
                        },
                        if uploading() { "…" } else { "+" }
                    }
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
                        span {
                            class: "member-avatar",
                            style: "background: hsl({avatar_hue(member.user.id)}, 55%, 42%)",
                            {initial(&member.user.username)}
                        }
                        span { class: "member-name", "{member.user.username}" }
                        span { class: "member-dot" }
                    }
                }
            }
        }
    }
}

/// Pair each message with whether it should render compactly: same author as
/// the previous message, less than five minutes apart.
fn group_messages(messages: &[Message]) -> Vec<(Message, bool)> {
    let mut out = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        let compact = i > 0 && {
            let prev = &messages[i - 1];
            prev.author.id == msg.author.id && msg.created_at - prev.created_at < 5 * 60 * 1000
        };
        out.push((msg.clone(), compact));
    }
    out
}

/// Split a message into inline image URLs, file-attachment URLs, and text.
fn extract_media(content: &str) -> (Vec<String>, Vec<String>, String) {
    let is_url = |w: &str| w.starts_with("http://") || w.starts_with("https://");
    let is_image = |w: &str| {
        ["gif", "png", "jpg", "jpeg", "webp"]
            .iter()
            .any(|ext| w.to_lowercase().ends_with(&format!(".{ext}")))
    };
    let mut images = Vec::new();
    let mut files = Vec::new();
    let mut rest = Vec::new();
    for word in content.split_whitespace() {
        if is_url(word) && is_image(word) {
            images.push(word.to_owned());
        } else if is_url(word) && word.contains("/files/") {
            files.push(word.to_owned());
        } else {
            rest.push(word);
        }
    }
    if images.is_empty() && files.is_empty() {
        (images, files, content.to_owned())
    } else {
        (images, files, rest.join(" "))
    }
}

const REACTION_EMOJIS: &[&str] = &["👍", "😂", "❤️", "🔥", "😮", "😭"];

#[component]
fn MessageRow(msg: Message, compact: bool) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut palette_open = use_signal(|| false);
    let mut editing = use_signal(|| false);
    let mut edit_draft = use_signal(String::new);

    let hue = avatar_hue(msg.author.id);
    let (images, files, text) = extract_media(&msg.content);
    let me_id = session().user.id;
    let own = msg.author.id == me_id;
    let msg_id = msg.id;
    let content_for_edit = msg.content.clone();

    // Aggregate raw reaction entries into (emoji, count, reacted-by-me).
    let mut reaction_groups: Vec<(String, usize, bool)> = Vec::new();
    for entry in &msg.reactions {
        match reaction_groups.iter_mut().find(|(emoji, _, _)| *emoji == entry.emoji) {
            Some(group) => {
                group.1 += 1;
                group.2 |= entry.user_id == me_id;
            }
            None => reaction_groups.push((entry.emoji.clone(), 1, entry.user_id == me_id)),
        }
    }

    rsx! {
        div { class: if compact { "msg compact" } else { "msg" },
            div { class: "msg-actions",
                button {
                    title: "React",
                    onclick: move |_| palette_open.set(!palette_open()),
                    "🙂"
                }
                if own {
                    button {
                        title: "Edit",
                        onclick: move |_| {
                            edit_draft.set(content_for_edit.clone());
                            editing.set(true);
                        },
                        "✏️"
                    }
                    button {
                        title: "Delete",
                        onclick: move |_| ws.send(ClientEvent::DeleteMessage { message_id: msg_id }),
                        "🗑️"
                    }
                }
            }
            if palette_open() {
                div { class: "emoji-palette",
                    for emoji in REACTION_EMOJIS {
                        button {
                            key: "{emoji}",
                            onclick: move |_| {
                                ws.send(ClientEvent::ToggleReaction {
                                    message_id: msg_id,
                                    emoji: emoji.to_string(),
                                });
                                palette_open.set(false);
                            },
                            "{emoji}"
                        }
                    }
                }
            }
            if compact {
                div { class: "msg-gutter" }
            } else {
                div {
                    class: "avatar",
                    style: "background: hsl({hue}, 55%, 42%)",
                    {initial(&msg.author.username)}
                }
            }
            div { class: "msg-content",
                if !compact {
                    div { class: "msg-head",
                        span {
                            class: "msg-author",
                            style: "color: hsl({hue}, 65%, 68%)",
                            "{msg.author.username}"
                        }
                        span { class: "msg-time", {format_time(msg.created_at)} }
                    }
                }
                if editing() {
                    input {
                        class: "edit-input",
                        value: "{edit_draft}",
                        oninput: move |e| edit_draft.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                let content = edit_draft().trim().to_string();
                                if !content.is_empty() {
                                    ws.send(ClientEvent::EditMessage { message_id: msg_id, content });
                                }
                                editing.set(false);
                            } else if e.key() == Key::Escape {
                                editing.set(false);
                            }
                        },
                    }
                    div { class: "edit-hint", "Enter to save · Esc to cancel" }
                } else if !text.is_empty() {
                    div { class: "msg-body",
                        md::Md { nodes: md::parse_markdown(&text) }
                        if msg.edited_at.is_some() {
                            span { class: "edited-tag", " (edited)" }
                        }
                    }
                }
                for (i, src) in images.into_iter().enumerate() {
                    img { key: "{i}", class: "msg-img", src: "{src}", loading: "lazy" }
                }
                for (i, url) in files.into_iter().enumerate() {
                    {
                        let filename = url.rsplit('/').next().unwrap_or("file").to_owned();
                        rsx! {
                            div {
                                key: "f{i}",
                                class: "msg-file",
                                title: "Download {filename}",
                                onclick: move |_| {
                                    let _ = open::that(&url);
                                },
                                span { class: "msg-file-icon", "📄" }
                                span { class: "msg-file-name", "{filename}" }
                                span { class: "msg-file-dl", "Download" }
                            }
                        }
                    }
                }
                if !reaction_groups.is_empty() {
                    div { class: "reactions",
                        for (emoji, count, mine) in reaction_groups {
                            button {
                                key: "{emoji}",
                                class: if mine { "react-chip mine" } else { "react-chip" },
                                onclick: {
                                    let emoji = emoji.clone();
                                    move |_| ws.send(ClientEvent::ToggleReaction {
                                        message_id: msg_id,
                                        emoji: emoji.clone(),
                                    })
                                },
                                "{emoji} {count}"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn avatar_hue(user_id: i64) -> i64 {
    (user_id * 137) % 360
}

#[cfg(windows)]
fn play_notification_sound() {
    use winapi::um::playsoundapi::{PlaySoundW, SND_ALIAS, SND_ASYNC};
    let alias: Vec<u16> = "SystemAsterisk\0".encode_utf16().collect();
    unsafe {
        PlaySoundW(alias.as_ptr(), std::ptr::null_mut(), SND_ALIAS | SND_ASYNC);
    }
}

#[cfg(not(windows))]
fn play_notification_sound() {}

fn initial(username: &str) -> String {
    username.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
}

fn format_time(unix_ms: i64) -> String {
    use chrono::{Datelike, Duration, Local};
    let Some(dt) = chrono::DateTime::from_timestamp_millis(unix_ms) else {
        return String::new();
    };
    let dt = dt.with_timezone(&Local);
    let now = Local::now();
    let clock = dt.format("%-I:%M %p");
    if dt.date_naive() == now.date_naive() {
        format!("Today at {clock}")
    } else if dt.date_naive() == (now - Duration::days(1)).date_naive() {
        format!("Yesterday at {clock}")
    } else if dt.year() == now.year() {
        dt.format("%b %-d at %-I:%M %p").to_string()
    } else {
        dt.format("%b %-d, %Y").to_string()
    }
}
