//! NotDiscord in the browser — the phone client. Milestone 1: text, images,
//! DMs, live updates. Voice/video need the browser's WebRTC path and come
//! later. Served by the server itself at /app, installable as a PWA.

mod api;
mod icons;
mod md;

use std::collections::HashMap;

use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use gloo_storage::Storage;
use icons::Icon;
use serde::Deserialize;
use shared::{Channel, ClientEvent, Message, MusicState, ServerEvent, User, UserStatus};
use wasm_bindgen::prelude::*;

// The voice glue (pwa/voice.js) wraps livekit-client; state comes back as
// JSON by polling, so nothing async crosses the wasm boundary except calls.
#[wasm_bindgen(js_namespace = ndVoice)]
extern "C" {
    #[wasm_bindgen(js_name = join)]
    fn voice_join_js(url: &str, token: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = leave)]
    fn voice_leave_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = setMuted)]
    fn voice_set_muted_js(muted: bool) -> js_sys::Promise;
    #[wasm_bindgen(js_name = setDeafened)]
    fn voice_set_deafened_js(deafened: bool) -> js_sys::Promise;
    #[wasm_bindgen(js_name = setVolume)]
    fn voice_set_volume_js(identity: &str, volume: f64);
    #[wasm_bindgen(js_name = getState)]
    fn voice_get_state_js() -> String;
}

// Push subscription glue (pwa/push.js).
#[wasm_bindgen(js_namespace = ndPush)]
extern "C" {
    #[wasm_bindgen(js_name = current)]
    fn push_current_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = enable)]
    fn push_enable_js(vapid_key: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = disable)]
    fn push_disable_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = getState)]
    fn push_state_js() -> String;
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct PushGlue {
    supported: bool,
    permission: String,
    endpoint: String,
    #[serde(default)]
    error: String,
}

/// The browser's PushSubscription, as push.js hands it over.
#[derive(Debug, Clone, Deserialize)]
struct PushSub {
    endpoint: String,
    p256dh: String,
    auth: String,
}

