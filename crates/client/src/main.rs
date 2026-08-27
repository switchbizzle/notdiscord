#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod emoji;
mod icons;
mod md;
mod menu;
mod camera;
mod share;
mod tray;
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

use shared::{Channel, ClientEvent, GifResult, Message, Profile, ServerEvent, Tag, UpdateProfileRequest, User, UserStatus};

fn main() {
    // Panics land in a crash log (release builds have no console to read).
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = api::config_root() {
            let path = dir.join("crash.log");
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let entry = format!(
                "[{}] thread '{}' {}\n{}\n\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                std::thread::current().name().unwrap_or("?"),
                info,
                std::backtrace::Backtrace::force_capture(),
            );
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(entry.as_bytes());
            }
        }
        default_panic(info);
    }));

    // A second instance is how updates break: the extra process keeps the
    // old exe locked ("could not stage update: Access is denied"). Close-to-
    // tray makes accidental double launches easy, so reveal the existing
    // window and bow out instead.
    ensure_single_instance();

    // Claim the tray/menu event slots before dioxus can (write-once OnceCells,
    // first setter wins) — otherwise tray menu clicks go nowhere.
    tray::claim_event_handlers();

    // Clean up binaries left behind by self-updates. Old versions staged as
    // NotDiscord.old.exe; current ones use unique names (NotDiscord.old-*.exe)
    // so a locked leftover can never block the next update.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("NotDiscord.old") && name.ends_with(".exe") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    let window = WindowBuilder::new()
        .with_title("NotDiscord")
        .with_inner_size(LogicalSize::new(1100.0, 720.0))
        .with_min_inner_size(LogicalSize::new(900.0, 560.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new()
                .with_window(window)
                .with_menu(None)
                // The webview's own menu is Back / Reload / Save as / Print —
                // none of which means anything here. `menu.rs` puts a menu
                // that fits the clicked element in its place.
                .with_disable_context_menu(true)
                // X hides to the system tray; the tray menu's Quit exits.
                .with_close_behaviour(dioxus::desktop::WindowCloseBehaviour::WindowHides),
        )
        .launch(App);
}

/// A destructive action awaiting user confirmation.
#[derive(Clone, PartialEq)]
enum ConfirmAction {
    DeleteChannel { id: i64, name: String },
    SetBan { user_id: i64, username: String, banned: bool },
    SetRole { user_id: i64, username: String, make_admin: bool },
    DeleteTag { id: i64, name: String },
    DeleteSticker { id: i64, name: String },
}

impl ConfirmAction {
    fn description(&self) -> String {
        match self {
            Self::DeleteChannel { name, .. } => {
                format!("Delete #{name}? All of its messages will be permanently deleted.")
            }
            Self::SetBan { username, banned: true, .. } => {
                format!("Ban {username}? They'll be logged out immediately and can't log back in.")
            }
            Self::SetBan { username, banned: false, .. } => format!("Unban {username}?"),
            Self::SetRole { username, make_admin: true, .. } => {
                format!("Make {username} an admin? They'll be able to delete anyone's messages, delete channels, and ban members.")
            }
            Self::SetRole { username, make_admin: false, .. } => {
                format!("Remove {username}'s admin role?")
            }
            Self::DeleteTag { name, .. } => format!("Delete the \"{name}\" tag from everyone?"),
            Self::DeleteSticker { name, .. } => format!("Delete the \"{name}\" sticker for everyone?"),
        }
    }

    fn confirm_label(&self) -> &'static str {
        match self {
            Self::DeleteChannel { .. } => "Delete channel",
            Self::SetBan { banned: true, .. } => "Ban",
            Self::SetBan { banned: false, .. } => "Unban",
            Self::SetRole { make_admin: true, .. } => "Make admin",
            Self::SetRole { make_admin: false, .. } => "Remove admin",
            Self::DeleteTag { .. } => "Delete tag",
            Self::DeleteSticker { .. } => "Delete sticker",
        }
    }
}

/// A track/video playing in the media dock (from a link preview card).
#[derive(Clone, PartialEq)]
struct NowPlaying {
    embed: String,
    height: i64,
    title: String,
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
    let mut servers = use_context_provider(|| Signal::new(api::load_servers()));
    let mut adding = use_signal(|| false);
    let mut restoring = use_signal(|| true);
    let window = use_window();

