#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod icons;
mod md;
mod voice;

use icons::Icon;

use std::collections::{HashMap, HashSet};

use dioxus::desktop::tao::window::UserAttentionType;
use dioxus::html::HasFileData;
use dioxus::desktop::{use_window, Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use shared::{Channel, ClientEvent, GifResult, Message, Profile, ServerEvent, UpdateProfileRequest, User, UserStatus};

fn main() {
    // Clean up the previous binary left behind by a self-update.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_file_name("NotDiscord.old.exe"));
    }
    let window = WindowBuilder::new()
        .with_title("NotDiscord")
        .with_inner_size(LogicalSize::new(1100.0, 720.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new()
                .with_window(window)
                .with_menu(None)
                .with_disable_context_menu(false),
        )
        .launch(App);
}

/// A file staged in the compose area, awaiting user confirmation to send.
#[derive(Clone, PartialEq)]
struct PendingFile {
    name: String,
    bytes: Vec<u8>,
    /// data: URL thumbnail for images small enough to preview inline.
    preview: Option<String>,
}

fn make_pending(name: String, bytes: Vec<u8>) -> PendingFile {
    use base64::Engine;
    let ext = name.rsplit('.').next().unwrap_or_default().to_lowercase();
    let mime = match ext.as_str() {
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    let preview = mime.filter(|_| bytes.len() <= 10 * 1024 * 1024).map(|mime| {
        format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes))
    });
    PendingFile { name, bytes, preview }
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
            div { class: "login-wrap",
                div { class: "splash-box",
                    div { class: "spinner" }
                    div { class: "splash", "NotDiscord" }
                    div { class: "splash-version", "v{env!(\"CARGO_PKG_VERSION\")}" }
                }
            }
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
    let mut session = use_signal(move || session);
    use_context_provider(|| session);
    let mut lightbox = use_context_provider(|| Signal::new(None::<String>));
    let mut react_target = use_context_provider(|| Signal::new(None::<i64>));
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
    let mut pending_files = use_signal(Vec::<PendingFile>::new);
    let mut drag_over = use_signal(|| false);
    let mut gif_open = use_signal(|| false);
    let mut gif_query = use_signal(String::new);
    let mut gif_results = use_signal(Vec::<GifResult>::new);
    let mut gif_status = use_signal(String::new);
    let mut status = use_signal(|| "connecting…".to_string());
    let voice_status = use_signal_sync(voice::VoiceStatus::default);
    let mic_level = use_signal_sync(|| 0.0f32);
    let voice = use_coroutine(move |rx| voice::voice_task(rx, voice_status, mic_level));
    let mut update_available = use_signal(|| None::<shared::ClientVersionInfo>);
    let mut updating = use_signal(|| false);
    let mut stickers = use_signal(Vec::<shared::Sticker>::new);
    let mut sticker_open = use_signal(|| false);
    let mut profile_card = use_signal(|| None::<Profile>);
    let mut bio_draft = use_signal(String::new);
    let mut editing_bio = use_signal(|| false);
    let mut audio_settings_open = use_signal(|| false);
    let mut audio_settings = use_signal(api::load_settings);
    let mut input_devices = use_signal(Vec::<String>::new);
    let mut output_devices = use_signal(Vec::<String>::new);

    // Rejoin the current voice channel (used after an audio device change).
    let rejoin_voice = move || {
        let Some(channel_id) = voice_status().channel_id else { return };
        let Some(channel) = channels().into_iter().find(|c| c.id == channel_id) else { return };
        spawn(async move {
            match api::voice_token(&session(), channel.id).await {
                Ok(grant) => voice.send(voice::VoiceCmd::Join {
                    channel_id: channel.id,
                    channel_name: channel.name.clone(),
                    url: grant.url,
                    token: grant.token,
                }),
                Err(e) => status.set(e),
            }
        });
    };

    // Check for a newer client build on the server, at launch and periodically.
    use_future(move || async move {
        loop {
            if let Ok(info) = api::client_version(&session()).await {
                if info.version != env!("CARGO_PKG_VERSION") {
                    update_available.set(Some(info));
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(15 * 60)).await;
        }
    });

    // Load the sticker collection.
    use_future(move || async move {
        if let Ok(list) = api::stickers(&session()).await {
            stickers.set(list);
        }
    });

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
                            ServerEvent::UserUpdated { user } => {
                                {
                                    let mut list = members.write();
                                    if let Some(m) = list.iter_mut().find(|m| m.user.id == user.id) {
                                        m.user = user.clone();
                                    }
                                }
                                {
                                    let mut msgs = messages.write();
                                    for m in msgs.iter_mut().filter(|m| m.author.id == user.id) {
                                        m.author = user.clone();
                                    }
                                }
                                if user.id == session().user.id {
                                    let mut s = session();
                                    s.user = user;
                                    api::save_session(&s);
                                    session.set(s);
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
                            ServerEvent::StickerCreated { sticker } => {
                                let mut list = stickers.write();
                                if !list.iter().any(|s| s.id == sticker.id) {
                                    list.push(sticker);
                                    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                                }
                            }
                            ServerEvent::StickerDeleted { sticker_id } => {
                                stickers.write().retain(|s| s.id != sticker_id);
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
        let files = pending_files();
        let Some(channel) = selected() else { return };
        if content.is_empty() && files.is_empty() {
            return;
        }
        draft.set(String::new());
        pending_files.set(Vec::new());
        spawn(async move {
            if !files.is_empty() {
                uploading.set(true);
                for file in files {
                    match api::upload(&session(), &file.name, file.bytes).await {
                        Ok(url) => ws.send(ClientEvent::SendMessage { channel_id: channel.id, content: url }),
                        Err(e) => status.set(e),
                    }
                }
                uploading.set(false);
            }
            if !content.is_empty() {
                ws.send(ClientEvent::SendMessage { channel_id: channel.id, content });
            }
        });
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

    // Stage dropped/picked files in the compose area for confirmation.
    let upload_files = move |files: Vec<dioxus::html::FileData>| {
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
                pending_files.write().push(make_pending(file.name(), bytes.to_vec()));
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

    // Ctrl+V with an image on the clipboard stages it for confirmation.
    let paste_image = move || {
        spawn(async move {
            let clip = tokio::task::spawn_blocking(|| {
                let mut clipboard = arboard::Clipboard::new().ok()?;
                let img = clipboard.get_image().ok()?;
                Some((img.width as u32, img.height as u32, img.bytes.into_owned()))
            })
            .await
            .ok()
            .flatten();
            let Some((width, height, rgba)) = clip else { return };

            let png = tokio::task::spawn_blocking(move || {
                let img = image::RgbaImage::from_raw(width, height, rgba)?;
                let mut buf = Vec::new();
                image::DynamicImage::ImageRgba8(img)
                    .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                    .ok()?;
                Some(buf)
            })
            .await
            .ok()
            .flatten();
            let Some(png) = png else { return };
            pending_files.write().push(make_pending("pasted.png".into(), png));
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
            if updating() {
                div { class: "update-overlay",
                    div { class: "spinner" }
                    div { class: "update-overlay-title", "Updating NotDiscord…" }
                    div { class: "update-overlay-sub", "downloading and restarting, hang tight" }
                }
            }
            if let Some(profile) = profile_card() {
                div {
                    class: "profile-overlay",
                    onclick: move |_| {
                        profile_card.set(None);
                        editing_bio.set(false);
                    },
                    div {
                        class: "profile-card",
                        onclick: move |e| e.stop_propagation(),
                        UserAvatar { user: profile.user.clone(), class: "profile-avatar" }
                        div { class: "profile-name",
                            style: "color: hsl({avatar_hue(profile.user.id)}, 65%, 68%)",
                            "{profile.user.username}"
                        }
                        div { class: "profile-joined", "Member since {format_date(profile.created_at)}" }
                        if editing_bio() {
                            textarea {
                                class: "profile-bio-edit",
                                rows: "3",
                                value: "{bio_draft}",
                                oninput: move |e| bio_draft.set(e.value()),
                            }
                            button {
                                class: "profile-btn primary",
                                onclick: move |_| {
                                    spawn(async move {
                                        let req = UpdateProfileRequest { avatar: None, bio: Some(bio_draft()) };
                                        match api::update_profile(&session(), req).await {
                                            Ok(p) => {
                                                profile_card.set(Some(p));
                                                editing_bio.set(false);
                                            }
                                            Err(e) => status.set(e),
                                        }
                                    });
                                },
                                "Save"
                            }
                        } else {
                            if !profile.bio.is_empty() {
                                div { class: "profile-bio", "{profile.bio}" }
                            }
                            if profile.user.id == session().user.id {
                                div { class: "profile-actions",
                                    button {
                                        class: "profile-btn",
                                        onclick: move |_| {
                                            spawn(async move {
                                                let Some(file) = rfd::AsyncFileDialog::new()
                                                    .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                                                    .pick_file()
                                                    .await
                                                else {
                                                    return;
                                                };
                                                let bytes = file.read().await;
                                                if bytes.len() > 8 * 1024 * 1024 {
                                                    status.set("avatar too large (max 8 MB)".into());
                                                    return;
                                                }
                                                match api::upload(&session(), &file.file_name(), bytes).await {
                                                    Ok(url) => {
                                                        let req = UpdateProfileRequest { avatar: Some(url), bio: None };
                                                        match api::update_profile(&session(), req).await {
                                                            Ok(p) => profile_card.set(Some(p)),
                                                            Err(e) => status.set(e),
                                                        }
                                                    }
                                                    Err(e) => status.set(e),
                                                }
                                            });
                                        },
                                        "Change picture"
                                    }
                                    button {
                                        class: "profile-btn",
                                        onclick: move |_| {
                                            bio_draft.set(profile_card().map(|p| p.bio).unwrap_or_default());
                                            editing_bio.set(true);
                                        },
                                        "Edit bio"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(url) = lightbox() {
                div {
                    class: "lightbox",
                    onclick: move |_| lightbox.set(None),
                    img { class: "lightbox-img", src: "{url}" }
                    div { class: "lightbox-actions",
                        button {
                            onclick: {
                                let url = url.clone();
                                move |e: MouseEvent| {
                                    e.stop_propagation();
                                    let _ = open::that(&url);
                                }
                            },
                            "Open in browser"
                        }
                        button {
                            onclick: {
                                let url = url.clone();
                                move |e: MouseEvent| {
                                    e.stop_propagation();
                                    let save_url = if url.contains("/files/") {
                                        format!("{url}?dl=1")
                                    } else {
                                        url.clone()
                                    };
                                    let _ = open::that(&save_url);
                                }
                            },
                            "Save"
                        }
                        span { class: "lightbox-hint", "click anywhere to close" }
                    }
                }
            }
            div { class: "sidebar",
                div { class: "sidebar-title", "NotDiscord" }
                div { class: "channel-list",
                    for channel in channels().into_iter().filter(|c| c.kind == "text") {
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
                    div { class: "section-row",
                        span { class: "section-label", "Voice" }
                        button {
                            class: "audio-settings-btn",
                            title: "Audio settings",
                            onclick: move |_| {
                                let opening = !audio_settings_open();
                                if opening {
                                    input_devices.set(voice::list_input_devices());
                                    output_devices.set(voice::list_output_devices());
                                }
                                audio_settings_open.set(opening);
                            },
                            Icon { name: "settings", size: 14 }
                        }
                    }
                    if audio_settings_open() {
                        div { class: "audio-settings",
                            label { "Microphone" }
                            select {
                                onchange: move |e| {
                                    let v = e.value();
                                    let mut s = audio_settings.write();
                                    s.input_device = if v.is_empty() { None } else { Some(v) };
                                    api::save_settings(&s);
                                    drop(s);
                                    rejoin_voice();
                                },
                                option { value: "", selected: audio_settings().input_device.is_none(), "Default" }
                                for name in input_devices() {
                                    option {
                                        value: "{name}",
                                        selected: audio_settings().input_device.as_deref() == Some(name.as_str()),
                                        "{name}"
                                    }
                                }
                            }
                            label { "Output" }
                            select {
                                onchange: move |e| {
                                    let v = e.value();
                                    let mut s = audio_settings.write();
                                    s.output_device = if v.is_empty() { None } else { Some(v) };
                                    api::save_settings(&s);
                                    drop(s);
                                    rejoin_voice();
                                },
                                option { value: "", selected: audio_settings().output_device.is_none(), "Default" }
                                for name in output_devices() {
                                    option {
                                        value: "{name}",
                                        selected: audio_settings().output_device.as_deref() == Some(name.as_str()),
                                        "{name}"
                                    }
                                }
                            }
                            label { "Mic volume · {(audio_settings().input_volume * 100.0) as i32}%" }
                            input {
                                r#type: "range",
                                class: "settings-slider",
                                min: "0",
                                max: "200",
                                value: "{(audio_settings().input_volume * 100.0) as i32}",
                                oninput: move |e| {
                                    if let Ok(v) = e.value().parse::<f32>() {
                                        audio_settings.write().input_volume = v / 100.0;
                                        voice.send(voice::VoiceCmd::SetMicVolume(v / 100.0));
                                    }
                                },
                            }
                            MicMeter { level: mic_level }
                            if voice_status().channel_id.is_none() {
                                div { class: "settings-hint", "join a voice channel to test your mic" }
                            }
                            label { class: "ns-toggle-row",
                                input {
                                    r#type: "checkbox",
                                    checked: audio_settings().noise_suppression,
                                    onchange: move |e| {
                                        let enabled = e.checked();
                                        audio_settings.write().noise_suppression = enabled;
                                        voice.send(voice::VoiceCmd::SetNoiseSuppression(enabled));
                                    },
                                }
                                " Noise suppression"
                            }
                            label { "Output volume · {(audio_settings().output_volume * 100.0) as i32}%" }
                            input {
                                r#type: "range",
                                class: "settings-slider",
                                min: "0",
                                max: "200",
                                value: "{(audio_settings().output_volume * 100.0) as i32}",
                                oninput: move |e| {
                                    if let Ok(v) = e.value().parse::<f32>() {
                                        audio_settings.write().output_volume = v / 100.0;
                                        voice.send(voice::VoiceCmd::SetMasterVolume(v / 100.0));
                                    }
                                },
                            }
                        }
                    }
                    for channel in channels().into_iter().filter(|c| c.kind == "voice") {
                        button {
                            key: "v{channel.id}",
                            class: if voice_status().channel_id == Some(channel.id) { "channel voice active" } else { "channel voice" },
                            onclick: move |_| {
                                let channel = channel.clone();
                                spawn(async move {
                                    match api::voice_token(&session(), channel.id).await {
                                        Ok(grant) => voice.send(voice::VoiceCmd::Join {
                                            channel_id: channel.id,
                                            channel_name: channel.name.clone(),
                                            url: grant.url,
                                            token: grant.token,
                                        }),
                                        Err(e) => status.set(e),
                                    }
                                });
                            },
                            Icon { name: "volume", size: 15 }
                            span { class: "voice-channel-name", "{channel.name}" }
                        }
                    }
                }
                if voice_status().channel_id.is_some() || !voice_status().error.is_empty() {
                    div { class: "voice-panel",
                        if !voice_status().error.is_empty() {
                            div {
                                class: "voice-error",
                                title: "Click to dismiss",
                                onclick: move |_| {
                                    let mut vs = voice_status;
                                    vs.write().error = String::new();
                                },
                                "{voice_status().error}"
                            }
                        } else {
                            div { class: "voice-head",
                                Icon { name: "volume", size: 14 }
                                if voice_status().connecting {
                                    "joining {voice_status().channel_name}…"
                                } else {
                                    "{voice_status().channel_name}"
                                }
                            }
                            for p in voice_status().participants {
                                div {
                                    key: "{p.identity}",
                                    class: if p.speaking { "voice-user speaking" } else { "voice-user" },
                                    {
                                        let uid = p.identity.strip_prefix("user-").and_then(|s| s.parse::<i64>().ok());
                                        let user = uid
                                            .and_then(|id| members().into_iter().find(|m| m.user.id == id))
                                            .map(|m| m.user)
                                            .unwrap_or(User { id: uid.unwrap_or(0), username: p.name.clone(), avatar: None });
                                        rsx! { UserAvatar { user, class: "voice-avatar" } }
                                    }
                                    span { class: "voice-name", "{p.name}" }
                                    if p.is_me && voice_status().muted {
                                        span { class: "voice-mic-off", Icon { name: "mic-off", size: 12 } }
                                    }
                                    if !p.is_me {
                                        input {
                                            r#type: "range",
                                            class: "volume-slider",
                                            min: "0",
                                            max: "200",
                                            title: "Volume for {p.name}",
                                            value: "{(voice_status().volumes.get(&p.identity).copied().unwrap_or(1.0) * 100.0) as i32}",
                                            oninput: {
                                                let identity = p.identity.clone();
                                                move |e| {
                                                    if let Ok(v) = e.value().parse::<f32>() {
                                                        voice.send(voice::VoiceCmd::SetVolume {
                                                            identity: identity.clone(),
                                                            volume: v / 100.0,
                                                        });
                                                    }
                                                }
                                            },
                                        }
                                    }
                                }
                            }
                            div { class: "voice-controls",
                                button {
                                    class: if voice_status().muted { "voice-btn muted" } else { "voice-btn" },
                                    title: if voice_status().muted { "Unmute" } else { "Mute" },
                                    onclick: move |_| voice.send(voice::VoiceCmd::ToggleMute),
                                    if voice_status().muted { Icon { name: "mic-off" } } else { Icon { name: "mic" } }
                                }
                                button {
                                    class: "voice-btn leave",
                                    title: "Disconnect",
                                    onclick: move |_| voice.send(voice::VoiceCmd::Leave),
                                    Icon { name: "phone-off" }
                                }
                            }
                        }
                    }
                }
                if let Some(info) = update_available() {
                    button {
                        class: "update-banner",
                        disabled: updating(),
                        onclick: move |_| {
                            let info = info.clone();
                            spawn(async move {
                                updating.set(true);
                                // Leave voice gracefully so no ghost participant
                                // lingers in the room through the restart.
                                if voice_status().channel_id.is_some() {
                                    voice.send(voice::VoiceCmd::Leave);
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                }
                                let url = format!("{}{}", session().base_url, info.url);
                                let result = async {
                                    let resp = reqwest::get(&url).await.map_err(|e| e.to_string())?;
                                    if !resp.status().is_success() {
                                        return Err(format!("download failed ({})", resp.status()));
                                    }
                                    resp.bytes().await.map_err(|e| e.to_string())
                                }
                                .await;
                                match result {
                                    Ok(bytes) => {
                                        if let Err(e) = apply_self_update(bytes.to_vec()) {
                                            status.set(e);
                                            updating.set(false);
                                        }
                                    }
                                    Err(e) => {
                                        status.set(format!("update failed: {e}"));
                                        updating.set(false);
                                    }
                                }
                            });
                        },
                        if updating() { "⬇ downloading update…" } else { "⬆ Update v{info.version} — install & restart" }
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
                    div {
                        class: "me-info",
                        onclick: move |_| {
                            spawn(async move {
                                match api::profile(&session(), session().user.id).await {
                                    Ok(p) => profile_card.set(Some(p)),
                                    Err(e) => status.set(e),
                                }
                            });
                        },
                        UserAvatar { user: session().user.clone(), class: "member-avatar" }
                        div { class: "me-text",
                            span { class: "me-name", "{session().user.username}" }
                            span { class: "me-status", "{status}" }
                        }
                    }
                    button { class: "logout", title: "Log out", onclick: logout, Icon { name: "power", size: 16 } }
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
                if sticker_open() {
                    div { class: "sticker-panel",
                        div { class: "sticker-grid",
                            for sticker in stickers() {
                                div { key: "{sticker.id}", class: "sticker-cell",
                                    img {
                                        class: "sticker-img",
                                        src: "{sticker.url}",
                                        title: "{sticker.name}",
                                        loading: "lazy",
                                        onclick: {
                                            let url = sticker.url.clone();
                                            move |_| {
                                                if let Some(channel) = selected() {
                                                    ws.send(ClientEvent::SendMessage {
                                                        channel_id: channel.id,
                                                        content: url.clone(),
                                                    });
                                                }
                                                sticker_open.set(false);
                                            }
                                        },
                                    }
                                    if sticker.creator_id == session().user.id {
                                        button {
                                            class: "sticker-delete",
                                            title: "Delete sticker",
                                            onclick: {
                                                let id = sticker.id;
                                                move |_| {
                                                    spawn(async move {
                                                        if let Err(e) = api::delete_sticker(&session(), id).await {
                                                            status.set(e);
                                                        }
                                                    });
                                                }
                                            },
                                            "✕"
                                        }
                                    }
                                }
                            }
                            button {
                                class: "sticker-add",
                                title: "Add a sticker from an image file",
                                onclick: move |_| {
                                    spawn(async move {
                                        let Some(file) = rfd::AsyncFileDialog::new()
                                            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                                            .pick_file()
                                            .await
                                        else {
                                            return;
                                        };
                                        let name = file.file_name();
                                        let bytes = file.read().await;
                                        if bytes.len() > 8 * 1024 * 1024 {
                                            status.set("sticker too large (max 8 MB)".into());
                                            return;
                                        }
                                        let sticker_name = name
                                            .rsplit_once('.')
                                            .map(|(stem, _)| stem.to_owned())
                                            .unwrap_or_else(|| name.clone());
                                        match api::upload(&session(), &name, bytes).await {
                                            Ok(url) => {
                                                // Arrives back via the StickerCreated broadcast.
                                                if let Err(e) = api::create_sticker(&session(), sticker_name, url).await {
                                                    status.set(e);
                                                }
                                            }
                                            Err(e) => status.set(e),
                                        }
                                    });
                                },
                                "+"
                            }
                        }
                    }
                }
                if let Some(target) = react_target() {
                    div { class: "react-palette",
                        span { class: "react-palette-label", "React:" }
                        for emoji in REACTION_EMOJIS {
                            button {
                                key: "{emoji}",
                                onclick: move |_| {
                                    ws.send(ClientEvent::ToggleReaction {
                                        message_id: target,
                                        emoji: emoji.to_string(),
                                    });
                                    react_target.set(None);
                                },
                                "{emoji}"
                            }
                        }
                        button {
                            class: "react-palette-close",
                            onclick: move |_| react_target.set(None),
                            "✕"
                        }
                    }
                }
                if !pending_files().is_empty() {
                    div { class: "pending-row",
                        for (i, file) in pending_files().into_iter().enumerate() {
                            div { key: "{i}", class: "pending-card",
                                if let Some(preview) = file.preview.clone() {
                                    img { class: "pending-thumb", src: "{preview}" }
                                } else {
                                    span { class: "pending-icon", Icon { name: "file", size: 22 } }
                                }
                                span { class: "pending-name", "{file.name}" }
                                button {
                                    class: "pending-remove",
                                    title: "Remove",
                                    onclick: move |_| {
                                        pending_files.write().remove(i);
                                    },
                                    "✕"
                                }
                            }
                        }
                        span { class: "pending-hint", "Enter to send · Esc to cancel" }
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
                        class: "attach sticker-btn",
                        title: "Send a sticker",
                        onclick: move |_| sticker_open.set(!sticker_open()),
                        Icon { name: "tag" }
                    }
                    button {
                        class: "attach",
                        title: "Upload a file, image, or GIF",
                        disabled: uploading(),
                        onclick: move |_| {
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
                                pending_files.write().push(make_pending(name, bytes));
                            });
                        },
                        if uploading() { "…" } else { Icon { name: "plus" } }
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
                            } else if e.key() == Key::Escape {
                                pending_files.set(Vec::new());
                            } else if e.key() == Key::Character("v".into())
                                && e.modifiers().contains(Modifiers::CONTROL)
                            {
                                paste_image();
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
                        onclick: {
                            let user_id = member.user.id;
                            move |_| {
                                spawn(async move {
                                    match api::profile(&session(), user_id).await {
                                        Ok(p) => profile_card.set(Some(p)),
                                        Err(e) => status.set(e),
                                    }
                                });
                            }
                        },
                        UserAvatar { user: member.user.clone(), class: "member-avatar" }
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

/// Split a message into inline images, inline videos, file attachments, and text.
fn extract_media(content: &str) -> (Vec<String>, Vec<String>, Vec<String>, String) {
    let is_url = |w: &str| w.starts_with("http://") || w.starts_with("https://");
    let has_ext = |w: &str, exts: &[&str]| {
        exts.iter().any(|ext| w.to_lowercase().ends_with(&format!(".{ext}")))
    };
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut files = Vec::new();
    let mut rest = Vec::new();
    for word in content.split_whitespace() {
        if is_url(word) && has_ext(word, &["gif", "png", "jpg", "jpeg", "webp"]) {
            images.push(word.to_owned());
        } else if is_url(word) && has_ext(word, &["webm", "mp4", "mov"]) {
            videos.push(word.to_owned());
        } else if is_url(word) && word.contains("/files/") {
            files.push(word.to_owned());
        } else {
            rest.push(word);
        }
    }
    if images.is_empty() && videos.is_empty() && files.is_empty() {
        (images, videos, files, content.to_owned())
    } else {
        (images, videos, files, rest.join(" "))
    }
}

const REACTION_EMOJIS: &[&str] = &[
    "👍", "👎", "😂", "❤️", "🔥", "😮", "😭", "🎉", "💀", "👀", "🤡", "🫡",
];

#[component]
fn MessageRow(msg: Message, compact: bool) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut lightbox = use_context::<Signal<Option<String>>>();
    let mut react_target = use_context::<Signal<Option<i64>>>();
    let mut editing = use_signal(|| false);
    let mut edit_draft = use_signal(String::new);

    let hue = avatar_hue(msg.author.id);
    let (images, videos, files, text) = extract_media(&msg.content);
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
                    onclick: move |_| {
                        react_target.set(if react_target() == Some(msg_id) { None } else { Some(msg_id) });
                    },
                    Icon { name: "smile", size: 16 }
                }
                if own {
                    button {
                        title: "Edit",
                        onclick: move |_| {
                            edit_draft.set(content_for_edit.clone());
                            editing.set(true);
                        },
                        Icon { name: "edit", size: 16 }
                    }
                    button {
                        title: "Delete",
                        onclick: move |_| ws.send(ClientEvent::DeleteMessage { message_id: msg_id }),
                        Icon { name: "trash", size: 16 }
                    }
                }
            }
            if compact {
                div { class: "msg-gutter" }
            } else {
                UserAvatar { user: msg.author.clone(), class: "avatar" }
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
                    img {
                        key: "{i}",
                        class: "msg-img",
                        src: "{src}",
                        loading: "lazy",
                        onclick: {
                            let src = src.clone();
                            move |_| lightbox.set(Some(src.clone()))
                        },
                    }
                }
                for (i, src) in videos.into_iter().enumerate() {
                    video {
                        key: "v{i}",
                        class: "msg-video",
                        src: "{src}",
                        autoplay: true,
                        muted: true,
                        r#loop: true,
                        controls: true,
                        preload: "metadata",
                    }
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
                                span { class: "msg-file-icon", Icon { name: "file", size: 22 } }
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

/// Live mic input level bar. Isolated so 10Hz meter ticks only re-render this.
#[component]
fn MicMeter(level: voice::MicLevelSignal) -> Element {
    let pct = (level() * 100.0).clamp(0.0, 100.0);
    rsx! {
        div { class: "mic-meter",
            div { class: "mic-meter-fill", style: "width: {pct}%" }
        }
    }
}

/// Avatar image if the user has one, else a colored initial circle.
/// `class` supplies the size (avatar / member-avatar / voice-avatar / profile-avatar).
#[component]
fn UserAvatar(user: User, class: String) -> Element {
    match user.avatar.clone() {
        Some(url) => rsx! { img { class: "{class} avatar-img", src: "{url}" } },
        None => rsx! {
            span {
                class: "{class}",
                style: "background: hsl({avatar_hue(user.id)}, 55%, 42%)",
                {initial(&user.username)}
            }
        },
    }
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

/// Replace the running executable with `new_exe_bytes` and restart.
/// Windows allows renaming a running exe, so: rename self aside, write the
/// new binary at the original path, spawn it, exit.
fn apply_self_update(new_exe_bytes: Vec<u8>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = exe.with_file_name("NotDiscord.old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("could not stage update: {e}"))?;
    if let Err(e) = std::fs::write(&exe, &new_exe_bytes) {
        // Roll back so the app still launches next time.
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("could not write update: {e}"));
    }
    std::process::Command::new(&exe)
        .spawn()
        .map_err(|e| format!("update installed but relaunch failed: {e}"))?;
    std::process::exit(0);
}

fn format_date(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%b %-d, %Y").to_string())
        .unwrap_or_default()
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
