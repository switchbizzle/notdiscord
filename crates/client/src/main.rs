#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod emoji;
mod frames;
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

/// The logo as raw RGBA, for the places Windows wants pixels rather than a
/// resource: the window's own corner and the tray. Baked into the binary so
/// there's no file to lose next to the exe.
const LOGO_PNG: &[u8] = include_bytes!("../assets/logo-256.png");

pub fn logo_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(LOGO_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Paint the OS title bar in the app's own colours.
///
/// Windows draws the caption itself, so a dark app gets a bar in whatever
/// shade the system picked — Jon: "it sticks out too much". Windows 11 lets
/// an app name the colours; Windows 10 ignores these and keeps its own bar,
/// which is why the results aren't checked.
#[cfg(windows)]
fn style_title_bar(hwnd: isize) {
    use winapi::um::dwmapi::DwmSetWindowAttribute;

    // COLORREF is 0x00BBGGRR, not RGB.
    const CAPTION: u32 = 0x0022_1f1e; // #1e1f22, the sidebar's own grey
    const TEXT: u32 = 0x00a4_9b94; // #949ba4, the muted grey used for labels
    const BORDER: u32 = 0x0031_2d2b; // #2b2d31, one step up from the caption
    // Attribute ids, from dwmapi.h. winapi 0.3 predates the Windows 11 ones.
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWA_TEXT_COLOR: u32 = 36;

    unsafe {
        let dark: u32 = 1;
        for (attr, value) in [
            (DWMWA_USE_IMMERSIVE_DARK_MODE, dark),
            (DWMWA_CAPTION_COLOR, CAPTION),
            (DWMWA_TEXT_COLOR, TEXT),
            (DWMWA_BORDER_COLOR, BORDER),
        ] {
            DwmSetWindowAttribute(
                hwnd as _,
                attr,
                &value as *const u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

#[cfg(not(windows))]
fn style_title_bar(_hwnd: isize) {}

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

    // Hold the COM multithreaded apartment open for the process's whole life:
    // cpal caches WASAPI objects (device enumerator) in the apartment of the
    // thread that first created them, and if that apartment ever tears down,
    // later audio opens fail. This parked thread guarantees it never does.
    std::thread::Builder::new()
        .name("com-mta-anchor".into())
        .spawn(|| {
            voice::com_init_mta();
            loop {
                std::thread::park();
            }
        })
        .ok();

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
    // Reopen at the size and place you left it — updates restart the app, and
    // resizing after every one gets old.
    let saved = api::load_window();
    let mut window = WindowBuilder::new()
        .with_title("NotDiscord")
        .with_inner_size(match saved {
            Some(s) => LogicalSize::new(s.width, s.height),
            None => LogicalSize::new(1100.0, 720.0),
        })
        .with_min_inner_size(LogicalSize::new(900.0, 560.0));
    // The exe's icon comes from the resource table (build.rs); this is the one
    // the window itself carries, which is what alt-tab and the title bar show.
    if let Some((rgba, w, h)) = logo_rgba() {
        if let Ok(icon) = dioxus::desktop::tao::window::Icon::from_rgba(rgba, w, h) {
            window = window.with_window_icon(Some(icon));
        }
    }
    if let Some(state) = saved {
        if let (Some(x), Some(y)) = (state.x, state.y) {
            window = window.with_position(dioxus::desktop::tao::dpi::LogicalPosition::new(x, y));
        }
        window = window.with_maximized(state.maximized);
    }
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new()
                .with_window(window)
                .with_menu(None)
                // The webview's own menu is Back / Reload / Save as / Print —
                // none of which means anything here. `menu.rs` puts a menu
                // that fits the clicked element in its place.
                .with_disable_context_menu(true)
                // Live video for the Video tab. The WebView can only show what
                // it can fetch, so each stream is served as a JPEG at
                // http://ndvideo.localhost/<identity>:<camera|screen>.
                .with_custom_protocol("ndvideo", |_id, request| {
                    use dioxus::desktop::wry::http::Response;
                    let key = request.uri().path().trim_start_matches('/').to_owned();
                    // The page is served from another origin, so <img> is fine
                    // but anything scripted needs to be allowed explicitly.
                    match frames::latest(&key) {
                        Some(jpeg) => Response::builder()
                            .status(200)
                            .header("Content-Type", "image/jpeg")
                            .header("Cache-Control", "no-store")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(std::borrow::Cow::Owned(jpeg.to_vec()))
                            .unwrap(),
                        // Nothing decoded yet: the next tick will have one.
                        None => Response::builder()
                            .status(204)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(std::borrow::Cow::Borrowed(&[][..]))
                            .unwrap(),
                    }
                })
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

/// Which message the reaction palette is for, and which one to scroll to.
///
/// These are wrappers rather than bare `Signal<Option<i64>>` because dioxus
/// keys context by TYPE: two contexts of the same type are one context, the
/// later provider quietly wins, and every consumer of the first gets the
/// second. That is exactly what happened — the React button set the jump
/// target, so it scrolled to the message instead of opening the palette, and
/// the palette never opened at all (Jon, #feature-requests).
#[derive(Clone, Copy)]
pub struct ReactTarget(pub Signal<Option<i64>>);

#[derive(Clone, Copy)]
pub struct JumpTo(pub Signal<Option<i64>>);

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

    // The title bar is the OS's, so it has to be told our colours.
    use_hook(|| {
        #[cfg(windows)]
        {
            use dioxus::desktop::tao::platform::windows::WindowExtWindows;
            style_title_bar(window.hwnd() as isize);
        }
    });

    // System tray: created once, lives for the app's lifetime.
    let tray_handle: tray::TrayHandle = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(tray::create())));
    let tray_unread = use_context_provider(|| Signal::new(false));

    // Remember where and how big the window is, so an update (which restarts
    // the app) doesn't reset it. Resizing fires a storm of events, so the
    // write is debounced: the handler only stamps a signal.
    {
        let window = window.clone();
        let mut moved = use_signal(|| 0u64);
        dioxus::desktop::use_wry_event_handler(move |event, _| {
            use dioxus::desktop::tao::event::{Event, WindowEvent};
            if let Event::WindowEvent { event: WindowEvent::Resized(_) | WindowEvent::Moved(_), .. } = event {
                // Only a nudge. Reading geometry here catches the window
                // mid-maximize, when the size is already full-screen but
                // is_maximized() hasn't caught up — and that combination
                // would be saved as the size to restore to.
                let n = *moved.peek();
                moved.set(n + 1);
            }
        });
        use_future(move || {
            // Rc isn't Copy, and the future is built fresh each time.
            let window = window.clone();
            async move {
            let mut last: Option<api::WindowState> = api::load_window();
            let mut seen = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let ticks = moved();
                if ticks == seen {
                    continue;
                }
                seen = ticks;
                let w = &window.window;
                if w.is_minimized() {
                    continue;
                }
                let maximized = w.is_maximized();
                let scale = w.scale_factor();
                let mut next = last.unwrap_or(api::WindowState {
                    width: 1100.0,
                    height: 720.0,
                    x: None,
                    y: None,
                    maximized,
                });
                // Maximized keeps the previous size as what to restore to.
                if !maximized {
                    let size = w.inner_size().to_logical::<f64>(scale);
                    let pos = w.outer_position().ok().map(|p| p.to_logical::<f64>(scale));
                    next.width = size.width;
                    next.height = size.height;
                    next.x = pos.map(|p| p.x);
                    next.y = pos.map(|p| p.y);
                }
                next.maximized = maximized;
                if last != Some(next) {
                    api::save_window(&next);
                    last = Some(next);
                }
            }
            }
        });
    }

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
                            // The one-element loop is load-bearing: a `key` only
                            // forces a REMOUNT inside list diffing. Bare in this
                            // position, switching servers just swapped the prop,
                            // and MainView's use_signal(|| session) never reads
                            // props again — the rail looked dead (you stayed
                            // logged into the old server).
                            for s in [s] {
                                MainView { key: "{s.base_url}", session: s.clone() }
                            }
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
    // No context menu here. The rail renders above the component that
    // provides one, and use_context panics rather than returning None.
    // Forgetting a server lives on the login screen instead, which is
    // exactly where you are when you're deciding whether you still want it.
    let file = servers();
    rsx! {
        div { class: "server-rail",
            for (i, s) in file.servers.iter().enumerate() {
                {
                    // Signed out: still yours, still one click away, just not
                    // logged in. It stays greyed until you sign back in.
                    let signed_out = s.token.is_empty();
                    let label = if signed_out {
                        format!("{} — signed out", s.server_name)
                    } else {
                        format!("{} — {}", s.server_name, s.user.username)
                    };
                    let name = s.server_name.clone();
                    let icon = s.server_icon.clone();
                    let hue = rail_hue(s);
                    let initial_letter = initial(&name);
                    rsx! {
                        button {
                            key: "{s.base_url}",
                            class: match (i == file.active && !adding(), signed_out) {
                                (true, true) => "rail-server active signed-out",
                                (true, false) => "rail-server active",
                                (false, true) => "rail-server signed-out",
                                (false, false) => "rail-server",
                            },
                            title: "{label}",
                            style: "background: hsl({hue}, 55%, 42%)",
                            onclick: move |_| {
                                let mut file = api::load_servers();
                                file.active = i;
                                api::save_servers(&file);
                                servers.set(file);
                                adding.set(false);
                            },
                            if let Some(icon) = icon {
                                img { class: "rail-img", src: "{icon}" }
                            } else {
                                {initial_letter}
                            }
                        }
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
            // active_entry, not active_session: a signed-out server has no
            // session, and its address is exactly what this form wants.
            .active_entry()
            .filter(|_| !*adding.peek())
            .map(|s| s.base_url.clone())
            .or_else(|| std::env::var("NOTDISCORD_SERVER").ok())
            .unwrap_or_else(|| "https://notdiscord.switchbhost.com".into())
    });
    let mut username = use_signal(move || {
        // A signed-out entry remembers whose account it was. Retyping the
        // password is the point of signing out; retyping the name isn't.
        servers
            .peek()
            .active_entry()
            .filter(|s| !*adding.peek() && s.token.is_empty())
            .map(|s| s.user.username.clone())
            .unwrap_or_default()
    });
    let mut password = use_signal(String::new);
    let mut invite = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);
    // Forgot-password flow: closed -> code emailed -> new password set.
    let mut forgot = use_signal(|| false);
    let mut reset_sent = use_signal(|| false);
    let mut reset_code = use_signal(String::new);
    let mut reset_pw = use_signal(String::new);
    let mut notice = use_signal(String::new);
    // A server with nobody on it can't be logged into, so the form offers to
    // claim it instead. Probed whenever the address settles.
    let mut needs_setup = use_signal(|| false);
    let mut server_name = use_signal(String::new);
    // Logging in is what almost everyone is here to do; an invite code box on
    // that screen is a question nobody signing in can answer (switchb).
    let mut registering = use_signal(|| false);

    use_future(move || async move {
        let mut last = String::new();
        loop {
            let current = base_url();
            if current != last {
                last = current.clone();
                // Let typing settle before asking; the address bar is edited
                // one character at a time.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if base_url() == current {
                    match api::server_info(&current).await {
                        Ok(info) => {
                            needs_setup.set(info.needs_setup);
                            if info.needs_setup && server_name().trim().is_empty() {
                                server_name.set(info.name);
                            }
                        }
                        Err(_) => needs_setup.set(false),
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    });

    let claim = move |_| {
        if busy() {
            return;
        }
        spawn(async move {
            busy.set(true);
            error.set(String::new());
            match api::setup(&base_url(), username(), password(), server_name(), invite()).await {
                Ok(s) => {
                    servers.set(api::upsert_server(s));
                    adding.set(false);
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
    };

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
                if needs_setup() {
                    div { class: "login-notice",
                        "Nobody has an account on this server yet — the first one is yours, and it's the admin."
                    }
                    label { "Server name" }
                    input {
                        value: "{server_name}",
                        oninput: move |e| server_name.set(e.value()),
                    }
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
                            if !needs_setup() {
                                submit(registering());
                            }
                        }
                    },
                }
                if needs_setup() {
                    label { "Invite code for everyone else (optional)" }
                    input {
                        value: "{invite}",
                        oninput: move |e| invite.set(e.value()),
                    }
                    div { class: "settings-hint",
                        "Leave it empty and anyone who can reach this server can register."
                    }
                } else if registering() {
                    label { "Invite code" }
                    input {
                        value: "{invite}",
                        oninput: move |e| invite.set(e.value()),
                    }
                    div { class: "settings-hint", "Whoever runs this server has it." }
                }
                if !error().is_empty() {
                    div { class: "login-error", "{error}" }
                }
                if !notice().is_empty() {
                    div { class: "login-notice", "{notice}" }
                }
                div { class: "login-buttons",
                    if needs_setup() {
                        button {
                            class: "primary",
                            disabled: busy(),
                            onclick: claim,
                            if busy() { "Setting up…" } else { "Set up this server" }
                        }
                    } else {
                        button {
                            class: "primary",
                            disabled: busy(),
                            onclick: move |_| submit(registering()),
                            if registering() { "Create account" } else { "Log in" }
                        }
                    }
                }
                if !needs_setup() {
                    button {
                        class: "login-switch",
                        onclick: move |_| {
                            error.set(String::new());
                            notice.set(String::new());
                            forgot.set(false);
                            registering.toggle();
                        },
                        if registering() { "I already have an account" } else { "I need an account" }
                    }
                }
                // No accounts yet means no password to have forgotten, and
                // neither does an account you haven't made.
                if needs_setup() || registering() {
                } else if !forgot() {
                    button {
                        class: "login-cancel",
                        onclick: move |_| {
                            forgot.set(true);
                            error.set(String::new());
                            notice.set(String::new());
                        },
                        "forgot password?"
                    }
                } else {
                    div { class: "login-forgot",
                        if !reset_sent() {
                            div { class: "settings-hint",
                                "enter your username above, and a reset code goes to your verified email"
                            }
                            button {
                                disabled: busy() || username().trim().is_empty(),
                                onclick: move |_| {
                                    spawn(async move {
                                        busy.set(true);
                                        error.set(String::new());
                                        match api::forgot_password(&base_url(), username().trim().to_string()).await {
                                            Ok(()) => {
                                                reset_sent.set(true);
                                                notice.set("if that account has a verified email, a code is on its way".into());
                                            }
                                            Err(e) => error.set(e),
                                        }
                                        busy.set(false);
                                    });
                                },
                                "Email me a code"
                            }
                        } else {
                            label { "Reset code" }
                            input {
                                spellcheck: "false",
                                value: "{reset_code}",
                                oninput: move |e| reset_code.set(e.value()),
                            }
                            label { "New password" }
                            input {
                                r#type: "password",
                                value: "{reset_pw}",
                                oninput: move |e| reset_pw.set(e.value()),
                            }
                            button {
                                class: "primary",
                                disabled: busy() || reset_code().trim().is_empty() || reset_pw().is_empty(),
                                onclick: move |_| {
                                    spawn(async move {
                                        busy.set(true);
                                        error.set(String::new());
                                        let result = api::reset_password(
                                            &base_url(),
                                            username().trim().to_string(),
                                            reset_code().trim().to_string(),
                                            reset_pw(),
                                        )
                                        .await;
                                        match result {
                                            Ok(()) => {
                                                forgot.set(false);
                                                reset_sent.set(false);
                                                reset_code.set(String::new());
                                                reset_pw.set(String::new());
                                                password.set(String::new());
                                                notice.set("password changed — log in with the new one".into());
                                            }
                                            Err(e) => error.set(e),
                                        }
                                        busy.set(false);
                                    });
                                },
                                "Reset password"
                            }
                        }
                        button {
                            class: "login-cancel",
                            onclick: move |_| {
                                forgot.set(false);
                                reset_sent.set(false);
                                notice.set(String::new());
                            },
                            "back to sign in"
                        }
                    }
                }
                if adding() && !servers().servers.is_empty() {
                    button {
                        class: "login-cancel",
                        onclick: move |_| adding.set(false),
                        "cancel"
                    }
                }
                // The deliberate half of what "Log out" used to do by
                // accident. Only for a server already in the rail, and only
                // once there's another one to fall back to.
                if !adding() && servers().servers.len() > 1 {
                    button {
                        class: "login-cancel",
                        onclick: move |_| {
                            let active = api::load_servers().active;
                            servers.set(api::remove_server(active));
                        },
                        "forget this server"
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

/// What the lightbox is showing: the images it was opened from, and where in
/// them we are. Carrying the list rather than looking one up on demand means
/// the arrows step through the same set the click came from — the channel's
/// photos when you clicked one in chat, the Files panel's when you clicked
/// there.
#[derive(Clone, PartialEq)]
struct Lightbox {
    urls: Vec<String>,
    at: usize,
}

impl Lightbox {
    /// One image with nothing to step to.
    fn only(url: String) -> Self {
        Self { urls: vec![url], at: 0 }
    }

    /// `url` in the context of `urls`, or on its own if it isn't one of them.
    fn within(url: String, urls: Vec<String>) -> Self {
        match urls.iter().position(|u| *u == url) {
            Some(at) => Self { urls, at },
            None => Self::only(url),
        }
    }

    fn url(&self) -> String {
        self.urls[self.at].clone()
    }

    /// Wrapping, because falling off the end of a gallery just makes you
    /// press the other arrow twice to get back.
    fn step(&mut self, delta: i32) {
        let n = self.urls.len() as i32;
        if n > 1 {
            self.at = (((self.at as i32 + delta) % n + n) % n) as usize;
        }
    }
}

/// Every image in the channel as it currently stands, oldest first — reading
/// order, which is the order the arrows should walk. Its own type: dioxus
/// keys context by type, and a bare Signal<Vec<String>> is too easy to
/// collide with later.
#[derive(Clone, Copy)]
struct Gallery(Memo<Vec<String>>);

#[component]
fn MainView(session: api::Session) -> Element {
    let mut session = use_signal(move || session);
    use_context_provider(|| session);
    let mut servers_file = use_context::<Signal<api::ServersFile>>();
    let mut lightbox = use_context_provider(|| Signal::new(None::<Lightbox>));
    let mut react_target = use_signal(|| None::<i64>);
    use_context_provider(|| ReactTarget(react_target));
    let mut channels = use_signal(Vec::<Channel>::new);
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    let gallery = use_memo(move || {
        let base = session.peek().base_url.clone();
        messages()
            .iter()
            .flat_map(|m| extract_media(&m.content, &base).0)
            .collect::<Vec<String>>()
    });
    use_context_provider(|| Gallery(gallery));
    // Runs after the overlay is in the DOM, which is the earliest the element
    // can take focus — without it the arrow keys go nowhere until you click.
    use_effect(move || {
        if lightbox().is_some() {
            dioxus::document::eval(
                "const el = document.getElementById('lightbox'); if (el) el.focus();",
            );
        }
    });
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
    // The Files panel: Some(list) swaps the right rail for the channel's
    // attachment explorer (Jon's spec). Declared before open_channel so a
    // channel switch can close it.
    let mut files_open = use_signal(|| None::<Vec<shared::FileEntry>>);
    let mut open_channel = move |channel: Channel| {
        let previous = unread.write().remove(&channel.id);
        divider_at.set(
            previous
                .filter(|(count, _, _)| *count > 0)
                .map(|(_, _, last_read)| last_read),
        );
        files_open.set(None);
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
    // Which of Chat / Music / Video is showing in the middle panel.
    let mut view_tab = use_signal(|| "chat");
    let mut music = use_signal(shared::MusicState::default);
    let mut music_picked = use_signal(HashSet::<u64>::new);
    let mut music_volume = use_signal(|| 100i64);
    let mut profile_card = use_signal(|| None::<Profile>);
    let mut new_tag_name = use_signal(String::new);
    let mut new_tag_color = use_signal(|| "#5865f2".to_string());
    let mut replying_to = use_context_provider(|| Signal::new(None::<Message>));
    // The right rail shows the room, or your conversations. Jon's spec: a
    // text button beside MEMBERS in the same row, and clicking one swaps the
    // rail without moving anything else on screen.
    // Channels this account has muted: no sound, no toast, no push. The
    // messages still arrive and still count as unread — a mute is about
    // noise, not about hiding what people said.
    let mut muted = use_signal(std::collections::HashSet::<i64>::new);
    // The sidebar's groups. Empty on a server nobody has organised, which is
    // the same list it always drew.
    let mut categories = use_signal(Vec::<shared::ChannelCategory>::new);
    let mut new_category = use_signal(String::new);
    let mut stats = use_signal(|| None::<shared::ServerStats>);
    let mut rail_dms = use_signal(|| false);
    let mut jump_to = use_signal(|| None::<i64>);
    use_context_provider(|| JumpTo(jump_to));
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
    let mut emoji_sel = use_signal(|| 0usize);
    let mut emoji_dismissed = use_signal(|| None::<String>);
    let mut incoming_call = use_signal(|| None::<(i64, User)>);
    let mut search_query = use_signal(String::new);
    let mut search_results = use_signal(|| None::<Vec<shared::SearchResult>>);
    let mut pins_open = use_signal(|| None::<Vec<Message>>);
    // Pings you'd otherwise miss: (id, channel_id, message_id, author, where, text).
    // Each expires on its own; clicking one jumps to the message.
    let mut toasts = use_signal(Vec::<(u64, i64, i64, User, String, String)>::new);
    let mut toast_seq = use_signal(|| 0u64);
    // True while the message list is scrolled away from the newest message,
    // which is when the "back to now" chip earns its place.
    let mut scrolled_up = use_signal(|| false);
    let mut highlight_msg = use_signal(|| None::<i64>);
    let mut bio_draft = use_signal(String::new);
    let mut editing_bio = use_signal(|| false);
    let mut status_draft = use_signal(String::new);
    let mut editing_status = use_signal(|| false);
    let mut settings_open = use_signal(|| false);
    let mut settings_tab = use_signal(|| "voice");
    let mut pw_current = use_signal(String::new);
    let mut pw_new = use_signal(String::new);
    let mut pw_confirm = use_signal(String::new);
    let mut pw_message = use_signal(|| (String::new(), false));
    // Email verification state on the Account tab. None = not fetched yet.
    let mut email_state = use_signal(|| None::<shared::EmailStatus>);
    let mut email_input = use_signal(String::new);
    let mut email_code = use_signal(String::new);
    let mut email_sent = use_signal(|| false);
    let mut email_message = use_signal(|| (String::new(), false));
    // Populated when the user must choose what to share (monitors + windows).
    let mut share_picker =
        use_signal(|| None::<(Vec<share::MonitorChoice>, Vec<share::WindowChoice>)>);
    use_context_provider(|| share_picker);
    // Thumbnails keyed "m{index}" / "w{hwnd}", filled in as each still is
    // grabbed so the picker paints immediately and fills in behind you.
    let mut share_thumbs = use_signal(HashMap::<String, String>::new);
    use_context_provider(|| share_thumbs);

    // Whenever the picker opens, grab one still per target on a background
    // thread. Sequential on purpose: a dozen simultaneous capture sessions
    // is a lot of GPU churn for pictures nobody has looked at yet.
    use_effect(move || {
        let Some((monitors, windows)) = share_picker() else {
            share_thumbs.write().clear();
            return;
        };
        let targets: Vec<(String, share::ShareTarget)> = monitors
            .iter()
            .map(|m| (format!("m{}", m.index), share::ShareTarget::Monitor(m.index)))
            .chain(
                windows
                    .iter()
                    .map(|w| (format!("w{}", w.hwnd), share::ShareTarget::Window(w.hwnd))),
            )
            .collect();
        spawn(async move {
            for (key, target) in targets {
                // Picker closed while we were working — stop capturing.
                if share_picker.peek().is_none() {
                    break;
                }
                let jpeg = tokio::task::spawn_blocking(move || {
                    voice::com_init_mta();
                    share::thumbnail(target)
                })
                .await
                .ok()
                .flatten();
                if let Some(jpeg) = jpeg {
                    use base64::Engine;
                    let uri = format!(
                        "data:image/jpeg;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&jpeg)
                    );
                    share_thumbs.write().insert(key, uri);
                }
            }
        });
    });
    let mut storage_info = use_signal(|| None::<shared::StorageInfo>);
    // None until the Server tab loads it; the draft is what's in the box.
    let mut invite_loaded = use_signal(|| false);
    let mut invite_draft = use_signal(String::new);
    let mut invite_message = use_signal(String::new);
    let mut persona_loaded = use_signal(|| false);
    let mut persona_draft = use_signal(String::new);
    let mut persona_message = use_signal(String::new);
    let mut bot_name_draft = use_signal(String::new);
    // 0 = don't announce; otherwise the channel releases are announced in.
    let mut announce_draft = use_signal(|| 0i64);
    let mut bot_model_draft = use_signal(String::new);
    // Credential key -> what the admin typed. Values are never loaded back
    // from the server, so an empty box means "leave it alone".
    let mut cred_drafts = use_signal(HashMap::<String, String>::new);
    let mut cred_status = use_signal(Vec::<shared::CredentialStatus>::new);
    let mut server_name_draft = use_signal(String::new);
    let mut retention_days = use_signal(|| 21i64);
    let mut audio_settings = use_signal(api::load_settings);
    let mut input_devices = use_signal(Vec::<String>::new);
    let mut output_devices = use_signal(Vec::<String>::new);

    // Server settings, opened by clicking the channel title.
    let mut server_settings_open = use_signal(|| false);
    let mut srv_pane = use_signal(|| "channel");
    // Which channel row is being renamed, and to what.
    let mut renaming_channel = use_signal(|| None::<(i64, String)>);

    // Commit the in-progress channel rename, if it says anything new.
    let mut save_channel_rename = move || {
        let Some((channel_id, draft)) = renaming_channel() else { return };
        let name = draft.trim().trim_start_matches('#').to_lowercase();
        renaming_channel.set(None);
        if name.is_empty() {
            return;
        }
        spawn(async move {
            if let Err(e) = api::rename_channel(&session(), channel_id, name).await {
                status.set(e);
            }
        });
    };

    let mut open_settings = move |tab: &'static str| {
        input_devices.set(voice::list_input_devices());
        output_devices.set(voice::list_output_devices());
        settings_tab.set(tab);
        settings_open.set(true);
    };

    // Everything the server panes show, fetched once when the panel opens.
    let mut open_server_settings = move |pane: &'static str| {
        srv_pane.set(pane);
        renaming_channel.set(None);
        server_name_draft.set(session().server_name);
        server_settings_open.set(true);
        if session().user.role != "admin" {
            return;
        }
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
                bot_model_draft.set(settings.model);
                cred_status.set(settings.credentials);
                cred_drafts.write().clear();
                // Unset means the server's default: the first text channel.
                announce_draft.set(settings.announce_channel.unwrap_or(-1));
                persona_loaded.set(true);
                persona_message.set(String::new());
            }
        });
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
            // The target may be far up in already-loaded history, where
            // highlighting alone changes nothing on screen. Bounded retry
            // because the row may still be rendering (or fetching, above).
            dioxus::document::eval(&format!(
                "(function() {{ let n = 0; const t = setInterval(() => {{ \
                    const el = document.getElementById('msg-{target}'); \
                    if (el) {{ el.scrollIntoView({{block: 'center'}}); clearInterval(t); }} \
                    else if (++n > 20) clearInterval(t); \
                 }}, 100); }})()"
            ));
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
        muted.set(api::mutes(&session()).await.into_iter().collect());
        categories.set(api::categories(&session()).await);
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
                // Your own role can change without you being told — an admin
                // promotes you, or the server's owner is fixed by hand. The
                // roster is authoritative, so adopt it rather than staying
                // locked out of controls the server would happily allow.
                if let Some(me) = users.iter().find(|u| u.user.id == session().user.id) {
                    if me.user.role != session().user.role {
                        let mut s = session();
                        s.user.role = me.user.role.clone();
                        api::update_saved_server(&s);
                        session.set(s);
                    }
                }
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
                                // A muted channel makes no sound and raises no
                                // toast, even for an @you — that is what muting
                                // one is for. It still counts as unread.
                                let mentioned = message.author.id != me.id
                                    && !muted().contains(&message.channel_id)
                                    && (is_dm
                                        || content_lower.contains("@everyone")
                                        || content_lower.contains(&format!("@{}", me.username.to_lowercase())));
                                if mentioned && !window.window.is_focused() {
                                    window.window.request_user_attention(Some(UserAttentionType::Informational));
                                    if audio_settings().notification_sounds {
                                        play_notification_sound();
                                    }
                                }
                                // A ping you're not looking at gets a toast: the
                                // sidebar badge is easy to miss, and on the Music
                                // or Video tab there's no chat in view at all.
                                let looking_here = selected().map(|c| c.id) == Some(message.channel_id)
                                    && view_tab() == "chat"
                                    && window.window.is_focused();
                                if mentioned && !looking_here && audio_settings().ping_toasts {
                                    let where_ = if is_dm {
                                        format!("@{}", message.author.username)
                                    } else {
                                        channels()
                                            .iter()
                                            .find(|c| c.id == message.channel_id)
                                            .map(|c| format!("# {}", c.name))
                                            .unwrap_or_default()
                                    };
                                    let id = toast_seq() + 1;
                                    toast_seq.set(id);
                                    let preview: String = message.content.chars().take(120).collect();
                                    toasts.write().push((
                                        id,
                                        message.channel_id,
                                        message.id,
                                        message.author.clone(),
                                        where_,
                                        preview,
                                    ));
                                    // Self-expiry, so no timer has to sweep the list.
                                    spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                                        toasts.write().retain(|t| t.0 != id);
                                    });
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
                            ServerEvent::StatusChanged { user_id, status: new_status } => {
                                if let Some(m) = members.write().iter_mut().find(|m| m.user.id == user_id) {
                                    m.status = new_status.clone();
                                }
                                // Keep an open profile card in step.
                                if profile_card.peek().as_ref().map(|p| p.user.id) == Some(user_id) {
                                    if let Some(p) = profile_card.write().as_mut() {
                                        p.status = new_status;
                                    }
                                }
                            }
                            ServerEvent::MessagePinChanged { channel_id, message_id, pinned } => {
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    let mut list = messages.write();
                                    if let Some(m) = list.iter_mut().find(|m| m.id == message_id) {
                                        m.pinned = pinned;
                                    }
                                }
                                // An open pin list only ever needs unpins live;
                                // it refetches whenever it's opened.
                                if !pinned {
                                    if let Some(pins) = pins_open.write().as_mut() {
                                        pins.retain(|m| m.id != message_id);
                                    }
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
                                        list.push(UserStatus { user, online, banned: false, tag_ids: Vec::new(), status: None });
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
                            ServerEvent::CategoriesChanged => {
                                // Coarse by design: an admin was editing
                                // settings, so refetch both lists rather than
                                // trying to patch them.
                                spawn(async move {
                                    categories.set(api::categories(&session()).await);
                                    if let Ok(chs) = api::channels(&session()).await {
                                        channels.set(chs);
                                    }
                                });
                            }
                            ServerEvent::ChannelRenamed { channel_id, name } => {
                                if let Some(channel) =
                                    channels.write().iter_mut().find(|c| c.id == channel_id)
                                {
                                    channel.name = name.clone();
                                }
                                // The header reads from `selected`, so it needs
                                // the new name too.
                                if selected().map(|c| c.id) == Some(channel_id) {
                                    if let Some(mut current) = selected() {
                                        current.name = name;
                                        selected.set(Some(current));
                                    }
                                }
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
        // On the Music tab a bare link means "queue this" — and it's queued
        // straight through the API rather than posted, so the channel doesn't
        // collect a command and a link preview card for every track.
        if view_tab() == "music"
            && !content.starts_with('/')
            && content.split_whitespace().count() == 1
            && (content.starts_with("http://") || content.starts_with("https://"))
        {
            let (channel_id, url) = (channel.id, content.clone());
            draft.set(String::new());
            spawn(async move {
                if let Err(e) = api::music_play(&session(), channel_id, url).await {
                    status.set(e);
                }
            });
            return;
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

    let logout = move |_| {
        // Signing out is not the same as leaving. The server stays in the
        // rail, signed out, because "I'm done for now" and "forget this place
        // exists" are different things — and until now they were the same
        // button, which is how switchb's second server disappeared.
        let active = api::load_servers().active;
        servers_file.set(api::sign_out(active));
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
            if let Some(pins) = pins_open() {
                div {
                    class: "settings-overlay",
                    onclick: move |_| pins_open.set(None),
                    div {
                        class: "settings-modal whatsnew-modal",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "whatsnew-head",
                            div { class: "whatsnew-title", "Pinned messages" }
                            div { class: "whatsnew-sub",
                                if pins.is_empty() {
                                    "nothing pinned in this channel yet"
                                } else {
                                    "{pins.len()} pinned — click to jump"
                                }
                            }
                        }
                        div { class: "settings-body whatsnew-body",
                            for pin in pins {
                                div {
                                    key: "{pin.id}",
                                    class: "search-hit",
                                    onclick: {
                                        let msg_id = pin.id;
                                        move |_| {
                                            pins_open.set(None);
                                            jump_to.set(Some(msg_id));
                                        }
                                    },
                                    div { class: "search-hit-head",
                                        span { class: "search-hit-author", "{pin.author.username}" }
                                        span { class: "release-date", {format_time(pin.created_at)} }
                                    }
                                    div { class: "search-hit-content",
                                        {pin.content.chars().take(220).collect::<String>()}
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
                                    email_input.set(String::new());
                                    email_code.set(String::new());
                                    email_sent.set(false);
                                    email_message.set((String::new(), false));
                                    settings_tab.set("account");
                                    spawn(async move {
                                        if let Ok(status) = api::email_status(&session()).await {
                                            email_state.set(Some(status));
                                        }
                                    });
                                },
                                "Account"
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
                                label { "Email" }
                                match email_state() {
                                    Some(shared::EmailStatus { email: Some(email), verified: true }) => rsx! {
                                        div { class: "settings-value", "{email} — verified ✓" }
                                    },
                                    Some(shared::EmailStatus { email: Some(email), verified: false }) => rsx! {
                                        div { class: "settings-value", "{email} — not verified" }
                                    },
                                    Some(_) => rsx! {
                                        div { class: "settings-hint", "no email yet — add one to enable password reset" }
                                    },
                                    None => rsx! {
                                        div { class: "settings-hint", "…" }
                                    },
                                }
                                div { class: "settings-row",
                                    input {
                                        placeholder: "you@example.com",
                                        spellcheck: "false",
                                        value: "{email_input}",
                                        oncontextmenu: move |e: Event<MouseData>| {
                                            menu::open(ctx_menu, &e, menu::text_field_items())
                                        },
                                        oninput: move |e| email_input.set(e.value()),
                                    }
                                    button {
                                        class: "profile-btn",
                                        disabled: email_input().trim().is_empty(),
                                        onclick: move |_| {
                                            spawn(async move {
                                                match api::email_request(&session(), email_input().trim().to_string()).await {
                                                    Ok(()) => {
                                                        email_sent.set(true);
                                                        email_message.set(("code sent — check your inbox (and spam)".into(), true));
                                                    }
                                                    Err(e) => email_message.set((e, false)),
                                                }
                                            });
                                        },
                                        "Send code"
                                    }
                                }
                                if email_sent() {
                                    div { class: "settings-row",
                                        input {
                                            placeholder: "6-digit code",
                                            spellcheck: "false",
                                            value: "{email_code}",
                                            oninput: move |e| email_code.set(e.value()),
                                        }
                                        button {
                                            class: "profile-btn primary",
                                            disabled: email_code().trim().is_empty(),
                                            onclick: move |_| {
                                                spawn(async move {
                                                    match api::email_verify(&session(), email_code().trim().to_string()).await {
                                                        Ok(()) => {
                                                            email_sent.set(false);
                                                            email_code.set(String::new());
                                                            email_input.set(String::new());
                                                            email_message.set(("email verified".into(), true));
                                                            if let Ok(status) = api::email_status(&session()).await {
                                                                email_state.set(Some(status));
                                                            }
                                                        }
                                                        Err(e) => email_message.set((e, false)),
                                                    }
                                                });
                                            },
                                            "Verify"
                                        }
                                    }
                                }
                                if !email_message().0.is_empty() {
                                    div {
                                        class: if email_message().1 { "settings-hint pw-good" } else { "settings-hint pw-bad" },
                                        "{email_message().0}"
                                    }
                                }
                                div { class: "settings-hint",
                                    "a verified email lets you reset a forgotten password from the login screen"
                                }
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
                                        // Slider, marker, and meter all share the
                                        // dB mapping, so the knob sits exactly on
                                        // the marker line.
                                        value: "{mic_pos(audio_settings().vad_threshold) as i32}",
                                        oninput: move |e| {
                                            if let Ok(v) = e.value().parse::<f32>() {
                                                let threshold = pos_to_rms(v);
                                                audio_settings.write().vad_threshold = threshold;
                                                voice.send(voice::VoiceCmd::SetVadThreshold(threshold));
                                            }
                                        },
                                    }
                                    div { class: "meter-wrap",
                                        MicMeter { level: mic_level }
                                        if audio_settings().vad_threshold > 0.0 {
                                            div {
                                                class: "meter-threshold",
                                                style: "left: {mic_pos(audio_settings().vad_threshold)}%",
                                            }
                                        }
                                    }
                                    div { class: "settings-hint",
                                        if voice_status().channel_id.is_none() {
                                            "join a voice channel to see your level here"
                                        } else if audio_settings().vad_threshold <= 0.0 {
                                            "always transmitting (open mic)"
                                        } else {
                                            "transmits only while the bar crosses the marker"
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
                                // In PTT mode the sensitivity block is gone, so
                                // the level meter lives here instead.
                                if audio_settings().voice_mode == "ptt" {
                                    div { class: "meter-wrap",
                                        MicMeter { level: mic_level }
                                    }
                                    if voice_status().channel_id.is_none() {
                                        div { class: "settings-hint", "join a voice channel to test your mic" }
                                    }
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
                                label { class: "ns-toggle-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: audio_settings().auto_gain,
                                        onchange: move |e| {
                                            let enabled = e.checked();
                                            audio_settings.write().auto_gain = enabled;
                                            voice.send(voice::VoiceCmd::SetAutoGain(enabled));
                                        },
                                    }
                                    " Auto mic level (boosts quiet microphones)"
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
                                label { class: "ns-toggle-row",
                                    input {
                                        r#type: "checkbox",
                                        checked: audio_settings().ping_toasts,
                                        onchange: move |e| {
                                            let mut s = audio_settings.write();
                                            s.ping_toasts = e.checked();
                                            api::save_settings(&s);
                                        },
                                    }
                                    " Popup for mentions and DMs"
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
            if server_settings_open() {
                {
                    let is_admin = session().user.role == "admin";
                    rsx! {
                        div {
                            class: "srv-overlay",
                            onclick: move |_| server_settings_open.set(false),
                            div {
                                class: "srv-shell",
                                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                                div { class: "srv-nav",
                                    div { class: "srv-nav-head", "{session().server_name}" }
                                    button {
                                        class: if srv_pane() == "channel" { "srv-nav-item active" } else { "srv-nav-item" },
                                        onclick: move |_| srv_pane.set("channel"),
                                        Icon { name: "tag", size: 15 }
                                        "Channels"
                                    }
                                    button {
                                        class: if srv_pane() == "overview" { "srv-nav-item active" } else { "srv-nav-item" },
                                        onclick: move |_| srv_pane.set("overview"),
                                        Icon { name: "settings", size: 15 }
                                        "Overview"
                                    }
                                    button {
                                        class: if srv_pane() == "roles" { "srv-nav-item active" } else { "srv-nav-item" },
                                        onclick: move |_| srv_pane.set("roles"),
                                        Icon { name: "user", size: 15 }
                                        "Roles & members"
                                    }
                                    button {
                                        class: if srv_pane() == "permissions" { "srv-nav-item active" } else { "srv-nav-item" },
                                        onclick: move |_| srv_pane.set("permissions"),
                                        Icon { name: "shield", size: 15 }
                                        "Permissions"
                                    }
                                    if is_admin {
                                        button {
                                            class: if srv_pane() == "stats" { "srv-nav-item active" } else { "srv-nav-item" },
                                            onclick: move |_| {
                                                srv_pane.set("stats");
                                                spawn(async move {
                                                    match api::server_stats(&session()).await {
                                                        Ok(s) => stats.set(Some(s)),
                                                        Err(e) => status.set(e),
                                                    }
                                                });
                                            },
                                            Icon { name: "eye", size: 15 }
                                            "Stats"
                                        }
                                        button {
                                            class: if srv_pane() == "bot" { "srv-nav-item active" } else { "srv-nav-item" },
                                            onclick: move |_| srv_pane.set("bot"),
                                            Icon { name: "message", size: 15 }
                                            "Bot"
                                        }
                                        button {
                                            class: if srv_pane() == "storage" { "srv-nav-item active" } else { "srv-nav-item" },
                                            onclick: move |_| srv_pane.set("storage"),
                                            Icon { name: "file", size: 15 }
                                            "Storage"
                                        }
                                    }
                                }
                                div { class: "srv-pane",
                                    div { class: "srv-pane-head",
                                        div { class: "srv-pane-title",
                                            {match srv_pane() {
                                                "channel" => "Channels",
                                                "overview" => "Overview",
                                                "roles" => "Roles & members",
                                                "permissions" => "Permissions",
                                                "bot" => "Bot",
                                                "stats" => "Stats",
                                                _ => "Storage",
                                            }}
                                        }
                                        button {
                                            class: "srv-close",
                                            title: "Close",
                                            onclick: move |_| server_settings_open.set(false),
                                            Icon { name: "x", size: 15 }
                                        }
                                    }
                                    div { class: "srv-pane-body",

                                        // ---- Channels ----
                                        if srv_pane() == "channel" {
                                            if !is_admin {
                                                div { class: "srv-note", "Only admins can add, rename or delete channels." }
                                            }
                                            for channel in channels().into_iter().filter(|c| c.kind != "dm") {
                                                {
                                                    let id = channel.id;
                                                    let name = channel.name.clone();
                                                    let chan_icon: &'static str =
                                                        if channel.kind == "voice" { "volume" } else { "tag" };
                                                    let current = selected().map(|c| c.id) == Some(id);
                                                    let editing_this = renaming_channel().is_some_and(|(cid, _)| cid == id);
                                                    let draft = renaming_channel()
                                                        .filter(|(cid, _)| *cid == id)
                                                        .map(|(_, d)| d)
                                                        .unwrap_or_default();
                                                    rsx! {
                                                        div {
                                                            key: "{id}",
                                                            class: if current { "srv-row current" } else { "srv-row" },
                                                            Icon { name: chan_icon, size: 15 }
                                                            if editing_this {
                                                                input {
                                                                    class: "srv-input",
                                                                    value: "{draft}",
                                                                    spellcheck: "false",
                                                                    oninput: move |e| renaming_channel.set(Some((id, e.value()))),
                                                                    onkeydown: move |e| {
                                                                        if e.key() == Key::Enter {
                                                                            save_channel_rename();
                                                                        } else if e.key() == Key::Escape {
                                                                            renaming_channel.set(None);
                                                                        }
                                                                    },
                                                                }
                                                                button {
                                                                    class: "srv-btn primary",
                                                                    onclick: move |_| save_channel_rename(),
                                                                    "Save"
                                                                }
                                                                button {
                                                                    class: "srv-btn",
                                                                    onclick: move |_| renaming_channel.set(None),
                                                                    "Cancel"
                                                                }
                                                            } else {
                                                                span { class: "srv-row-name", "{name}" }
                                                                if current {
                                                                    span { class: "srv-chip", "you're here" }
                                                                }
                                                                span { class: "grow" }
                                                                if is_admin && !categories().is_empty() && channel.kind == "text" {
                                                                    select {
                                                                        class: "srv-input srv-cat-pick",
                                                                        title: "Which group this sits in",
                                                                        onchange: move |e| {
                                                                            let pick = e.value().parse::<i64>().ok().filter(|v| *v > 0);
                                                                            spawn(async move {
                                                                                if let Err(err) =
                                                                                    api::set_channel_category(&session(), id, pick).await
                                                                                {
                                                                                    status.set(err);
                                                                                }
                                                                            });
                                                                        },
                                                                        option {
                                                                            value: "0",
                                                                            selected: channel.category_id.is_none(),
                                                                            "No category"
                                                                        }
                                                                        for category in categories() {
                                                                            option {
                                                                                key: "{category.id}",
                                                                                value: "{category.id}",
                                                                                selected: channel.category_id == Some(category.id),
                                                                                "{category.name}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                if is_admin {
                                                                    button {
                                                                        class: "srv-btn",
                                                                        onclick: {
                                                                            let name = name.clone();
                                                                            move |_| renaming_channel.set(Some((id, name.clone())))
                                                                        },
                                                                        Icon { name: "edit", size: 13 }
                                                                        "Rename"
                                                                    }
                                                                    button {
                                                                        class: "srv-btn danger",
                                                                        onclick: {
                                                                            let name = name.clone();
                                                                            move |_| confirm.set(Some(ConfirmAction::DeleteChannel {
                                                                                id,
                                                                                name: name.clone(),
                                                                            }))
                                                                        },
                                                                        Icon { name: "trash", size: 13 }
                                                                        "Delete"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            if is_admin {
                                                div { class: "srv-create",
                                                    input {
                                                        class: "srv-input",
                                                        placeholder: "new channel name",
                                                        value: "{new_channel}",
                                                        spellcheck: "false",
                                                        oninput: move |e| new_channel.set(e.value()),
                                                    }
                                                    button {
                                                        class: if new_channel_voice() { "srv-btn" } else { "srv-btn primary" },
                                                        onclick: move |_| new_channel_voice.set(false),
                                                        "Text"
                                                    }
                                                    button {
                                                        class: if new_channel_voice() { "srv-btn primary" } else { "srv-btn" },
                                                        onclick: move |_| new_channel_voice.set(true),
                                                        "Voice"
                                                    }
                                                    button {
                                                        class: "srv-btn",
                                                        disabled: new_channel().trim().is_empty(),
                                                        onclick: move |_| {
                                                            let name = new_channel().trim().to_string();
                                                            if name.is_empty() {
                                                                return;
                                                            }
                                                            let kind = if new_channel_voice() { "voice" } else { "text" };
                                                            new_channel.set(String::new());
                                                            spawn(async move {
                                                                if let Err(e) = api::create_channel(&session(), name, kind).await {
                                                                    status.set(e);
                                                                }
                                                            });
                                                        },
                                                        Icon { name: "plus", size: 13 }
                                                        "Create"
                                                    }
                                                }
                                                div { class: "srv-label srv-cat-head", "Categories" }
                                                div { class: "srv-hint",
                                                    "Groups for the channel list. Each one folds shut on its own, and deleting a group leaves its channels where they were before you made it."
                                                }
                                                for category in categories() {
                                                    {
                                                        let id = category.id;
                                                        let name = category.name.clone();
                                                        let count = channels()
                                                            .iter()
                                                            .filter(|c| c.category_id == Some(id))
                                                            .count();
                                                        rsx! {
                                                            div { key: "cat{id}", class: "srv-row",
                                                                Icon { name: "list-x", size: 15 }
                                                                input {
                                                                    class: "srv-input",
                                                                    value: "{name}",
                                                                    spellcheck: "false",
                                                                    // Saved when you leave the box, so
                                                                    // renaming isn't a round trip per key.
                                                                    onchange: move |e| {
                                                                        let name = e.value();
                                                                        spawn(async move {
                                                                            if let Err(err) =
                                                                                api::rename_category(&session(), id, name).await
                                                                            {
                                                                                status.set(err);
                                                                            }
                                                                        });
                                                                    },
                                                                }
                                                                span { class: "srv-chip",
                                                                    if count == 1 { "1 channel" } else { "{count} channels" }
                                                                }
                                                                button {
                                                                    class: "srv-btn danger",
                                                                    onclick: move |_| {
                                                                        spawn(async move {
                                                                            if let Err(err) =
                                                                                api::delete_category(&session(), id).await
                                                                            {
                                                                                status.set(err);
                                                                            }
                                                                        });
                                                                    },
                                                                    Icon { name: "trash", size: 13 }
                                                                    "Delete"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                div { class: "srv-create",
                                                    input {
                                                        class: "srv-input",
                                                        placeholder: "new category name",
                                                        value: "{new_category}",
                                                        spellcheck: "false",
                                                        oninput: move |e| new_category.set(e.value()),
                                                        onkeydown: move |e| {
                                                            if e.key() == Key::Enter {
                                                                let name = new_category().trim().to_string();
                                                                if !name.is_empty() {
                                                                    new_category.set(String::new());
                                                                    spawn(async move {
                                                                        if let Err(err) =
                                                                            api::create_category(&session(), name).await
                                                                        {
                                                                            status.set(err);
                                                                        }
                                                                    });
                                                                }
                                                            }
                                                        },
                                                    }
                                                    button {
                                                        class: "srv-btn",
                                                        disabled: new_category().trim().is_empty(),
                                                        onclick: move |_| {
                                                            let name = new_category().trim().to_string();
                                                            if name.is_empty() {
                                                                return;
                                                            }
                                                            new_category.set(String::new());
                                                            spawn(async move {
                                                                if let Err(err) =
                                                                    api::create_category(&session(), name).await
                                                                {
                                                                    status.set(err);
                                                                }
                                                            });
                                                        },
                                                        Icon { name: "plus", size: 13 }
                                                        "Add category"
                                                    }
                                                }
                                            }
                                        }

                                        // ---- Stats ----
                                        else if srv_pane() == "stats" {
                                            if let Some(s) = stats() {
                                                {
                                                    let pct = if s.uploads_cap_bytes > 0 {
                                                        (s.uploads_bytes as f64 / s.uploads_cap_bytes as f64 * 100.0).min(100.0)
                                                    } else {
                                                        0.0
                                                    };
                                                    rsx! {
                                                        div { class: "stat-grid",
                                                            for (label, value) in [
                                                                ("People".to_string(), format!("{}", s.people)),
                                                                ("Online now".to_string(), format!("{}", s.online)),
                                                                ("Admins".to_string(), format!("{}", s.admins)),
                                                                ("Messages".to_string(), thousands(s.messages)),
                                                                ("Text channels".to_string(), format!("{}", s.text_channels)),
                                                                ("Voice channels".to_string(), format!("{}", s.voice_channels)),
                                                                ("Conversations".to_string(), format!("{}", s.dms)),
                                                                ("Files kept".to_string(), thousands(s.upload_count)),
                                                                ("Database".to_string(), human_bytes(s.database_bytes)),
                                                                ("Uptime".to_string(), human_duration(s.uptime_secs)),
                                                            ] {
                                                                div { key: "{label}", class: "stat-cell",
                                                                    div { class: "stat-value", "{value}" }
                                                                    div { class: "stat-label", "{label}" }
                                                                }
                                                            }
                                                        }
                                                        div { class: "srv-field",
                                                            div { class: "srv-label", "Uploads" }
                                                            div { class: "stat-bar",
                                                                div { class: "stat-bar-fill", style: "width: {pct:.1}%" }
                                                            }
                                                            div { class: "srv-hint",
                                                                "{human_bytes(s.uploads_bytes)} of {human_bytes(s.uploads_cap_bytes)} — older files are swept on the schedule in Storage."
                                                            }
                                                        }
                                                        if let Some(founded) = s.founded_at {
                                                            div { class: "srv-hint", "Running since {day_label(founded)}." }
                                                        }
                                                    }
                                                }
                                            } else {
                                                div { class: "srv-note", "Counting…" }
                                            }
                                        }

                                        // ---- Overview ----
                                        else if srv_pane() == "overview" {
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Server name" }
                                                div { class: "srv-inline",
                                                    input {
                                                        class: "srv-input",
                                                        value: "{server_name_draft}",
                                                        disabled: !is_admin,
                                                        oninput: move |e| server_name_draft.set(e.value()),
                                                    }
                                                    if is_admin {
                                                        button {
                                                            class: "srv-btn primary",
                                                            onclick: move |_| {
                                                                let name = server_name_draft().trim().to_string();
                                                                if name.is_empty() {
                                                                    return;
                                                                }
                                                                spawn(async move {
                                                                    if let Err(e) = api::rename_server(&session(), name).await {
                                                                        status.set(e);
                                                                    }
                                                                });
                                                            },
                                                            "Save"
                                                        }
                                                    }
                                                }
                                            }
                                            if is_admin {
                                                div { class: "srv-field",
                                                    div { class: "srv-label", "Server icon" }
                                                    div { class: "srv-inline",
                                                        button {
                                                            class: "srv-btn",
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
                                                            Icon { name: "plus", size: 13 }
                                                            "Choose an image"
                                                        }
                                                        div { class: "srv-hint", "shown on the server rail — 8 MB max" }
                                                    }
                                                }
                                                div { class: "srv-field",
                                                    div { class: "srv-label", "Invite code" }
                                                    div { class: "srv-inline",
                                                        input {
                                                            class: "srv-input",
                                                            value: "{invite_draft}",
                                                            spellcheck: "false",
                                                            oninput: move |e| invite_draft.set(e.value()),
                                                        }
                                                        button {
                                                            class: "srv-btn",
                                                            title: "Copy",
                                                            onclick: move |_| {
                                                                menu::copy_to_clipboard(invite_draft().trim().to_owned());
                                                                invite_message.set("copied".into());
                                                            },
                                                            Icon { name: "copy", size: 13 }
                                                        }
                                                        button {
                                                            class: "srv-btn",
                                                            onclick: move |_| invite_draft.set(random_invite_code()),
                                                            "Shuffle"
                                                        }
                                                        button {
                                                            class: "srv-btn primary",
                                                            disabled: !invite_loaded(),
                                                            onclick: move |_| {
                                                                let code = invite_draft().trim().to_string();
                                                                spawn(async move {
                                                                    match api::set_invite(&session(), code).await {
                                                                        Ok(setting) => {
                                                                            invite_draft.set(setting.code);
                                                                            invite_message.set("saved".into());
                                                                        }
                                                                        Err(e) => invite_message.set(e),
                                                                    }
                                                                });
                                                            },
                                                            "Save"
                                                        }
                                                    }
                                                    if !invite_message().is_empty() {
                                                        div { class: "srv-hint", "{invite_message}" }
                                                    }
                                                }
                                            }
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Server ID" }
                                                div { class: "srv-mono", "{session().server_id}" }
                                            }
                                        }

                                        // ---- Roles & members ----
                                        else if srv_pane() == "roles" {
                                            if !is_admin {
                                                div { class: "srv-note", "Only admins can change roles or ban people." }
                                            }
                                            for member in members() {
                                                {
                                                    let user = member.user.clone();
                                                    let me = user.id == session().user.id;
                                                    let admin_member = user.role == "admin";
                                                    let banned = member.banned;
                                                    rsx! {
                                                        div { key: "{user.id}", class: "srv-row",
                                                            UserAvatar { user: user.clone(), class: "member-avatar" }
                                                            span { class: "srv-row-name", "{user.username}" }
                                                            if banned {
                                                                span { class: "role-badge banned-badge", "BANNED" }
                                                            } else if admin_member {
                                                                span { class: "role-badge", "ADMIN" }
                                                            }
                                                            span { class: "grow" }
                                                            if is_admin && !me {
                                                                button {
                                                                    class: "srv-btn",
                                                                    onclick: {
                                                                        let username = user.username.clone();
                                                                        let target = user.id;
                                                                        move |_| confirm.set(Some(ConfirmAction::SetRole {
                                                                            user_id: target,
                                                                            username: username.clone(),
                                                                            make_admin: !admin_member,
                                                                        }))
                                                                    },
                                                                    if admin_member { "Remove admin" } else { "Make admin" }
                                                                }
                                                                button {
                                                                    class: "srv-btn danger",
                                                                    onclick: {
                                                                        let username = user.username.clone();
                                                                        let target = user.id;
                                                                        move |_| confirm.set(Some(ConfirmAction::SetBan {
                                                                            user_id: target,
                                                                            username: username.clone(),
                                                                            banned: !banned,
                                                                        }))
                                                                    },
                                                                    if banned { "Unban" } else { "Ban" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            if is_admin {
                                                div { class: "srv-label", "Tags" }
                                                div { class: "srv-hint",
                                                    "Tags colour a name in chat. Assign them from someone's profile."
                                                }
                                                for tag in tags() {
                                                    div { key: "t{tag.id}", class: "srv-row",
                                                        span { class: "tag-pill", style: "background: {tag.color}", "{tag.name}" }
                                                        span { class: "grow" }
                                                        button {
                                                            class: "srv-btn danger",
                                                            onclick: {
                                                                let (id, name) = (tag.id, tag.name.clone());
                                                                move |_| confirm.set(Some(ConfirmAction::DeleteTag {
                                                                    id,
                                                                    name: name.clone(),
                                                                }))
                                                            },
                                                            Icon { name: "trash", size: 13 }
                                                        }
                                                    }
                                                }
                                                div { class: "srv-create",
                                                    input {
                                                        class: "srv-input",
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
                                                        class: "srv-btn",
                                                        disabled: new_tag_name().trim().is_empty(),
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

                                        // ---- Permissions ----
                                        else if srv_pane() == "permissions" {
                                            div { class: "srv-note",
                                                "There are two roles, and they're the same in every channel. "
                                                "Per-channel permissions aren't built yet — ask if you want them."
                                            }
                                            div { class: "srv-perm",
                                                div { class: "srv-perm-head",
                                                    span { class: "role-badge", "ADMIN" }
                                                    "can do everything a member can, plus:"
                                                }
                                                for line in [
                                                    "Create, rename and delete channels",
                                                    "Delete anyone's message",
                                                    "Ban and unban people",
                                                    "Promote and demote admins",
                                                    "Create and delete tags, emojis and stickers",
                                                    "Change the server name, icon and invite code",
                                                    "Set how long uploads are kept and the storage cap",
                                                    "Set the bot's name, avatar and personality",
                                                ] {
                                                    div { key: "{line}", class: "srv-perm-line",
                                                        Icon { name: "check", size: 13 }
                                                        "{line}"
                                                    }
                                                }
                                            }
                                            div { class: "srv-perm",
                                                div { class: "srv-perm-head",
                                                    span { class: "role-badge member-badge", "MEMBER" }
                                                    "can:"
                                                }
                                                for line in [
                                                    "Read and post in every channel",
                                                    "Edit and delete their own messages",
                                                    "React, reply and upload files",
                                                    "Join voice, share a camera or screen",
                                                    "Queue and control the music",
                                                    "Start a DM or a call with anyone",
                                                ] {
                                                    div { key: "{line}", class: "srv-perm-line",
                                                        Icon { name: "check", size: 13 }
                                                        "{line}"
                                                    }
                                                }
                                            }
                                        }

                                        // ---- Bot ----
                                        else if srv_pane() == "bot" {
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Bot name" }
                                                input {
                                                    class: "srv-input",
                                                    value: "{bot_name_draft}",
                                                    oninput: move |e| bot_name_draft.set(e.value()),
                                                }
                                            }
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Personality" }
                                                textarea {
                                                    class: "srv-textarea",
                                                    rows: "8",
                                                    value: "{persona_draft}",
                                                    spellcheck: "true",
                                                    oncontextmenu: move |e: Event<MouseData>| {
                                                        menu::open(ctx_menu, &e, menu::text_field_items())
                                                    },
                                                    oninput: move |e| persona_draft.set(e.value()),
                                                }
                                                div { class: "srv-hint",
                                                    "The system prompt the bot answers with. It keeps its memory across changes."
                                                }
                                            }
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Model" }
                                                input {
                                                    class: "srv-input",
                                                    value: "{bot_model_draft}",
                                                    placeholder: "{shared::DEFAULT_BOT_MODEL}",
                                                    oninput: move |e| bot_model_draft.set(e.value()),
                                                }
                                                div { class: "srv-hint",
                                                    "Any OpenRouter model id. Leave empty for the default."
                                                }
                                            }
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Keys" }
                                                div { class: "srv-hint",
                                                    "Stored on this server, never shown again once saved. Leave a box empty to keep what's already there."
                                                }
                                                for cred in cred_status() {
                                                    div { key: "{cred.key}", class: "srv-cred",
                                                        div { class: "srv-cred-head",
                                                            span { class: "srv-cred-name", "{cred.label}" }
                                                            if cred.from_env {
                                                                span { class: "srv-cred-tag env", "from environment" }
                                                            } else if cred.set {
                                                                span { class: "srv-cred-tag on", "set" }
                                                            } else {
                                                                span { class: "srv-cred-tag off", "not set" }
                                                            }
                                                        }
                                                        {
                                                            let key = cred.key.clone();
                                                            let clear = cred.key.clone();
                                                            let typed = cred_drafts().get(&cred.key).cloned().unwrap_or_default();
                                                            let placeholder: &str = if cred.from_env {
                                                                "•••••••• (set when the server started)"
                                                            } else if cred.set {
                                                                "•••••••• (saved)"
                                                            } else {
                                                                "paste here"
                                                            };
                                                            rsx! {
                                                                textarea {
                                                                    class: "srv-input srv-cred-input",
                                                                    rows: "2",
                                                                    value: "{typed}",
                                                                    placeholder: "{placeholder}",
                                                                    oncontextmenu: move |e: Event<MouseData>| {
                                                                        menu::open(ctx_menu, &e, menu::text_field_items())
                                                                    },
                                                                    oninput: move |e| {
                                                                        cred_drafts.write().insert(key.clone(), e.value());
                                                                    },
                                                                }
                                                                if cred.set && !cred.from_env {
                                                                    button {
                                                                        class: "srv-cred-clear",
                                                                        onclick: move |_| {
                                                                            // A single space is how we say "clear this"
                                                                            // without it looking like an untouched box.
                                                                            cred_drafts.write().insert(clear.clone(), " ".into());
                                                                        },
                                                                        "Remove"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        div { class: "srv-hint", "{cred.hint}" }
                                                    }
                                                }
                                            }
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Release announcements" }
                                                select {
                                                    class: "srv-input",
                                                    onchange: move |e| {
                                                        if let Ok(v) = e.value().parse::<i64>() {
                                                            announce_draft.set(v);
                                                        }
                                                    },
                                                    option {
                                                        value: "-1",
                                                        selected: announce_draft() < 0,
                                                        "First text channel (default)"
                                                    }
                                                    option {
                                                        value: "0",
                                                        selected: announce_draft() == 0,
                                                        "Don't announce"
                                                    }
                                                    for channel in channels().into_iter().filter(|c| c.kind == "text") {
                                                        option {
                                                            key: "{channel.id}",
                                                            value: "{channel.id}",
                                                            selected: announce_draft() == channel.id,
                                                            "# {channel.name}"
                                                        }
                                                    }
                                                }
                                                div { class: "srv-hint",
                                                    "Where the bot posts \"vX just shipped\" notes when an update goes out."
                                                }
                                            }
                                            div { class: "srv-inline",
                                                button {
                                                    class: "srv-btn primary",
                                                    disabled: !persona_loaded(),
                                                    onclick: move |_| {
                                                        let update = shared::BotSettingsUpdate {
                                                            persona: Some(persona_draft()),
                                                            name: Some(bot_name_draft().trim().to_string()),
                                                            avatar: None,
                                                            // -1 is the client's "leave it default";
                                                            // the server only stores real choices.
                                                            announce_channel: Some(announce_draft()).filter(|v| *v >= 0),
                                                            model: Some(bot_model_draft().trim().to_string()),
                                                            // Untouched boxes aren't sent, so saving the
                                                            // persona can't wipe a key by omission.
                                                            credentials: cred_drafts()
                                                                .into_iter()
                                                                .filter(|(_, v)| !v.is_empty())
                                                                .map(|(k, v)| (k, v.trim().to_string()))
                                                                .collect(),
                                                        };
                                                        spawn(async move {
                                                            match api::set_bot_settings(&session(), update).await {
                                                                Ok(settings) => {
                                                                    persona_draft.set(settings.persona);
                                                                    bot_name_draft.set(settings.name);
                                                                    bot_model_draft.set(settings.model);
                                                                    cred_status.set(settings.credentials);
                                                                    cred_drafts.write().clear();
                                                                    announce_draft.set(settings.announce_channel.unwrap_or(-1));
                                                                    persona_message.set("saved".into());
                                                                }
                                                                Err(e) => persona_message.set(e),
                                                            }
                                                        });
                                                    },
                                                    "Save"
                                                }
                                                button {
                                                    class: "srv-btn",
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
                                                            match api::upload(&session(), &file.file_name(), bytes).await {
                                                                Ok(url) => {
                                                                    let update = shared::BotSettingsUpdate {
                                                                        persona: None,
                                                                        name: None,
                                                                        avatar: Some(url),
                                                                        announce_channel: None,
                                                                        ..Default::default()
                                                                    };
                                                                    if let Err(e) = api::set_bot_settings(&session(), update).await {
                                                                        persona_message.set(e);
                                                                    } else {
                                                                        persona_message.set("avatar updated".into());
                                                                    }
                                                                }
                                                                Err(e) => persona_message.set(e),
                                                            }
                                                        });
                                                    },
                                                    "Set avatar"
                                                }
                                                if !persona_message().is_empty() {
                                                    span { class: "srv-hint", "{persona_message}" }
                                                }
                                            }
                                        }

                                        // ---- Storage ----
                                        else {
                                            div { class: "srv-field",
                                                div { class: "srv-label", "Keep uploads for" }
                                                select {
                                                    class: "srv-select",
                                                    onchange: move |e| {
                                                        let Ok(days) = e.value().parse::<i64>() else { return };
                                                        spawn(async move {
                                                            match api::set_retention(&session(), days).await {
                                                                Ok(setting) => retention_days.set(setting.days),
                                                                Err(e) => status.set(e),
                                                            }
                                                        });
                                                    },
                                                    option { value: "21", selected: retention_days() == 21, "3 weeks (default)" }
                                                    option { value: "30", selected: retention_days() == 30, "1 month" }
                                                    option { value: "60", selected: retention_days() == 60, "2 months" }
                                                    option { value: "90", selected: retention_days() == 90, "3 months" }
                                                }
                                                div { class: "srv-hint",
                                                    "Expired files disappear from chat. Avatars and stickers never expire."
                                                }
                                            }
                                            if let Some(info) = storage_info() {
                                                {
                                                    let cap_bytes = (info.cap_gb.max(1) as f64) * 1e9;
                                                    let pct = ((info.used_bytes as f64 / cap_bytes) * 100.0).clamp(0.0, 100.0);
                                                    let used_gb = info.used_bytes as f64 / 1e9;
                                                    rsx! {
                                                        div { class: "srv-field",
                                                            div { class: "srv-label", "Storage used" }
                                                            div { class: "srv-meter",
                                                                div {
                                                                    class: if pct > 90.0 { "srv-meter-fill hot" } else { "srv-meter-fill" },
                                                                    style: "width: {pct:.1}%",
                                                                }
                                                            }
                                                            div { class: "srv-hint",
                                                                "{used_gb:.2} GB of {info.cap_gb} GB"
                                                            }
                                                            div { class: "srv-inline",
                                                                input {
                                                                    class: "srv-input narrow",
                                                                    r#type: "number",
                                                                    min: "1",
                                                                    value: "{info.cap_gb}",
                                                                    onchange: move |e| {
                                                                        let Ok(cap) = e.value().parse::<i64>() else { return };
                                                                        spawn(async move {
                                                                            match api::set_storage_cap(&session(), cap).await {
                                                                                Ok(updated) => storage_info.set(Some(updated)),
                                                                                Err(e) => status.set(e),
                                                                            }
                                                                        });
                                                                    },
                                                                }
                                                                div { class: "srv-hint", "GB cap — uploads are refused past it" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
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
                        editing_status.set(false);
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
                        if editing_status() {
                            div { class: "status-edit-row",
                                input {
                                    class: "status-input",
                                    placeholder: "what's up? (leave empty to clear)",
                                    maxlength: "100",
                                    value: "{status_draft}",
                                    spellcheck: "true",
                                    oncontextmenu: move |e: Event<MouseData>| {
                                        menu::open(ctx_menu, &e, menu::text_field_items())
                                    },
                                    oninput: move |e| status_draft.set(e.value()),
                                    onkeydown: move |e| {
                                        if e.key() == Key::Enter {
                                            spawn(async move {
                                                let text = Some(status_draft()).filter(|s| !s.trim().is_empty());
                                                match api::set_status(&session(), text).await {
                                                    Ok(()) => editing_status.set(false),
                                                    Err(e) => status.set(e),
                                                }
                                            });
                                        } else if e.key() == Key::Escape {
                                            editing_status.set(false);
                                        }
                                    },
                                }
                                div { class: "edit-hint", "Enter to save · Esc to cancel" }
                            }
                        } else if let Some(text) = profile.status.clone() {
                            div { class: "profile-status", "{text}" }
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
                                    button {
                                        class: "profile-btn",
                                        onclick: move |_| {
                                            status_draft.set(
                                                profile_card().and_then(|p| p.status).unwrap_or_default(),
                                            );
                                            editing_status.set(true);
                                        },
                                        "Set status"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !toasts().is_empty() {
                div { class: "toast-stack",
                    for (id, channel_id, message_id, author, where_, preview) in toasts() {
                        div {
                            key: "{id}",
                            class: "ping-toast",
                            onclick: move |_| {
                                toasts.write().retain(|t| t.0 != id);
                                let Some(channel) = channels().into_iter().find(|c| c.id == channel_id) else {
                                    return;
                                };
                                if selected().map(|c| c.id) != Some(channel_id) {
                                    open_channel(channel);
                                }
                                view_tab.set("chat");
                                jump_to.set(Some(message_id));
                            },
                            UserAvatar { user: author.clone(), class: "toast-avatar" }
                            div { class: "toast-text",
                                div { class: "toast-head",
                                    span { class: "toast-author", "{author.username}" }
                                    span { class: "toast-where", "{where_}" }
                                }
                                div { class: "toast-body", "{preview}" }
                            }
                            button {
                                class: "toast-x",
                                title: "Dismiss",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    toasts.write().retain(|t| t.0 != id);
                                },
                                Icon { name: "x", size: 12 }
                            }
                        }
                    }
                }
            }
            if let Some(view) = lightbox() {
                {
                    let url = view.url();
                    let many = view.urls.len() > 1;
                    let position = format!("{} of {}", view.at + 1, view.urls.len());
                    let mut step = move |delta: i32| {
                        let mut view = lightbox().unwrap_or_else(|| Lightbox::only(String::new()));
                        view.step(delta);
                        lightbox.set(Some(view));
                    };
                    rsx! {
                div {
                    class: "lightbox",
                    id: "lightbox",
                    // Focused on open (see the use_effect below) so the arrow
                    // keys reach it without a click first.
                    tabindex: "0",
                    onclick: move |_| lightbox.set(None),
                    onkeydown: move |e: Event<KeyboardData>| match e.key() {
                        Key::Escape => lightbox.set(None),
                        Key::ArrowLeft => step(-1),
                        Key::ArrowRight => step(1),
                        _ => {}
                    },
                    if many {
                        button {
                            class: "lightbox-arrow left",
                            title: "Previous (\u{2190})",
                            onclick: move |e: MouseEvent| {
                                e.stop_propagation();
                                step(-1);
                            },
                            Icon { name: "chevron-down", size: 22 }
                        }
                        button {
                            class: "lightbox-arrow right",
                            title: "Next (\u{2192})",
                            onclick: move |e: MouseEvent| {
                                e.stop_propagation();
                                step(1);
                            },
                            Icon { name: "chevron-down", size: 22 }
                        }
                    }
                    img {
                        class: "lightbox-img",
                        src: "{url}",
                        // Clicking the photo itself shouldn't close the photo.
                        onclick: move |e: MouseEvent| e.stop_propagation(),
                    }
                    div { class: "lightbox-actions",
                        if many {
                            span { class: "lightbox-count", "{position}" }
                        }
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
                        span { class: "lightbox-hint",
                            if many { "\u{2190} \u{2192} to browse \u{b7} click outside to close" } else { "click anywhere to close" }
                        }
                    }
                }
                    }
                }
            }
            div { class: "sidebar",
                // Jon's ask: server settings live on the server tile, not on
                // the channel title.
                button {
                    class: "sidebar-title",
                    title: "Server settings",
                    onclick: move |_| open_server_settings("overview"),
                    "{session().server_name}"
                }
                div { class: "channel-list",
                    // Channels nobody has filed stay at the top, exactly where
                    // they were before categories existed. Then one collapsible
                    // section per category, in the order an admin made them.
                    for group in std::iter::once(None).chain(categories().into_iter().map(Some)) {
                        {
                            let group_id = group.as_ref().map(|c: &shared::ChannelCategory| c.id);
                            let in_group: Vec<Channel> = channels()
                                .into_iter()
                                .filter(|c| c.kind == "text" && c.category_id == group_id)
                                .collect();
                            let collapsed = group_id.is_some_and(|id| {
                                audio_settings().collapsed_categories.contains(&id)
                            });
                            // An empty category still shows its heading, or an
                            // admin who just made one would think it failed.
                            let hide_section = group.is_none() && in_group.is_empty();
                            rsx! {
                                if !hide_section {
                                    if let Some(category) = group.clone() {
                                        {
                                            let id = category.id;
                                            let chevron: &'static str =
                                                if collapsed { "chevron-down" } else { "chevron-up" };
                                            rsx! {
                                                button {
                                                    class: "section-row section-toggle",
                                                    title: if collapsed { "Show" } else { "Hide" },
                                                    onclick: move |_| {
                                                        let mut settings = audio_settings.write();
                                                        if let Some(at) = settings
                                                            .collapsed_categories
                                                            .iter()
                                                            .position(|c| *c == id)
                                                        {
                                                            settings.collapsed_categories.remove(at);
                                                        } else {
                                                            settings.collapsed_categories.push(id);
                                                        }
                                                        let saved = settings.clone();
                                                        drop(settings);
                                                        api::save_settings(&saved);
                                                    },
                                                    span { class: "section-label", "{category.name}" }
                                                    Icon { name: chevron, size: 12 }
                                                }
                                            }
                                        }
                                    }
                                    // Collapsed hides the quiet ones; anything
                                    // unread or currently open stays visible.
                                    for channel in in_group.into_iter().filter(|c| {
                                        !collapsed
                                            || unread().contains_key(&c.id)
                                            || selected_id == Some(c.id)
                                    }) {
                        button {
                            key: "{channel.id}",
                            // A muted channel stays legible but stops asking
                            // for attention: no bold-unread treatment, and the
                            // badge below goes grey.
                            class: if selected_id == Some(channel.id) {
                                "channel active"
                            } else if muted().contains(&channel.id) {
                                "channel muted"
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
                                    let is_muted = muted().contains(&id);
                                    let (label, icon) = if is_muted {
                                        ("Unmute channel", "volume")
                                    } else {
                                        ("Mute channel", "ban")
                                    };
                                    items.push(menu::item(label, icon, move || {
                                        // Flip it locally first: the menu closes
                                        // on click and a round trip would leave
                                        // the channel looking unchanged.
                                        let mut muted = muted;
                                        if is_muted {
                                            muted.write().remove(&id);
                                        } else {
                                            muted.write().insert(id);
                                        }
                                        spawn(async move {
                                            if let Err(e) = api::set_mute(&session(), id, !is_muted).await {
                                                status.set(e);
                                                // Put it back the way the server has it.
                                                if is_muted {
                                                    muted.write().insert(id);
                                                } else {
                                                    muted.write().remove(&id);
                                                }
                                            }
                                        });
                                    }));
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
                            if muted().contains(&channel.id) {
                                span { class: "chan-muted", title: "Muted", Icon { name: "ban", size: 12 } }
                            }
                            if let Some((count, mentions, _)) = unread().get(&channel.id).copied() {
                                if selected_id != Some(channel.id) {
                                    span {
                                        // A muted channel still counts, quietly:
                                        // a red badge is the shouting a mute was
                                        // meant to stop.
                                        class: if muted().contains(&channel.id) {
                                            "unread-badge"
                                        } else if mentions > 0 {
                                            "unread-badge ping"
                                        } else {
                                            "unread-badge"
                                        },
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
                                    }
                                }
                            }
                        }
                    // Direct messages live in the right rail now (Jon's
                    // spec), which is also the redundancy switchb flagged when
                    // the same name appeared three times down this side.
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
                            // Occupancy as stacked chips on the row itself
                            // (Jon's dedup): who's in there at a glance, names
                            // on hover — the full roster with sliders lives in
                            // the connected tile below, exactly once.
                            if let Some(occupants) = voice_rosters().get(&ch_id).cloned().filter(|o| !o.is_empty()) {
                                span {
                                    class: "chip-stack",
                                    title: occupants.iter().map(|(u, _, _)| u.username.clone()).collect::<Vec<_>>().join(", "),
                                    for (occupant, _, _) in occupants.iter().take(4) {
                                        UserAvatar {
                                            key: "{occupant.id}",
                                            user: occupant.clone(),
                                            class: "chip-avatar",
                                        }
                                    }
                                    if occupants.len() > 4 {
                                        span { class: "chip-more", "+{occupants.len() - 4}" }
                                    }
                                }
                                if occupants.iter().any(|(_, sharing, _)| *sharing) {
                                    span { class: "live-pill", "LIVE" }
                                }
                            }
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
                            if let Some((monitors, windows)) = share_picker() {
                                div { class: "share-picker",
                                    div { class: "share-picker-title", "Share what?" }
                                    div { class: "share-grid",
                                        for m in monitors {
                                            {
                                                let key = format!("m{}", m.index);
                                                let thumb = share_thumbs().get(&key).cloned();
                                                rsx! {
                                                    button {
                                                        key: "{key}",
                                                        class: "share-card",
                                                        title: "{m.label}",
                                                        onclick: move |_| {
                                                            voice.send(voice::VoiceCmd::StartScreenShare {
                                                                target: share::ShareTarget::Monitor(m.index),
                                                            });
                                                            share_picker.set(None);
                                                        },
                                                        if let Some(src) = thumb {
                                                            img { class: "share-thumb", src: "{src}" }
                                                        } else {
                                                            div { class: "share-thumb placeholder",
                                                                Icon { name: "screen", size: 18 }
                                                            }
                                                        }
                                                        div { class: "share-card-label", "{m.label}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if !windows.is_empty() {
                                        div { class: "share-picker-sub", "Windows" }
                                    }
                                    div { class: "share-grid share-grid-windows",
                                        for w in windows {
                                            {
                                                let key = format!("w{}", w.hwnd);
                                                let thumb = share_thumbs().get(&key).cloned();
                                                rsx! {
                                                    button {
                                                        key: "{key}",
                                                        class: "share-card",
                                                        title: "{w.label}",
                                                        onclick: move |_| {
                                                            voice.send(voice::VoiceCmd::StartScreenShare {
                                                                target: share::ShareTarget::Window(w.hwnd),
                                                            });
                                                            share_picker.set(None);
                                                        },
                                                        if let Some(src) = thumb {
                                                            img { class: "share-thumb", src: "{src}" }
                                                        } else {
                                                            div { class: "share-thumb placeholder",
                                                                Icon { name: "file", size: 18 }
                                                            }
                                                        }
                                                        div { class: "share-card-label", "{w.label}" }
                                                    }
                                                }
                                            }
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
                                            // Always offer the picker now that
                                            // single windows are shareable.
                                            share_picker.set(Some((share::list_monitors(), share::list_windows())));
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
                // Channels are created in Server settings → Channels now, so
                // the sidebar's own box is gone and the list takes the room.
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
                            span { class: "me-status",
                                // Your own custom status wins over the plain
                                // connection state ("online").
                                {
                                    let mine = members()
                                        .iter()
                                        .find(|m| m.user.id == session().user.id)
                                        .and_then(|m| m.status.clone());
                                    mine.unwrap_or_else(|| status())
                                }
                            }
                        }
                    }
                    // Grouped, so the gear sits beside log out instead of
                    // being spread into the middle of the bar (switchb).
                    div { class: "me-actions",
                        button {
                            class: "logout",
                            title: "Settings",
                            onclick: move |_| open_settings("voice"),
                            Icon { name: "settings", size: 16 }
                        }
                        button { class: "logout", title: "Log out", onclick: logout, Icon { name: "power", size: 16 } }
                    }
                }
            }
            div { class: "main",
                div { class: "channel-header",
                    div { class: "channel-header-label", "{selected_label}" }
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
                    button {
                        class: "call-btn",
                        title: "Pinned messages",
                        onclick: move |_| {
                            let Some(channel) = selected() else { return };
                            spawn(async move {
                                match api::channel_pins(&session(), channel.id).await {
                                    Ok(pins) => pins_open.set(Some(pins)),
                                    Err(e) => status.set(e),
                                }
                            });
                        },
                        Icon { name: "pin", size: 16 }
                    }
                    button {
                        class: if files_open().is_some() { "call-btn active" } else { "call-btn" },
                        title: "Files in this channel",
                        onclick: move |_| {
                            if files_open().is_some() {
                                files_open.set(None);
                                return;
                            }
                            let Some(channel) = selected() else { return };
                            spawn(async move {
                                match api::channel_files(&session(), channel.id).await {
                                    Ok(files) => files_open.set(Some(files)),
                                    Err(e) => status.set(e),
                                }
                            });
                        },
                        Icon { name: "file", size: 16 }
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
                            class: if view_tab() == "chat" { "view-tab active" } else { "view-tab" },
                            onclick: move |_| view_tab.set("chat"),
                            "Chat"
                        }
                        button {
                            class: if view_tab() == "music" { "view-tab active" } else { "view-tab" },
                            onclick: move |_| {
                                view_tab.set("music");
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
                        button {
                            class: if view_tab() == "video" { "view-tab active" } else { "view-tab" },
                            onclick: move |_| view_tab.set("video"),
                            // Lit while anyone in your call has a camera or a
                            // share running.
                            if voice_status().participants.iter().any(|p| p.sharing || p.camera) {
                                span { class: "view-tab-dot" }
                            }
                            "Video"
                        }
                    }
                }
                if view_tab() == "music" {
                    MusicPlayer { music, volume: music_volume }
                }
                if view_tab() == "video" {
                    VideoTab { status: voice_status, members }
                }
                div {
                    class: match view_tab() {
                        "music" => "messages music-chat",
                        "video" => "messages video-chat",
                        _ => "messages",
                    },
                    id: "message-list",
                    // Jon's spec: clicking back into the chat body dismisses
                    // the Files panel.
                    onclick: move |_| {
                        if files_open.peek().is_some() {
                            files_open.set(None);
                        }
                    },
                    // column-reverse puts scrollTop 0 at the NEWEST message;
                    // browsers disagree on the sign when you scroll away from
                    // it, so distance is what matters, not direction.
                    onscroll: move |e: Event<ScrollData>| {
                        let away = e.scroll_top().abs() > 220.0;
                        if away != *scrolled_up.peek() {
                            scrolled_up.set(away);
                        }
                    },
                    // column-reverse container keeps the view pinned to the
                    // newest message, so render newest first.
                    {
                        // Oldest message you hadn't seen: the NEW line goes
                        // above it (rendered after it, in this flipped list).
                        let first_unread = divider_at().and_then(|last_read| {
                            messages().iter().map(|m| m.id).filter(|id| *id > last_read).min()
                        });
                        // Pin rights mirror the server: admins anywhere, and
                        // either side of a DM (a DM has no admin).
                        let can_pin = session().user.role == "admin"
                            || selected().is_some_and(|c| c.kind == "dm");
                        // Messages that open a new day, so a date divider can go
                        // above them (rendered after, in this flipped list).
                        let day_starts: std::collections::HashSet<i64> = {
                            let list = messages();
                            list.iter()
                                .enumerate()
                                .filter(|(i, m)| {
                                    *i == 0 || different_day(list[i - 1].created_at, m.created_at)
                                })
                                .map(|(_, m)| m.id)
                                .collect()
                        };
                        rsx! {
                            for (msg, compact) in group_messages(&messages()).into_iter().rev() {
                                {
                                    let is_target = highlight_msg() == Some(msg.id);
                                    let divider_here = Some(msg.id) == first_unread;
                                    let day_here = day_starts.contains(&msg.id).then(|| day_label(msg.created_at));
                                    let msg_id = msg.id;
                                    rsx! {
                                        div { class: if is_target { "hit-wrap" } else { "" },
                                            MessageRow { key: "{msg_id}", msg, compact, can_pin }
                                        }
                                        if divider_here {
                                            div { class: "new-divider", span { class: "new-pill", "NEW" } }
                                        }
                                        if let Some(label) = day_here {
                                            div { class: "day-divider", span { class: "day-pill", "{label}" } }
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
                // Deep in history (or just jumped to a pin/search hit) — one
                // click back to the present instead of scrolling for it.
                if scrolled_up() && view_tab() == "chat" {
                    button {
                        class: "jump-present",
                        onclick: move |_| {
                            highlight_msg.set(None);
                            scrolled_up.set(false);
                            // Newest lives at scrollTop 0 in a column-reverse list.
                            dioxus::document::eval(
                                "const el = document.getElementById('message-list'); \
                                 if (el) el.scrollTo({top: 0, behavior: 'smooth'});",
                            );
                        },
                        Icon { name: "chevron-down", size: 14 }
                        "Back to now"
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
                    let hits = emoji_suggestions(&draft(), &emojis(), emoji_dismissed().as_deref());
                    rsx! {
                        if !hits.is_empty() {
                            div { class: "mention-pop emoji-pop",
                                for (i, hit) in hits.iter().enumerate() {
                                    div {
                                        key: "{hit.insert}",
                                        class: if i == emoji_sel() % hits.len() { "mention-row selected" } else { "mention-row" },
                                        onclick: {
                                            let insert = hit.insert.clone();
                                            move |_| {
                                                draft.set(complete_emoji(&draft(), &insert));
                                                emoji_sel.set(0);
                                            }
                                        },
                                        if let Some(url) = hit.url.clone() {
                                            img { class: "custom-emoji", src: "{url}" }
                                        } else {
                                            span { class: "emoji-pop-glyph", "{hit.glyph}" }
                                        }
                                        span { class: "emoji-pop-name", ":{hit.name}:" }
                                        if hit.face {
                                            span { class: "emoji-pop-key", "tab" }
                                        }
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
                            emoji_sel.set(0);
                            // Only the word that was dismissed stays dismissed.
                            if let Some(d) = emoji_dismissed() {
                                let still_typing_it = emoji_partial(&draft())
                                    .is_some_and(|p| p.starts_with(&d))
                                    || emoticon_partial(&draft())
                                        .is_some_and(|t| t == d && emoji::emoticon(&t).is_some());
                                if !still_typing_it {
                                    emoji_dismissed.set(None);
                                }
                            }
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
                            let hits = emoji_suggestions(&draft(), &emojis(), emoji_dismissed().as_deref());
                            if !hits.is_empty() {
                                let sel = emoji_sel() % hits.len();
                                // A typed face is already a message people
                                // mean to send, so Enter keeps sending it and
                                // only Tab (or a click) takes the emoji. A
                                // half-typed `:name` is nobody's intended
                                // message, so there Enter completes.
                                let takes_enter = !hits[sel].face;
                                match e.key() {
                                    Key::ArrowDown => {
                                        e.prevent_default();
                                        emoji_sel.set(sel + 1);
                                        return;
                                    }
                                    Key::ArrowUp => {
                                        e.prevent_default();
                                        emoji_sel.set((sel + hits.len() - 1) % hits.len());
                                        return;
                                    }
                                    Key::Tab => {
                                        e.prevent_default();
                                        draft.set(complete_emoji(&draft(), &hits[sel].insert));
                                        emoji_sel.set(0);
                                        return;
                                    }
                                    Key::Enter if takes_enter => {
                                        e.prevent_default();
                                        draft.set(complete_emoji(&draft(), &hits[sel].insert));
                                        emoji_sel.set(0);
                                        return;
                                    }
                                    Key::Escape => {
                                        e.prevent_default();
                                        let face = emoticon_partial(&draft())
                                            .filter(|t| emoji::emoticon(t).is_some());
                                        emoji_dismissed.set(face.or_else(|| emoji_partial(&draft())));
                                        emoji_sel.set(0);
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
            // The rail carries the queue while the Music tab is open, the
            // Files explorer while it's toggled on, and the member list the
            // rest of the time.
            if view_tab() == "music" {
                div { class: "members rail-queue",
                    MusicQueue { music, picked: music_picked }
                }
            } else if let Some(files) = files_open() {
                div { class: "members files-rail",
                    div { class: "members-title", "Files" }
                    if files.is_empty() {
                        div { class: "files-empty", "no files in this channel yet" }
                    }
                    for f in files {
                        {
                            let abs = format!("{}{}", session().base_url, f.url);
                            let is_img = {
                                let lower = f.name.to_lowercase();
                                [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e))
                            };
                            let kind_icon = file_kind_icon(&f.name);
                            let abs_open = abs.clone();
                            let abs_menu = abs.clone();
                            let msg_id = f.message_id;
                            rsx! {
                                div {
                                    key: "{f.url}",
                                    class: "file-row",
                                    title: "{f.name}",
                                    onclick: move |_| {
                                        if is_img {
                                            // The panel's own images, in the
                                            // order it lists them.
                                            let base = session().base_url;
                                            let shown: Vec<String> = files_open()
                                                .unwrap_or_default()
                                                .iter()
                                                .filter(|f| {
                                                    let lower = f.name.to_lowercase();
                                                    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
                                                        .iter()
                                                        .any(|e| lower.ends_with(e))
                                                })
                                                .map(|f| format!("{base}{}", f.url))
                                                .collect();
                                            lightbox.set(Some(Lightbox::within(abs_open.clone(), shown)));
                                        } else {
                                            let _ = open::that(&abs_open);
                                        }
                                    },
                                    oncontextmenu: move |e: Event<MouseData>| {
                                        let abs = abs_menu.clone();
                                        let abs2 = abs_menu.clone();
                                        menu::open(ctx_menu, &e, vec![
                                            menu::item("Jump to message", "reply", move || {
                                                let mut jump = jump_to;
                                                jump.set(Some(msg_id));
                                                let mut files = files_open;
                                                files.set(None);
                                            }),
                                            menu::item("Save as…", "download", move || menu::save_url_as(abs.clone())),
                                            menu::item("Copy link", "link", move || menu::copy_to_clipboard(abs2.clone())),
                                        ]);
                                    },
                                    if is_img {
                                        // ?thumb: a 40px tile shouldn't pull a
                                        // 12 MB photo over the wire.
                                        img { class: "file-thumb", src: "{abs}?thumb=1", loading: "lazy" }
                                    } else {
                                        div { class: "file-thumb file-thumb-icon",
                                            Icon { name: kind_icon, size: 20 }
                                        }
                                    }
                                    div { class: "file-meta",
                                        div { class: "file-name", "{f.name}" }
                                        div { class: "file-sub",
                                            "{f.uploader} · {format_date(f.created_at)} · {human_size(f.size)}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
            div { class: "members",
                div { class: "rail-tabs",
                    button {
                        class: if rail_dms() { "rail-tab" } else { "rail-tab on" },
                        onclick: move |_| rail_dms.set(false),
                        "Members"
                    }
                    button {
                        class: if rail_dms() { "rail-tab on" } else { "rail-tab" },
                        onclick: move |_| rail_dms.set(true),
                        "Direct Messages"
                        // Unread DMs are always pings, so the count belongs
                        // on the tab you can't see from.
                        {
                            let waiting: i64 = channels()
                                .iter()
                                .filter(|c| c.kind == "dm")
                                .filter_map(|c| unread().get(&c.id).map(|(count, _, _)| *count))
                                .sum();
                            rsx! {
                                if waiting > 0 && !rail_dms() {
                                    span { class: "unread-badge ping", "{waiting}" }
                                }
                            }
                        }
                    }
                }
                if rail_dms() {
                    {
                        // Most recent first, by the last thing said. A DM with
                        // nothing in it yet sorts to the bottom rather than
                        // vanishing — you just opened it on purpose.
                        let mut dms: Vec<Channel> =
                            channels().into_iter().filter(|c| c.kind == "dm").collect();
                        dms.sort_by_key(|c| std::cmp::Reverse(c.last_at.unwrap_or(0)));
                        let me_id = session().user.id;
                        rsx! {
                            if dms.is_empty() {
                                div { class: "rail-empty",
                                    "No conversations yet. Open someone's profile in Members and hit Message."
                                }
                            }
                            for channel in dms {
                                button {
                                    key: "raildm{channel.id}",
                                    class: if selected().is_some_and(|c| c.id == channel.id) {
                                        "rail-dm on"
                                    } else {
                                        "rail-dm"
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
                                            // A conversation can be muted like
                                            // any other channel.
                                            let is_muted = muted().contains(&id);
                                            let label = if is_muted { "Unmute" } else { "Mute" };
                                            let icon = if is_muted { "volume" } else { "ban" };
                                            items.push(menu::item(label, icon, move || {
                                                let mut muted = muted;
                                                if is_muted {
                                                    muted.write().remove(&id);
                                                } else {
                                                    muted.write().insert(id);
                                                }
                                                spawn(async move {
                                                    if let Err(e) = api::set_mute(&session(), id, !is_muted).await {
                                                        status.set(e);
                                                        if is_muted {
                                                            muted.write().insert(id);
                                                        } else {
                                                            muted.write().remove(&id);
                                                        }
                                                    }
                                                });
                                            }));
                                            menu::open(ctx_menu, &e, items);
                                        }
                                    },
                                    if let Some(peer) = dm_peer(&channel, me_id) {
                                        UserAvatar { user: peer, class: "dm-avatar" }
                                    }
                                    div { class: "rail-dm-who",
                                        span { class: "rail-dm-name", "{dm_peer_name(&channel, me_id)}" }
                                        if let Some(at) = channel.last_at {
                                            span { class: "rail-dm-when", {format_time(at)} }
                                        }
                                    }
                                    if let Some((count, _, _)) = unread().get(&channel.id).copied() {
                                        if !selected().is_some_and(|c| c.id == channel.id) {
                                            span { class: "unread-badge ping", "{count}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
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
                        div { class: "member-main",
                            div { class: "member-line",
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
                            }
                            if let Some(text) = member.status.clone() {
                                div { class: "member-status", "{text}" }
                            }
                        }
                        span { class: "member-dot" }
                    }
                }
                }
            }
            }
        }
    }
}

/// 1234567 -> "1,234,567". Long numbers in a stats grid are unreadable
/// without it.
fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Bytes at whatever scale reads best — nobody wants a disk figure in bytes.
fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Uptime, at one unit of precision: "3d 4h", "12m".
fn human_duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
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

/// Split a message into inline images, inline videos, file attachments, and
/// text. `base` is this server's origin: the web app posts its uploads as
/// server-relative paths (it has no reason to know its own hostname), so a
/// photo shared from a phone arrives here as "/files/…" and has to be made
/// absolute before it can be rendered — otherwise it shows as bare text.
fn extract_media(content: &str, base: &str) -> (Vec<String>, Vec<String>, Vec<String>, String) {
    let has_ext = |w: &str, exts: &[&str]| {
        exts.iter().any(|ext| w.to_lowercase().ends_with(&format!(".{ext}")))
    };
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut files = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    // Line by line, keeping untouched lines verbatim: rebuilding the message
    // out of whitespace-separated words flattens it, and a code block posted
    // alongside a screenshot loses every newline and indent it had.
    for line in content.lines() {
        let mut kept: Vec<&str> = Vec::new();
        let mut pulled = false;
        for word in line.split_whitespace() {
            let absolute = if word.starts_with("http://") || word.starts_with("https://") {
                Some(word.to_owned())
            } else if word.starts_with("/files/") {
                Some(format!("{}{word}", base.trim_end_matches('/')))
            } else {
                None
            };
            match absolute {
                Some(url) if has_ext(&url, &["gif", "png", "jpg", "jpeg", "webp"]) => {
                    images.push(url);
                    pulled = true;
                }
                Some(url) if has_ext(&url, &["webm", "mp4", "mov"]) => {
                    videos.push(url);
                    pulled = true;
                }
                Some(url) if url.contains("/files/") => {
                    files.push(url);
                    pulled = true;
                }
                _ => kept.push(word),
            }
        }
        match (pulled, kept.is_empty()) {
            (false, _) => lines.push(line.to_owned()),
            (true, true) => {}
            (true, false) => lines.push(kept.join(" ")),
        }
    }
    (images, videos, files, lines.join("\n").trim().to_owned())
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

/// The video tab: the call, in the middle panel where chat normally sits.
///
/// Everyone in the call gets a tile — a live screen share or camera where
/// there is one, their avatar where there isn't — with the call controls
/// underneath and chat still below that. Frames arrive as JPEGs from the
/// `ndvideo` protocol rather than through dioxus, so a moving picture never
/// touches the diffing loop.
#[component]
fn VideoTab(status: voice::VoiceStatusSignal, members: Signal<Vec<UserStatus>>) -> Element {
    let voice = use_coroutine_handle::<voice::VoiceCmd>();
    let mut share_picker_ctx =
        use_context::<Signal<Option<(Vec<share::MonitorChoice>, Vec<share::WindowChoice>)>>>();
    // Label of the tile filling the panel, if any (double-click to toggle).
    let mut focused = use_signal(|| None::<String>);

    // Tiles refresh by re-requesting their frame, and the stamp that makes
    // each request unique comes from here. It used to be a JS interval that
    // rewrote every src, but that cleared itself the moment it ticked with no
    // tiles on screen — before the first paint, or any lull in the call — and
    // nothing restarted it, so the picture froze until a tab swap remounted
    // the whole thing. A signal can't die like that: it starts with the first
    // render and stops when the tab unmounts, which is also what stops the
    // requests, and with them the encoding.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        let mut frame = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(70)).await;
            frame += 1;
            tick.set(frame);
        }
    });
    let stamp = tick();

    let snapshot = status();
    let in_call = snapshot.channel_id.is_some();

    // One tile per stream, plus one per person who isn't sending video.
    let mut tiles: Vec<(Option<String>, String, bool, i64)> = Vec::new();
    for person in &snapshot.participants {
        let user_id = person
            .identity
            .strip_prefix("user-")
            .and_then(|id| id.parse::<i64>().ok())
            .unwrap_or(0);
        if person.sharing {
            // Your own share is a copy of the capture, not a subscription —
            // you never receive your own track back.
            let key = if person.is_me {
                "self:screen".to_string()
            } else {
                format!("{}:screen", person.identity)
            };
            tiles.push((Some(key), format!("{} · screen", person.name), person.speaking, user_id));
        }
        if person.camera {
            // Your own camera never comes back off the wire — it's the local
            // preview, published under its own key.
            let key = if person.is_me {
                "self:camera".to_string()
            } else {
                format!("{}:camera", person.identity)
            };
            tiles.push((Some(key), format!("{} · camera", person.name), person.speaking, user_id));
        }
        if !person.sharing && !person.camera {
            tiles.push((None, person.name.clone(), person.speaking, user_id));
        }
    }

    let mic_icon: &'static str = if snapshot.muted { "mic-off" } else { "mic" };
    let deaf_icon: &'static str = if snapshot.deafened { "headphones-off" } else { "headphones" };
    let wide = tiles.len() <= 2;

    // A focused tile that left the call (share stopped, person hung up)
    // would otherwise hide everyone behind an empty panel.
    {
        let still_there = tiles.iter().any(|(_, label, _, _)| Some(label.as_str()) == focused().as_deref());
        if focused().is_some() && !still_there {
            focused.set(None);
        }
    }

    rsx! {
        div { class: if focused().is_some() { "video-tab expanded" } else { "video-tab" },
            if focused().is_some() {
                button {
                    class: "video-unfocus",
                    title: "Back to everyone (or double-click the tile)",
                    onclick: move |_| focused.set(None),
                    Icon { name: "x", size: 14 }
                }
            }
            if !in_call {
                div { class: "video-empty",
                    Icon { name: "camera", size: 26 }
                    div { class: "video-empty-title", "You're not in a call" }
                    div { class: "video-empty-sub",
                        "Join a voice channel on the left, then turn on a camera or share a screen."
                    }
                }
            } else if tiles.is_empty() {
                div { class: "video-empty",
                    div { class: "video-empty-title", "Connecting…" }
                }
            } else {
                div {
                    class: match (focused().is_some(), wide) {
                        (true, _) => "video-grid focused",
                        (false, true) => "video-grid wide",
                        (false, false) => "video-grid",
                    },
                    for (key, label, speaking, user_id) in tiles {
                        {
                            let initial = label
                                .chars()
                                .next()
                                .map(|c| c.to_uppercase().to_string())
                                .unwrap_or_else(|| "?".into());
                            // While one tile is focused the others step aside.
                            let is_focused = focused().as_deref() == Some(label.as_str());
                            let hidden = focused().is_some() && !is_focused;
                            let tile_class = match (hidden, is_focused, speaking) {
                                (true, _, _) => "video-tile hidden",
                                (_, true, true) => "video-tile focused speaking",
                                (_, true, false) => "video-tile focused",
                                (_, false, true) => "video-tile speaking",
                                _ => "video-tile",
                            };
                            let label_for_click = label.clone();
                            rsx! {
                                div {
                                    key: "{label}",
                                    class: "{tile_class}",
                                    title: "Double-click to expand",
                                    // Same gesture as the pop-out call window
                                    // (v0.40.0): double-click fills the panel,
                                    // double-click again restores the grid.
                                    ondoubleclick: move |_| {
                                        let mut f = focused;
                                        if f.peek().as_deref() == Some(label_for_click.as_str()) {
                                            f.set(None);
                                        } else {
                                            f.set(Some(label_for_click.clone()));
                                        }
                                    },
                                    match key {
                                        Some(vkey) => rsx! {
                                            img {
                                                class: "video-frame",
                                                src: "http://ndvideo.localhost/{vkey}?t={stamp}",
                                                // Empty alt: a frame that hasn't
                                                // arrived yet shows nothing rather
                                                // than a broken-image icon.
                                                alt: "",
                                            }
                                        },
                                        None => rsx! {
                                            div {
                                                class: "video-avatar",
                                                style: "background: hsl({avatar_hue(user_id)}, 55%, 42%)",
                                                "{initial}"
                                            }
                                        },
                                    }
                                    span { class: "video-name", "{label}" }
                                }
                            }
                        }
                    }
                }
            }
            if in_call {
                div { class: "video-controls",
                    button {
                        class: if snapshot.muted { "video-btn danger" } else { "video-btn" },
                        title: if snapshot.muted { "Unmute" } else { "Mute" },
                        onclick: move |_| voice.send(voice::VoiceCmd::ToggleMute),
                        Icon { name: mic_icon, size: 17 }
                    }
                    button {
                        class: if snapshot.deafened { "video-btn danger" } else { "video-btn" },
                        title: if snapshot.deafened { "Undeafen" } else { "Deafen" },
                        onclick: move |_| voice.send(voice::VoiceCmd::ToggleDeafen),
                        Icon { name: deaf_icon, size: 17 }
                    }
                    button {
                        class: if snapshot.camera_self { "video-btn on" } else { "video-btn" },
                        title: if snapshot.camera_self { "Turn the camera off" } else { "Turn the camera on" },
                        onclick: move |_| {
                            if status.peek().camera_self {
                                voice.send(voice::VoiceCmd::StopCamera);
                            } else {
                                voice.send(voice::VoiceCmd::StartCamera);
                            }
                        },
                        Icon { name: "camera", size: 17 }
                    }
                    button {
                        class: if snapshot.sharing_self { "video-btn on" } else { "video-btn" },
                        title: if snapshot.sharing_self { "Stop sharing" } else { "Share your screen" },
                        onclick: move |_| {
                            if status.peek().sharing_self {
                                voice.send(voice::VoiceCmd::StopScreenShare);
                            } else {
                                // Same monitor/window picker the sidebar uses.
                                share_picker_ctx.set(Some((share::list_monitors(), share::list_windows())));
                            }
                        },
                        Icon { name: "screen", size: 17 }
                    }
                    span { class: "grow" }
                    button {
                        class: "video-btn leave",
                        title: "Leave the call",
                        onclick: move |_| voice.send(voice::VoiceCmd::Leave),
                        Icon { name: "phone-off", size: 17 }
                    }
                }
            }
        }
    }
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
                                // The song bar above ends short of the card
                                // edge, because a duration label sits after it.
                                // This spacer is that label's width, so the two
                                // bars finish on the same line (Jon).
                                span { class: "music-vol-pad" }
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
fn MessageRow(msg: Message, compact: bool, can_pin: bool) -> Element {
    let session = use_context::<Signal<api::Session>>();
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut lightbox = use_context::<Signal<Option<Lightbox>>>();
    let gallery = use_context::<Gallery>().0;
    let mut react_target = use_context::<ReactTarget>().0;
    let members_ctx = use_context::<Signal<Vec<UserStatus>>>();
    let tags_ctx = use_context::<Signal<Vec<Tag>>>();
    let emojis_ctx = use_context::<Signal<Vec<shared::CustomEmoji>>>();
    let mut replying_ctx = use_context::<Signal<Option<Message>>>();
    let mut jump_ctx = use_context::<JumpTo>().0;
    let ctx_menu = use_context::<menu::MenuSignal>();
    let mut editing = use_signal(|| false);
    let mut edit_draft = use_signal(String::new);

    // The bot's music message renders as a transport card, not as text.
    let player = msg.content.strip_prefix(shared::PLAYER_MARKER).map(str::to_owned);
    let (images, videos, files, text) = match player {
        Some(_) => (Vec::new(), Vec::new(), Vec::new(), String::new()),
        None => extract_media(&msg.content, &session().base_url),
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
            if can_pin {
                let pinned = msg.pinned;
                let label = if pinned { "Unpin message" } else { "Pin message" };
                items.push(menu::item(label, "pin", move || {
                    spawn(async move {
                        let _ = api::set_pinned(&session(), msg_id, !pinned).await;
                    });
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
            id: "msg-{msg_id}",
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
                        if msg.pinned {
                            span { class: "pin-flag", title: "Pinned message",
                                Icon { name: "pin", size: 11 }
                            }
                        }
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
                        // Inline images render at ~340px, so the thumbnail is
                        // all anyone sees; the lightbox still opens the
                        // original. Uploads from other hosts pass through
                        // unchanged — ?thumb is only ours to answer.
                        src: if src.contains("/files/") { format!("{src}?thumb=1") } else { src.clone() },
                        loading: "lazy",
                        onclick: {
                            let src = src.clone();
                            move |_| lightbox.set(Some(Lightbox::within(src.clone(), gallery())))
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
                                            lightbox.set(Some(Lightbox::within(src.clone(), gallery())));
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
                        // No autoplay, no loop: these were fine when every
                        // video here was a two-second clip, but a phone
                        // recording that starts itself and never stops is a
                        // jump-scare in a chat window (switchb, on a 1:41 one).
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
                // The picker belongs under the message it is for, not
                // pinned above the composer — Jon: "otherwise the reaction
                // replies button bar for emojis will show right below that
                // current message (easier to press)".
                if react_target() == Some(msg_id) {
                    div { class: "react-palette",
                        for emoji in REACTION_EMOJIS {
                            button {
                                key: "{emoji}",
                                onclick: move |_| {
                                    ws.send(ClientEvent::ToggleReaction {
                                        message_id: msg_id,
                                        emoji: emoji.to_string(),
                                    });
                                    react_target.set(None);
                                },
                                "{emoji}"
                            }
                        }
                        for custom in emojis_ctx() {
                            button {
                                key: "c{custom.id}",
                                title: ":{custom.name}:",
                                onclick: {
                                    let token = format!(":{}:", custom.name);
                                    move |_| {
                                        ws.send(ClientEvent::ToggleReaction {
                                            message_id: msg_id,
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
/// One row of the `:` autocomplete.
#[derive(Clone, PartialEq)]
struct EmojiHit {
    /// What lands in the message. A server emoji goes in as `:name:` because
    /// that is what the renderer looks up; a unicode one goes in as itself,
    /// which needs no decoding at the other end.
    insert: String,
    name: String,
    /// Some for a server emoji (its image), None for a unicode one.
    url: Option<String>,
    glyph: String,
    /// A typed face like ":)" rather than a `:name`. Enter still sends the
    /// message for these — see the keydown handler.
    face: bool,
}

/// The `:word` being typed at the caret, if there is one.
///
/// Deliberately strict, because a colon is a common character: a URL
/// ("https://"), a clock ("12:30") and an already-completed ":shrug:" all
/// contain one and none of them should open a popup.
fn emoji_partial(draft: &str) -> Option<String> {
    let idx = draft.rfind(':')?;
    let boundary_ok = idx == 0 || !draft[..idx].chars().last().unwrap().is_alphanumeric();
    let partial = &draft[idx + 1..];
    if !boundary_ok || !partial.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    // One letter matches most of the catalog, which is noise, not help.
    if partial.chars().count() < 2 {
        return None;
    }
    Some(partial.to_lowercase())
}

/// `dismissed` is a partial the typist pressed Escape on: the popup stays
/// shut while they keep typing that same word, and opens again for the next
/// one. Closing it shouldn't mean editing their message, and it shouldn't
/// mean it springs back on the next keystroke either.
/// The `:`- or `;`-led tail at the caret that might be a typed face.
///
/// Matched whole, which is what keeps it clear of everything else a colon
/// appears in: "https://x" leaves "//x" after the last colon, and no face is
/// spelled that way.
fn emoticon_partial(draft: &str) -> Option<String> {
    let idx = draft.rfind([':', ';'])?;
    let boundary_ok = idx == 0 || !draft[..idx].chars().last().unwrap().is_alphanumeric();
    if !boundary_ok {
        return None;
    }
    let tail = &draft[idx..];
    (tail.chars().count() <= 4).then(|| tail.to_owned())
}

fn emoji_suggestions(
    draft: &str,
    customs: &[shared::CustomEmoji],
    dismissed: Option<&str>,
) -> Vec<EmojiHit> {
    if let Some(tail) = emoticon_partial(draft) {
        if let Some((glyph, name)) = emoji::emoticon(&tail) {
            if dismissed.is_some_and(|d| d == tail) {
                return Vec::new();
            }
            return vec![EmojiHit {
                insert: glyph.to_owned(),
                name: name.to_owned(),
                url: None,
                glyph: glyph.to_owned(),
                face: true,
            }];
        }
    }
    let Some(partial) = emoji_partial(draft) else {
        return Vec::new();
    };
    if dismissed.is_some_and(|d| partial.starts_with(d)) {
        return Vec::new();
    }
    // Server emojis first: they're the ones this crew actually made.
    let mut hits: Vec<EmojiHit> = customs
        .iter()
        .filter(|e| e.name.to_lowercase().starts_with(&partial))
        .map(|e| EmojiHit {
            insert: format!(":{}:", e.name),
            name: e.name.clone(),
            url: Some(e.url.clone()),
            glyph: String::new(),
            face: false,
        })
        .collect();
    hits.extend(emoji::autocomplete(&partial).into_iter().map(|(glyph, name)| EmojiHit {
        insert: glyph.to_owned(),
        name: name.to_owned(),
        url: None,
        glyph: glyph.to_owned(),
        face: false,
    }));
    hits.truncate(8);
    hits
}

fn complete_emoji(draft: &str, insert: &str) -> String {
    match draft.rfind([':', ';']) {
        Some(idx) => format!("{}{insert} ", &draft[..idx]),
        None => draft.to_owned(),
    }
}

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

/// Slider/meter position (0..100) for a raw i16-scale RMS value, on a dB
/// scale spanning -60..0 dBFS. One mapping for the meter fill, the threshold
/// marker, AND the sensitivity slider, so they always line up.
fn mic_pos(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * (rms / 32768.0).log10();
    ((db + 60.0) / 60.0 * 100.0).clamp(1.0, 100.0)
}

/// Inverse of mic_pos: slider position back to raw RMS (0 stays 0 = open mic).
fn pos_to_rms(pos: f32) -> f32 {
    if pos <= 0.0 {
        return 0.0;
    }
    let db = pos / 100.0 * 60.0 - 60.0;
    32768.0 * 10f32.powf(db / 20.0)
}

/// Live mic input level bar. Isolated so 10Hz meter ticks only re-render this.
#[component]
fn MicMeter(level: voice::MicLevelSignal) -> Element {
    let pct = mic_pos(level());
    rsx! {
        div { class: "mic-meter",
            div { class: "mic-meter-fill", style: "width: {pct}%" }
        }
    }
}

#[cfg(test)]
mod logo_tests {
    #[test]
    fn the_logo_decodes_at_a_usable_size() {
        // The window and tray icons are built from this at runtime, and a
        // missing or truncated asset would only show up as a blank icon on
        // someone's taskbar.
        let (rgba, w, h) = super::logo_rgba().expect("assets/logo-256.png decodes");
        assert_eq!((w, h), (256, 256));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // Not a blank square: the mark has to actually be in there.
        let opaque = rgba.chunks(4).filter(|p| p[3] > 200).count();
        assert!(opaque > (w * h / 2) as usize, "logo looks empty: {opaque} solid pixels");
    }
}

#[cfg(test)]
mod media_tests {
    use super::extract_media;

    const BASE: &str = "https://notdiscord.example.com";

    #[test]
    fn an_upload_from_the_phone_still_renders_as_media() {
        // The web app posts server-relative paths; the desktop app posts
        // absolute ones. Both have to end up as playable media, or a video
        // shared from someone's phone shows up here as a line of text.
        let (images, videos, files, text) =
            extract_media("/files/abc123/clip.mp4", BASE);
        assert_eq!(videos, vec![format!("{BASE}/files/abc123/clip.mp4")]);
        assert!(images.is_empty() && files.is_empty() && text.is_empty());

        let (images, ..) = extract_media("/files/abc123/photo.JPG", BASE);
        assert_eq!(images, vec![format!("{BASE}/files/abc123/photo.JPG")]);

        let (.., files, _) = extract_media("/files/abc123/notes.pdf", BASE);
        assert_eq!(files, vec![format!("{BASE}/files/abc123/notes.pdf")]);

        // A trailing slash on the base must not double up.
        let (_, videos, ..) = extract_media("/files/abc/clip.mp4", "https://host/");
        assert_eq!(videos, vec!["https://host/files/abc/clip.mp4"]);
    }

    #[test]
    fn a_message_keeps_its_shape() {
        // The bug this guards: rebuilding the text out of whitespace-split
        // words flattened every message onto one line, so a code block
        // pasted next to a screenshot lost its newlines and its indenting.
        let code = "look:
```rust
fn main() {
    let x = 1;
}
```";
        let (.., text) = extract_media(code, BASE);
        assert_eq!(text, code, "a message with no attachments is untouched");

        let with_image = format!("{code}
/files/abc/shot.png");
        let (images, _, _, text) = extract_media(&with_image, BASE);
        assert_eq!(images.len(), 1);
        assert_eq!(text, code, "the picture goes, the formatting stays");

        // A line that mixes words and an attachment keeps its words.
        let (_, videos, _, text) = extract_media("first
see this /files/a/b.mp4 clip
last", BASE);
        assert_eq!(videos.len(), 1);
        assert_eq!(text, "first
see this clip
last");
    }

    #[test]
    fn absolute_links_and_plain_text_are_unchanged() {
        let absolute = format!("{BASE}/files/abc123/clip.mp4");
        let (_, videos, _, text) = extract_media(&absolute, BASE);
        assert_eq!(videos, vec![absolute]);
        assert!(text.is_empty());

        // Not an upload path: stays text, and never gets a host glued to it.
        let (images, videos, files, text) = extract_media("see /files-elsewhere/x.png ok", BASE);
        assert!(images.is_empty() && videos.is_empty() && files.is_empty());
        assert_eq!(text, "see /files-elsewhere/x.png ok");

        // Text alongside an attachment keeps the words and drops the URL.
        let (.., videos, _, text) = extract_media("check this /files/a/b.mp4 out", BASE);
        assert_eq!(videos.len(), 1);
        assert_eq!(text, "check this out");
    }
}

#[cfg(test)]
mod mic_scale_tests {
    use super::{mic_pos, pos_to_rms};

    #[test]
    fn slider_and_marker_agree() {
        // The knob position and the marker drawn from the stored RMS must be
        // the same number, or the "line just below your voice" promise lies.
        for p in 1..=100 {
            let round = mic_pos(pos_to_rms(p as f32));
            assert!((round - p as f32).abs() < 0.5, "pos {p} -> {round}");
        }
    }

    #[test]
    fn zero_stays_open_mic() {
        assert_eq!(pos_to_rms(0.0), 0.0);
        assert_eq!(mic_pos(0.0), 0.0);
    }

    #[test]
    fn scale_is_sane() {
        // Full scale i16 RMS pegs the bar; the 300-RMS gate floor sits low.
        assert!(mic_pos(32768.0) > 99.0);
        let floor = mic_pos(300.0);
        assert!((25.0..40.0).contains(&floor), "floor at {floor}%");
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

/// "3.2 MB"-style byte counts for the Files panel.
fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// Placeholder icon for a non-previewable attachment, by extension.
fn file_kind_icon(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "webm" | "mov" | "mkv" | "avi" => "camera",
        "mp3" | "wav" | "ogg" | "flac" | "m4a" => "music",
        _ => "file",
    }
}

/// Divider label for the day a message was sent: "Today", "Yesterday", or
/// the full date. Local time, so it matches the timestamps beside names.
fn day_label(unix_ms: i64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp_millis(unix_ms) else {
        return String::new();
    };
    let date = dt.with_timezone(&chrono::Local).date_naive();
    let today = chrono::Local::now().date_naive();
    match (today - date).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => date.format("%B %-d, %Y").to_string(),
    }
}

/// True when two timestamps fall on different local days.
fn different_day(a: i64, b: i64) -> bool {
    let day = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.with_timezone(&chrono::Local).date_naive())
    };
    match (day(a), day(b)) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
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