    // System tray: created once, lives for the app's lifetime.
    let tray_handle: tray::TrayHandle = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(tray::create())));
    let tray_unread = use_context_provider(|| Signal::new(false));

    // Reflect unread state on the tray icon.
    {
        let tray_handle = tray_handle.clone();
        use_effect(move || {
            let unread = tray_unread();
            if let Some(tray) = tray_handle.borrow().as_ref() {
                tray.set_unread(unread);
            }
        });
    }

    // Tray menu (Open/Quit) + icon clicks, drained from the queue our
    // main()-registered handlers fill.
    {
        let tray_handle = tray_handle.clone();
        let window = window.clone();
        use_future(move || {
            let tray_handle = tray_handle.clone();
            let window = window.clone();
            async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                    for action in tray::poll_events(&tray_handle) {
                        match action {
                            tray::TrayAction::Show => {
                                window.window.set_visible(true);
                                window.window.set_minimized(false);
                                window.window.set_focus();
                            }
                            tray::TrayAction::Quit => quit_now("quitting from tray"),
                        }
                    }
                }
            }
        });
    }

    // Belt and braces: dioxus forwards tray menu events too, in case its
    // registration ever wins the race.
    {
        let tray_handle = tray_handle.clone();
        let window = window.clone();
        dioxus::desktop::use_tray_menu_event_handler(move |event| {
            let borrow = tray_handle.borrow();
            let Some(tray) = borrow.as_ref() else { return };
            if event.id() == &tray.open_id {
                window.window.set_visible(true);
                window.window.set_minimized(false);
                window.window.set_focus();
            } else if event.id() == &tray.quit_id {
                quit_now("quitting from tray (dioxus path)");
            }
        });
    }

    // Resume the active saved server: validate the token and refresh the
    // user + server identity.
    use_future(move || async move {
        let active = servers.peek().active_session().cloned();
        if let Some(saved) = active {
            match api::me(&saved).await {
                Ok(user) => {
                    let mut refreshed = api::Session { user, ..saved };
                    if let Ok(info) = api::server_info(&refreshed.base_url).await {
                        refreshed.server_name = info.name;
                        refreshed.server_id = info.id;
                    }
                    api::update_saved_server(&refreshed);
                    servers.set(api::load_servers());
                }
                Err(_) => adding.set(true),
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
            {
                let file = servers();
                let active = file.active_session().cloned();
                match active {
                    Some(s) if !adding() => rsx! {
                        div { class: "shell",
                            ServerRail { adding }
                            MainView { key: "{s.base_url}", session: s }
                        }
                    },
                    _ => rsx! {
                        div { class: "shell",
                            if !file.servers.is_empty() {
                                ServerRail { adding }
                            }
                            LoginView { adding }
                        }
                    },
                }
            }
        }
    }
}

/// Discord-style far-left rail: one circle per saved server, + to add.
#[component]
fn ServerRail(adding: Signal<bool>) -> Element {
    let mut servers = use_context::<Signal<api::ServersFile>>();
    let file = servers();
    rsx! {
        div { class: "server-rail",
            for (i, s) in file.servers.iter().enumerate() {
                button {
                    key: "{s.base_url}",
                    class: if i == file.active && !adding() { "rail-server active" } else { "rail-server" },
                    title: "{s.server_name} — {s.user.username}",
                    style: "background: hsl({rail_hue(s)}, 55%, 42%)",
                    onclick: move |_| {
                        let mut file = api::load_servers();
                        file.active = i;
                        api::save_servers(&file);
                        servers.set(file);
                        adding.set(false);
                    },
                    if let Some(icon) = s.server_icon.clone() {
                        img { class: "rail-img", src: "{icon}" }
                    } else {
                        {initial(&s.server_name)}
                    }
                }
            }
            button {
                class: "rail-add",
                title: "Add a server",
                onclick: move |_| adding.set(true),
                "+"
            }
        }
    }
}

fn rail_hue(session: &api::Session) -> i64 {
    let seed: i64 = session.server_id.bytes().map(|b| b as i64).sum::<i64>()
        + session.base_url.bytes().map(|b| b as i64).sum::<i64>();
    (seed * 37) % 360
}

#[component]
fn LoginView(adding: Signal<bool>) -> Element {
    let mut servers = use_context::<Signal<api::ServersFile>>();
    let mut base_url = use_signal(move || {
        servers
            .peek()
            .active_session()
            .filter(|_| !*adding.peek())
            .map(|s| s.base_url.clone())
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
                    servers.set(api::upsert_server(s));
                    adding.set(false);
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
                if adding() && !servers().servers.is_empty() {
                    button {
                        class: "login-cancel",
                        onclick: move |_| adding.set(false),
                        "cancel"
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
fn MainView(session: api::Session) -> Element {
    let mut session = use_signal(move || session);
    use_context_provider(|| session);
    let mut servers_file = use_context::<Signal<api::ServersFile>>();
    let mut lightbox = use_context_provider(|| Signal::new(None::<String>));
    let mut react_target = use_context_provider(|| Signal::new(None::<i64>));
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let mut members = use_signal(Vec::<UserStatus>::new);
    let mut tags = use_signal(Vec::<Tag>::new);
    use_context_provider(|| members);
    use_context_provider(|| tags);
    // Server emojis, shared with the markdown renderer for :name: lookups.
    let mut emojis = use_signal(Vec::<shared::CustomEmoji>::new);
    use_context_provider(|| emojis);
    let mut has_more = use_signal(|| false);
    let mut loading_older = use_signal(|| false);
    // channel id -> (unread messages, how many ping you, last id you'd read).
    // Server-backed, so it survives restarts and matches on every device.
    let mut unread = use_signal(HashMap::<i64, (i64, i64, i64)>::new);
    // Newest message you'd already seen in the open channel; the NEW divider
    // is drawn right after it.
    let mut divider_at = use_signal(|| None::<i64>);
    let mut confirm = use_signal(|| None::<ConfirmAction>);
    let mut voice_rosters = use_signal(HashMap::<i64, Vec<(User, bool, bool)>>::new);
    let window = use_window();

    // Mirror unread state onto the tray badge.
    let mut tray_unread = use_context::<Signal<bool>>();
    use_effect(move || {
        let has_unread = !unread().is_empty();
        if tray_unread.peek().clone() != has_unread {
            tray_unread.set(has_unread);
        }
    });

    // user id -> (channel they are typing in, username, expiry timestamp)
    let mut typing = use_signal(HashMap::<i64, (i64, String, i64)>::new);
    let mut last_typing_sent = use_signal(|| 0i64);
    let mut draft = use_signal(String::new);
    let mut new_channel = use_signal(String::new);
    let mut new_channel_voice = use_signal(|| false);
    let mut uploading = use_signal(|| false);
    let mut pending_files = use_signal(Vec::<PendingFile>::new);
    let mut drag_over = use_signal(|| false);
    let mut gif_open = use_signal(|| false);
    let mut gif_query = use_signal(String::new);
    let mut gif_results = use_signal(Vec::<GifResult>::new);
    let mut gif_status = use_signal(String::new);
    let mut status = use_signal(|| "connecting…".to_string());

    // Open a channel: remember where the reader left off (for the NEW line),
    // clear its badge, load history, and report the new read position.
    let mut open_channel = move |channel: Channel| {
        let previous = unread.write().remove(&channel.id);
        divider_at.set(
            previous
                .filter(|(count, _, _)| *count > 0)
                .map(|(_, _, last_read)| last_read),
        );
        selected.set(Some(channel.clone()));
        messages.set(Vec::new());
        has_more.set(false);
        spawn(async move {
            match api::messages(&session(), channel.id, None).await {
                Ok(msgs) => {
                    has_more.set(msgs.len() == api::HISTORY_PAGE);
                    if let Some(newest) = msgs.last().map(|m| m.id) {
                        api::mark_read(&session(), channel.id, newest).await;
                    }
                    messages.set(msgs);
                }
                Err(e) => status.set(e),
            }
        });
    };
    // Clear a channel's badge without opening it: the newest message id is
    // what "read up to here" means, so fetch it first.
    let mark_channel_read = move |channel_id: i64| {
        spawn(async move {
            let Ok(msgs) = api::messages(&session(), channel_id, None).await else { return };
            let Some(newest) = msgs.iter().map(|m| m.id).max() else { return };
            api::mark_read(&session(), channel_id, newest).await;
            let mut unread = unread;
            unread.write().remove(&channel_id);
        });
    };
    let voice_status = use_signal_sync(voice::VoiceStatus::default);
    let mic_level = use_signal_sync(|| 0.0f32);
    let voice = use_coroutine(move |rx| voice::voice_task(rx, voice_status, mic_level));
    let mut whats_new = use_signal(|| None::<Vec<shared::ChangelogEntry>>);
    let mut update_available = use_signal(|| None::<shared::ClientVersionInfo>);
    let mut updating = use_signal(|| false);
    let mut update_progress = use_signal(|| 0.0f32);
    let mut stickers = use_signal(Vec::<shared::Sticker>::new);
    let mut sticker_open = use_signal(|| false);
    let mut emoji_open = use_signal(|| false);
    let mut emoji_query = use_signal(String::new);
    let mut slash_sel = use_signal(|| 0usize);
    // The music tab: shared player state, polled while it's open.
    let mut music_open = use_signal(|| false);
    let mut music = use_signal(shared::MusicState::default);
    let mut music_picked = use_signal(HashSet::<u64>::new);
    let mut music_volume = use_signal(|| 100i64);
    let mut profile_card = use_signal(|| None::<Profile>);
    let mut new_tag_name = use_signal(String::new);
    let mut new_tag_color = use_signal(|| "#5865f2".to_string());
    let mut replying_to = use_context_provider(|| Signal::new(None::<Message>));
    let mut jump_to = use_context_provider(|| Signal::new(None::<i64>));
    // The one open right-click menu, wherever it was opened from.
    let ctx_menu: menu::MenuSignal = use_context_provider(|| Signal::new(None::<menu::Menu>));
    // A person is a person wherever they turn up, so the member list and the
    // voice roster offer the same menu.
    let member_items = move |user: User, banned: bool, is_admin: bool| -> Vec<menu::Item> {
        let me = session().user.id;
        let mut items = vec![menu::item("View profile", "user", {
            let id = user.id;
            move || {
                spawn(async move {
                    let (mut profile_card, mut status) = (profile_card, status);
                    match api::profile(&session(), id).await {
                        Ok(profile) => profile_card.set(Some(profile)),
                        Err(e) => status.set(e),
                    }
                });
            }
        })];
        if user.id != me {
            items.push(menu::item("Message", "message", {
                let id = user.id;
                move || {
                    spawn(async move {
                        let (mut channels, mut status) = (channels, status);
                        let mut open = open_channel;
                        match api::create_dm(&session(), id).await {
                            Ok(channel) => {
                                if !channels().iter().any(|c| c.id == channel.id) {
                                    channels.write().push(channel.clone());
                                }
                                open(channel);
                            }
                            Err(e) => status.set(e),
                        }
                    });
                }
            }));
        }
        items.push(menu::item("Mention in chat", "at-sign", {
            let username = user.username.clone();
            move || {
                let mut draft = draft;
                let current = draft();
                let gap = if current.is_empty() || current.ends_with(' ') { "" } else { " " };
                draft.set(format!("{current}{gap}@{username} "));
            }
        }));
        if session().user.role == "admin" && user.id != me {
            let label = if is_admin { "Remove admin" } else { "Make admin" };
            items.push(menu::item(label, "shield", {
                let (id, username) = (user.id, user.username.clone());
                move || {
                    let mut confirm = confirm;
                    confirm.set(Some(ConfirmAction::SetRole {
                        user_id: id,
                        username: username.clone(),
                        make_admin: !is_admin,
                    }));
                }
            }));
            items.push(menu::danger(if banned { "Unban" } else { "Ban" }, "ban", {
                let (id, username) = (user.id, user.username.clone());
                move || {
                    let mut confirm = confirm;
                    confirm.set(Some(ConfirmAction::SetBan {
                        user_id: id,
                        username: username.clone(),
                        banned: !banned,
                    }));
                }
            }));
        }
        items
    };
    // The embedded media player: lives in a dock OUTSIDE the message list, so
    // chat traffic re-rendering messages can never interrupt playback.
    let mut now_playing = use_context_provider(|| Signal::new(None::<NowPlaying>));
    let mut player_volume = use_signal(|| 100i64);
    let mut mention_sel = use_signal(|| 0usize);
    let mut incoming_call = use_signal(|| None::<(i64, User)>);
    let mut search_query = use_signal(String::new);
    let mut search_results = use_signal(|| None::<Vec<shared::SearchResult>>);
    let mut highlight_msg = use_signal(|| None::<i64>);
    let mut bio_draft = use_signal(String::new);
    let mut editing_bio = use_signal(|| false);
    let mut settings_open = use_signal(|| false);
    let mut settings_tab = use_signal(|| "voice");
    let mut pw_current = use_signal(String::new);
    let mut pw_new = use_signal(String::new);
    let mut pw_confirm = use_signal(String::new);
    let mut pw_message = use_signal(|| (String::new(), false));
    // Populated when the user must choose which monitor to share.
    let mut share_picker = use_signal(|| None::<Vec<share::MonitorChoice>>);
    let mut storage_info = use_signal(|| None::<shared::StorageInfo>);
    // None until the Server tab loads it; the draft is what's in the box.
    let mut invite_loaded = use_signal(|| false);
    let mut invite_draft = use_signal(String::new);
    let mut invite_message = use_signal(String::new);
    let mut persona_loaded = use_signal(|| false);
    let mut persona_draft = use_signal(String::new);
    let mut persona_message = use_signal(String::new);
    let mut bot_name_draft = use_signal(String::new);
    let mut server_name_draft = use_signal(String::new);
    let mut retention_days = use_signal(|| 21i64);
    let mut audio_settings = use_signal(api::load_settings);
    let mut input_devices = use_signal(Vec::<String>::new);
    let mut output_devices = use_signal(Vec::<String>::new);

    let mut open_settings = move |tab: &'static str| {
        input_devices.set(voice::list_input_devices());
        output_devices.set(voice::list_output_devices());
        settings_tab.set(tab);
        settings_open.set(true);
    };

    // Jump to a referenced message: highlight it, loading history if needed.
    use_effect(move || {
        if let Some(target) = jump_to() {
            jump_to.set(None);
            highlight_msg.set(Some(target));
            if !messages.peek().iter().any(|m| m.id == target) {
                if let Some(ch) = selected.peek().clone() {
                    spawn(async move {
                        if let Ok(msgs) = api::messages(&session(), ch.id, Some(target + 1)).await {
                            has_more.set(msgs.len() == api::HISTORY_PAGE);
                            messages.set(msgs);
                        }
                    });
                }
            }
        }
    });

    // Executes a confirmed destructive action.
    let run_confirm = move |action: ConfirmAction| {
        spawn(async move {
            let result = match &action {
                ConfirmAction::DeleteChannel { id, .. } => api::delete_channel(&session(), *id).await,
                ConfirmAction::SetBan { user_id, banned, .. } => {
                    api::set_ban(&session(), *user_id, *banned).await.map(|_| ())
                }
                ConfirmAction::SetRole { user_id, make_admin, .. } => {
                    let role = if *make_admin { "admin" } else { "member" };
                    api::set_role(&session(), *user_id, role).await.map(|_| ())
                }
                ConfirmAction::DeleteTag { id, .. } => api::delete_tag(&session(), *id).await,
                ConfirmAction::DeleteSticker { id, .. } => api::delete_sticker(&session(), *id).await,
            };
            if let Err(e) = result {
                status.set(e);
            } else if let ConfirmAction::SetBan { user_id, .. } | ConfirmAction::SetRole { user_id, .. } = &action {
                // Refresh the open profile card so badges/buttons update.
                if profile_card.peek().as_ref().map(|p| p.user.id) == Some(*user_id) {
                    if let Ok(p) = api::profile(&session(), *user_id).await {
                        profile_card.set(Some(p));
                    }
                }
            }
        });
    };

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

    // First launch after an update: greet with what changed.
    use_future(move || async move {
        if api::load_settings().last_seen_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
            if let Ok(entries) = api::changelog(&session()).await {
                whats_new.set(Some(entries));
            }
        }
    });

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

    // Load custom tags.
    use_future(move || async move {
        if let Ok(list) = api::tags(&session()).await {
            tags.set(list);
        }
    });

    // Load server emojis.
    use_future(move || async move {
        if let Ok(list) = api::emojis(&session()).await {
            emojis.set(list);
        }
    });

    // Initial data load: channel list, unread badges, then history for the
    // first channel.
    use_future(move || async move {
        match api::channels(&session()).await {
            Ok(chs) => {
                if let Ok(counts) = api::unread(&session()).await {
                    unread.set(
                        counts
                            .into_iter()
                            .filter(|u| u.count > 0)
                            .map(|u| (u.channel_id, (u.count, u.mentions, u.last_read_id)))
                            .collect(),
                    );
                }
                if let Some(first) = chs.first().cloned() {
                    // Opening it marks it read and draws the NEW line.
                    open_channel(first);
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

            // Voice presence is connection-scoped on the server: it drops us
            // from the roster when the socket dies. Re-announce on every
            // connect, or a reconnect leaves us invisible in everyone's
            // sidebar while we're still sitting in the call.
            {
                let voice_now = voice_status.peek();
                let channel_id = voice_now.channel_id.filter(|_| !voice_now.connecting);
                let (sharing, camera) = (voice_now.sharing_self, voice_now.camera_self);
                drop(voice_now);
                if channel_id.is_some() {
                    let announce = ClientEvent::VoiceState { channel_id, sharing, camera };
                    let text = serde_json::to_string(&announce).expect("serialize event");
                    let _ = socket.send(WsMsg::Text(text.into())).await;
                }
            }

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
                                let is_dm = channels()
                                    .iter()
                                    .any(|c| c.id == message.channel_id && c.kind == "dm");
                                let content_lower = message.content.to_lowercase();
                                let mentioned = message.author.id != me.id
                                    && (is_dm
                                        || content_lower.contains("@everyone")
                                        || content_lower.contains(&format!("@{}", me.username.to_lowercase())));
                                if mentioned && !window.window.is_focused() {
                                    window.window.request_user_attention(Some(UserAttentionType::Informational));
                                    if audio_settings().notification_sounds {
                                        play_notification_sound();
                                    }
                                }
                                if selected().map(|c| c.id) == Some(message.channel_id) {
                                    // Visible channel: reading it counts as read.
                                    let (channel_id, message_id) = (message.channel_id, message.id);
                                    messages.write().push(message);
                                    spawn(async move {
                                        api::mark_read(&session(), channel_id, message_id).await;
                                    });
                                } else if message.author.id != me.id {
                                    let mut counts = unread.write();
                                    // A fresh entry remembers the id just before this
                                    // message, which is exactly where NEW belongs.
                                    let entry = counts
                                        .entry(message.channel_id)
                                        .or_insert((0, 0, message.id - 1));
                                    entry.0 += 1;
                                    if mentioned {
                                        entry.1 += 1;
                                    }
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
                                    api::update_saved_server(&s);
                                    session.set(s);
                                }
                            }
                            ServerEvent::PresenceChanged { user, online } => {
                                let mut list = members.write();
                                match list.iter_mut().find(|m| m.user.id == user.id) {
                                    Some(entry) => entry.online = online,
                                    None => {
                                        list.push(UserStatus { user, online, banned: false, tag_ids: Vec::new() });
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
                            ServerEvent::VoiceSnapshot { entries } => {
                                let mut map = HashMap::<i64, Vec<(User, bool, bool)>>::new();
                                for entry in entries {
                                    map.entry(entry.channel_id).or_default().push((entry.user, entry.sharing, entry.camera));
                                }
                                voice_rosters.set(map);
                            }
                            ServerEvent::VoiceStateChanged { user, channel_id, sharing, camera } => {
                                {
                                    let mut map = voice_rosters.write();
                                    for users in map.values_mut() {
                                        users.retain(|(u, _, _)| u.id != user.id);
                                    }
                                    if let Some(ch) = channel_id {
                                        map.entry(ch).or_default().push((user.clone(), sharing, camera));
                                    }
                                    map.retain(|_, users| !users.is_empty());
                                }
                                // Ring on an incoming DM call.
                                match channel_id {
                                    Some(ch) => {
                                        let me = session().user.id;
                                        let is_my_dm = channels()
                                            .iter()
                                            .any(|c| c.id == ch && c.kind == "dm");
                                        if is_my_dm && user.id != me && voice_status().channel_id != Some(ch) {
                                            incoming_call.set(Some((ch, user)));
                                            if audio_settings().notification_sounds {
                                                play_ring_sound();
                                            }
                                        }
                                    }
                                    None => {
                                        if incoming_call().is_some_and(|(_, caller)| caller.id == user.id) {
                                            incoming_call.set(None);
                                        }
                                    }
                                }
                            }
                            ServerEvent::ServerRenamed { name } => {
                                let mut s = session();
                                s.server_name = name;
                                api::update_saved_server(&s);
                                session.set(s);
                                servers_file.set(api::load_servers());
                            }
                            ServerEvent::TagsChanged => {
                                if let Ok(list) = api::tags(&session()).await {
                                    tags.set(list);
                                }
                                if let Ok(users) = api::users(&session()).await {
                                    members.set(users);
                                }
                            }
                            ServerEvent::EmojisChanged => {
                                if let Ok(list) = api::emojis(&session()).await {
                                    emojis.set(list);
                                }
                            }
                            ServerEvent::ServerIconChanged { icon } => {
                                let mut s = session();
                                s.server_icon = Some(icon);
                                api::update_saved_server(&s);
                                session.set(s);
                                servers_file.set(api::load_servers());
                            }
                            ServerEvent::ChannelDeleted { channel_id } => {
                                channels.write().retain(|c| c.id != channel_id);
                                unread.write().remove(&channel_id);
                                if voice_status().channel_id == Some(channel_id) {
                                    voice.send(voice::VoiceCmd::Leave);
                                }
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    let next = channels().into_iter().find(|c| c.kind == "text");
                                    selected.set(next.clone());
                                    messages.set(Vec::new());
                                    has_more.set(false);
                                    if let Some(ch) = next {
                                        spawn(async move {
                                            if let Ok(msgs) = api::messages(&session(), ch.id, None).await {
                                                has_more.set(msgs.len() == api::HISTORY_PAGE);
                                                messages.set(msgs);
                                            }
                                        });
                                    }
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

    // Announce our own voice channel over the chat WebSocket whenever it
    // changes, so everyone's sidebar shows who's in voice.
    let mut announced_voice = use_signal(|| (None::<i64>, false, false));
    use_effect(move || {
        let current = voice_status().channel_id.filter(|_| !voice_status().connecting);
        let sharing = voice_status().sharing_self;
        let camera = voice_status().camera_self;
        if *announced_voice.peek() != (current, sharing, camera) {
            announced_voice.set((current, sharing, camera));
            ws.send(ClientEvent::VoiceState { channel_id: current, sharing, camera });
        }
    });

    let mut send = move || {
        let mut content = draft().trim().to_string();
        let files = pending_files();
        let Some(channel) = selected() else { return };
        if content.is_empty() && files.is_empty() {
            return;
        }
        // On the music tab, a bare link means "queue this" — Jon's spec.
        if music_open()
            && !content.starts_with('/')
            && content.split_whitespace().count() == 1
            && (content.starts_with("http://") || content.starts_with("https://"))
        {
            content = format!("/play {content}");
        }
        let reply_target = replying_to().map(|m| m.id);
        replying_to.set(None);
        draft.set(String::new());
        pending_files.set(Vec::new());
        spawn(async move {
            if !files.is_empty() {
                uploading.set(true);
                for file in files {
                    match api::upload(&session(), &file.name, file.bytes).await {
                        Ok(url) => ws.send(ClientEvent::SendMessage { channel_id: channel.id, content: url, reply_to: None }),
                        Err(e) => status.set(e),
                    }
                }
                uploading.set(false);
            }
            if !content.is_empty() {
                ws.send(ClientEvent::SendMessage {
                    channel_id: channel.id,
                    content,
                    reply_to: reply_target,
                });
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
            let kind = if new_channel_voice() { "voice" } else { "text" };
            // Success arrives back via the ChannelCreated broadcast.
            if let Err(e) = api::create_channel(&session(), name, kind).await {
                status.set(e);
            } else {
                new_channel.set(String::new());
                new_channel_voice.set(false);
            }
        });
    };

    let logout = move |_| {
        let active = api::load_servers().active;
        servers_file.set(api::remove_server(active));
    };

    let selected_id = selected().map(|c| c.id);
    let me_id = session().user.id;
    let (selected_label, selected_name) = match selected() {
        Some(c) if c.kind == "dm" => {
            let peer = dm_peer_name(&c, me_id);
            (format!("@ {peer}"), format!("@{peer}"))
        }
        Some(c) => (format!("# {}", c.name), format!("#{}", c.name)),
        None => (String::new(), String::new()),
    };
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
                div { class: "drop-overlay", "Drop to upload to {selected_name}" }
            }
            {menu::view(ctx_menu)}
            if updating() {
                div { class: "update-overlay",
                    div { class: "spinner" }
                    div { class: "update-overlay-title", "Updating NotDiscord…" }
                    div { class: "update-overlay-sub",
                        if update_progress() > 0.0 {
                            "downloading — {(update_progress() * 100.0) as i32}%"
                        } else {
                            "connecting…"
                        }
                    }
                    div { class: "update-bar",
                        div {
                            class: "update-bar-fill",
                            style: "width: {(update_progress() * 100.0).clamp(0.0, 100.0)}%",
                        }
                    }
                }
            }
            if let Some(entries) = whats_new() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| {
                        whats_new.set(None);
                        let mut s = api::load_settings();
                        s.last_seen_version = Some(env!("CARGO_PKG_VERSION").into());
                        api::save_settings(&s);
                    },
                    div {
                        class: "settings-modal whatsnew-modal",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "whatsnew-head",
                            div { class: "whatsnew-title", "What's new" }
                            div { class: "whatsnew-sub", "you're on v{env!(\"CARGO_PKG_VERSION\")}" }
                        }
                        div { class: "settings-body whatsnew-body",
                            for (i, entry) in entries.into_iter().take(10).enumerate() {
                                div {
                                    key: "{entry.version}",
                                    class: if i == 0 { "release latest" } else { "release" },
                                    div { class: "release-head",
                                        span { class: "release-version", "v{entry.version}" }
                                        if i == 0 {
                                            span { class: "role-badge", "NEW" }
                                        }
                                        span { class: "release-date", "{entry.date}" }
                                    }
                                    ul { class: "release-changes",
                                        for (j, change) in entry.changes.iter().enumerate() {
                                            li { key: "{j}", "{change}" }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "whatsnew-foot",
                            button {
                                class: "profile-btn primary",
                                onclick: move |_| {
                                    whats_new.set(None);
                                    let mut s = api::load_settings();
                                    s.last_seen_version = Some(env!("CARGO_PKG_VERSION").into());
                                    api::save_settings(&s);
                                },
                                "Nice"
                            }
                        }
                    }
                }
            }
            if let Some(results) = search_results() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| search_results.set(None),
                    div {
                        class: "settings-modal whatsnew-modal",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "whatsnew-head",
                            div { class: "whatsnew-title", "Search" }
                            div { class: "whatsnew-sub",
                                if results.is_empty() {
                                    "no messages match \"{search_query}\""
                                } else {
                                    "{results.len()} result(s) for \"{search_query}\" — click to jump"
                                }
                            }
                        }
                        div { class: "settings-body whatsnew-body",
                            for result in results {
                                div {
                                    key: "{result.message.id}",
                                    class: "search-hit",
                                    onclick: {
                                        let msg_id = result.message.id;
                                        let channel_id = result.message.channel_id;
                                        move |_| {
                                            search_results.set(None);
                                            let Some(channel) = channels().into_iter().find(|c| c.id == channel_id) else {
                                                status.set("that channel is no longer available".into());
                                                return;
                                            };
                                            unread.write().remove(&channel.id);
                                            divider_at.set(None);
                                            selected.set(Some(channel));
                                            messages.set(Vec::new());
                                            has_more.set(false);
                                            highlight_msg.set(Some(msg_id));
                                            spawn(async move {
                                                match api::messages(&session(), channel_id, Some(msg_id + 1)).await {
                                                    Ok(msgs) => {
                                                        has_more.set(msgs.len() == api::HISTORY_PAGE);
                                                        // MAX() on the server keeps this from
                                                        // rewinding a further-along read position.
                                                        if let Some(newest) = msgs.last().map(|m| m.id) {
                                                            api::mark_read(&session(), channel_id, newest).await;
                                                        }
                                                        messages.set(msgs);
                                                    }
                                                    Err(e) => status.set(e),
                                                }
                                            });
                                        }
                                    },
                                    div { class: "search-hit-head",
                                        span { class: "search-hit-channel",
                                            if result.channel_kind == "dm" { "DM" } else { "# {result.channel_name}" }
                                        }
                                        span { class: "search-hit-author", "{result.message.author.username}" }
                                        span { class: "release-date", {format_time(result.message.created_at)} }
                                    }
                                    div { class: "search-hit-content",
                                        {result.message.content.chars().take(220).collect::<String>()}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if settings_open() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| settings_open.set(false),
                    div {
                        class: "settings-modal",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "settings-tabs",
                            button {
                                class: if settings_tab() == "voice" { "settings-tab active" } else { "settings-tab" },
                                onclick: move |_| settings_tab.set("voice"),
                                "Voice"
                            }
                            button {
                                class: if settings_tab() == "app" { "settings-tab active" } else { "settings-tab" },
                                onclick: move |_| settings_tab.set("app"),
                                "App"
                            }
                            button {
                                class: if settings_tab() == "account" { "settings-tab active" } else { "settings-tab" },
                                onclick: move |_| {
                                    pw_current.set(String::new());
                                    pw_new.set(String::new());
                                    pw_confirm.set(String::new());
                                    pw_message.set((String::new(), false));
                                    settings_tab.set("account");
                                },
                                "Account"
                            }
                            if session().user.role == "admin" {
                                button {
                                    class: if settings_tab() == "server" { "settings-tab active" } else { "settings-tab" },
                                    onclick: move |_| {
                                        server_name_draft.set(session().server_name);
                                        settings_tab.set("server");
                                        spawn(async move {
                                            if let Ok(setting) = api::get_retention(&session()).await {
                                                retention_days.set(setting.days);
                                            }
                                            if let Ok(info) = api::get_storage(&session()).await {
                                                storage_info.set(Some(info));
                                            }
                                            if let Ok(setting) = api::get_invite(&session()).await {
                                                invite_draft.set(setting.code);
                                                invite_loaded.set(true);
                                                invite_message.set(String::new());
                                            }
                                            if let Ok(settings) = api::get_bot_settings(&session()).await {
                                                persona_draft.set(settings.persona);
                                                bot_name_draft.set(settings.name);
                                                persona_loaded.set(true);
                                                persona_message.set(String::new());
                                            }
                                        });
                                    },
                                    "Server"
                                }
                            }
                            button {
                                class: "settings-close",
                                onclick: move |_| settings_open.set(false),
                                Icon { name: "x", size: 14 }
                            }
                        }
                        div { class: "settings-body",
                            if settings_tab() == "account" {
                                label { "Logged in as" }
                                div { class: "settings-value", "{session().user.username}" }
                                label { "Current password" }
                                input {
                                    r#type: "password",
                                    value: "{pw_current}",
                                    oninput: move |e| pw_current.set(e.value()),
                                }
                                label { "New password" }
                                input {
                                    r#type: "password",
                                    value: "{pw_new}",
                                    oninput: move |e| pw_new.set(e.value()),
                                }
                                label { "Confirm new password" }
                                input {
                                    r#type: "password",
                                    value: "{pw_confirm}",
                                    oninput: move |e| pw_confirm.set(e.value()),
                                }
                                // Live rule check so problems show before submitting.
                                if !pw_new().is_empty() {
                                    if let Some(problem) = shared::password_problem(&pw_new(), &session().user.username) {
                                        div { class: "settings-hint pw-bad", "{problem}" }
                                    } else if !pw_confirm().is_empty() && pw_confirm() != pw_new() {
                                        div { class: "settings-hint pw-bad", "passwords don't match" }
                                    } else if !pw_confirm().is_empty() {
                                        div { class: "settings-hint pw-good", "looks good" }
                                    }
                                }
                                if !pw_message().0.is_empty() {
                                    div {
                                        class: if pw_message().1 { "settings-hint pw-good" } else { "settings-hint pw-bad" },
                                        "{pw_message().0}"
                                    }
                                }
                                button {
                                    class: "profile-btn primary",
                                    disabled: pw_current().is_empty()
                                        || pw_new().is_empty()
                                        || pw_confirm() != pw_new()
                                        || shared::password_problem(&pw_new(), &session().user.username).is_some(),
                                    onclick: move |_| {
                                        spawn(async move {
                                            match api::change_password(&session(), pw_current(), pw_new()).await {
                                                Ok(()) => {
                                                    pw_current.set(String::new());
                                                    pw_new.set(String::new());
                                                    pw_confirm.set(String::new());
                                                    pw_message.set(("password changed — other devices were logged out".into(), true));
                                                }
                                                Err(e) => pw_message.set((e, false)),
                                            }
                                        });
                                    },
                                    "Change password"
                                }
                                div { class: "settings-hint", "changing your password signs you out everywhere else" }
                            } else if settings_tab() == "server" {
                                label { "Server name" }
                                input {
                                    value: "{server_name_draft}",
                                    oninput: move |e| server_name_draft.set(e.value()),
                                }
                                label { "Server ID" }
                                div { class: "settings-value settings-mono", "{session().server_id}" }
                                label { "Keep uploads for" }
                                select {
                                    onchange: move |e| {
                                        if let Ok(days) = e.value().parse::<i64>() {
                                            spawn(async move {
                                                match api::set_retention(&session(), days).await {
                                                    Ok(setting) => {
                                                        retention_days.set(setting.days);
                                                        status.set(format!("uploads now expire after {} days", setting.days));
                                                    }
                                                    Err(e) => status.set(e),
                                                }
                                            });
                                        }
                                    },
                                    option { value: "21", selected: retention_days() == 21, "3 weeks (default)" }
                                    option { value: "30", selected: retention_days() == 30, "1 month" }
                                    option { value: "60", selected: retention_days() == 60, "2 months" }
                                    option { value: "90", selected: retention_days() == 90, "3 months" }
                                }
                                div { class: "settings-hint", "expired files disappear from chat; avatars and stickers never expire" }
                                if let Some(info) = storage_info() {
                                    label { "Storage" }
                                    {
                                        let used_gb = info.used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                                        let pct = ((used_gb / info.cap_gb as f64) * 100.0).min(100.0);
                                        rsx! {
                                            div { class: "storage-row",
                                                div { class: "storage-bar",
                                                    div {
                                                        class: if pct >= 90.0 { "storage-fill full" } else { "storage-fill" },
                                                        style: "width: {pct:.1}%",
                                                    }
                                                }
                                                span { class: "storage-text", "{used_gb:.2} GB of {info.cap_gb} GB used" }
                                            }
                                        }
                                    }
                                    select {
                                        onchange: move |e| {
                                            if let Ok(cap) = e.value().parse::<i64>() {
                                                spawn(async move {
                                                    match api::set_storage_cap(&session(), cap).await {
                                                        Ok(info) => {
                                                            status.set(format!("storage cap is now {} GB", info.cap_gb));
                                                            storage_info.set(Some(info));
                                                        }
                                                        Err(e) => status.set(e),
                                                    }
                                                });
                                            }
                                        },
                                        option { value: "10", selected: info.cap_gb == 10, "10 GB" }
                                        option { value: "30", selected: info.cap_gb == 30, "30 GB (default)" }
                                        option { value: "50", selected: info.cap_gb == 50, "50 GB" }
                                        option { value: "100", selected: info.cap_gb == 100, "100 GB" }
                                        option { value: "200", selected: info.cap_gb == 200, "200 GB" }
                                    }
                                    div { class: "settings-hint", "uploads are refused once the cap is reached" }
                                }
                                if invite_loaded() {
                                    label { "Invite code" }
                                    div { class: "invite-row",
                                        input {
                                            class: "invite-input",
                                            spellcheck: "false",
                                            placeholder: "empty = anyone can join",
                                            value: "{invite_draft}",
                                            oninput: move |e| invite_draft.set(e.value()),
                                        }
                                        button {
                                            class: "profile-btn",
                                            title: "Copy the invite code",
                                            disabled: invite_draft().trim().is_empty(),
                                            onclick: move |_| {
                                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                                    if clipboard.set_text(invite_draft().trim().to_owned()).is_ok() {
                                                        invite_message.set("copied".into());
                                                    }
                                                }
                                            },
                                            "Copy"
                                        }
                                        button {
                                            class: "profile-btn",
                                            title: "Generate a fresh code",
                                            onclick: move |_| invite_draft.set(random_invite_code()),
                                            "Shuffle"
                                        }
                                        button {
                                            class: "profile-btn primary",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    match api::set_invite(&session(), invite_draft().trim().to_owned()).await {
                                                        Ok(setting) => {
                                                            invite_message.set(if setting.code.is_empty() {
                                                                "saved — registration is now open to anyone".into()
                                                            } else {
                                                                "saved — newcomers need the new code".into()
                                                            });
                                                            invite_draft.set(setting.code);
                                                        }
                                                        Err(e) => invite_message.set(e),
                                                    }
                                                });
                                            },
                                            "Save code"
                                        }
                                    }
                                    if invite_message().is_empty() {
                                        div { class: "settings-hint", "newcomers must type this code to sign up; people already in stay in" }
                                    } else {
                                        div { class: "settings-hint pw-good", "{invite_message}" }
                                    }
                                }
                                if persona_loaded() {
                                    label { "Bot name & avatar" }
                                    div { class: "invite-row",
                                        input {
                                            class: "invite-input",
                                            value: "{bot_name_draft}",
                                            oninput: move |e| bot_name_draft.set(e.value()),
                                        }
                                        button {
                                            class: "profile-btn",
                                            title: "Pick an avatar image for the bot",
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
                                                        persona_message.set("avatar too large (max 8 MB)".into());
                                                        return;
                                                    }
                                                    let uploaded = match api::upload(&session(), &file.file_name(), bytes).await {
                                                        Ok(url) => url,
                                                        Err(e) => return persona_message.set(e),
                                                    };
                                                    let update = shared::BotSettingsUpdate { avatar: Some(uploaded), ..Default::default() };
                                                    match api::set_bot_settings(&session(), update).await {
                                                        Ok(_) => persona_message.set("avatar updated".into()),
                                                        Err(e) => persona_message.set(e),
                                                    }
                                                });
                                            },
                                            "Avatar"
                                        }
                                        button {
                                            class: "profile-btn primary",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    let update = shared::BotSettingsUpdate { name: Some(bot_name_draft()), ..Default::default() };
                                                    match api::set_bot_settings(&session(), update).await {
                                                        Ok(settings) => {
                                                            bot_name_draft.set(settings.name.clone());
                                                            persona_message.set(format!("the bot now answers to @{}", settings.name));
                                                        }
                                                        Err(e) => persona_message.set(e),
                                                    }
                                                });
                                            },
                                            "Rename"
                                        }
                                    }
                                    label { "Bot personality" }
                                    textarea {
                                        class: "persona-edit",
                                        rows: "5",
                                        spellcheck: "false",
                                        value: "{persona_draft}",
                                        oninput: move |e| persona_draft.set(e.value()),
                                    }
                                    div { class: "invite-row",
                                        button {
                                            class: "profile-btn",
                                            title: "Restore the built-in personality",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    // Empty resets server-side to the default.
                                                    let update = shared::BotSettingsUpdate { persona: Some(String::new()), ..Default::default() };
                                                    match api::set_bot_settings(&session(), update).await {
                                                        Ok(settings) => {
                                                            persona_draft.set(settings.persona);
                                                            persona_message.set("reset to the default personality".into());
                                                        }
                                                        Err(e) => persona_message.set(e),
                                                    }
                                                });
                                            },
                                            "Reset"
                                        }
                                        button {
                                            class: "profile-btn primary",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    let update = shared::BotSettingsUpdate { persona: Some(persona_draft()), ..Default::default() };
                                                    match api::set_bot_settings(&session(), update).await {
                                                        Ok(settings) => {
                                                            persona_draft.set(settings.persona);
                                                            persona_message.set("saved — the bot will act like this from its next reply".into());
                                                        }
                                                        Err(e) => persona_message.set(e),
                                                    }
                                                });
                                            },
                                            "Save personality"
                                        }
                                    }
                                    if persona_message().is_empty() {
                                        div { class: "settings-hint", "how the bot talks — rewrite it however the crew votes" }
                                    } else {
                                        div { class: "settings-hint pw-good", "{persona_message}" }
                                    }
                                }
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
                                                status.set("icon too large (max 8 MB)".into());
                                                return;
                                            }
                                            match api::upload(&session(), &file.file_name(), bytes).await {
                                                Ok(url) => {
                                                    if let Err(e) = api::set_server_icon(&session(), url).await {
                                                        status.set(e);
                                                    }
                                                }
                                                Err(e) => status.set(e),
                                            }
                                        });
                                    },
                                    "Change server icon"
                                }
                                button {
                                    class: "profile-btn primary",
                                    onclick: move |_| {
                                        spawn(async move {
                                            match api::rename_server(&session(), server_name_draft()).await {
                                                // The rename lands for everyone via broadcast.
                                                Ok(_) => settings_open.set(false),
                                                Err(e) => status.set(e),
                                            }
                                        });
                                    },
                                    "Save"
                                }
                            } else if settings_tab() == "voice" {
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
                                div { class: "invite-row",
                                    select {
                                        onchange: move |e| {
                                            let v = e.value();
                                            let mut s = audio_settings.write();
                                            s.output_device = if v.is_empty() { None } else { Some(v) };
                                            api::save_settings(&s);
                                            drop(s);
                                            rejoin_voice();
                                        },
                                        option { value: "", selected: audio_settings().output_device.is_none(), "System default" }
                                        for name in output_devices() {
                                            option {
                                                value: "{name}",
                                                selected: audio_settings().output_device.as_deref() == Some(name.as_str()),
                                                "{name}"
                                            }
                                        }
                                    }
                                    button {
                                        class: "profile-btn",
                                        title: "Play a test cue on this device",
                                        onclick: move |_| voice::play_test_cue(),
                                        "Test"
                                    }
                                }
                                {
                                    // Voice and cues use THIS device — which is easy to have
                                    // pointing somewhere your headphones aren't.
                                    let default_name = voice::default_output_name().unwrap_or_else(|| "unknown".into());
                                    let hint = match audio_settings().output_device {
                                        Some(picked) if picked != default_name => format!(
                                            "voice + join cues play on \"{picked}\" — your system default is \"{default_name}\". Hit Test if you hear nothing."
                                        ),
                                        _ => format!("voice + join cues play on your system default (\"{default_name}\")"),
                                    };
                                    rsx! { div { class: "settings-hint", "{hint}" } }
                                }
                                label { "Voice mode" }
                                div { class: "mode-row",
                                    button {
                                        class: if audio_settings().voice_mode != "ptt" { "mode-btn active" } else { "mode-btn" },
                                        onclick: move |_| {
                                            audio_settings.write().voice_mode = "vad".into();
                                            voice.send(voice::VoiceCmd::SetVoiceMode {
                                                mode: "vad".into(),
                                                key: audio_settings().ptt_key,
                                            });
                                        },
                                        "Voice activity"
                                    }
                                    button {
                                        class: if audio_settings().voice_mode == "ptt" { "mode-btn active" } else { "mode-btn" },
                                        onclick: move |_| {
                                            audio_settings.write().voice_mode = "ptt".into();
                                            voice.send(voice::VoiceCmd::SetVoiceMode {
                                                mode: "ptt".into(),
                                                key: audio_settings().ptt_key,
                                            });
                                        },
                                        "Push to talk"
                                    }
                                }
                                if audio_settings().voice_mode != "ptt" {
                                    label { "Mic sensitivity — talk and set the line just below your voice" }
                                    input {
                                        r#type: "range",
                                        class: "settings-slider",
                                        min: "0",
                                        max: "100",
                                        value: "{(audio_settings().vad_threshold / 20.0) as i32}",
                                        oninput: move |e| {
                                            if let Ok(v) = e.value().parse::<f32>() {
                                                let threshold = v * 20.0; // 0..=2000 RMS
                                                audio_settings.write().vad_threshold = threshold;
                                                voice.send(voice::VoiceCmd::SetVadThreshold(threshold));
                                            }
                                        },
                                    }
                                    div { class: "settings-hint",
                                        if audio_settings().vad_threshold <= 0.0 {
                                            "always transmitting (open mic)"
                                        } else {
                                            "transmits only when you speak above the marker"
                                        }
                                    }
                                }
                                if audio_settings().voice_mode == "ptt" {
                                    label { "Push-to-talk key (works while in game)" }
                                    select {
                                        onchange: move |e| {
                                            let key = e.value();
                                            audio_settings.write().ptt_key = key.clone();
                                            voice.send(voice::VoiceCmd::SetVoiceMode { mode: "ptt".into(), key });
                                        },
                                        for key in voice::PTT_KEY_CHOICES {
                                            option {
                                                value: "{key}",
                                                selected: audio_settings().ptt_key == *key,
                                                "{key}"
                                            }
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
                                div { class: "meter-wrap",
                                    MicMeter { level: mic_level }
                                    if audio_settings().voice_mode != "ptt" && audio_settings().vad_threshold > 0.0 {
                                        div {
                                            class: "meter-threshold",
                                            style: "left: {(audio_settings().vad_threshold / 10000.0 * 100.0).clamp(0.0, 100.0)}%",
                                        }
                                    }
                                }
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
                            } else {
                                label { "Version" }
                                div { class: "settings-value", "v{env!(\"CARGO_PKG_VERSION\")}" }
                                label { "Server" }
                                div { class: "settings-value", "{session().base_url}" }
                                label { class: "ns-toggle-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: audio_settings().voice_join_sounds,
                                        onchange: move |e| {
                                            let mut s = audio_settings.write();
                                            s.voice_join_sounds = e.checked();
                                            api::save_settings(&s);
                                        },
                                    }
                                    " Voice join/leave sounds"
                                }
                                label { class: "ns-toggle-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: audio_settings().notification_sounds,
                                        onchange: move |e| {
                                            let mut s = audio_settings.write();
                                            s.notification_sounds = e.checked();
                                            api::save_settings(&s);
                                        },
                                    }
                                    " Notification sounds"
                                }
                                button {
                                    class: "profile-btn",
                                    onclick: move |_| {
                                        spawn(async move {
                                            match api::client_version(&session()).await {
                                                Ok(info) => {
                                                    if info.version != env!("CARGO_PKG_VERSION") {
                                                        update_available.set(Some(info));
                                                        status.set("update available — see the banner".into());
                                                    } else {
                                                        status.set("you're on the latest version".into());
                                                    }
                                                }
                                                Err(e) => status.set(e),
                                            }
                                        });
                                    },
                                    "Check for updates"
                                }
                                button {
                                    class: "profile-btn",
                                    onclick: move |_| {
                                        settings_open.set(false);
                                        spawn(async move {
                                            match api::changelog(&session()).await {
                                                Ok(entries) => whats_new.set(Some(entries)),
                                                Err(e) => status.set(e),
                                            }
                                        });
                                    },
                                    "What's new"
                                }
                                div { class: "settings-hint", "closing the window keeps NotDiscord in the tray — this really exits" }
                                button {
                                    class: "profile-btn danger",
                                    onclick: move |_| quit_now("quitting from settings"),
                                    "Quit NotDiscord"
                                }
                            }
                        }
                    }
                }
            }
            if let Some((call_channel, caller)) = incoming_call() {
                div { class: "incoming-call",
                    UserAvatar { user: caller.clone(), class: "member-avatar" }
                    div { class: "incoming-call-text",
                        span { class: "incoming-call-name", "{caller.username}" }
                        span { class: "incoming-call-sub", "is calling…" }
                    }
                    button {
                        class: "profile-btn primary",
                        onclick: {
                            let caller_name = caller.username.clone();
                            move |_| {
                                incoming_call.set(None);
                                let caller_name = caller_name.clone();
                                spawn(async move {
                                    match api::voice_token(&session(), call_channel).await {
                                        Ok(grant) => voice.send(voice::VoiceCmd::Join {
                                            channel_id: call_channel,
                                            channel_name: format!("@{caller_name}"),
                                            url: grant.url,
                                            token: grant.token,
                                        }),
                                        Err(e) => status.set(e),
                                    }
                                });
                            }
                        },
                        "Join"
                    }
                    button {
                        class: "profile-btn",
                        onclick: move |_| incoming_call.set(None),
                        "Ignore"
                    }
                }
            }
            if let Some(action) = confirm() {
                div {
                    class: "settings-overlay confirm-overlay",
                    onclick: move |_| confirm.set(None),
                    div {
                        class: "confirm-modal",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "confirm-title", "Are you sure?" }
                        div { class: "confirm-body", {action.description()} }
                        div { class: "confirm-buttons",
                            button {
                                class: "profile-btn",
                                onclick: move |_| confirm.set(None),
                                "Cancel"
                            }
                            button {
                                class: "profile-btn danger",
                                onclick: {
                                    let action = action.clone();
                                    move |_| {
                                        confirm.set(None);
                                        run_confirm(action.clone());
                                    }
                                },
                                {action.confirm_label()}
                            }
                        }
                    }
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
                        div { class: "profile-badges",
                            if profile.user.role == "admin" {
                                span { class: "role-badge", "ADMIN" }
                            }
                            if profile.banned {
                                span { class: "role-badge banned-badge", "BANNED" }
                            }
                            for tag in profile.tags.iter() {
                                span { class: "tag-pill", style: "background: {tag.color}", "{tag.name}" }
                            }
                        }
                        div { class: "profile-joined", "Member since {format_date(profile.created_at)}" }
                        if editing_bio() {
                            textarea {
                                class: "profile-bio-edit",
                                rows: "3",
                                value: "{bio_draft}",
                                spellcheck: "true",
                                oncontextmenu: move |e: Event<MouseData>| {
                                    menu::open(ctx_menu, &e, menu::text_field_items())
                                },
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
                            if profile.user.id != session().user.id {
                                button {
                                    class: "profile-btn primary",
                                    onclick: {
                                        let target = profile.user.id;
                                        move |_| {
                                            spawn(async move {
                                                match api::create_dm(&session(), target).await {
                                                    Ok(channel) => {
                                                        if !channels().iter().any(|c| c.id == channel.id) {
                                                            channels.write().push(channel.clone());
                                                        }
                                                        profile_card.set(None);
                                                        open_channel(channel.clone());
                                                    }
                                                    Err(e) => status.set(e),
                                                }
                                            });
                                        }
                                    },
                                    "Message"
                                }
                            }
                            if session().user.role == "admin" {
                                div { class: "tag-manager",
                                    div { class: "tag-manager-title", "Tags" }
                                    for tag in tags() {
                                        div { key: "{tag.id}", class: "tag-row",
                                            label { class: "tag-assign",
                                                input {
                                                    r#type: "checkbox",
                                                    checked: profile.tags.iter().any(|t| t.id == tag.id),
                                                    onchange: {
                                                        let target = profile.user.id;
                                                        let tag_id = tag.id;
                                                        move |e: Event<FormData>| {
                                                            let assigned = e.checked();
                                                            spawn(async move {
                                                                match api::assign_tag(&session(), target, tag_id, assigned).await {
                                                                    Ok(()) => {
                                                                        if let Ok(p) = api::profile(&session(), target).await {
                                                                            profile_card.set(Some(p));
                                                                        }
                                                                    }
                                                                    Err(e) => status.set(e),
                                                                }
                                                            });
                                                        }
                                                    },
                                                }
                                                span { class: "tag-pill", style: "background: {tag.color}", "{tag.name}" }
                                            }
                                            button {
                                                class: "tag-delete",
                                                title: "Delete this tag everywhere",
                                                onclick: {
                                                    let tag_id = tag.id;
                                                    let tag_name = tag.name.clone();
                                                    move |_| {
                                                        confirm.set(Some(ConfirmAction::DeleteTag {
                                                            id: tag_id,
                                                            name: tag_name.clone(),
                                                        }));
                                                    }
                                                },
                                                "✕"
                                            }
                                        }
                                    }
                                    div { class: "tag-create",
                                        input {
                                            class: "tag-name-input",
                                            placeholder: "new tag",
                                            value: "{new_tag_name}",
                                            oninput: move |e| new_tag_name.set(e.value()),
                                        }
                                        input {
                                            r#type: "color",
                                            class: "tag-color-input",
                                            value: "{new_tag_color}",
                                            oninput: move |e| new_tag_color.set(e.value()),
                                        }
                                        button {
                                            class: "profile-btn",
                                            onclick: move |_| {
                                                let name = new_tag_name().trim().to_string();
                                                if name.is_empty() {
                                                    return;
                                                }
                                                spawn(async move {
                                                    match api::create_tag(&session(), name, new_tag_color()).await {
                                                        Ok(_) => new_tag_name.set(String::new()),
                                                        Err(e) => status.set(e),
                                                    }
                                                });
                                            },
                                            "Add"
                                        }
                                    }
                                }
                            }
                            if session().user.role == "admin" && profile.user.id != session().user.id {
                                div { class: "profile-actions",
                                    button {
                                        class: "profile-btn",
                                        onclick: {
                                            let target = profile.user.id;
                                            let username = profile.user.username.clone();
                                            let make_admin = profile.user.role != "admin";
                                            move |_| {
                                                confirm.set(Some(ConfirmAction::SetRole {
                                                    user_id: target,
                                                    username: username.clone(),
                                                    make_admin,
                                                }));
                                            }
                                        },
                                        if profile.user.role == "admin" { "Remove admin" } else { "Make admin" }
                                    }
                                    button {
                                        class: "profile-btn danger",
                                        onclick: {
                                            let target = profile.user.id;
                                            let username = profile.user.username.clone();
                                            let ban = !profile.banned;
                                            move |_| {
                                                confirm.set(Some(ConfirmAction::SetBan {
                                                    user_id: target,
                                                    username: username.clone(),
                                                    banned: ban,
                                                }));
                                            }
                                        },
                                        if profile.banned { "Unban" } else { "Ban" }
                                    }
                                }
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
                div { class: "sidebar-title", "{session().server_name}" }
                div { class: "channel-list",
                    for channel in channels().into_iter().filter(|c| c.kind == "text") {
                        button {
                            key: "{channel.id}",
                            class: if selected_id == Some(channel.id) {
                                "channel active"
                            } else if unread().contains_key(&channel.id) {
                                "channel unread"
                            } else {
                                "channel"
                            },
                            onclick: {
                                let channel = channel.clone();
                                move |_| open_channel(channel.clone())
                            },
                            oncontextmenu: {
                                let (id, name) = (channel.id, channel.name.clone());
                                move |e: Event<MouseData>| {
                                    let mut items = Vec::new();
                                    if unread().contains_key(&id) {
                                        items.push(menu::item("Mark as read", "check-square", move || {
                                            mark_channel_read(id)
                                        }));
                                    }
                                    if session().user.role == "admin" {
                                        let name = name.clone();
                                        items.push(menu::danger("Delete channel", "trash", move || {
                                            let mut confirm = confirm;
                                            confirm.set(Some(ConfirmAction::DeleteChannel {
                                                id,
                                                name: name.clone(),
                                            }));
                                        }));
                                    }
                                    menu::open(ctx_menu, &e, items);
                                }
                            },
                            span { class: "chan-name", "# {channel.name}" }
                            if let Some((count, mentions, _)) = unread().get(&channel.id).copied() {
                                if selected_id != Some(channel.id) {
                                    span {
                                        class: if mentions > 0 { "unread-badge ping" } else { "unread-badge" },
                                        "{count}"
                                    }
                                }
                            }
                            if session().user.role == "admin" {
                                span {
                                    class: "chan-del",
                                    title: "Delete channel",
                                    onclick: {
                                        let id = channel.id;
                                        let name = channel.name.clone();
                                        move |e: MouseEvent| {
                                            e.stop_propagation();
                                            confirm.set(Some(ConfirmAction::DeleteChannel { id, name: name.clone() }));
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                    if channels().iter().any(|c| c.kind == "dm") {
                        div { class: "section-label", "Direct Messages" }
                    }
                    for channel in channels().into_iter().filter(|c| c.kind == "dm") {
                        button {
                            key: "dm{channel.id}",
                            class: if selected_id == Some(channel.id) {
                                "channel dm active"
                            } else if unread().contains_key(&channel.id) {
                                "channel dm unread"
                            } else {
                                "channel dm"
                            },
                            onclick: {
                                let channel = channel.clone();
                                move |_| open_channel(channel.clone())
                            },
                            oncontextmenu: {
                                let id = channel.id;
                                move |e: Event<MouseData>| {
                                    let mut items = Vec::new();
                                    if unread().contains_key(&id) {
                                        items.push(menu::item("Mark as read", "check-square", move || {
                                            mark_channel_read(id)
                                        }));
                                    }
                                    menu::open(ctx_menu, &e, items);
                                }
                            },
                            if let Some(peer) = dm_peer(&channel, session().user.id) {
                                UserAvatar { user: peer, class: "dm-avatar" }
                            }
                            span { class: "chan-name", "{dm_peer_name(&channel, session().user.id)}" }
                            if voice_rosters().get(&channel.id).is_some_and(|v| !v.is_empty()) {
                                span { class: "dm-call-live", Icon { name: "phone", size: 11 } }
                            }
                            // Every DM message pings, so the badge is always hot.
                            if let Some((count, _, _)) = unread().get(&channel.id).copied() {
                                if selected_id != Some(channel.id) {
                                    span { class: "unread-badge ping", "{count}" }
                                }
                            }
                        }
                    }
                    div { class: "section-row",
                        span { class: "section-label", "Voice" }
                        button {
                            class: "audio-settings-btn",
                            title: "Voice settings",
                            onclick: move |_| open_settings("voice"),
                            Icon { name: "settings", size: 14 }
                        }
                    }
                    for channel in channels().into_iter().filter(|c| c.kind == "voice") {
                        {
                            let ch_id = channel.id;
                            let ch_name = channel.name.clone();
                            rsx! {
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
                            oncontextmenu: {
                                let name = ch_name.clone();
                                move |e: Event<MouseData>| {
                                    let mut items = Vec::new();
                                    // Leaving isn't something a left-click can do here.
                                    if voice_status().channel_id == Some(ch_id) {
                                        items.push(menu::item("Disconnect", "phone-off", move || {
                                            voice.send(voice::VoiceCmd::Leave);
                                        }));
                                    }
                                    if session().user.role == "admin" {
                                        let name = name.clone();
                                        items.push(menu::danger("Delete channel", "trash", move || {
                                            let mut confirm = confirm;
                                            confirm.set(Some(ConfirmAction::DeleteChannel {
                                                id: ch_id,
                                                name: name.clone(),
                                            }));
                                        }));
                                    }
                                    menu::open(ctx_menu, &e, items);
                                }
                            },
                            Icon { name: "volume", size: 15 }
                            span { class: "voice-channel-name chan-name", "{channel.name}" }
                            if session().user.role == "admin" {
                                span {
                                    class: "chan-del",
                                    title: "Delete voice channel",
                                    onclick: {
                                        let name = channel.name.clone();
                                        move |e: MouseEvent| {
                                            e.stop_propagation();
                                            confirm.set(Some(ConfirmAction::DeleteChannel { id: ch_id, name: name.clone() }));
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                        if let Some(occupants) = voice_rosters().get(&ch_id).cloned() {
                            div { class: "voice-occupants",
                                for (occupant, occ_sharing, occ_camera) in occupants {
                                    div {
                                        key: "{occupant.id}",
                                        class: "voice-occupant",
                                        oncontextmenu: {
                                            let occupant = occupant.clone();
                                            move |e: Event<MouseData>| {
                                                let is_admin = occupant.role == "admin";
                                                menu::open(ctx_menu, &e, member_items(
                                                    occupant.clone(),
                                                    false,
                                                    is_admin,
                                                ));
                                            }
                                        },
                                        UserAvatar { user: occupant.clone(), class: "dm-avatar occupant-avatar" }
                                        span { class: "voice-occupant-name", "{occupant.username}" }
                                        if occ_sharing {
                                            span { class: "live-pill", "LIVE" }
                                        }
                                        if occ_camera {
                                            span { class: "cam-pill", "CAM" }
                                        }
                                    }
                                }
                            }
                        }
                            }
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
                                            .unwrap_or(User {
                                                id: uid.unwrap_or(0),
                                                username: p.name.clone(),
                                                avatar: None,
                                                role: "member".into(),
                                            });
                                        rsx! { UserAvatar { user, class: "voice-avatar" } }
                                    }
                                    span { class: "voice-name", "{p.name}" }
                                    if p.sharing {
                                        span { class: "live-pill", "LIVE" }
                                        if !p.is_me {
                                            button {
                                                class: "watch-btn",
                                                title: "Watch {p.name}'s screen",
                                                onclick: {
                                                    let identity = p.identity.clone();
                                                    move |_| voice.send(voice::VoiceCmd::WatchScreen {
                                                        identity: identity.clone(),
                                                    })
                                                },
                                                "Watch"
                                            }
                                        }
                                    }
                                    if p.camera {
                                        span { class: "cam-pill", "CAM" }
                                        if !p.is_me {
                                            button {
                                                class: "watch-btn",
                                                title: "Watch {p.name}'s camera",
                                                onclick: {
                                                    let identity = p.identity.clone();
                                                    move |_| voice.send(voice::VoiceCmd::WatchCamera {
                                                        identity: identity.clone(),
                                                    })
                                                },
                                                "Watch"
                                            }
                                        }
                                    }
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
                            if audio_settings().voice_mode == "ptt" {
                                div {
                                    class: if voice_status().ptt_held { "ptt-hint held" } else { "ptt-hint" },
                                    if voice_status().ptt_held {
                                        "transmitting — {audio_settings().ptt_key}"
                                    } else {
                                        "hold {audio_settings().ptt_key} to talk"
                                    }
                                }
                            }
                            if let Some(monitors) = share_picker() {
                                div { class: "share-picker",
                                    div { class: "share-picker-title", "Share which screen?" }
                                    for m in monitors {
                                        button {
                                            key: "{m.index}",
                                            class: "share-picker-option",
                                            onclick: move |_| {
                                                voice.send(voice::VoiceCmd::StartScreenShare { monitor: Some(m.index) });
                                                share_picker.set(None);
                                            },
                                            Icon { name: "screen", size: 14 }
                                            "{m.label}"
                                        }
                                    }
                                    button {
                                        class: "share-picker-cancel",
                                        onclick: move |_| share_picker.set(None),
                                        "Cancel"
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
                                    class: if voice_status().deafened { "voice-btn muted" } else { "voice-btn" },
                                    title: if voice_status().deafened { "Undeafen" } else { "Deafen" },
                                    onclick: move |_| voice.send(voice::VoiceCmd::ToggleDeafen),
                                    if voice_status().deafened { Icon { name: "headphones-off" } } else { Icon { name: "headphones" } }
                                }
                                button {
                                    class: if voice_status().sharing_self { "voice-btn sharing" } else { "voice-btn" },
                                    title: if voice_status().sharing_self { "Stop sharing your screen" } else { "Share your screen" },
                                    onclick: move |_| {
                                        if voice_status().sharing_self {
                                            voice.send(voice::VoiceCmd::StopScreenShare);
                                        } else if share_picker().is_some() {
                                            share_picker.set(None);
                                        } else {
                                            let monitors = share::list_monitors();
                                            if monitors.len() > 1 {
                                                share_picker.set(Some(monitors));
                                            } else {
                                                voice.send(voice::VoiceCmd::StartScreenShare { monitor: None });
                                            }
                                        }
                                    },
                                    Icon { name: "screen" }
                                }
                                button {
                                    class: if voice_status().camera_self { "voice-btn camera-on" } else { "voice-btn" },
                                    title: if voice_status().camera_self { "Turn off your camera" } else { "Turn on your camera" },
                                    onclick: move |_| {
                                        if voice_status().camera_self {
                                            voice.send(voice::VoiceCmd::StopCamera);
                                        } else {
                                            voice.send(voice::VoiceCmd::StartCamera);
                                        }
                                    },
                                    Icon { name: "camera" }
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
                                update_progress.set(0.0);
                                let url = format!("{}{}", session().base_url, info.url);
                                let result = download_with_progress(&url, update_progress).await;
                                match result {
                                    Ok(bytes) => {
                                        if let Err(e) = apply_self_update(bytes) {
                                            status.set(e);
                                            updating.set(false);
                                        }
                                    }
                                    Err(e) => {
                                        status.set(format!("update failed: {e} — click the banner to retry"));
                                        updating.set(false);
                                    }
                                }
                            });
                        },
                        if updating() { "⬇ downloading update…" } else { "⬆ Update v{info.version} — install & restart" }
                    }
                }
                div { class: "new-channel-row",
                    button {
                        class: "new-channel-kind",
                        title: if new_channel_voice() { "Creating a voice channel — click for text" } else { "Creating a text channel — click for voice" },
                        onclick: move |_| new_channel_voice.set(!new_channel_voice()),
                        if new_channel_voice() { Icon { name: "volume", size: 14 } } else { "#" }
                    }
                    input {
                        class: "new-channel",
                        placeholder: if new_channel_voice() { "+ new voice channel" } else { "+ new channel" },
                        value: "{new_channel}",
                        oninput: move |e| new_channel.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                add_channel();
                            }
                        },
                    }
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
                    button {
                        class: "logout",
                        title: "Settings",
                        onclick: move |_| open_settings("voice"),
                        Icon { name: "settings", size: 16 }
                    }
                    button { class: "logout", title: "Log out", onclick: logout, Icon { name: "power", size: 16 } }
                }
            }
            div { class: "main",
                div { class: "channel-header",
                    span { class: "channel-header-label", "{selected_label}" }
                    if selected().is_some_and(|c| c.kind == "dm") {
                        button {
                            class: "call-btn",
                            title: "Start a voice call",
                            onclick: move |_| {
                                let Some(channel) = selected() else { return };
                                let peer = dm_peer_name(&channel, session().user.id);
                                spawn(async move {
                                    match api::voice_token(&session(), channel.id).await {
                                        Ok(grant) => voice.send(voice::VoiceCmd::Join {
                                            channel_id: channel.id,
                                            channel_name: format!("@{peer}"),
                                            url: grant.url,
                                            token: grant.token,
                                        }),
                                        Err(e) => status.set(e),
                                    }
                                });
                            },
                            Icon { name: "phone", size: 16 }
                        }
                    }
                    input {
                        class: "search-input",
                        placeholder: "search messages…",
                        value: "{search_query}",
                        // Search terms aren't prose; red squiggles under names
                        // and slang would be noise.
                        spellcheck: "false",
                        oncontextmenu: move |e: Event<MouseData>| {
                            menu::open(ctx_menu, &e, menu::text_field_items())
                        },
                        oninput: move |e| search_query.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                let q = search_query();
                                if q.trim().is_empty() {
                                    return;
                                }
                                spawn(async move {
                                    match api::search(&session(), &q).await {
                                        Ok(results) => search_results.set(Some(results)),
                                        Err(e) => status.set(e),
                                    }
                                });
                            }
                        },
                    }
                    // Jon's ask: the tab lives at the top right of the middle
                    // panel and swaps the body between chat and the player.
                    div { class: "view-tabs",
                        button {
                            class: if music_open() { "view-tab" } else { "view-tab active" },
                            onclick: move |_| music_open.set(false),
                            "Chat"
                        }
                        button {
                            class: if music_open() { "view-tab active" } else { "view-tab" },
                            onclick: move |_| {
                                music_open.set(true);
                                spawn(async move {
                                    if let Ok(state) = api::music_state(&session()).await {
                                        music.set(state);
                                    }
                                });
                            },
                            if music().active && !music().paused {
                                span { class: "view-tab-dot" }
                            }
                            "Music"
                        }
                    }
                }
                if music_open() {
                    MusicPlayer { music, volume: music_volume }
                }
                div { class: if music_open() { "messages music-chat" } else { "messages" },
                    // column-reverse container keeps the view pinned to the
                    // newest message, so render newest first.
                    {
                        // Oldest message you hadn't seen: the NEW line goes
                        // above it (rendered after it, in this flipped list).
                        let first_unread = divider_at().and_then(|last_read| {
                            messages().iter().map(|m| m.id).filter(|id| *id > last_read).min()
                        });
                        rsx! {
                            for (msg, compact) in group_messages(&messages()).into_iter().rev() {
                                {
                                    let is_target = highlight_msg() == Some(msg.id);
                                    let divider_here = Some(msg.id) == first_unread;
                                    let msg_id = msg.id;
                                    rsx! {
                                        div { class: if is_target { "hit-wrap" } else { "" },
                                            MessageRow { key: "{msg_id}", msg, compact }
                                        }
                                        if divider_here {
                                            div { class: "new-divider", span { class: "new-pill", "NEW" } }
                                        }
                                    }
                                }
                            }
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
                                                reply_to: None,
                                            });
                                        }
                                        gif_open.set(false);
                                    },
                                }
                            }
                        }
                    }
                }
                if emoji_open() {
                    div { class: "emoji-panel",
                        input {
                            class: "gif-search",
                            placeholder: "Search emoji…",
                            value: "{emoji_query}",
                            oninput: move |e| emoji_query.set(e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Escape {
                                    emoji_open.set(false);
                                }
                            },
                        }
                        div { class: "emoji-scroll",
                            // Server emojis first — they're the ones people want.
                            {
                                let query = emoji_query().trim().to_lowercase();
                                let mine: Vec<shared::CustomEmoji> = emojis()
                                    .into_iter()
                                    .filter(|e| query.is_empty() || e.name.contains(&query))
                                    .collect();
                                rsx! {
                                    div { class: "emoji-section",
                                        div { class: "emoji-cat", "Server emojis" }
                                        div { class: "emoji-grid",
                                            for emoji in mine {
                                                button {
                                                    key: "c{emoji.id}",
                                                    class: "emoji-cell custom",
                                                    title: ":{emoji.name}:",
                                                    onclick: {
                                                        let name = emoji.name.clone();
                                                        move |_| {
                                                            draft.set(format!("{}:{name}: ", draft()));
                                                            notify_typing();
                                                        }
                                                    },
                                                    img { class: "custom-emoji", src: "{emoji.url}" }
                                                }
                                            }
                                            button {
                                                class: "emoji-cell emoji-add",
                                                title: "Upload a new server emoji (named after the file)",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let Some(file) = rfd::AsyncFileDialog::new()
                                                            .add_filter("Images", &["png", "gif", "jpg", "jpeg", "webp"])
                                                            .pick_file()
                                                            .await
                                                        else {
                                                            return;
                                                        };
                                                        let filename = file.file_name();
                                                        let bytes = file.read().await;
                                                        if bytes.len() > 2 * 1024 * 1024 {
                                                            status.set("emoji too large (max 2 MB)".into());
                                                            return;
                                                        }
                                                        // Name comes from the file stem, tidied to :snake_case:.
                                                        let stem = filename
                                                            .rsplit_once('.')
                                                            .map(|(s, _)| s.to_owned())
                                                            .unwrap_or_else(|| filename.clone());
                                                        let name: String = stem
                                                            .to_lowercase()
                                                            .chars()
                                                            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                                                            .collect();
                                                        match api::upload(&session(), &filename, bytes).await {
                                                            Ok(url) => {
                                                                // Arrives back via the EmojisChanged broadcast.
                                                                match api::create_emoji(&session(), name.clone(), url).await {
                                                                    Ok(_) => status.set(format!("added :{name}:")),
                                                                    Err(e) => status.set(e),
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
                            }
                            if emoji_query().trim().is_empty() {
                                // Full catalog, grouped.
                                for (category, list) in emoji::CATALOG {
                                    div { key: "{category}", class: "emoji-section",
                                        div { class: "emoji-cat", "{category}" }
                                        div { class: "emoji-grid",
                                            for (glyph, keywords) in list.iter() {
                                                button {
                                                    key: "{glyph}",
                                                    class: "emoji-cell",
                                                    title: "{keywords}",
                                                    onclick: move |_| {
                                                        draft.set(format!("{}{glyph}", draft()));
                                                        notify_typing();
                                                    },
                                                    "{glyph}"
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                {
                                    let hits = emoji::search(&emoji_query());
                                    rsx! {
                                        if hits.is_empty() {
                                            div { class: "gif-status", "nothing matches \"{emoji_query}\"" }
                                        }
                                        div { class: "emoji-grid",
                                            for (glyph, keywords) in hits {
                                                button {
                                                    key: "{glyph}",
                                                    class: "emoji-cell",
                                                    title: "{keywords}",
                                                    onclick: move |_| {
                                                        draft.set(format!("{}{glyph}", draft()));
                                                        notify_typing();
                                                    },
                                                    "{glyph}"
                                                }
                                            }
                                        }
                                    }
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
                                                        reply_to: None,
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
                                                let name = sticker.name.clone();
                                                move |_| {
                                                    confirm.set(Some(ConfirmAction::DeleteSticker {
                                                        id,
                                                        name: name.clone(),
                                                    }));
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
                        // React with the server's own emojis too.
                        for custom in emojis() {
                            button {
                                key: "c{custom.id}",
                                title: ":{custom.name}:",
                                onclick: {
                                    let token = format!(":{}:", custom.name);
                                    move |_| {
                                        ws.send(ClientEvent::ToggleReaction {
                                            message_id: target,
                                            emoji: token.clone(),
                                        });
                                        react_target.set(None);
                                    }
                                },
                                img { class: "custom-emoji", src: "{custom.url}" }
                            }
                        }
                        button {
                            class: "react-palette-close",
                            onclick: move |_| react_target.set(None),
                            "✕"
                        }
                    }
                }
                if let Some(target) = replying_to() {
                    div { class: "reply-bar",
                        Icon { name: "reply", size: 13 }
                        span { class: "reply-bar-label", "Replying to {target.author.username}" }
                        span { class: "reply-bar-snippet",
                            {target.content.chars().take(80).collect::<String>()}
                        }
                        button {
                            class: "pending-remove",
                            title: "Cancel reply",
                            onclick: move |_| replying_to.set(None),
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
                {
                    // Slash commands: no syntax to memorize.
                    let commands = slash_suggestions(&draft());
                    rsx! {
                        if !commands.is_empty() {
                            div { class: "mention-pop slash-pop",
                                for (i, (name, help)) in commands.iter().enumerate() {
                                    div {
                                        key: "{name}",
                                        class: if i == slash_sel() % commands.len() { "mention-row selected" } else { "mention-row" },
                                        onclick: {
                                            let name = *name;
                                            move |_| {
                                                draft.set(format!("{name} "));
                                                slash_sel.set(0);
                                            }
                                        },
                                        span { class: "slash-name", "{name}" }
                                        span { class: "slash-help", "{help}" }
                                    }
                                }
                            }
                        }
                    }
                }
                {
                    let suggestions = mention_suggestions(&draft(), &members());
                    rsx! {
                        if !suggestions.is_empty() {
                            div { class: "mention-pop",
                                for (i, name) in suggestions.iter().enumerate() {
                                    div {
                                        key: "{name}",
                                        class: if i == mention_sel() % suggestions.len() { "mention-row selected" } else { "mention-row" },
                                        onclick: {
                                            let name = name.clone();
                                            move |_| {
                                                draft.set(complete_mention(&draft(), &name));
                                                mention_sel.set(0);
                                            }
                                        },
                                        "@{name}"
                                    }
                                }
                            }
                        }
                    }
                }
                // Media dock: the one place embeds actually play. Deliberately
                // outside the message list so chat re-renders can't kill it.
                if let Some(np) = now_playing() {
                    div { class: "player-dock",
                        div { class: "player-dock-head",
                            span { class: "player-dock-title", title: "{np.title}", "{np.title}" }
                            input {
                                class: "player-dock-vol",
                                r#type: "range",
                                min: "0",
                                max: "100",
                                title: "Player volume",
                                value: "{player_volume}",
                                oninput: move |e| {
                                    let Ok(v) = e.value().parse::<i64>() else { return };
                                    player_volume.set(v);
                                    // Best-effort volume via each host's postMessage API.
                                    dioxus::document::eval(&format!(
                                        "var f=document.querySelector('.player-dock iframe');\
                                         if(f){{var w=f.contentWindow,s=f.src;\
                                         if(s.indexOf('soundcloud')>=0)w.postMessage(JSON.stringify({{method:'setVolume',value:{v}}}),'*');\
                                         else if(s.indexOf('youtube')>=0)w.postMessage(JSON.stringify({{event:'command',func:'setVolume',args:[{v}]}}),'*');\
                                         else if(s.indexOf('vimeo')>=0)w.postMessage(JSON.stringify({{method:'setVolume',value:{}}}),'*');}}",
                                        v as f64 / 100.0
                                    ));
                                },
                            }
                            button {
                                class: "player-dock-close",
                                title: "Stop and close",
                                onclick: move |_| now_playing.set(None),
                                Icon { name: "x", size: 12 }
                            }
                        }
                        iframe {
                            key: "{np.embed}",
                            class: "player-dock-frame",
                            src: "{np.embed}",
                            style: "height: {np.height}px",
                            // Eager: Edge's lazy-loading intervention otherwise
                            // defers the frame into a permanent black box.
                            "loading": "eager",
                            allow: "autoplay; encrypted-media; clipboard-write; picture-in-picture; fullscreen",
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
                            if opening {
                                emoji_open.set(false);
                                sticker_open.set(false);
                                if gif_results().is_empty() {
                                    search_gifs(String::new());
                                }
                            }
                        },
                        "GIF"
                    }
                    button {
                        class: "attach emoji-btn",
                        title: "Insert an emoji",
                        onclick: move |_| {
                            let opening = !emoji_open();
                            emoji_open.set(opening);
                            if opening {
                                emoji_query.set(String::new());
                                gif_open.set(false);
                                sticker_open.set(false);
                            }
                        },
                        Icon { name: "smile" }
                    }
                    button {
                        class: "attach sticker-btn",
                        title: "Send a sticker",
                        onclick: move |_| {
                            let opening = !sticker_open();
                            sticker_open.set(opening);
                            if opening {
                                emoji_open.set(false);
                                gif_open.set(false);
                            }
                        },
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
                        placeholder: "Message {selected_name}",
                        value: "{draft}",
                        // Prose: worth spell-checking.
                        spellcheck: "true",
                        oncontextmenu: move |e: Event<MouseData>| {
                            menu::open(ctx_menu, &e, menu::text_field_items())
                        },
                        oninput: move |e| {
                            draft.set(e.value());
                            mention_sel.set(0);
                            slash_sel.set(0);
                            notify_typing();
                        },
                        onkeydown: move |e| {
                            // Slash popup takes the keys first — it only ever
                            // shows while the verb is still being typed.
                            let commands = slash_suggestions(&draft());
                            if !commands.is_empty() {
                                let sel = slash_sel() % commands.len();
                                match e.key() {
                                    Key::ArrowDown => {
                                        e.prevent_default();
                                        slash_sel.set(sel + 1);
                                        return;
                                    }
                                    Key::ArrowUp => {
                                        e.prevent_default();
                                        slash_sel.set((sel + commands.len() - 1) % commands.len());
                                        return;
                                    }
                                    Key::Tab => {
                                        e.prevent_default();
                                        draft.set(format!("{} ", commands[sel].0));
                                        slash_sel.set(0);
                                        return;
                                    }
                                    // Enter completes the command unless it's
                                    // already an exact match (then it sends).
                                    Key::Enter if draft().trim() != commands[sel].0 => {
                                        e.prevent_default();
                                        draft.set(format!("{} ", commands[sel].0));
                                        slash_sel.set(0);
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                            let suggestions = mention_suggestions(&draft(), &members());
                            if !suggestions.is_empty() {
                                let sel = mention_sel() % suggestions.len();
                                match e.key() {
                                    Key::ArrowDown => {
                                        e.prevent_default();
                                        mention_sel.set(sel + 1);
                                        return;
                                    }
                                    Key::ArrowUp => {
                                        e.prevent_default();
                                        mention_sel.set((sel + suggestions.len() - 1) % suggestions.len());
                                        return;
                                    }
                                    Key::Enter | Key::Tab => {
                                        e.prevent_default();
                                        draft.set(complete_mention(&draft(), &suggestions[sel]));
                                        mention_sel.set(0);
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                            if e.key() == Key::Enter {
                                send();
                            } else if e.key() == Key::Escape {
                                pending_files.set(Vec::new());
                                replying_to.set(None);
                            } else if e.key() == Key::Character("v".into())
                                && e.modifiers().contains(Modifiers::CONTROL)
                            {
                                paste_image();
                            }
                        },
                    }
                }
            }
            // The rail carries the queue while the Music tab is open, and the
            // member list the rest of the time.
            if music_open() {
                div { class: "members rail-queue",
                    MusicQueue { music, picked: music_picked }
                }
            } else {
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
                        oncontextmenu: {
                            let member = member.clone();
                            move |e: Event<MouseData>| {
                                menu::open(ctx_menu, &e, member_items(
                                    member.user.clone(),
                                    member.banned,
                                    member.user.role == "admin",
                                ));
                            }
                        },
                        UserAvatar { user: member.user.clone(), class: "member-avatar" }
                        span {
                            class: "member-name",
                            style: "color: {name_color(member.user.id, &members(), &tags())}",
                            "{member.user.username}"
                        }
                        for tag_id in member.tag_ids.iter().take(1) {
                            if let Some(tag) = tags().iter().find(|t| t.id == *tag_id) {
                                span {
                                    class: "tag-pill",
                                    style: "background: {tag.color}",
                                    "{tag.name}"
                                }
                            }
                        }
                        if member.banned {
                            span { class: "role-badge banned-badge", "BANNED" }
                        } else if member.user.role == "admin" {
                            span { class: "role-badge", "ADMIN" }
                        }
                        span { class: "member-dot" }
                    }
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

/// Links in a message worth asking the server for a card about: bare web URLs
/// that aren't already rendered inline as an image, video, or attachment.
fn preview_urls(text: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if !word.starts_with("http://") && !word.starts_with("https://") {
            continue;
        }
        // Trailing sentence punctuation isn't part of the link.
        let url = word
            .trim_end_matches(|c| matches!(c, '.' | ',' | ')' | '!' | '?' | ';' | ':' | '\''))
            .to_owned();
        if url.contains("/files/") || urls.contains(&url) {
            continue;
        }
        urls.push(url);
        // Two cards is plenty; a wall of them buries the conversation.
        if urls.len() == 2 {
            break;
        }
    }
    urls
}

/// Cards fetched this session. The server caches them too — this just keeps
/// scrolling back through history off the network entirely.
fn preview_cache() -> &'static std::sync::Mutex<HashMap<String, Option<shared::LinkPreview>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<shared::LinkPreview>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// An OpenGraph card under a message. Everything shown here was fetched by the
/// server, so linking somewhere never exposes anyone's IP to that site — and
/// for hosts we can embed, the play button swaps in a real player.
#[component]
fn LinkCard(url: String) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let mut card = use_signal(|| None::<shared::LinkPreview>);
    let mut now_playing = use_context::<Signal<Option<NowPlaying>>>();

    use_future({
        let url = url.clone();
        move || {
            let url = url.clone();
            async move {
                let cached = preview_cache().lock().ok().and_then(|c| c.get(&url).cloned());
                let preview = match cached {
                    Some(hit) => hit,
                    None => {
                        let fetched = api::link_preview(&session(), &url).await;
                        if let Ok(mut cache) = preview_cache().lock() {
                            cache.insert(url.clone(), fetched.clone());
                        }
                        fetched
                    }
                };
                card.set(preview);
            }
        }
    });

    let Some(preview) = card() else {
        return rsx! {};
    };
    let open_url = url.clone();
    let playable = preview.embed.clone();

    // Clicking play loads the track into the media dock (a stable element
    // outside the message list, so chat traffic can't interrupt playback).
    let start = {
        let preview = preview.clone();
        move |_| {
            let Some(embed) = preview.embed.clone() else { return };
            // YouTube only honors postMessage volume control with the JS API on.
            let embed = if embed.contains("/embed/") && embed.contains("youtube") {
                format!("{embed}?enablejsapi=1")
            } else {
                embed
            };
            now_playing.set(Some(NowPlaying {
                embed,
                height: preview.embed_height.max(120),
                title: if preview.title.is_empty() { preview.url.clone() } else { preview.title.clone() },
            }));
        }
    };

    rsx! {
        div { class: "link-card",
            if let Some(image) = preview.image.clone() {
                div { class: "link-card-media",
                    // Eager, not lazy: Edge's lazy intervention swaps deferred
                    // images for a grey cloud placeholder and never recovers.
                    img { class: "link-card-img", src: "{image}", loading: "eager" }
                    if playable.is_some() {
                        button {
                            class: "link-card-play",
                            title: "Play here",
                            onclick: start.clone(),
                            Icon { name: "play", size: 26 }
                        }
                    }
                }
            }
            div { class: "link-card-body",
                if !preview.site_name.is_empty() {
                    div { class: "link-card-site", "{preview.site_name}" }
                }
                if !preview.title.is_empty() {
                    div {
                        class: "link-card-title",
                        title: "{open_url}",
                        onclick: move |_| {
                            let _ = open::that(&open_url);
                        },
                        "{preview.title}"
                    }
                }
                if !preview.description.is_empty() {
                    div { class: "link-card-desc", "{preview.description}" }
                }
                if playable.is_some() && preview.image.is_none() {
                    button {
                        class: "link-card-playrow",
                        onclick: start,
                        Icon { name: "play", size: 14 }
                        span { "Play here" }
                    }
                }
            }
        }
    }
}

/// The bot's music player message, rendered as a live transport card:
/// `⟦player⟧playing` / title / up-next. Buttons drive the sidecar directly.
#[component]
fn PlayerCard(data: String) -> Element {
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut lines = data.lines();
    let paused = lines.next().unwrap_or_default().trim() == "paused";
    let title = lines.next().unwrap_or_default().to_owned();
    let up_next = lines.collect::<Vec<_>>().join(" ");

    let mut press = move |action: &str| {
        ws.send(ClientEvent::MusicControl { action: action.to_owned() });
    };
    // Precomputed so the rsx attributes stay single-typed.
    let toggle_title: &str = if paused { "Resume" } else { "Pause" };
    let toggle_icon: &'static str = if paused { "play" } else { "pause" };
    let toggle_action: &'static str = if paused { "resume" } else { "pause" };
    let state_label: &str = if paused { "paused" } else { "now playing" };

    rsx! {
        div { class: "player-card",
            button {
                class: "player-btn primary",
                title: "{toggle_title}",
                onclick: move |_| press(toggle_action),
                Icon { name: toggle_icon, size: 18 }
            }
            div { class: "player-info",
                div { class: "player-title",
                    span { class: "player-state", "{state_label}" }
                    "{title}"
                }
                if !up_next.is_empty() {
                    div { class: "player-next", "{up_next}" }
                }
            }
            button {
                class: "player-btn",
                title: "Skip to the next track",
                onclick: move |_| press("skip"),
                Icon { name: "skip", size: 16 }
            }
            button {
                class: "player-btn",
                title: "Stop and leave voice",
                onclick: move |_| press("stop"),
                Icon { name: "stop", size: 16 }
            }
        }
    }
}

/// Really exit (closing the window only hides to the tray). Returns `()` so it
/// can sit in an event handler.
fn quit_now(reason: &str) {
    api::debug_log(reason);
    std::process::exit(0);
}

/// The image URL for a `:name:` token, when the server has that emoji.
fn custom_emoji_url(emojis: &[shared::CustomEmoji], token: &str) -> Option<String> {
    let name = token.strip_prefix(':')?.strip_suffix(':')?;
    emojis.iter().find(|e| e.name == name).map(|e| e.url.clone())
}

/// True when a message is nothing but `:emoji:` tokens — those render big,
/// the way Discord jumbos an emoji-only message.
fn only_emojis(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed.split_whitespace().all(|word| {
            word.strip_prefix(':')
                .and_then(|w| w.strip_suffix(':'))
                .is_some_and(|name| shared::emoji_name_problem(name).is_none())
        })
}

fn fmt_secs(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".into();
    }
    let total = secs as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// The music tab: the shared player, plus a queue everyone can edit. State
/// comes from the server once a second so every screen agrees.
#[component]
fn MusicPlayer(music: Signal<shared::MusicState>, volume: Signal<i64>) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let voice = use_coroutine_handle::<voice::VoiceCmd>();

    // Poll while the tab is open; closing it unmounts this and stops the loop.
    // The queue in the rail reads the same signal, so one poll feeds both.
    use_future(move || async move {
        loop {
            if let Ok(state) = api::music_state(&session()).await {
                music.set(state);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    let mut control = move |action: &'static str| {
        spawn(async move {
            let _ = api::music_control(&session(), action).await;
            if let Ok(state) = api::music_state(&session()).await {
                music.set(state);
            }
        });
    };
    let state = music();
    let toggle_title: &str = if state.paused { "Resume" } else { "Pause" };
    let toggle_icon: &'static str = if state.paused { "play" } else { "pause" };
    let state_label: &str = if state.paused { "paused" } else { "now playing" };
    let progress = match (&state.now_playing, state.position) {
        (Some(track), pos) => match track.duration {
            Some(len) if len > 0.0 => ((pos / len) * 100.0).clamp(0.0, 100.0),
            _ => 0.0,
        },
        _ => 0.0,
    };

    rsx! {
        div { class: "music-tab",
            if let Some(track) = state.now_playing.clone() {
                div { class: "music-now",
                    div { class: "music-art",
                        if let Some(art) = track.art.clone() {
                            img { src: "{art}", alt: "cover art", loading: "eager" }
                        } else {
                            span { class: "music-art-fallback", "♪" }
                        }
                    }
                    div { class: "music-meta",
                        div { class: "music-source", "{state_label}" }
                        div { class: "music-title", title: "{track.title}", "{track.title}" }
                        if !track.artist.is_empty() {
                            div { class: "music-artist", "{track.artist}" }
                        }
                        div { class: "music-scrub",
                            span { class: "music-time", {fmt_secs(state.position)} }
                            div { class: "music-bar",
                                div { class: "music-bar-fill", style: "width: {progress:.1}%" }
                            }
                            span { class: "music-time",
                                {track.duration.map(fmt_secs).unwrap_or_else(|| "--:--".into())}
                            }
                        }
                        div { class: "music-controls",
                            button {
                                class: "music-btn primary",
                                title: "{toggle_title}",
                                onclick: move |_| control(if music().paused { "resume" } else { "pause" }),
                                Icon { name: toggle_icon, size: 18 }
                            }
                            button {
                                class: "music-btn",
                                title: "Skip to the next track",
                                onclick: move |_| control("skip"),
                                Icon { name: "skip", size: 15 }
                            }
                            button {
                                class: "music-btn",
                                title: "Stop and leave voice",
                                onclick: move |_| control("stop"),
                                Icon { name: "stop", size: 15 }
                            }
                            div { class: "music-vol",
                                Icon { name: "volume", size: 15 }
                                input {
                                    r#type: "range",
                                    min: "0",
                                    max: "200",
                                    value: "{volume}",
                                    title: "Volume — just for you",
                                    oninput: move |e| {
                                        let Ok(v) = e.value().parse::<i64>() else { return };
                                        volume.set(v);
                                        let identity = music().bot_identity;
                                        if !identity.is_empty() {
                                            voice.send(voice::VoiceCmd::SetVolume {
                                                identity,
                                                volume: v as f32 / 100.0,
                                            });
                                        }
                                    },
                                }
                                span { class: "music-vol-label",
                                    Icon { name: "user", size: 12 }
                                    "just you"
                                }
                            }
                        }
                    }
                }
            } else {
                div { class: "music-empty",
                    span { class: "music-empty-note", "♪" }
                    div {
                        div { class: "music-empty-title", "Nothing playing" }
                        div { class: "music-empty-sub",
                            "Join a voice channel, then paste a track or playlist link below to start."
                        }
                    }
                }
            }

        }
    }
}

/// The queue, which lives in the right-hand rail while the Music tab is open:
/// the members list steps aside for it, and the chat underneath the player
/// gets the height back.
#[component]
fn MusicQueue(music: Signal<shared::MusicState>, picked: Signal<HashSet<u64>>) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let ctx_menu = use_context::<menu::MenuSignal>();

    let mut edit_queue = move |req: shared::MusicQueueRequest| {
        spawn(async move {
            let _ = api::music_queue(&session(), req).await;
            if let Ok(state) = api::music_state(&session()).await {
                music.set(state);
            }
        });
    };

    let state = music();
    let selected_count = picked().len();
    let remove_label = if selected_count > 0 {
        format!("Remove {selected_count}")
    } else {
        "Remove".to_string()
    };

    rsx! {
        div { class: "queue-panel",
            div { class: "music-queue-head",
                span { class: "music-queue-title", "Up next" }
                span { class: "music-queue-count",
                    {match state.queue.len() {
                        0 => "queue's empty".to_string(),
                        1 => "1 track".to_string(),
                        n => format!("{n} tracks"),
                    }}
                }
                span { class: "grow" }
                button {
                    class: "qbtn",
                    disabled: selected_count == 0,
                    onclick: move |_| {
                        let ids: Vec<u64> = picked().iter().copied().collect();
                        picked.write().clear();
                        edit_queue(shared::MusicQueueRequest {
                            action: "remove".into(),
                            ids,
                            ..Default::default()
                        });
                    },
                    Icon { name: "trash", size: 13 }
                    "{remove_label}"
                }
                button {
                    class: "qbtn danger",
                    disabled: state.queue.is_empty(),
                    onclick: move |_| {
                        picked.write().clear();
                        edit_queue(shared::MusicQueueRequest {
                            action: "clear".into(),
                            ..Default::default()
                        });
                    },
                    Icon { name: "list-x", size: 13 }
                    "Clear"
                }
            }

            div { class: "music-queue",
                for (i, track) in state.queue.iter().cloned().enumerate() {
                    {
                        let id = track.id;
                        let is_picked = picked().contains(&id);
                        let url = track.url.clone();
                        let title = track.title.clone();
                        rsx! {
                            div {
                                key: "{id}",
                                class: if is_picked { "music-row picked" } else { "music-row" },
                                oncontextmenu: move |e: Event<MouseData>| {
                                    let (url, title) = (url.clone(), title.clone());
                                    menu::open(ctx_menu, &e, vec![
                                        menu::item("Play next", "chevron-up", move || {
                                            let mut edit = edit_queue;
                                            edit(shared::MusicQueueRequest {
                                                action: "move".into(),
                                                id: Some(id),
                                                // Clamped server-side, so -index lands it on top.
                                                offset: Some(-(i as i64)),
                                                ..Default::default()
                                            });
                                        }),
                                        menu::item("Copy track link", "link", {
                                            let url = url.clone();
                                            move || menu::copy_to_clipboard(url.clone())
                                        }),
                                        menu::item("Copy title", "copy", move || {
                                            menu::copy_to_clipboard(title.clone())
                                        }),
                                        menu::danger("Remove from queue", "trash", move || {
                                            let (mut edit, mut picked) = (edit_queue, picked);
                                            picked.write().remove(&id);
                                            edit(shared::MusicQueueRequest {
                                                action: "remove".into(),
                                                ids: vec![id],
                                                ..Default::default()
                                            });
                                        }),
                                    ]);
                                },
                                button {
                                    class: if is_picked { "qcheck on" } else { "qcheck" },
                                    title: if is_picked { "Deselect" } else { "Select" },
                                    onclick: move |_| {
                                        if is_picked {
                                            picked.write().remove(&id);
                                        } else {
                                            picked.write().insert(id);
                                        }
                                    },
                                    Icon { name: "check", size: 12 }
                                }
                                span { class: "music-row-idx", "{i + 1}" }
                                div { class: "music-row-meta",
                                    div { class: "music-row-title", "{track.title}" }
                                    if !track.artist.is_empty() {
                                        div { class: "music-row-artist", "{track.artist}" }
                                    }
                                }
                                span { class: "music-row-dur",
                                    {track.duration.map(fmt_secs).unwrap_or_default()}
                                }
                                div { class: "music-move",
                                    button {
                                        title: "Move up",
                                        disabled: i == 0,
                                        onclick: move |_| edit_queue(shared::MusicQueueRequest {
                                            action: "move".into(),
                                            id: Some(id),
                                            offset: Some(-1),
                                            ..Default::default()
                                        }),
                                        Icon { name: "chevron-up", size: 14 }
                                    }
                                    button {
                                        title: "Move down",
                                        onclick: move |_| edit_queue(shared::MusicQueueRequest {
                                            action: "move".into(),
                                            id: Some(id),
                                            offset: Some(1),
                                            ..Default::default()
                                        }),
                                        Icon { name: "chevron-down", size: 14 }
                                    }
                                }
                            }
                        }
                    }
                }
                if state.queue.is_empty() {
                    div { class: "music-queue-empty",
                        "Paste a link below to queue something up."
                    }
                }
            }
        }
    }
}

/// Commands offered by the `/` popup above the compose box.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/play", "queue a SoundCloud track or playlist — /play <url>"),
    ("/pause", "pause the music"),
    ("/resume", "resume the music"),
    ("/skip", "skip to the next track"),
    ("/queue", "see what's playing and what's next"),
    ("/stop", "stop the music and leave voice"),
    ("/ask", "ask the bot a question — /ask <question>"),
    ("/image", "have the bot draw something — /image <prompt>"),
];

/// Matching commands while the verb is still being typed (empty once the
/// user moves on to arguments).
fn slash_suggestions(draft: &str) -> Vec<(&'static str, &'static str)> {
    let draft = draft.trim_start();
    if !draft.starts_with('/') {
        return Vec::new();
    }
    let verb = draft.split_whitespace().next().unwrap_or("/");
    // Already typing arguments: the popup has done its job.
    if draft.len() > verb.len() {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(verb))
        .copied()
        .collect()
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
    let members_ctx = use_context::<Signal<Vec<UserStatus>>>();
    let tags_ctx = use_context::<Signal<Vec<Tag>>>();
    let emojis_ctx = use_context::<Signal<Vec<shared::CustomEmoji>>>();
    let mut replying_ctx = use_context::<Signal<Option<Message>>>();
    let mut jump_ctx = use_context::<Signal<Option<i64>>>();
    let ctx_menu = use_context::<menu::MenuSignal>();
    let mut editing = use_signal(|| false);
    let mut edit_draft = use_signal(String::new);

    // The bot's music message renders as a transport card, not as text.
    let player = msg.content.strip_prefix(shared::PLAYER_MARKER).map(str::to_owned);
    let (images, videos, files, text) = match player {
        Some(_) => (Vec::new(), Vec::new(), Vec::new(), String::new()),
        None => extract_media(&msg.content),
    };
    let links = if player.is_some() { Vec::new() } else { preview_urls(&text) };
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

    // Right-click offers what you can actually do to a message — and nothing
    // else. Edit and Delete only appear when they'd work.
    let msg_menu = {
        let msg = msg.clone();
        move |e: Event<MouseData>| {
            let content = msg.content.clone();
            let msg_for_reply = msg.clone();
            let reply_to = msg.reply_to;
            let mut items = vec![
                menu::item("Reply", "reply", move || {
                    let mut replying = replying_ctx;
                    replying.set(Some(msg_for_reply.clone()));
                }),
                menu::item("Add reaction", "smile", move || {
                    let mut target = react_target;
                    target.set(Some(msg_id));
                }),
                menu::item("Copy text", "copy", move || {
                    menu::copy_selection_or(content.clone())
                }),
            ];
            if let Some(original) = reply_to {
                items.push(menu::item("Jump to the original", "reply", move || {
                    let mut jump = jump_ctx;
                    jump.set(Some(original));
                }));
            }
            if own {
                let content = msg.content.clone();
                items.push(menu::item("Edit message", "edit", move || {
                    let (mut draft, mut is_editing) = (edit_draft, editing);
                    draft.set(content.clone());
                    is_editing.set(true);
                }));
            }
            if own || session().user.role == "admin" {
                items.push(menu::danger("Delete message", "trash", move || {
                    ws.send(ClientEvent::DeleteMessage { message_id: msg_id });
                }));
            }
            menu::open(ctx_menu, &e, items);
        }
    };

    rsx! {
        div {
            class: if compact { "msg compact" } else { "msg" },
            oncontextmenu: msg_menu,
            div { class: "msg-actions",
                button {
                    title: "Reply",
                    onclick: {
                        let msg_for_reply = msg.clone();
                        move |_| replying_ctx.set(Some(msg_for_reply.clone()))
                    },
                    Icon { name: "reply", size: 16 }
                }
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
                }
                if own || session().user.role == "admin" {
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
                if let Some(preview) = msg.reply_preview.clone() {
                    div {
                        class: "reply-ref",
                        title: "Jump to the original message",
                        onclick: {
                            let target = msg.reply_to;
                            move |_| {
                                if let Some(id) = target {
                                    jump_ctx.set(Some(id));
                                }
                            }
                        },
                        Icon { name: "reply", size: 11 }
                        span { class: "reply-ref-author", "{preview.author}" }
                        span { class: "reply-ref-snippet",
                            {preview.content.chars().take(90).collect::<String>()}
                        }
                    }
                }
                if !compact {
                    div { class: "msg-head",
                        span {
                            class: "msg-author",
                            style: "color: {name_color(msg.author.id, &members_ctx(), &tags_ctx())}",
                            "{msg.author.username}"
                        }
                        span { class: "msg-time", {format_time(msg.created_at)} }
                    }
                }
                if editing() {
                    input {
                        class: "edit-input",
                        value: "{edit_draft}",
                        spellcheck: "true",
                        oncontextmenu: move |e: Event<MouseData>| {
                            menu::open(ctx_menu, &e, menu::text_field_items())
                        },
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
                } else if let Some(data) = player.clone() {
                    PlayerCard { data }
                } else if !text.is_empty() {
                    div { class: if only_emojis(&text) { "msg-body jumbo" } else { "msg-body" },
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
                        oncontextmenu: {
                            let src = src.clone();
                            move |e: Event<MouseData>| {
                                let src = src.clone();
                                menu::open(ctx_menu, &e, vec![
                                    menu::item("View image", "eye", {
                                        let src = src.clone();
                                        move || {
                                            let mut lightbox = lightbox;
                                            lightbox.set(Some(src.clone()));
                                        }
                                    }),
                                    menu::item("Save image as…", "download", {
                                        let src = src.clone();
                                        move || menu::save_url_as(src.clone())
                                    }),
                                    menu::item("Copy image link", "link", {
                                        let src = src.clone();
                                        move || menu::copy_to_clipboard(src.clone())
                                    }),
                                    menu::item("Open in browser", "external-link", move || {
                                        let _ = open::that(&src);
                                    }),
                                ]);
                            }
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
                        let menu_url = url.clone();
                        rsx! {
                            div {
                                key: "f{i}",
                                class: "msg-file",
                                title: "Download {filename}",
                                onclick: {
                                    let url = url.clone();
                                    move |_| {
                                        let _ = open::that(&url);
                                    }
                                },
                                oncontextmenu: move |e: Event<MouseData>| {
                                    let url = menu_url.clone();
                                    menu::open(ctx_menu, &e, vec![
                                        menu::item("Save as…", "download", {
                                            let url = url.clone();
                                            move || menu::save_url_as(url.clone())
                                        }),
                                        menu::item("Copy link", "link", {
                                            let url = url.clone();
                                            move || menu::copy_to_clipboard(url.clone())
                                        }),
                                        menu::item("Open in browser", "external-link", move || {
                                            let _ = open::that(&url);
                                        }),
                                    ]);
                                },
                                span { class: "msg-file-icon", Icon { name: "file", size: 22 } }
                                span { class: "msg-file-name", "{filename}" }
                                span { class: "msg-file-dl", "Download" }
                            }
                        }
                    }
                }
                for link in links {
                    LinkCard { key: "{link}", url: link }
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
                                // Server emojis show their image; unicode stays text.
                                if let Some(url) = custom_emoji_url(&emojis_ctx(), &emoji) {
                                    img { class: "custom-emoji", src: "{url}" }
                                } else {
                                    span { "{emoji}" }
                                }
                                span { " {count}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A memorable word-word-NN invite code in the house style.
fn random_invite_code() -> String {
    const A: &[&str] = &[
        "scoopity", "wiggly", "snazzy", "crunchy", "sneaky", "bouncy", "spicy", "wobbly",
        "zesty", "grumpy", "shiny", "fuzzy", "salty", "chunky", "peppy", "quirky",
    ];
    const B: &[&str] = &[
        "doopity", "walrus", "pickle", "goblin", "noodle", "biscuit", "penguin", "waffle",
        "gremlin", "burrito", "mango", "raccoon", "nugget", "pretzel", "yeti", "llama",
    ];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        A[nanos % A.len()],
        B[(nanos / 31) % B.len()],
        10 + (nanos / 977) % 90,
    )
}

fn avatar_hue(user_id: i64) -> i64 {
    (user_id * 137) % 360
}

/// The partial name being typed after a trailing `@`, if the draft ends
/// mid-mention (e.g. "hey @jo").
fn mention_partial(draft: &str) -> Option<String> {
    let idx = draft.rfind('@')?;
    let boundary_ok = idx == 0 || !draft[..idx].chars().last().unwrap().is_alphanumeric();
    let partial = &draft[idx + 1..];
    if !boundary_ok || !partial.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(partial.to_lowercase())
}

fn mention_suggestions(draft: &str, members: &[UserStatus]) -> Vec<String> {
    let Some(partial) = mention_partial(draft) else {
        return Vec::new();
    };
    let mut names: Vec<String> = members
        .iter()
        .filter(|m| !m.banned)
        .map(|m| m.user.username.clone())
        .filter(|n| n.to_lowercase().starts_with(&partial) && n.to_lowercase() != partial)
        .take(5)
        .collect();
    if "everyone".starts_with(&partial) && partial != "everyone" {
        names.push("everyone".into());
    }
    names
}

fn complete_mention(draft: &str, name: &str) -> String {
    match draft.rfind('@') {
        Some(idx) => format!("{}@{} ", &draft[..idx], name),
        None => draft.to_owned(),
    }
}

/// Username display color: first assigned tag's color, else the avatar hue.
fn name_color(user_id: i64, members: &[UserStatus], tags: &[Tag]) -> String {
    members
        .iter()
        .find(|m| m.user.id == user_id)
        .and_then(|m| m.tag_ids.first())
        .and_then(|tid| tags.iter().find(|t| t.id == *tid))
        .map(|t| t.color.clone())
        .unwrap_or_else(|| format!("hsl({}, 65%, 68%)", avatar_hue(user_id)))
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

/// The other participant's name in a DM channel.
fn dm_peer_name(channel: &Channel, me_id: i64) -> String {
    channel
        .dm_members
        .iter()
        .find(|u| u.id != me_id)
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "unknown".into())
}

/// The other participant in a DM channel, for avatar rendering.
fn dm_peer(channel: &Channel, me_id: i64) -> Option<User> {
    channel.dm_members.iter().find(|u| u.id != me_id).cloned()
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


/// Synthesized two-tone "ba-bloop" WAV, built once in memory.
#[cfg(windows)]
fn notification_wav() -> &'static [u8] {
    use std::sync::OnceLock;
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(|| {
        const RATE: u32 = 44100;
        let mut samples: Vec<i16> = Vec::new();
        fn tone(samples: &mut Vec<i16>, freq: f32, ms: u32, gain: f32) {
            let n = RATE * ms / 1000;
            for i in 0..n {
                let t = i as f32 / RATE as f32;
                let env = (1.0 - i as f32 / n as f32).powf(1.6);
                let s = (t * freq * std::f32::consts::TAU).sin() * env * gain;
                samples.push((s * 32767.0) as i16);
            }
        }
        tone(&mut samples, 659.25, 90, 0.32);
        samples.extend(std::iter::repeat(0).take((RATE * 35 / 1000) as usize));
        tone(&mut samples, 880.0, 150, 0.32);

        let data_len = (samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend(b"RIFF");
        wav.extend((36 + data_len).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(RATE.to_le_bytes());
        wav.extend((RATE * 2).to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(data_len.to_le_bytes());
        for s in samples {
            wav.extend(s.to_le_bytes());
        }
        wav
    })
}

/// Incoming-call ring: two double-pulses, distinct from the message bloop.
/// Plays on the voice output device (not the system default) so it lands
/// where the user actually listens.
fn play_ring_sound() {
    const RATE: u32 = 48000;
    let mut samples: Vec<f32> = Vec::new();
    for _ in 0..2 {
        for _ in 0..2 {
            let n = RATE * 120 / 1000;
            for i in 0..n {
                let t = i as f32 / RATE as f32;
                let env = (1.0 - i as f32 / n as f32).powf(1.2);
                samples.push(
                    ((t * 784.0 * std::f32::consts::TAU).sin()
                        + (t * 988.0 * std::f32::consts::TAU).sin() * 0.6)
                        * env
                        * 0.22,
                );
            }
            samples.extend(std::iter::repeat(0.0).take((RATE * 60 / 1000) as usize));
        }
        samples.extend(std::iter::repeat(0.0).take((RATE * 250 / 1000) as usize));
    }
    voice::play_samples_on_voice_output(samples, RATE);
}

#[cfg(windows)]
fn play_notification_sound() {
    use winapi::um::playsoundapi::{PlaySoundW, SND_ASYNC, SND_MEMORY};
    let wav = notification_wav();
    unsafe {
        PlaySoundW(wav.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_ASYNC);
    }
}

#[cfg(not(windows))]
fn play_notification_sound() {}

fn initial(username: &str) -> String {
    username.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
}

/// Download with per-chunk stall detection: 30s without a byte fails the
/// download instead of hanging forever.
async fn download_with_progress(url: &str, mut progress: Signal<f32>) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed ({})", resp.status()));
    }
    let total = resp.content_length();
    let mut stream = resp.bytes_stream();
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
            .await
            .map_err(|_| "download stalled (no data for 30s)".to_string())?;
        match chunk {
            Some(Ok(data)) => {
                bytes.extend_from_slice(&data);
                if let Some(total) = total.filter(|t| *t > 0) {
                    progress.set(bytes.len() as f32 / total as f32);
                }
            }
            Some(Err(e)) => return Err(e.to_string()),
            None => break,
        }
    }
    if let Some(total) = total {
        if (bytes.len() as u64) < total {
            return Err("download ended early".into());
        }
    }
    Ok(bytes)
}

/// The single-instance mutex handle, held for the process's lifetime.
/// Released explicitly right before an update relaunch spawns our successor.
#[cfg(windows)]
static INSTANCE_MUTEX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Only one NotDiscord at a time: hold a named mutex for the process's
/// lifetime; if it's already held, surface the existing instance's window
/// (it's probably hiding in the tray) and exit. Duplicate instances are how
/// updates break — the extra process keeps the renamed exe locked forever.
#[cfg(windows)]
fn ensure_single_instance() {
    // Dev escape hatch: test harnesses run a second instance on purpose.
    if std::env::var("NOTDISCORD_ALLOW_SECOND_INSTANCE").is_ok() {
        return;
    }
    use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::synchapi::CreateMutexW;
    use winapi::um::winuser::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW};

    let name: Vec<u16> = "Local\\NotDiscordSingleInstance\0".encode_utf16().collect();
    let title: Vec<u16> = "NotDiscord\0".encode_utf16().collect();
    for _ in 0..10 {
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
        if handle.is_null() {
            return; // Can't create mutexes at all: don't block launching.
        }
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            // We're the one instance; hold the mutex until the process dies.
            INSTANCE_MUTEX.store(handle as usize, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        unsafe { CloseHandle(handle) };
        // Someone's running. If their window exists, bring it up and bow out.
        let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if !hwnd.is_null() {
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            std::process::exit(0);
        }
        // Mutex held but no window: almost certainly an exiting process
        // (update relaunch, or a quit in progress). Wait it out briefly.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::process::exit(0);
}

#[cfg(not(windows))]
fn ensure_single_instance() {}

/// Let go of the single-instance mutex so an update relaunch can start
/// while this process is still winding down.
fn release_single_instance() {
    #[cfg(windows)]
    {
        use winapi::um::handleapi::CloseHandle;
        let handle = INSTANCE_MUTEX.swap(0, std::sync::atomic::Ordering::Relaxed);
        if handle != 0 {
            unsafe { CloseHandle(handle as *mut winapi::ctypes::c_void) };
        }
    }
}

/// Replace the running executable with `new_exe_bytes` and restart.
/// Windows allows renaming a running exe, so: rename self aside, write the
/// new binary at the original path, spawn it, exit.
fn apply_self_update(new_exe_bytes: Vec<u8>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Unique per-process name: never collides with a leftover another
    // process still has locked.
    let old = exe.with_file_name(format!("NotDiscord.old-{}.exe", std::process::id()));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| {
        let dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
        format!("could not stage update in {dir}: {e}")
    })?;
    if let Err(e) = std::fs::write(&exe, &new_exe_bytes) {
        // Roll back so the app still launches next time.
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("could not write update: {e}"));
    }
    release_single_instance();
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