impl From<PushSub> for shared::PushSubscribeRequest {
    fn from(s: PushSub) -> Self {
        Self { endpoint: s.endpoint, p256dh: s.p256dh, auth: s.auth }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct VoicePeer {
    identity: String,
    name: String,
    speaking: bool,
    local: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct VoiceGlue {
    connected: bool,
    connecting: bool,
    error: String,
    muted: bool,
    #[serde(default)]
    deafened: bool,
    participants: Vec<VoicePeer>,
}

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
        // livekit-client + our glue, self-hosted next to the bundle.
        document::Script { src: "/app/livekit-client.umd.min.js" }
        document::Script { src: "/app/voice.js" }
        document::Script { src: "/app/push.js" }
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
    // The right-side members panel, collapsed by default on a phone.
    let mut members_open = use_signal(|| false);
    // "chat" | "music" — the phone works as a remote for the music bot.
    let mut tab = use_signal(|| "chat");
    let mut music = use_signal(MusicState::default);
    // user_id -> voice channel_id, for the 🔊 pills in the members panel.
    let mut voice_users = use_signal(HashMap::<i64, i64>::new);
    // The voice channel THIS device is connected to (id, name), plus the
    // glue's live view of the room.
    let mut voice_conn = use_signal(|| None::<(i64, String)>);
    let mut voice_glue = use_signal(VoiceGlue::default);
    // Settings sheet (notifications live here).
    let mut settings_open = use_signal(|| false);
    let mut notify = use_signal(|| None::<shared::NotifyPrefs>);
    let mut push_glue = use_signal(PushGlue::default);
    let mut notify_msg = use_signal(String::new);

    // Load prefs when the sheet opens: the browser's current subscription
    // decides whether this device shows as on.
    let mut load_notify = move || {
        // Support and permission are known synchronously — show them before
        // the (slower) subscription lookup, so the sheet never opens claiming
        // the browser can't do notifications when it can.
        if let Ok(state) = serde_json::from_str::<PushGlue>(&push_state_js()) {
            push_glue.set(state);
        }
        spawn(async move {
            let sub_json = wasm_bindgen_futures::JsFuture::from(push_current_js())
                .await
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let endpoint = serde_json::from_str::<PushSub>(&sub_json)
                .map(|s| s.endpoint)
                .unwrap_or_default();
            if let Ok(state) = serde_json::from_str::<PushGlue>(&push_state_js()) {
                push_glue.set(state);
            }
            match api::notify_prefs(&sess(), &endpoint).await {
                Ok(prefs) => notify.set(Some(prefs)),
                Err(e) => notify_msg.set(e),
            }
        });
    };
    // channel_id -> unread count, for drawer badges.
    let mut unread = use_signal(HashMap::<i64, i64>::new);
    // The message being replied to, shared with MessageRow via context.
    let replying = use_context_provider(|| Signal::new(None::<Message>));
    let mut replying = replying;
    let me_id = sess().user.id;


    let refresh_music = move || {
        spawn(async move {
            if let Ok(state) = api::music_state(&sess()).await {
                music.set(state);
            }
        });
    };

    // While the music tab is up, keep position/queue roughly current.
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(4000).await;
            if *tab.peek() == "music" {
                if let Ok(state) = api::music_state(&sess()).await {
                    music.set(state);
                }
            }
        }
    });

    let mut open_channel = move |channel: Channel| {
        let id = channel.id;
        unread.write().remove(&id);
        replying.set(None);
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
            if let Ok(list) = api::unread(&sess()).await {
                unread.set(list.into_iter().filter(|u| u.count > 0).map(|u| (u.channel_id, u.count)).collect());
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
            // A reconnect wiped our server-side voice presence (it's
            // connection-scoped) — re-announce if we're still in a call.
            if let Some((id, _)) = voice_conn.peek().clone() {
                if let Ok(text) = serde_json::to_string(&ClientEvent::VoiceState {
                    channel_id: Some(id),
                    sharing: false,
                    camera: false,
                }) {
                    let _ = sink.send(gloo_net::websocket::Message::Text(text)).await;
                }
            }
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
                                } else if message.author.id != me_id {
                                    *unread.write().entry(message.channel_id).or_insert(0) += 1;
                                }
                            }
                            ServerEvent::ReactionAdded { channel_id, message_id, emoji, user_id } => {
                                if selected.peek().as_ref().map(|c| c.id) == Some(channel_id) {
                                    if let Some(m) = messages.write().iter_mut().find(|m| m.id == message_id) {
                                        m.reactions.push(shared::ReactionEntry { emoji, user_id });
                                    }
                                }
                            }
                            ServerEvent::ReactionRemoved { channel_id, message_id, emoji, user_id } => {
                                if selected.peek().as_ref().map(|c| c.id) == Some(channel_id) {
                                    if let Some(m) = messages.write().iter_mut().find(|m| m.id == message_id) {
                                        m.reactions.retain(|r| !(r.emoji == emoji && r.user_id == user_id));
                                    }
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
                            ServerEvent::UserUpdated { user } => {
                                if let Some(m) = members.write().iter_mut().find(|m| m.user.id == user.id) {
                                    m.user = user;
                                }
                            }
                            ServerEvent::VoiceSnapshot { entries } => {
                                voice_users.set(entries.into_iter().map(|e| (e.user.id, e.channel_id)).collect());
                            }
                            ServerEvent::VoiceStateChanged { user, channel_id, .. } => {
                                match channel_id {
                                    Some(id) => {
                                        voice_users.write().insert(user.id, id);
                                    }
                                    None => {
                                        voice_users.write().remove(&user.id);
                                    }
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

    // Poll the JS glue while in a call: speaking rings, errors, and dropped
    // connections all surface here.
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(700).await;
            if voice_conn.peek().is_none() {
                continue;
            }
            let Ok(state) = serde_json::from_str::<VoiceGlue>(&voice_get_state_js()) else {
                continue;
            };
            let dropped = !state.connected && !state.connecting;
            if *voice_glue.peek() != state {
                voice_glue.set(state);
            }
            if dropped {
                voice_conn.set(None);
                status.set("voice disconnected".into());
                ws.send(ClientEvent::VoiceState { channel_id: None, sharing: false, camera: false });
            }
        }
    });

    let join_voice = move |channel: Channel| {
        spawn(async move {
            status.set(String::new());
            match api::voice_token(&sess(), channel.id).await {
                Ok(grant) => {
                    let _ = wasm_bindgen_futures::JsFuture::from(voice_join_js(&grant.url, &grant.token)).await;
                    let state: VoiceGlue =
                        serde_json::from_str(&voice_get_state_js()).unwrap_or_default();
                    if state.connected {
                        voice_conn.set(Some((channel.id, channel.name.clone())));
                        drawer.set(false);
                        ws.send(ClientEvent::VoiceState {
                            channel_id: Some(channel.id),
                            sharing: false,
                            camera: false,
                        });
                    }
                    if !state.error.is_empty() {
                        status.set(state.error.clone());
                    }
                    voice_glue.set(state);
                }
                Err(e) => status.set(e),
            }
        });
    };

    let leave_voice = move |_| {
        spawn(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(voice_leave_js()).await;
            voice_conn.set(None);
            voice_glue.set(VoiceGlue::default());
            ws.send(ClientEvent::VoiceState { channel_id: None, sharing: false, camera: false });
        });
    };

    let mut send = move |_| {
        let content = draft().trim().to_owned();
        let Some(channel) = selected() else { return };
        if content.is_empty() {
            return;
        }
        draft.set(String::new());
        // On the music tab a bare link queues instead of chatting, exactly
        // like the desktop music tab (no spam in the channel).
        let is_link = (content.starts_with("http://") || content.starts_with("https://"))
            && !content.contains(' ');
        if *tab.peek() == "music" && is_link {
            spawn(async move {
                if let Err(e) = api::music_play(&sess(), channel.id, content).await {
                    status.set(e);
                }
                if let Ok(s) = api::music_state(&sess()).await {
                    music.set(s);
                }
            });
            return;
        }
        let reply_to = replying.peek().as_ref().map(|m| m.id);
        replying.set(None);
        ws.send(ClientEvent::SendMessage { channel_id: channel.id, content, reply_to });
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

    let mic_icon: &'static str = if voice_glue().muted { "mic-off" } else { "mic" };
    let deafen_icon: &'static str = if voice_glue().deafened { "headphones-off" } else { "headphones" };
    let selected_label = match selected() {
        Some(c) if c.kind == "dm" => format!("@{}", dm_peer(&c, me_id)),
        Some(c) => format!("# {}", c.name),
        None => "NotDiscord".into(),
    };

    rsx! {
        div { class: "app",
            header { class: "topbar",
                button { class: "burger", onclick: move |_| drawer.set(!drawer()),
                    Icon { name: "menu", size: 20 }
                    if !unread().is_empty() {
                        span { class: "live-dot unread-dot" }
                    }
                }
                div { class: "topbar-title", "{selected_label}" }
                button {
                    class: if tab() == "music" { "topbtn active" } else { "topbtn" },
                    onclick: move |_| {
                        if tab() == "music" {
                            tab.set("chat");
                        } else {
                            tab.set("music");
                            refresh_music();
                        }
                    },
                    Icon { name: "music", size: 18 }
                    if music().active && !music().paused {
                        span { class: "live-dot" }
                    }
                }
                button {
                    class: if members_open() { "topbtn active" } else { "topbtn" },
                    onclick: move |_| members_open.set(!members_open()),
                    Icon { name: "user", size: 18 }
                }
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
                            span { class: "grow", "# {channel.name}" }
                            if let Some(n) = unread().get(&channel.id).copied() {
                                span { class: "unread-badge", "{n}" }
                            }
                        }
                    }
                    div { class: "drawer-section", "Voice" }
                    for channel in channels().into_iter().filter(|c| c.kind == "voice") {
                        {
                            let here: Vec<String> = voice_users()
                                .iter()
                                .filter(|(_, chan)| **chan == channel.id)
                                .filter_map(|(uid, _)| {
                                    members().iter().find(|m| m.user.id == *uid).map(|m| m.user.username.clone())
                                })
                                .collect();
                            let in_this = voice_conn().map(|(id, _)| id) == Some(channel.id);
                            let channel_for_join = channel.clone();
                            rsx! {
                                button {
                                    key: "v{channel.id}",
                                    class: if in_this { "drawer-chan active" } else { "drawer-chan" },
                                    onclick: move |_| {
                                        if voice_conn.peek().as_ref().map(|(id, _)| *id) != Some(channel_for_join.id) {
                                            join_voice(channel_for_join.clone());
                                        }
                                    },
                                    Icon { name: "volume", size: 15 }
                                    span { class: "person-name", "{channel.name}" }
                                    if !here.is_empty() {
                                        span { class: "person-status", {here.join(", ")} }
                                    }
                                }
                            }
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
                            // Unread on the DM with this person, if one exists.
                            if let Some(n) = channels()
                                .iter()
                                .find(|c| c.kind == "dm" && c.dm_members.iter().any(|u| u.id == member.user.id))
                                .and_then(|c| unread().get(&c.id).copied())
                            {
                                span { class: "unread-badge", "{n}" }
                            }
                        }
                    }
                    button {
                        class: "drawer-logout",
                        onclick: move |_| {
                            drawer.set(false);
                            settings_open.set(true);
                            notify_msg.set(String::new());
                            load_notify();
                        },
                        Icon { name: "settings", size: 15 }
                        " Settings"
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

            if tab() == "music" {
                main { class: "music-view",
                    {
                        let state = music();
                        let play_icon: &'static str = if state.paused { "play" } else { "pause" };
                        rsx! {
                            div { class: "np-card",
                                if let Some(track) = state.now_playing.clone() {
                                    if let Some(art) = track.art.clone() {
                                        img { class: "np-art", src: "{art}" }
                                    }
                                    div { class: "np-text",
                                        div { class: "np-title", "{track.title}" }
                                        if !track.artist.is_empty() {
                                            div { class: "np-artist", "{track.artist}" }
                                        }
                                        if let Some(total) = track.duration {
                                            div { class: "np-progress",
                                                div {
                                                    class: "np-progress-fill",
                                                    style: "width: {(state.position / total * 100.0).clamp(0.0, 100.0)}%",
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    div { class: "np-empty",
                                        "Nothing playing — paste a link below. The music plays in the voice channel, so this works as a remote."
                                    }
                                }
                            }
                            if state.active {
                                div { class: "music-controls",
                                    button {
                                        class: "mbtn",
                                        onclick: move |_| {
                                            let action = if music.peek().paused { "resume" } else { "pause" };
                                            spawn(async move {
                                                if let Err(e) = api::music_control(&sess(), action).await {
                                                    status.set(e);
                                                }
                                                if let Ok(s) = api::music_state(&sess()).await {
                                                    music.set(s);
                                                }
                                            });
                                        },
                                        Icon { name: play_icon, size: 18 }
                                    }
                                    button {
                                        class: "mbtn",
                                        onclick: move |_| {
                                            spawn(async move {
                                                let _ = api::music_control(&sess(), "skip").await;
                                                if let Ok(s) = api::music_state(&sess()).await {
                                                    music.set(s);
                                                }
                                            });
                                        },
                                        Icon { name: "skip", size: 18 }
                                    }
                                    button {
                                        class: "mbtn stop",
                                        onclick: move |_| {
                                            spawn(async move {
                                                let _ = api::music_control(&sess(), "stop").await;
                                                if let Ok(s) = api::music_state(&sess()).await {
                                                    music.set(s);
                                                }
                                            });
                                        },
                                        Icon { name: "stop", size: 18 }
                                    }
                                }
                            }
                            div { class: "queue-head", "Up next" }
                            if state.queue.is_empty() {
                                div { class: "np-empty", "queue's empty" }
                            }
                            for track in state.queue.clone() {
                                div { key: "{track.id}", class: "queue-row",
                                    div { class: "queue-title", "{track.title}" }
                                    button {
                                        class: "queue-x",
                                        onclick: {
                                            let id = track.id;
                                            move |_| {
                                                spawn(async move {
                                                    let _ = api::music_queue(&sess(), shared::MusicQueueRequest {
                                                        action: "remove".into(),
                                                        id: None,
                                                        offset: None,
                                                        ids: vec![id],
                                                    }).await;
                                                    if let Ok(s) = api::music_state(&sess()).await {
                                                        music.set(s);
                                                    }
                                                });
                                            }
                                        },
                                        Icon { name: "x", size: 14 }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                main { class: "messages",
                    // column-reverse pins the view to the newest message.
                    for msg in messages().into_iter().rev() {
                        MessageRow { key: "{msg.id}", msg, me_id }
                    }
                    if has_more() {
                        button { class: "load-older", onclick: load_older, "Load older messages" }
                    }
                }
            }

            if settings_open() {
                div { class: "drawer-overlay", onclick: move |_| settings_open.set(false) }
                div { class: "sheet",
                    div { class: "sheet-head",
                        div { class: "sheet-title", "Settings" }
                        button { class: "sheet-x", onclick: move |_| settings_open.set(false),
                            Icon { name: "x", size: 14 }
                        }
                    }
                    div { class: "sheet-section", "Notifications" }
                    {
                        let glue = push_glue();
                        let prefs = notify();
                        let on = prefs.as_ref().is_some_and(|p| p.subscribed) && glue.permission == "granted";
                        let level = prefs.as_ref().map(|p| p.level.clone()).unwrap_or_else(|| "mentions".into());
                        let vapid = prefs.as_ref().map(|p| p.vapid_key.clone()).unwrap_or_default();
                        rsx! {
                            if !glue.supported {
                                div { class: "sheet-hint",
                                    "This browser can't do notifications. On iPhone, add NotDiscord to your Home Screen first."
                                }
                            } else {
                                button {
                                    class: if on { "sheet-toggle on" } else { "sheet-toggle" },
                                    onclick: move |_| {
                                        let vapid = vapid.clone();
                                        spawn(async move {
                                            notify_msg.set(String::new());
                                            if on {
                                                let json = wasm_bindgen_futures::JsFuture::from(push_disable_js())
                                                    .await.ok().and_then(|v| v.as_string()).unwrap_or_default();
                                                if let Ok(sub) = serde_json::from_str::<PushSub>(&json) {
                                                    let _ = api::push_unsubscribe(&sess(), sub.into()).await;
                                                }
                                                notify_msg.set("notifications off on this device".into());
                                            } else {
                                                let json = wasm_bindgen_futures::JsFuture::from(push_enable_js(&vapid))
                                                    .await.ok().and_then(|v| v.as_string()).unwrap_or_default();
                                                match serde_json::from_str::<PushSub>(&json) {
                                                    Ok(sub) => match api::push_subscribe(&sess(), sub.into()).await {
                                                        Ok(()) => notify_msg.set("notifications on for this device".into()),
                                                        Err(e) => notify_msg.set(e),
                                                    },
                                                    Err(_) => {
                                                        let state: PushGlue = serde_json::from_str(&push_state_js()).unwrap_or_default();
                                                        notify_msg.set(if state.error.is_empty() {
                                                            "notifications weren't allowed".into()
                                                        } else {
                                                            state.error
                                                        });
                                                    }
                                                }
                                            }
                                            load_notify();
                                        });
                                    },
                                    if on {
                                        Icon { name: "check", size: 15 }
                                        " Notifications are on for this device"
                                    } else {
                                        Icon { name: "user", size: 15 }
                                        " Turn on notifications for this device"
                                    }
                                }
                            }
                            div { class: "sheet-section", "Notify me about" }
                            for (value, label, hint) in [
                                ("all", "Everything", "every message in every channel"),
                                ("mentions", "Mentions & DMs", "when someone @s you or messages you directly"),
                                ("none", "Nothing", "no notifications at all"),
                            ] {
                                button {
                                    key: "{value}",
                                    class: if level == value { "sheet-option picked" } else { "sheet-option" },
                                    onclick: move |_| {
                                        spawn(async move {
                                            match api::set_notify_level(&sess(), value).await {
                                                Ok(()) => {
                                                    notify_msg.set(String::new());
                                                    load_notify();
                                                }
                                                Err(e) => notify_msg.set(e),
                                            }
                                        });
                                    },
                                    div { class: "sheet-option-main",
                                        div { class: "sheet-option-label", "{label}" }
                                        div { class: "sheet-option-hint", "{hint}" }
                                    }
                                    if level == value {
                                        Icon { name: "check", size: 15 }
                                    }
                                }
                            }
                            div { class: "sheet-hint",
                                "Notifications only arrive while the app is closed — if you're already looking, you'd just see the message."
                            }
                            if !notify_msg().is_empty() {
                                div { class: "sheet-note", "{notify_msg}" }
                            }
                        }
                    }
                }
            }

            if members_open() {
                div { class: "drawer-overlay", onclick: move |_| members_open.set(false) }
                aside { class: "members-panel",
                    div { class: "drawer-section", "Members" }
                    for member in members() {
                        div { key: "m{member.user.id}", class: "member-row",
                            Avatar { user: member.user.clone() }
                            div { class: "member-col",
                                div { class: "member-line",
                                    span { class: "member-name", "{member.user.username}" }
                                    if member.user.role == "admin" {
                                        span { class: "role-badge", "ADMIN" }
                                    }
                                    if voice_users().contains_key(&member.user.id) {
                                        span { class: "voice-pill", Icon { name: "volume", size: 13 } }
                                    }
                                }
                                if let Some(text) = member.status.clone() {
                                    div { class: "member-status", "{text}" }
                                }
                            }
                            span { class: if member.online { "dot online" } else { "dot" } }
                        }
                    }
                }
            }

            if let Some((_, chan_name)) = voice_conn() {
                div { class: "voice-bar",
                    div { class: "voice-bar-top",
                        Icon { name: "volume", size: 16 }
                        div { class: "voice-bar-chan", "{chan_name}" }
                        span { class: "grow" }
                        button {
                            class: if voice_glue().muted { "vbtn muted" } else { "vbtn" },
                            onclick: move |_| {
                                let now_muted = !voice_glue.peek().muted;
                                spawn(async move {
                                    let _ = wasm_bindgen_futures::JsFuture::from(voice_set_muted_js(now_muted)).await;
                                });
                            },
                            Icon { name: mic_icon, size: 16 }
                        }
                        button {
                            class: if voice_glue().deafened { "vbtn muted" } else { "vbtn" },
                            onclick: move |_| {
                                let now = !voice_glue.peek().deafened;
                                spawn(async move {
                                    let _ = wasm_bindgen_futures::JsFuture::from(voice_set_deafened_js(now)).await;
                                });
                            },
                            Icon { name: deafen_icon, size: 16 }
                        }
                        button { class: "vbtn leave", onclick: leave_voice,
                            Icon { name: "phone-off", size: 16 }
                        }
                    }
                    div { class: "voice-bar-peers",
                        for p in voice_glue().participants {
                            div {
                                key: "{p.identity}",
                                class: if p.speaking { "peer-row speaking" } else { "peer-row" },
                                span { class: "peer-name", "{p.name}" }
                                if !p.local {
                                    input {
                                        r#type: "range",
                                        class: "peer-volume",
                                        min: "0",
                                        max: "200",
                                        value: "100",
                                        oninput: {
                                            let identity = p.identity.clone();
                                            move |e: Event<FormData>| {
                                                if let Ok(v) = e.value().parse::<f64>() {
                                                    voice_set_volume_js(&identity, v / 100.0);
                                                }
                                            }
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !status().is_empty() {
                div { class: "statusbar", "{status}" }
            }

            if let Some(target) = replying() {
                div { class: "reply-bar",
                    Icon { name: "reply", size: 13 }
                    span { class: "reply-bar-text",
                        "Replying to {target.author.username}: {target.content.chars().take(50).collect::<String>()}"
                    }
                    button { class: "reply-bar-x", onclick: move |_| replying.set(None),
                        Icon { name: "x", size: 13 }
                    }
                }
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
                    if uploading() {
                        "…"
                    } else {
                        Icon { name: "plus", size: 18 }
                    }
                }
                input {
                    class: "draft",
                    placeholder: if tab() == "music" { "Paste a link to queue it" } else { "Message" },
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            send(());
                        }
                    },
                }
                button { class: "send", onclick: move |_| send(()), Icon { name: "send", size: 16 } }
            }
        }
    }
}

#[component]
fn Avatar(user: User) -> Element {
    let hue = (user.id * 137) % 360;
    match user.avatar.clone() {
        Some(url) => rsx! { img { class: "avatar", src: "{url}" } },
        None => rsx! {
            div {
                class: "avatar initial",
                style: "background: hsl({hue}, 55%, 45%)",
                {user.username.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}
            }
        },
    }
}

/// The quick-react strip: the crew's high-traffic emojis.
const QUICK_REACTIONS: [&str; 6] = ["👍", "😂", "❤️", "😮", "😢", "🔥"];

#[component]
fn MessageRow(msg: Message, me_id: i64) -> Element {
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut replying = use_context::<Signal<Option<Message>>>();
    let mut strip_open = use_signal(|| false);
    let msg_id = msg.id;

    // The bot's music-player card is a desktop thing.
    if msg.content.starts_with(shared::PLAYER_MARKER) {
        return rsx! {
            div { class: "msg",
                div { class: "msg-body dim", "🎵 music player — open the desktop app for controls" }
            }
        };
    }
    let (images, videos, files, text) = extract_media(&msg.content);

    // Aggregate raw reaction entries into (emoji, count, reacted-by-me).
    let mut reaction_groups: Vec<(String, usize, bool)> = Vec::new();
    for entry in &msg.reactions {
        match reaction_groups.iter_mut().find(|(e, _, _)| *e == entry.emoji) {
            Some(g) => {
                g.1 += 1;
                g.2 |= entry.user_id == me_id;
            }
            None => reaction_groups.push((entry.emoji.clone(), 1, entry.user_id == me_id)),
        }
    }

    rsx! {
        div { class: "msg",
            div { class: "msg-head",
                span { class: "msg-author", "{msg.author.username}" }
                span { class: "msg-time", {format_time(msg.created_at)} }
                span { class: "grow" }
                button {
                    class: "msg-act",
                    onclick: move |_| strip_open.set(!strip_open()),
                    Icon { name: "smile", size: 14 }
                }
                button {
                    class: "msg-act",
                    onclick: {
                        let msg_for_reply = msg.clone();
                        move |_| replying.set(Some(msg_for_reply.clone()))
                    },
                    Icon { name: "reply", size: 14 }
                }
            }
            if let Some(preview) = msg.reply_preview.clone() {
                div { class: "reply-ref", "↩ {preview.author}: {preview.content.chars().take(60).collect::<String>()}" }
            }
            if !text.is_empty() {
                div { class: "msg-body",
                    md::Md { nodes: md::parse_markdown(&text) }
                }
            }
            for (i, src) in images.into_iter().enumerate() {
                {
                    // Phones especially shouldn't pull full-size photos to
                    // draw a 340px image.
                    let shown = if src.contains("/files/") { format!("{src}?thumb=1") } else { src.clone() };
                    rsx! {
                        a { key: "{i}", href: "{src}", target: "_blank",
                            img { class: "msg-img", src: "{shown}", loading: "lazy" }
                        }
                    }
                }
            }
            for (i, src) in videos.into_iter().enumerate() {
                video { key: "v{i}", class: "msg-video", src: "{src}", controls: true, preload: "metadata" }
            }
            for (i, (url, name)) in files.into_iter().enumerate() {
                a { key: "f{i}", class: "msg-file", href: "{url}", target: "_blank",
                    Icon { name: "file", size: 14 }
                    " {name}"
                }
            }
            if !reaction_groups.is_empty() || strip_open() {
                div { class: "reaction-row",
                    for (emoji, count, mine) in reaction_groups {
                        button {
                            key: "{emoji}",
                            class: if mine { "reaction-pill mine" } else { "reaction-pill" },
                            onclick: {
                                let emoji = emoji.clone();
                                move |_| ws.send(ClientEvent::ToggleReaction { message_id: msg_id, emoji: emoji.clone() })
                            },
                            "{emoji} {count}"
                        }
                    }
                    if strip_open() {
                        for quick in QUICK_REACTIONS {
                            button {
                                key: "q{quick}",
                                class: "reaction-pill quick",
                                onclick: move |_| {
                                    strip_open.set(false);
                                    ws.send(ClientEvent::ToggleReaction { message_id: msg_id, emoji: quick.to_string() });
                                },
                                "{quick}"
                            }
                        }
                    }
                }
            }
        }
    }
}
