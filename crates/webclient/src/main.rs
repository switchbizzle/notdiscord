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

// Upload glue (pwa/upload.js). The file stays in the browser, which streams
// it from disk straight into the request; wasm only sees its name and size.
#[wasm_bindgen(js_namespace = ndUpload)]
extern "C" {
    #[wasm_bindgen(js_name = stage)]
    fn upload_stage_js(input_id: &str) -> String;
    #[wasm_bindgen(js_name = send)]
    fn upload_send_js(url: &str, token: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = progress)]
    fn upload_progress_js() -> f64;
    #[wasm_bindgen(js_name = discard)]
    fn upload_discard_js();
}

/// What the glue tells us about a picked file: enough to show it and name it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct StagedFile {
    name: String,
    size: u64,
    /// "image", "video", or "file".
    kind: String,
    /// A blob: URL for the preview; empty for a plain file.
    url: String,
}

/// "4.3 MB", the way a person would say it.
fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_048_576.0 {
        format!("{:.1} MB", b / 1_048_576.0)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

// Install glue (pwa/install.js).
#[wasm_bindgen(js_namespace = ndInstall)]
extern "C" {
    #[wasm_bindgen(js_name = prompt)]
    fn install_prompt_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = getState)]
    fn install_state_js() -> String;
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct InstallGlue {
    /// True once the browser has offered us an install event to replay.
    available: bool,
    installed: bool,
    ios: bool,
    #[serde(default)]
    outcome: String,
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
/// How long a typing indicator stays up after the last Typing event, and how
/// rarely we send one while someone types. Same numbers as the desktop, so a
/// phone and a desktop agree about who is typing.
/// A name's colour: the first tag assigned to that person, else the hue their
/// avatar already uses. Same rule as the desktop, so one person is the same
/// colour on both.
fn name_color(user_id: i64, members: &[UserStatus], tags: &[shared::Tag]) -> String {
    members
        .iter()
        .find(|m| m.user.id == user_id)
        .and_then(|m| m.tag_ids.first())
        .and_then(|tid| tags.iter().find(|t| t.id == *tid))
        .map(|t| t.color.clone())
        .unwrap_or_else(|| format!("hsl({}, 65%, 68%)", (user_id * 137) % 360))
}

/// Pair each message with whether it should render compactly: same author as
/// the one before it, within five minutes. Identical rule to the desktop, so
/// a conversation breaks into the same blocks on both.
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

fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// How long a message may sit unconfirmed before it is called failed.
/// Generous: a slow phone connection is not the same as a lost message.
const SEND_TIMEOUT_MS: i64 = 12_000;
const TYPING_TTL_MS: i64 = 4000;
const TYPING_SEND_INTERVAL_MS: i64 = 2500;

/// Collapsed category ids, per device.
/// The composer input, so a mention tap can hand focus back to it.
const COMPOSER_ID: &str = "nd-composer";
/// Five lines of 14.5px at 1.35 plus the padding; past this it scrolls.
const COMPOSER_MAX_PX: i32 = 132;
const COLLAPSED_KEY: &str = "notdiscord_collapsed";
const SESSION_KEY: &str = "nd_session";

fn main() {
    dioxus::launch(App);
}

fn load_session() -> Option<api::Session> {
    gloo_storage::LocalStorage::get(SESSION_KEY).ok()
}

/// Why the last session ended, kept where the session itself is kept: the
/// sign-out swaps the whole tree for the login screen, and a fresh component
/// remembers nothing. localStorage also survives the reload freshen.js can
/// do at any moment.
const SIGNOUT_KEY: &str = "nd_signout_reason";

/// Sign out of a session the server no longer honours, saying why. Not the
/// same act as the Log out button: nobody asked for this one, so the reason
/// has to survive to the login screen or it looks like a crash.
fn signed_out(mut session: Signal<Option<api::Session>>, reason: &str) {
    if session.peek().is_none() {
        return;
    }
    let _ = gloo_storage::LocalStorage::set(SIGNOUT_KEY, reason);
    save_session(&None);
    session.set(None);
}

/// The notice for the login screen, read once and cleared.
fn take_signout_notice() -> String {
    let notice: String = gloo_storage::LocalStorage::get(SIGNOUT_KEY).unwrap_or_default();
    gloo_storage::LocalStorage::delete(SIGNOUT_KEY);
    notice
}

fn save_session(session: &Option<api::Session>) {
    match session {
        Some(s) => {
            // A fresh token is not answerable for the last one's 401.
            api::forget_session_death();
            let _ = gloo_storage::LocalStorage::set(SESSION_KEY, s);
        }
        None => gloo_storage::LocalStorage::delete(SESSION_KEY),
    }
}

/// The host this client is talking to — the "Server" row says where your
/// messages actually go, which on a self-hosted server is not obvious.
/// Copy to the clipboard. Best-effort: a browser that refuses — or an
/// insecure origin, where the API simply isn't there — leaves the message
/// where it was, which is no worse than never offering to copy it.
fn copy_text(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// "online", or "online · tinkering" when they've set a status. Presence
/// leads because it's the part that decides whether messaging is a
/// conversation or a note left on the fridge.
fn presence_line(online: bool, mode: &str, status: Option<String>) -> String {
    let presence = shared::presence_label(shared::effective_presence(online, mode));
    match status {
        Some(status) if !status.trim().is_empty() => format!("{presence} · {status}"),
        _ => presence.to_string(),
    }
}

/// The class the presence dot wears. One place, so a mode added later can't
/// be drawn one way in the DM list and another in the members sheet.
fn presence_dot(online: bool, mode: &str) -> String {
    format!("presence {}", shared::effective_presence(online, mode))
}

/// Fit the composer to its text, up to five lines, after which it scrolls.
/// Measured by resetting the height and reading scrollHeight, which is the
/// only way that is right for wrapped lines too. Via an attribute rather
/// than the style object, which would cost a web-sys feature for one line.
///
/// An empty box is left at one row without measuring anything. scrollHeight
/// counts the PLACEHOLDER when there is no text, so a placeholder long
/// enough to wrap made an empty composer two lines tall — which is how
/// switchb found this, in a channel whose name was one word too long.
fn autosize_composer(empty: bool) {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(COMPOSER_ID))
    else {
        return;
    };
    let _ = el.set_attribute("style", "height: auto");
    if empty {
        return;
    }
    // +2 for the borders, which scrollHeight does not include and
    // border-box height does.
    let wanted = (el.scroll_height() + 2).min(COMPOSER_MAX_PX);
    let _ = el.set_attribute("style", &format!("height: {wanted}px"));
}

/// The box a picture will take once it arrives, from the size the server
/// sent: the same caps .msg-img applies — full width, 420px tall, 150px
/// either way for a sticker — worked out up front so the list does not
/// jump when the bytes land. Width is the number to pin; the ratio gives
/// the height, and max-width: 100% in the stylesheet still wins on a
/// narrow screen, shrinking both together.
fn image_box_style(width: u32, height: u32, sticker: bool) -> String {
    let (max_w, max_h) = if sticker { (150.0, 150.0) } else { (f64::INFINITY, 420.0) };
    let scale = (max_w / width as f64).min(max_h / height as f64).min(1.0);
    let shown = (width as f64 * scale).round();
    format!("width: {shown}px; aspect-ratio: {width} / {height}")
}

/// Put the cursor back in the composer. Tapping a suggestion moves focus to
/// the button it was on, which on a phone closes the keyboard — and having to
/// tap the field again after every mention would be worse than typing the
/// name out. Best-effort: if the element has gone, the person just taps.
fn focus_composer() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(COMPOSER_ID))
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// A short vibration, the phone-native way of saying "look at me" — and
/// unlike a ringtone it needs no user gesture to be allowed. Best-effort: a
/// browser without the API just doesn't buzz.
fn buzz() {
    haptic(400);
}

/// A tap you can feel. The long-press that opens a message's actions gives
/// one, so you know it took without watching for the sheet.
fn haptic(ms: u32) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().vibrate_with_duration(ms);
    }
}

fn host_name() -> String {
    web_sys::window().map(|w| w.location().host().unwrap_or_default()).unwrap_or_default()
}

/// The address this page was served from, and the fallback a message link is
/// minted against when the server hasn't been told a public URL.
fn origin() -> String {
    web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default()
}

/// The permalink in the address bar, if this tab was opened by one.
fn deep_linked_message() -> Option<(i64, i64)> {
    let path = web_sys::window()?.location().pathname().ok()?;
    shared::parse_message_path(&path)
}

/// The channel that was open when the phone put the app away. Android
/// drops a backgrounded tab whenever it wants the memory back, and the app
/// then boots from nothing — at the channel list, every time, which Jon read
/// as "it forgot where I was". It had. localStorage, next to the session.
const LAST_CHANNEL_KEY: &str = "nd_last_channel";

fn remembered_channel() -> Option<i64> {
    gloo_storage::LocalStorage::get::<i64>(LAST_CHANNEL_KEY).ok()
}

fn remember_channel(id: Option<i64>) {
    match id {
        Some(id) => {
            let _ = gloo_storage::LocalStorage::set(LAST_CHANNEL_KEY, id);
        }
        None => gloo_storage::LocalStorage::delete(LAST_CHANNEL_KEY),
    }
}

/// The channel a push notification asked for: the service worker opens
/// `/app/?channel=12` when there is no window to hand the tap to.
fn deep_linked_channel() -> Option<i64> {
    let search = web_sys::window()?.location().search().ok()?;
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("channel="))
        .and_then(|id| id.parse().ok())
}

/// Put the address bar back to `/app/` once a deep link has been consumed.
/// Not cosmetic: `freshen.js` re-fetches "./" to notice a new build, and from
/// `/app/channels/7/12` that resolves to a 404 — the phone would quietly stop
/// noticing deploys for as long as the tab stayed open.
fn forget_deep_link() {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some("/app/"));
    }
}

/// Scroll a message into view. False when the row isn't in the DOM (yet, or
/// at all), so the caller can try again while the list renders.
fn scroll_to_message(message_id: i64) -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let Some(row) = document.get_element_by_id(&format!("msg-{message_id}")) else {
        return false;
    };
    let options = web_sys::ScrollIntoViewOptions::new();
    options.set_block(web_sys::ScrollLogicalPosition::Center);
    row.scroll_into_view_with_scroll_into_view_options(&options);
    true
}

/// (year, month, day) on the phone's own clock — the only calendar that
/// means anything for "was this today".
fn local_day(ms: i64) -> (u32, u32, u32) {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    (d.get_full_year(), d.get_month(), d.get_date())
}

fn different_day(a: i64, b: i64) -> bool {
    local_day(a) != local_day(b)
}

/// "Today", "Yesterday", "3 days ago", or the date. The words are
/// shared::day_words, so the two apps read alike; the arithmetic is JS's
/// rather than chrono's, which this crate doesn't carry.
fn day_label(ms: i64) -> String {
    let (y, m, d) = local_day(ms);
    let now = js_sys::Date::new_0();
    // Local midnight of each day, then whole days between them. A DST
    // change makes one of those days 23 or 25 hours; rounding absorbs it.
    let midnight = |y: u32, m: u32, d: u32| js_sys::Date::new_with_year_month_day(y, m as i32, d as i32).get_time();
    let today = midnight(now.get_full_year(), now.get_month(), now.get_date());
    let days_ago = ((today - midnight(y, m, d)) / 86_400_000.0).round() as i64;
    if let Some(words) = shared::day_words(days_ago) {
        return words;
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"month".into(), &"long".into());
    let _ = js_sys::Reflect::set(&opts, &"day".into(), &"numeric".into());
    let _ = js_sys::Reflect::set(&opts, &"year".into(), &"numeric".into());
    d.to_locale_date_string("en-US", &opts).into()
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
/// The open image and the list it came from, so the arrows (and a swipe) step
/// through the channel's photos instead of trapping you on the one you tapped.
#[derive(Clone, PartialEq)]
struct Lightbox {
    urls: Vec<String>,
    at: usize,
}

impl Lightbox {
    fn only(url: String) -> Self {
        Self { urls: vec![url], at: 0 }
    }

    fn within(url: String, urls: Vec<String>) -> Self {
        match urls.iter().position(|u| *u == url) {
            Some(at) => Self { urls, at },
            None => Self::only(url),
        }
    }

    fn url(&self) -> String {
        self.urls[self.at].clone()
    }

    /// Wraps — running off the end of a gallery is never what you meant.
    fn step(&mut self, delta: i32) {
        let n = self.urls.len() as i32;
        if n > 1 {
            self.at = (((self.at as i32 + delta) % n + n) % n) as usize;
        }
    }
}

/// The channel's images, oldest first. Newtyped because dioxus keys context
/// by type and a bare Signal<Vec<String>> invites a collision.
#[derive(Clone, Copy)]
struct Gallery(Memo<Vec<String>>);

/// The server's `NOTDISCORD_PUBLIC_URL`, when it has one. A phone on the LAN
/// is served from an address nobody outside can open, so a link meant to be
/// shared prefers this over the origin. Newtyped for the same reason as
/// everything else here.
#[derive(Clone, Copy)]
struct PublicUrl(Signal<Option<String>>);

/// A permalink someone tapped inside the app: (channel, message).
#[derive(Clone, Copy)]
struct OpenMessage(Signal<Option<(i64, i64)>>);

/// Where this device mints message links.
fn permalink_base(public: &Option<String>) -> String {
    public.clone().unwrap_or_else(origin)
}

/// Every address we'd recognise our own server by, so a permalink pasted into
/// chat opens the message here instead of in a second tab.
fn permalink_bases(public: &Option<String>) -> Vec<String> {
    let mut bases = vec![origin()];
    if let Some(public) = public {
        bases.push(public.clone());
    }
    bases
}

fn extract_media(content: &str) -> (Vec<String>, Vec<String>, Vec<(String, String)>, String) {
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut files = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    // Line by line, and untouched lines are kept verbatim: joining every word
    // with a single space collapsed the whole message onto one line, which
    // turned code blocks into a run-on and ate every indent in them.
    for line in content.lines() {
        let mut kept: Vec<&str> = Vec::new();
        let mut pulled = false;
        for token in line.split_whitespace() {
            let is_url = shared::is_web_url(token) || token.starts_with("/files/");
            if is_url {
                let lower = token.to_lowercase();
                let path = lower.split(['?', '#']).next().unwrap_or("");
                if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| path.ends_with(e)) {
                    images.push(token.to_owned());
                    pulled = true;
                    continue;
                }
                if [".mp4", ".webm", ".mov"].iter().any(|e| path.ends_with(e)) {
                    videos.push(token.to_owned());
                    pulled = true;
                    continue;
                }
                if token.starts_with("/files/") {
                    let name = token.rsplit('/').next().unwrap_or("file").to_owned();
                    files.push((token.to_owned(), name));
                    pulled = true;
                    continue;
                }
            }
            kept.push(token);
        }
        match (pulled, kept.is_empty()) {
            // Nothing was taken out of this line, so it stands as written.
            (false, _) => lines.push(line.to_owned()),
            // The line was only an attachment; drop it rather than leave a gap.
            (true, true) => {}
            (true, false) => lines.push(kept.join(" ")),
        }
    }
    (images, videos, files, lines.join("\n").trim().to_owned())
}

/// Links in a message worth asking the server for a card about: web URLs that
/// aren't already rendered inline as an image, video, or attachment.
fn preview_urls(text: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if !shared::is_web_url(word) {
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
        // Two cards is plenty; a wall of them buries the conversation, and on
        // a phone it buries it faster.
        if urls.len() == 2 {
            break;
        }
    }
    urls
}

thread_local! {
    /// Cards fetched this session. The server caches them too — this keeps
    /// scrolling back through history off the network entirely.
    static PREVIEW_CACHE: std::cell::RefCell<HashMap<String, Option<shared::LinkPreview>>> =
        std::cell::RefCell::new(HashMap::new());
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
            // A backstop for dev builds: released pages catch the install
            // event in the document head (see release-webapp.ps1), which is
            // early enough to beat a repeat visitor's browser. This must not
            // clear what that already caught.
            "if ('serviceWorker' in navigator) {{ navigator.serviceWorker.register('/app/sw.js', {{ scope: '/app/' }}); }}
             window.__ndInstallEvent = window.__ndInstallEvent || null;
             window.addEventListener('beforeinstallprompt', function (e) {{ e.preventDefault(); window.__ndInstallEvent = e; }});"
        }
        // livekit-client + our glue, self-hosted next to the bundle.
        document::Script { src: "/app/livekit-client.umd.min.js" }
        document::Script { src: "/app/voice.js" }
        document::Script { src: "/app/push.js" }
        document::Script { src: "/app/install.js" }
        document::Script { src: "/app/upload.js" }
        // Notices when a newer build is on the server and reloads, because an
        // installed app otherwise runs whatever it booted with forever.
        document::Script { src: "/app/freshen.js" }
        if session().is_some() {
            Main { session }
        } else {
            Gate { session }
        }
    }
}

/// Login, or setup on a server nobody has claimed yet. Asking a self-hoster
/// for credentials that cannot exist is the least helpful thing a fresh
/// server could do, so it asks who they are instead.
#[component]
fn Gate(session: Signal<Option<api::Session>>) -> Element {
    let info = use_resource(api::server_info);
    match info() {
        // Still asking. A blank moment beats flashing the wrong screen.
        None => rsx! { div { class: "login-wrap" } },
        Some(Some(info)) if info.needs_setup => rsx! { Setup { session } },
        _ => rsx! { Login { session } },
    }
}

#[component]
fn Setup(session: Signal<Option<api::Session>>) -> Element {
    let mut server_name = use_signal(|| "NotDiscord".to_string());
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut invite = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);

    let mut claim = move || {
        if busy() {
            return;
        }
        spawn(async move {
            busy.set(true);
            error.set(String::new());
            let req = shared::SetupRequest {
                username: username(),
                password: password(),
                server_name: server_name(),
                invite: Some(invite()).filter(|c| !c.trim().is_empty()),
            };
            match api::setup(req).await {
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
                div { class: "login-mark", Icon { name: "shield", size: 22 } }
                h1 { "Set up your server" }
                p { class: "login-sub",
                    "Nobody has an account here yet. The first one is yours, and it's the admin."
                }
                div { class: "login-fields",
                    div { class: "field",
                        label { "Server name" }
                        input {
                            value: "{server_name}",
                            oninput: move |e| server_name.set(e.value()),
                        }
                    }
                    div { class: "field",
                        label { "Your username" }
                        input {
                            value: "{username}",
                            autocapitalize: "none",
                            oninput: move |e| username.set(e.value()),
                        }
                    }
                    div { class: "field",
                        label { "Password" }
                        input {
                            r#type: "password",
                            value: "{password}",
                            oninput: move |e| password.set(e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Enter {
                                    claim();
                                }
                            },
                        }
                    }
                    div { class: "field",
                        label { "Invite code for everyone else (optional)" }
                        input {
                            value: "{invite}",
                            autocapitalize: "none",
                            oninput: move |e| invite.set(e.value()),
                        }
                        span { class: "field-hint",
                            "Leave it empty and anyone who finds this server can register. You can change it later in Server settings."
                        }
                    }
                }
                if !error().is_empty() {
                    div { class: "login-error", "{error}" }
                }
                button {
                    class: "btn btn-primary login-cta",
                    disabled: busy(),
                    onclick: move |_| claim(),
                    if busy() { "Setting up…" } else { "Create my server" }
                }
            }
        }
    }
}

#[component]
fn Login(session: Signal<Option<api::Session>>) -> Element {
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut invite = use_signal(String::new);
    let mut error = use_signal(String::new);
    // Set when we arrived here because the server ended the session rather
    // than because anyone asked to leave. Cleared by the first thing typed.
    let mut notice = use_signal(take_signout_notice);
    let mut busy = use_signal(|| false);
    // Logging in is what almost everyone is here to do; an invite code box on
    // that screen is a question nobody signing in can answer.
    let mut registering = use_signal(|| false);

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
                div { class: "login-mark", Icon { name: "message", size: 22 } }
                h1 { "NotDiscord" }
                if registering() {
                    p { class: "login-sub", "make yourself an account" }
                } else {
                    p { class: "login-sub", "the phone-sized version" }
                }
                div { class: "login-fields",
                    div { class: "field",
                        label { "Username" }
                        input {
                            value: "{username}",
                            autocapitalize: "none",
                            oninput: move |e| username.set(e.value()),
                        }
                    }
                    div { class: "field",
                        label { "Password" }
                        input {
                            r#type: "password",
                            value: "{password}",
                            oninput: move |e| password.set(e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Enter {
                                    submit(registering());
                                }
                            },
                        }
                    }
                    if registering() {
                        div { class: "field",
                            label { "Invite code" }
                            input {
                                value: "{invite}",
                                autocapitalize: "none",
                                placeholder: "ask whoever runs the server",
                                oninput: move |e| invite.set(e.value()),
                            }
                        }
                    }
                }
                if !notice().is_empty() {
                    div { class: "login-note", "{notice}" }
                }
                if !error().is_empty() {
                    div { class: "login-error", "{error}" }
                }
                button {
                    class: "btn btn-primary login-cta",
                    disabled: busy(),
                    onclick: move |_| {
                        notice.set(String::new());
                        submit(registering());
                    },
                    if registering() { "Create account" } else { "Log in" }
                }
                button {
                    class: "login-switch",
                    onclick: move |_| {
                        error.set(String::new());
                        registering.toggle();
                    },
                    if registering() { "I already have an account" } else { "I need an account" }
                }
            }
        }
    }
}

#[component]
fn Main(session: Signal<Option<api::Session>>) -> Element {
    let sess = use_memo(move || session().expect("main renders only with a session"));
    // Link cards fetch through the API from deep in the message list, so the
    // session has to be reachable by context. Main is unmounted on logout, so
    // this snapshot can't outlive the account it belongs to.
    use_context_provider(|| Signal::new(sess()));
    let mut channels = use_signal(Vec::<Channel>::new);
    // Provided as well as held: the message row needs it to colour a name.
    let mut members = use_context_provider(|| Signal::new(Vec::<UserStatus>::new()));
    let mut selected = use_signal(|| None::<Channel>);
    let mut messages = use_signal(Vec::<Message>::new);
    // Provided rather than passed: the renderer needs it several components
    // deep, in the middle of a message body.
    let mut emojis = use_context_provider(|| Signal::new(Vec::<shared::CustomEmoji>::new()));
    let mut categories = use_signal(Vec::<shared::ChannelCategory>::new);
    // user id -> (channel, name, expiry)
    let mut typing = use_signal(std::collections::HashMap::<i64, (i64, String, i64)>::new);
    let mut last_typing_sent = use_signal(|| 0i64);
    // Messages shown before the server has confirmed them. A pending message
    // carries a negative id — real ids are a positive autoincrement, so the
    // sign alone says "not yet real" without changing the wire type.
    let mut pending_at = use_signal(std::collections::HashMap::<i64, i64>::new);
    let mut failed_sends = use_signal(std::collections::HashSet::<i64>::new);
    let mut next_temp_id = use_signal(|| -1i64);
    // The server's own name, so the Chats tab says where you are rather than
    // what the app is called.
    let mut server_name = use_signal(String::new);
    // Tags colour names; provided because the message row is a component away.
    let tags = use_context_provider(|| Signal::new(Vec::<shared::Tag>::new()));
    let mut stickers = use_context_provider(|| Signal::new(Vec::<shared::Sticker>::new()));
    // Which groups this person keeps shut, remembered on this device only —
    // it's a per-phone preference, not something the server should carry.
    let mut collapsed = use_signal(|| {
        gloo_storage::LocalStorage::get::<Vec<i64>>(COLLAPSED_KEY).unwrap_or_default()
    });
    let mut lightbox = use_context_provider(|| Signal::new(None::<Lightbox>));
    // Asked once at load, so a message link can point at the address the crew
    // uses rather than the one this phone happens to be on.
    let mut public_url = use_signal(|| None::<String>);
    use_context_provider(|| PublicUrl(public_url));
    // Where a finger went down, so touchend can measure how far it travelled.
    let mut swipe_from = use_signal(|| None::<f64>);
    let gallery = use_memo(move || {
        messages().iter().flat_map(|m| extract_media(&m.content).0).collect::<Vec<String>>()
    });
    use_context_provider(|| Gallery(gallery));
    let mut has_more = use_signal(|| false);
    // True from tapping a channel until its history is back, so the screen
    // can say so instead of standing blank.
    let mut loading_channel = use_signal(|| false);
    // Bumped when the local date changes, so the day dividers move from
    // "Today" to "Yesterday" to "2 days ago" at midnight on their own.
    let mut day_tick = use_signal(|| 0u32);
    let mut draft = use_signal(String::new);
    // One-off messages — an error, "voice disconnected" — shown as a toast
    // that dismisses itself. Connection state is NOT this: see `offline`.
    let mut status = use_signal(String::new);
    // True from the moment the socket drops until it is back. Drawn as a
    // small pill, not a bar across the title.
    let mut offline = use_signal(|| false);
    let mut uploading = use_signal(|| false);
    // A picked file waiting on the preview sheet, and how far its send is.
    let mut staged = use_signal(|| None::<StagedFile>);
    let mut upload_pct = use_signal(|| 0.0f64);
    // Which of the four tabs is showing.
    let mut tab = use_signal(|| "chats");
    // A full-screen layer over the tabs: "channel", "search", "settings" or
    // "profile". None means you are looking at a tab. The tab bar hides
    // whenever one of these is up, which is what makes a channel feel like a
    // screen rather than a pane.
    let mut overlay = use_signal(|| None::<&'static str>);
    // The bottom sheet, if one is up: "attach", "stickers" or "members".
    let mut sheet = use_signal(|| None::<&'static str>);
    // The call, expanded to fill the screen. The mini bar above the tabs
    // shows whenever this is false and you're connected.
    let mut call_open = use_signal(|| false);
    // Seconds since this device joined, ticked by the voice poll below so
    // the clock only runs while there is a call to time.
    let mut call_secs = use_signal(|| 0i64);
    let mut call_since = use_signal(|| 0i64);
    // Somebody has joined the voice room of a DM you're in and you aren't
    // there yet: (the DM's channel id, who). That IS the ring — a DM channel
    // doubles as its own voice room, so a call is a join, and the desktop
    // has rung off exactly this event since v0.20.0.
    let mut incoming_call = use_signal(|| None::<(i64, User)>);
    // What the volume sliders draw. LiveKit holds the real gain; without
    // this the slider would snap back to 100 on every re-render.
    let mut volumes = use_signal(HashMap::<String, i64>::new);
    // Search.
    let mut query = use_signal(String::new);
    let mut results = use_signal(Vec::<shared::SearchResult>::new);
    let mut searching = use_signal(|| false);
    // Edit profile.
    let mut status_draft = use_signal(String::new);
    let mut bio_draft = use_signal(String::new);
    let mut profile_saved = use_signal(|| false);
    let mut my_tags = use_signal(Vec::<shared::Tag>::new);
    // The music tab queues through its own field rather than the composer.
    let mut music_link = use_signal(String::new);
    // Password change, in Settings.
    let mut pw_current = use_signal(String::new);
    let mut pw_new = use_signal(String::new);
    let mut pw_confirm = use_signal(String::new);
    // (text, is_good) — one line under the button for either outcome.
    let mut pw_message = use_signal(|| (String::new(), false));
    // The GIF sheet.
    let mut gif_query = use_signal(String::new);
    let mut gif_results = use_signal(Vec::<shared::GifResult>::new);
    let mut gif_status = use_signal(String::new);
    let mut music = use_signal(MusicState::default);
    // user_id -> voice channel_id, for the 🔊 pills in the members panel.
    let mut voice_users = use_signal(HashMap::<i64, i64>::new);
    // The voice channel THIS device is connected to (id, name), plus the
    // glue's live view of the room.
    let mut voice_conn = use_signal(|| None::<(i64, String)>);
    let mut voice_glue = use_signal(VoiceGlue::default);
    let mut notify = use_signal(|| None::<shared::NotifyPrefs>);
    let mut push_glue = use_signal(PushGlue::default);
    let mut install_glue = use_signal(InstallGlue::default);
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
        if let Ok(state) = serde_json::from_str::<InstallGlue>(&install_state_js()) {
            install_glue.set(state);
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
    // channel_id -> unread count, for the channel-list and tab badges.
    // channel_id -> (unread count, id of the newest message you had read).
    // Every channel is kept, not just the noisy ones: the second number is
    // what draws the NEW divider, and it has to be known for a channel you
    // are about to open even when its count is zero right now.
    // channel_id -> (unread count, how many of those ping you, newest id
    // you had read). The middle number is what tells "someone said my
    // name" apart from "people were talking" — the server has always sent
    // it, and until now the phone threw it away.
    let mut unread = use_signal(HashMap::<i64, (i64, i64, i64)>::new);
    // The last-read id snapshotted the moment a channel opens — before the
    // open marks everything read and would erase it. None means no divider.
    let mut divider_at = use_signal(|| None::<i64>);
    // The message being replied to, shared with MessageRow via context.
    let replying = use_context_provider(|| Signal::new(None::<Message>));
    let mut replying = replying;
    let me_id = sess().user.id;
    let me_admin = sess().user.role == "admin";


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

    // Composer state for every channel that isn't the open one. The open
    // channel's draft lives in `draft`/`replying`; a switch swaps the two, so
    // a half-typed message can't follow you into somebody else's room and get
    // sent there by one stray tap. The reply target travels with the text or
    // you'd be left replying to a message in a channel you left.
    let mut drafts = use_signal(HashMap::<i64, (String, Option<Message>)>::new);

    // The message a permalink is pointing at, highlighted once we get there.
    let mut highlight = use_signal(|| None::<i64>);
    // A permalink that changed channels leaves its message id here. The jump
    // waits for the switch's own fetch to land: started any earlier, the two
    // race and you end up looking at whichever finished last.
    let mut pending_jump = use_signal(|| None::<i64>);

    // Scroll a message into view, loading the history around it first when it
    // isn't in the page we have. The phone has no pins or search-hit jumping
    // yet, so this exists for permalinks alone.
    let jump_to_message = move |channel_id: i64, message_id: i64| {
        spawn(async move {
            if !messages.peek().iter().any(|m| m.id == message_id) {
                // `before` is exclusive, so +1 puts the target at the end of
                // the page — the same trick the desktop's jump uses.
                if let Ok(msgs) = api::messages(&sess(), channel_id, Some(message_id + 1)).await {
                    has_more.set(msgs.len() == api::HISTORY_PAGE);
                    messages.set(msgs);
                }
            }
            highlight.set(Some(message_id));
            // The row may still be rendering. A couple of seconds of trying,
            // then give up quietly rather than spinning forever.
            for _ in 0..20 {
                if scroll_to_message(message_id) {
                    return;
                }
                gloo_timers::future::TimeoutFuture::new(100).await;
            }
        });
    };

    let mut open_channel = move |channel: Channel| {
        let id = channel.id;
        // Where you left off, taken now: the load below marks the channel
        // read, and after that the server no longer remembers.
        let previous = unread.peek().get(&id).copied();
        divider_at.set(previous.filter(|(count, _, _)| *count > 0).map(|(_, _, last_read)| last_read));
        // The badge clears at once; the last-read id is carried forward
        // until the load can replace it with something newer.
        unread.write().insert(id, (0, 0, previous.map(|(_, _, last_read)| last_read).unwrap_or(0)));
        // Park what's in the composer under the channel we're leaving, then
        // put back whatever this one was holding. Taken out of the map rather
        // than copied: the live signals are the open channel's draft.
        // Re-opening the channel you're already in is not a switch: leave the
        // composer alone rather than parking it and restoring nothing.
        let leaving = selected.peek().as_ref().map(|c: &Channel| c.id);
        if leaving != Some(id) {
            if let Some(leaving) = leaving {
                let text = draft.peek().clone();
                let reply = replying.peek().clone();
                if text.trim().is_empty() && reply.is_none() {
                    drafts.write().remove(&leaving);
                } else {
                    drafts.write().insert(leaving, (text, reply));
                }
            }
            let (text, reply) = drafts.write().remove(&id).unwrap_or_default();
            draft.set(text);
            replying.set(reply);
        }
        selected.set(Some(channel));
        overlay.set(Some("channel"));
        sheet.set(None);
        messages.set(Vec::new());
        highlight.set(None);
        loading_channel.set(true);
        spawn(async move {
            let loaded = api::messages(&sess(), id, None).await;
            // Only the channel still open gets to clear it: a slow fetch for
            // one you have already left must not say the next one is done.
            // And cleared after the list is set, not before, or "nothing
            // here yet" flashes between the two.
            let still_here = selected.peek().as_ref().map(|c: &Channel| c.id) == Some(id);
            match loaded {
                Ok(msgs) => {
                    has_more.set(msgs.len() == api::HISTORY_PAGE);
                    // The first message after the divider, worked out before
                    // the list is handed over and the ids move with it.
                    let first_unread = divider_at
                        .peek()
                        .and_then(|last_read| msgs.iter().map(|m| m.id).filter(|i| *i > last_read).min());
                    if let Some(newest) = msgs.last().map(|m| m.id) {
                        api::mark_read(&sess(), id, newest).await;
                        unread.write().insert(id, (0, 0, newest));
                    }
                    messages.set(msgs);
                    if still_here {
                        loading_channel.set(false);
                    }
                    if let Some(target) = pending_jump.write().take() {
                        jump_to_message(id, target);
                    } else if let Some(first) = first_unread {
                        // Land on where you left off, not on the newest
                        // message with forty unread somewhere above it. The
                        // row may still be rendering; same patience as a
                        // permalink jump, then give up quietly.
                        spawn(async move {
                            for _ in 0..20 {
                                if scroll_to_message(first) {
                                    return;
                                }
                                gloo_timers::future::TimeoutFuture::new(100).await;
                            }
                        });
                    }
                }
                Err(e) => {
                    status.set(e);
                    if still_here {
                        loading_channel.set(false);
                    }
                }
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
            // The list everything else hangs off. A few tries, because the
            // first request after Android restores the app can go out before
            // the connection is really back, and an empty Chats tab that never
            // fills is what that used to look like.
            for attempt in 0..5u32 {
                match api::channels(&sess()).await {
                    Ok(list) => {
                        channels.set(list);
                        break;
                    }
                    Err(e) if attempt == 4 => status.set(e),
                    Err(_) => gloo_timers::future::TimeoutFuture::new(1500).await,
                }
            }
            if let Ok(list) = api::users(&sess()).await {
                members.set(list);
            }
            if let Ok(list) = api::unread(&sess()).await {
                unread.set(list.into_iter().map(|u| (u.channel_id, (u.count, u.mentions, u.last_read_id))).collect());
            }
            if let Ok(list) = api::emojis(&sess()).await {
                emojis.set(list);
            }
            categories.set(api::categories(&sess()).await);
            if let Some(info) = api::server_info().await {
                server_name.set(info.name);
                public_url.set(info.public_url.filter(|u| !u.trim().is_empty()));
            }
            {
                let mut tags = tags;
                tags.set(api::tags(&sess()).await);
            }
            stickers.set(api::stickers(&sess()).await);
        });
    });

    // A link this tab was opened by, or one tapped inside it. Either way the
    // channel list has to be loaded first — the ids in a link mean nothing
    // until we know which channels this account can see.
    let mut open_message = use_signal(deep_linked_message);
    use_context_provider(|| OpenMessage(open_message));
    // The service worker opens `/app/?channel=12` when a push is tapped with
    // no window to hand it to, and nothing used to read it back.
    let mut open_channel_id = use_signal(deep_linked_channel);
    // Read once, at the first render — before the effect below has had a
    // chance to clear it for "no channel open yet".
    let mut resume_channel = use_signal(remembered_channel);

    // Keep the note current: a channel on screen is remembered, backing out
    // to the list forgets it, so a reload lands where you actually were.
    use_effect(move || match (overlay(), selected()) {
        (Some("channel"), Some(channel)) => remember_channel(Some(channel.id)),
        (None, _) => remember_channel(None),
        _ => {}
    });

    use_effect(move || {
        let linked = open_message().map(|(channel_id, _)| channel_id).or_else(|| open_channel_id());
        // A link someone tapped beats where we happened to be.
        let resuming = linked.is_none();
        let Some(channel_id) = linked.or_else(|| resume_channel()) else { return };
        let list = channels();
        if list.is_empty() {
            // Still loading, or an account with nothing to see. Either way
            // there is nothing to open yet; the next change re-runs this.
            return;
        }
        let target = open_message().map(|(_, message_id)| message_id);
        open_message.set(None);
        open_channel_id.set(None);
        resume_channel.set(None);
        forget_deep_link();
        let Some(channel) = list.iter().find(|c| c.id == channel_id).cloned() else {
            // A link to somewhere you can't see is worth saying. A remembered
            // channel that has since gone is not; the list is fine.
            if !resuming {
                status.set("that link points at a channel you can't see".into());
            }
            return;
        };
        match target {
            // Already here: no reload, just find the message.
            Some(message_id) if selected.peek().as_ref().map(|c: &Channel| c.id) == Some(channel_id) => {
                overlay.set(Some("channel"));
                jump_to_message(channel_id, message_id);
            }
            // `open_channel` loads the newest page and then hands the jump on,
            // which pages back to the message if it isn't in it.
            Some(message_id) => {
                pending_jump.set(Some(message_id));
                open_channel(channel);
            }
            None => open_channel(channel),
        }
    });

    // Everything that could have changed while we weren't listening. Called
    // when the socket comes back and when the app comes back to the
    // foreground — on a phone those are the same moment more often than
    // not, and either alone would leave the open channel showing what it
    // showed when you switched away.
    let resync = move || {
        spawn(async move {
            // The channel you are looking at, first.
            if *overlay.peek() == Some("channel") {
                if let Some(channel) = selected.peek().clone() {
                    if let Ok(msgs) = api::messages(&sess(), channel.id, None).await {
                        has_more.set(msgs.len() == api::HISTORY_PAGE);
                        if let Some(newest) = msgs.last().map(|m| m.id) {
                            api::mark_read(&sess(), channel.id, newest).await;
                            unread.write().insert(channel.id, (0, 0, newest));
                        }
                        messages.set(msgs);
                    }
                }
            }
            // The channel list too: it is fetched once at boot, and if that
            // one request failed — a phone coming back with the radio not
            // quite up yet — the Chats tab stayed empty until a manual reload
            // while everything else recovered (Jon).
            if let Ok(list) = api::channels(&sess()).await {
                channels.set(list);
            }
            // Then the badges and who's around, which drift the same way.
            if let Ok(list) = api::unread(&sess()).await {
                unread.set(list.into_iter().map(|u| (u.channel_id, (u.count, u.mentions, u.last_read_id))).collect());
            }
            if let Ok(list) = api::users(&sess()).await {
                members.set(list);
            }
        });
    };

    // Back to the foreground. The socket usually died while we were away
    // and its reconnect resyncs; this covers the times it didn't die and
    // simply missed things, and it is cheap. Polled, like everything else
    // here, rather than a Closure wired into addEventListener.
    use_future(move || async move {
        let mut was_hidden = false;
        let mut today = local_day(now_ms());
        loop {
            gloo_timers::future::TimeoutFuture::new(1000).await;
            let hidden = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| d.hidden())
                .unwrap_or(false);
            if was_hidden && !hidden {
                resync();
            }
            was_hidden = hidden;
            // The date turned over — at midnight, or a phone waking up on
            // another day. Checked, not scheduled, for the second reason.
            let date = local_day(now_ms());
            if date != today {
                today = date;
                let n = *day_tick.peek();
                day_tick.set(n.wrapping_add(1));
            }
            // A request came back 401 somewhere. That is the server saying
            // this token is not a session any more — the socket may never
            // have noticed, having been asleep through the whole thing.
            if api::session_died() {
                signed_out(session, "You were signed out. Your password may have been changed on another device.");
                return;
            }
        }
    });

    // A toast that has sat unchanged for four seconds has been read. This
    // is one loop rather than a timer at each of the twenty places that set
    // one, and a message that changes restarts the clock.
    use_future(move || async move {
        let mut seen: (String, i64) = (String::new(), 0);
        loop {
            gloo_timers::future::TimeoutFuture::new(500).await;
            let current = status.peek().clone();
            if current.is_empty() {
                continue;
            }
            if current != seen.0 {
                seen = (current, now_ms());
            } else if now_ms() - seen.1 >= 4000 {
                status.set(String::new());
            }
        }
    });

    // Nobody sends a "stopped typing" event, so indicators expire on a timer.
    use_future(move || async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(1000).await;
            let now = now_ms();
            if typing.peek().values().any(|(_, _, expiry)| *expiry < now) {
                typing.write().retain(|_, (_, _, expiry)| *expiry >= now);
            }
            // A send that hasn't come back by now probably never will: the
            // socket ignores write errors, so one dequeued as the connection
            // died is gone without a word. Say so instead of leaving a ghost
            // sitting there forever.
            let overdue: Vec<i64> = pending_at
                .peek()
                .iter()
                .filter(|(id, at)| now - **at > SEND_TIMEOUT_MS && !failed_sends.peek().contains(id))
                .map(|(id, _)| *id)
                .collect();
            if !overdue.is_empty() {
                failed_sends.write().extend(overdue);
            }
        }
    });

    // WebSocket: outgoing ClientEvents in, ServerEvents applied to signals.
    let ws = use_coroutine(move |mut rx: UnboundedReceiver<ClientEvent>| async move {
        // Anything the socket refused. Without this, an event pulled off the
        // queue just as the connection died was dropped with `let _ =` and
        // never mentioned again — a message you watched disappear.
        let mut outbox: Vec<ClientEvent> = Vec::new();
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
            // getTimezoneOffset is minutes BEHIND UTC, so negate it to get
            // "minutes to add to UTC", which is what the server stores.
            let tz = -js_sys::Date::new_0().get_timezone_offset() as i64;
            let url = format!("{proto}://{host}/ws?token={}&tz={tz}", sess().token);
            let Ok(socket) = gloo_net::websocket::futures::WebSocket::open(&url) else {
                gloo_timers::future::TimeoutFuture::new(3000).await;
                continue;
            };
            // open() returning Ok means the handshake has STARTED, nothing
            // more — a dead server takes seconds to say no, and clearing the
            // pill here left it off for most of every retry. It clears when
            // the first frame arrives, below.
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
            // Whatever didn't make it last time goes first, in order.
            let mut requeue = Vec::new();
            for event in outbox.drain(..) {
                match serde_json::to_string(&event) {
                    Ok(text) => {
                        if sink.send(gloo_net::websocket::Message::Text(text)).await.is_err() {
                            requeue.push(event);
                        }
                    }
                    Err(_) => {}
                }
            }
            outbox = requeue;

            // select! needs fused streams; the coroutine receiver already is.
            let mut stream = stream.fuse();
            loop {
                futures_util::select! {
                    out = rx.next() => {
                        let Some(event) = out else { return };
                        if let Ok(text) = serde_json::to_string(&event) {
                            if sink.send(gloo_net::websocket::Message::Text(text)).await.is_err() {
                                // Hold it and reconnect rather than losing it.
                                outbox.push(event);
                                break;
                            }
                        }
                    }
                    incoming = stream.next() => {
                        let Some(Ok(gloo_net::websocket::Message::Text(text))) = incoming else {
                            break;
                        };
                        let Ok(event) = serde_json::from_str::<ServerEvent>(&text) else {
                            continue;
                        };
                        // A frame from the server is the proof the socket is
                        // real. If we had been offline, this is the moment
                        // the pill goes and the catch-up starts; the first
                        // connect was never offline, so it does neither.
                        if *offline.peek() {
                            offline.set(false);
                            resync();
                        }
                        match event {
                            ServerEvent::MessageCreated { message } => {
                                typing.write().remove(&message.author.id);
                                // Our own message coming back: drop the ghost
                                // it confirms. Matched on content rather than
                                // an id the server doesn't know about — two
                                // identical messages resolve against either
                                // echo, which is indistinguishable anyway.
                                if message.author.id == sess().user.id {
                                    let mut list = messages.write();
                                    if let Some(at) = list
                                        .iter()
                                        .position(|m| m.id < 0 && m.content == message.content)
                                    {
                                        let ghost = list[at].id;
                                        list.remove(at);
                                        drop(list);
                                        pending_at.write().remove(&ghost);
                                        failed_sends.write().remove(&ghost);
                                    }
                                }
                                if selected.peek().as_ref().map(|c| c.id) == Some(message.channel_id) {
                                    let (id, chan) = (message.id, message.channel_id);
                                    // A resync can fetch a message whose echo
                                    // is still on its way; one copy is plenty.
                                    if messages.peek().iter().any(|m| m.id == id) {
                                        continue;
                                    }
                                    messages.write().push(message);
                                    // Read the moment it lands, and remembered
                                    // as read, so leaving and coming back
                                    // doesn't draw a divider above it.
                                    unread.write().insert(chan, (0, 0, id));
                                    spawn(async move { api::mark_read(&sess(), chan, id).await });
                                } else if message.author.id != me_id {
                                    // Does this one ping me? The same rule the
                                    // desktop and the server's push use: any
                                    // DM, @everyone, or my name.
                                    let is_dm = channels.peek().iter().any(|c| c.id == message.channel_id && c.kind == "dm");
                                    let lower = message.content.to_lowercase();
                                    let pings_me = is_dm
                                        || lower.contains("@everyone")
                                        || lower.contains(&format!("@{}", sess().user.username.to_lowercase()));
                                    // A channel never seen before starts from
                                    // 0: everything in it is new, which is
                                    // the truth.
                                    let mut map = unread.write();
                                    let entry = map.entry(message.channel_id).or_insert((0, 0, 0));
                                    entry.0 += 1;
                                    if pings_me {
                                        entry.1 += 1;
                                    }
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
                                // A draft for a channel that no longer exists
                                // has nowhere to be sent.
                                drafts.write().remove(&channel_id);
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
                            ServerEvent::PresenceModeChanged { user_id, mode } => {
                                if let Some(m) = members.write().iter_mut().find(|m| m.user.id == user_id) {
                                    m.presence = mode;
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
                                        // Ring on an incoming DM call: a room of a
                                        // DM I'm in, joined by somebody who isn't
                                        // me, while I'm not in it. Same rule as the
                                        // desktop, so the two never disagree about
                                        // what a call is.
                                        let is_my_dm = channels.peek().iter().any(|c| c.id == id && c.kind == "dm");
                                        let already_there = voice_conn.peek().as_ref().map(|(c, _)| *c) == Some(id);
                                        if is_my_dm && user.id != me_id && !already_there {
                                            incoming_call.set(Some((id, user.clone())));
                                            buzz();
                                        }
                                    }
                                    None => {
                                        voice_users.write().remove(&user.id);
                                        // The caller gave up before you answered:
                                        // the ring must not outlive the call.
                                        if incoming_call.peek().as_ref().is_some_and(|(_, who)| who.id == user.id) {
                                            incoming_call.set(None);
                                        }
                                    }
                                }
                            }
                            ServerEvent::Typing { channel_id, user } => {
                                // Your own keystrokes are not news to you.
                                if user.id != sess().user.id {
                                    typing.write().insert(
                                        user.id,
                                        (channel_id, user.username, now_ms() + TYPING_TTL_MS),
                                    );
                                }
                            }
                            ServerEvent::MessagePinChanged { message_id, pinned, .. } => {
                                for list in messages.write().iter_mut() {
                                    if list.id == message_id {
                                        list.pinned = pinned;
                                    }
                                }
                            }
                            ServerEvent::ServerRenamed { name } => server_name.set(name),
                            ServerEvent::Error { message } => status.set(message),
                            // The server has stopped honouring this token and
                            // is about to close the socket. Going quietly is
                            // wrong: say what happened, on the screen that
                            // asks for the password again.
                            ServerEvent::SignedOut { reason } => {
                                signed_out(session, &reason);
                                return;
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
                            ServerEvent::TagsChanged => {
                                spawn(async move {
                                    let mut tags = tags;
                                    tags.set(api::tags(&sess()).await);
                                });
                            }
                            ServerEvent::CategoriesChanged => {
                                spawn(async move {
                                    categories.set(api::categories(&sess()).await);
                                });
                            }
                            ServerEvent::EmojisChanged => {
                                spawn(async move {
                                    if let Ok(list) = api::emojis(&sess()).await {
                                        emojis.set(list);
                                    }
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Dropped: reconnect after a beat.
            offline.set(true);
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
            // One re-render a second, and only while there is a call.
            let since = *call_since.peek();
            if since > 0 {
                let elapsed = ((now_ms() - since) / 1000).max(0);
                if elapsed != *call_secs.peek() {
                    call_secs.set(elapsed);
                }
            }
            if dropped {
                voice_conn.set(None);
                call_open.set(false);
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
                        // A DM's stored name is "dm:2:5"; what you're in a
                        // call with is a person.
                        let label = if channel.kind == "dm" {
                            format!("@{}", dm_peer(&channel, me_id))
                        } else {
                            channel.name.clone()
                        };
                        voice_conn.set(Some((channel.id, label)));
                        // Answering, or calling: either way there's nothing
                        // left to ring about.
                        incoming_call.set(None);
                        // Straight into the call screen, and start the clock.
                        call_since.set(now_ms());
                        call_secs.set(0);
                        call_open.set(true);
                        overlay.set(None);
                        volumes.set(HashMap::new());
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
            call_open.set(false);
            call_secs.set(0);
            ws.send(ClientEvent::VoiceState { channel_id: None, sharing: false, camera: false });
        });
    };

    // The box follows the draft wherever it changes — typing, the send
    // clearing it, a mention completing, a channel switch putting one back.
    // Runs after the render, so the element already holds the new text.
    use_effect(move || {
        autosize_composer(draft().is_empty());
    });

    let mut send = move |_| {
        let content = draft().trim().to_owned();
        let Some(channel) = selected() else { return };
        if content.is_empty() {
            return;
        }
        draft.set(String::new());
        let reply_to = replying.peek().as_ref().map(|m| m.id);
        let reply_preview = replying.peek().as_ref().map(|m| shared::ReplyPreview {
            author: m.author.username.clone(),
            content: m.content.clone(),
        });
        replying.set(None);

        // Put it on screen now. On a bad connection the socket may take
        // seconds to get this out, and staring at an empty channel wondering
        // whether it sent is the thing being fixed.
        let temp_id = next_temp_id();
        next_temp_id.set(temp_id - 1);
        messages.write().push(Message {
            id: temp_id,
            channel_id: channel.id,
            author: sess().user.clone(),
            content: content.clone(),
            created_at: now_ms(),
            edited_at: None,
            reactions: Vec::new(),
            reply_to,
            reply_preview,
            pinned: false,
            // The echo brings the sizes; an upload you just sent is in the
            // browser's cache anyway, so there is nothing to wait for.
            media: Vec::new(),
        });
        pending_at.write().insert(temp_id, now_ms());
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

    // One call for "open the sheet" (empty query = trending) and for the
    // search button, so the sheet is never blank and the two can't drift.
    let search_gifs = move |query: String| {
        spawn(async move {
            gif_status.set("searching…".into());
            match api::gifs(&sess(), &query).await {
                Ok(list) => {
                    gif_status.set(if list.is_empty() { "nothing for that".into() } else { String::new() });
                    gif_results.set(list);
                }
                // The server says plainly when GIPHY isn't set up; show it.
                Err(e) => gif_status.set(e),
            }
        });
    };

    let mut run_search = move |_| {
        let q = query().trim().to_owned();
        if q.is_empty() {
            results.set(Vec::new());
            return;
        }
        searching.set(true);
        spawn(async move {
            match api::search(&sess(), &q).await {
                Ok(hits) => results.set(hits),
                Err(e) => status.set(e),
            }
            searching.set(false);
        });
    };

    // The bio and the tags that colour your name both live on the profile
    // endpoint, so the edit screen reads it once on the way in.
    let load_profile = move || {
        spawn(async move {
            if let Ok(profile) = api::my_profile(&sess(), me_id).await {
                bio_draft.set(profile.bio);
                my_tags.set(profile.tags);
            }
        });
    };

    let save_profile = move |_| {
        let status_text = status_draft();
        let bio = bio_draft();
        spawn(async move {
            if let Err(e) = api::set_status(&sess(), &status_text).await {
                status.set(e);
                return;
            }
            if let Err(e) = api::update_profile(&sess(), None, Some(bio)).await {
                status.set(e);
                return;
            }
            profile_saved.set(true);
            // The roster is what draws your status everywhere else.
            if let Ok(list) = api::users(&sess()).await {
                members.set(list);
            }
        });
    };

    let mut queue_link = move |_| {
        let url = music_link().trim().to_owned();
        if url.is_empty() {
            return;
        }
        // The bot announces what it queued in a text channel, so it needs
        // one: the room you last had open, else the first on the server.
        let Some(channel_id) = selected()
            .filter(|c| c.kind == "text")
            .map(|c| c.id)
            .or_else(|| channels().iter().find(|c| c.kind == "text").map(|c| c.id))
        else {
            status.set("no text channel for the bot to post in".into());
            return;
        };
        music_link.set(String::new());
        spawn(async move {
            if let Err(e) = api::music_play(&sess(), channel_id, url).await {
                status.set(e);
            }
            if let Ok(s) = api::music_state(&sess()).await {
                music.set(s);
            }
        });
    };

    // Picking a file no longer sends it. It goes to a preview sheet with a
    // Send button and a progress bar. It used to upload straight away behind
    // a "…" on the plus button, and a big photo on mobile data spent ten
    // seconds there looking exactly like "did not send" — right up until it
    // appeared (Jon, twice, and once from the camera). The three inputs
    // differ only in their accept filter; the glue takes the File out of
    // whichever fired.
    let mut stage_file = move |input_id: &'static str| {
        let json = upload_stage_js(input_id);
        match serde_json::from_str::<StagedFile>(&json) {
            Ok(file) => {
                staged.set(Some(file));
                upload_pct.set(0.0);
                sheet.set(Some("preview"));
            }
            // Nothing picked: the dialog was cancelled. Back to chat.
            Err(_) => sheet.set(None),
        }
    };

    let mut discard_staged = move || {
        upload_discard_js();
        staged.set(None);
        upload_pct.set(0.0);
    };

    let mut send_staged = move |_| {
        if uploading() {
            return;
        }
        let Some(file) = staged() else { return };
        let Some(channel) = selected.peek().clone() else { return };
        uploading.set(true);
        spawn(async move {
            // Progress is polled off the glue, in the house style: a number
            // read every 100ms, no Closure to wire up or tear down.
            let ticker = spawn(async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(100).await;
                    upload_pct.set(upload_progress_js());
                }
            });
            let name = js_sys::encode_uri_component(&file.name).as_string().unwrap_or_default();
            let url = format!("/api/upload?name={name}");
            let result = wasm_bindgen_futures::JsFuture::from(upload_send_js(&url, &sess().token)).await;
            ticker.cancel();
            match result {
                Ok(body) => {
                    let body = body.as_string().unwrap_or_default();
                    match serde_json::from_str::<shared::UploadResponse>(&body) {
                        Ok(out) => {
                            ws.send(ClientEvent::SendMessage {
                                channel_id: channel.id,
                                content: out.url,
                                reply_to: None,
                            });
                            discard_staged();
                            sheet.set(None);
                        }
                        Err(_) => status.set("the server answered something odd".into()),
                    }
                }
                // The file is still staged: the sheet stays up and Send
                // tries again, rather than making you find the photo twice.
                // A cancel rejects too, and is not news to the person who did it.
                Err(e) => {
                    let message = e.as_string().unwrap_or_else(|| "upload failed".into());
                    if message != "cancelled" {
                        status.set(message);
                    }
                }
            }
            uploading.set(false);
        });
    };

    let mic_icon: &'static str = if voice_glue().muted { "mic-off" } else { "mic" };
    let deafen_icon: &'static str = if voice_glue().deafened { "headphones-off" } else { "headphones" };

    // Values the shell reads more than once. Computed here rather than inside
    // the macro so the four tabs stay readable.
    let over = overlay();
    let in_call = voice_conn().is_some();
    let total_unread: i64 = unread().values().map(|(count, _, _)| *count).sum();
    // Whether any of it is aimed at me: the tab badge changes colour on this.
    // Gated on there being something unread at all — the server reports a
    // DM as "mentions: 1" whether or not you have read it, so on its own
    // that number would keep the tab lit after every DM was read.
    let any_mention = unread().values().any(|(count, mentions, _)| *count > 0 && *mentions > 0);
    let online_now = members().iter().filter(|m| m.online).count();
    let upload_percent = (upload_pct() * 100.0).round() as i64;
    // DMs newest-first: the conversation you're actually having belongs at
    // the top, which is what `last_at` is for.
    let dm_list = {
        let mut list: Vec<Channel> = channels().into_iter().filter(|c| c.kind == "dm").collect();
        list.sort_by(|a, b| b.last_at.unwrap_or(0).cmp(&a.last_at.unwrap_or(0)));
        list
    };
    let voice_list: Vec<Channel> = channels().into_iter().filter(|c| c.kind == "voice").collect();
    let call_name = voice_conn().map(|(_, name)| name).unwrap_or_default();
    // Everyone in the room but you — the line under the call's name.
    let call_others: Vec<String> =
        voice_glue().participants.iter().filter(|p| !p.local).map(|p| p.name.clone()).collect();
    let call_peers_line = if call_others.is_empty() {
        "just you".to_string()
    } else {
        format!("{} + you", call_others.join(", "))
    };
    let call_clock = format!("{}:{:02}", call_secs() / 60, call_secs() % 60);
    let selected_id = selected().map(|c| c.id);
    let chan_is_dm = selected().is_some_and(|c| c.kind == "dm");
    let chan_title = match selected() {
        Some(c) if c.kind == "dm" => dm_peer(&c, me_id),
        Some(c) => format!("# {}", c.name),
        None => String::new(),
    };
    // A channel has no topic field, so for a DM the second line says the
    // thing this app does know: whether they're around, and what they're up
    // to if they've said. "direct message" used to sit here, which told you
    // only what the screen you were already looking at had told you.
    let chan_topic = match selected() {
        Some(c) if c.kind == "dm" => {
            let peer = dm_peer(&c, me_id);
            let entry = members().into_iter().find(|m| m.user.username == peer);
            let online = entry.as_ref().is_some_and(|m| m.online);
            let mode = entry.as_ref().map(|m| m.presence.clone()).unwrap_or_default();
            presence_line(online, &mode, entry.and_then(|m| m.status))
        }
        _ => String::new(),
    };
    let notify_prefs = notify();
    let notify_on =
        notify_prefs.as_ref().is_some_and(|p| p.subscribed) && push_glue().permission == "granted";
    let notify_label = if !notify_on {
        "off".to_string()
    } else {
        match notify_prefs.as_ref().map(|p| p.level.clone()).unwrap_or_default().as_str() {
            "all" => "everything".to_string(),
            "none" => "muted".to_string(),
            _ => "mentions & DMs".to_string(),
        }
    };
    // Your own mode, read from the roster so it stays right after somebody
    // else's client changes it — the server tells everyone, including you.
    let my_presence = members()
        .iter()
        .find(|m| m.user.id == me_id)
        .map(|m| m.presence.clone())
        .unwrap_or_else(|| "online".to_string());
    let armed = !draft().trim().is_empty();

    rsx! {
        div { class: "app",
            // Connection state is a small pill near the top that covers
            // nothing; a one-off message is a toast at the bottom that goes
            // away by itself. Both float over everything, or the first
            // overlay to open would hide them.
            if offline() {
                div { class: "conn-pill", "reconnecting…" }
            }
            if !status().is_empty() {
                div { class: "toast", onclick: move |_| status.set(String::new()), "{status}" }
            }

            div { class: "app-body",

                // ---------------- Chats ----------------
                if tab() == "chats" && over.is_none() {
                    div { class: "screen",
                        div { class: "screen-head tight",
                            div { class: "grow",
                                div { class: "screen-title",
                                    if server_name().is_empty() { "NotDiscord" } else { "{server_name}" }
                                }
                                div { class: "screen-sub", "{online_now} online · self-hosted" }
                            }
                            button {
                                class: "hbtn",
                                aria_label: "Search",
                                onclick: move |_| {
                                    query.set(String::new());
                                    results.set(Vec::new());
                                    overlay.set(Some("search"));
                                },
                                Icon { name: "search", size: 19 }
                            }
                            button {
                                class: "hbtn",
                                aria_label: "Members",
                                onclick: move |_| sheet.set(Some("members")),
                                Icon { name: "user", size: 19 }
                            }
                        }
                        div { class: "scroll grow", style: "padding: 4px 10px 14px",
                            // Ungrouped channels first, then each category —
                            // the same order the desktop shows, so the two
                            // don't disagree about where a channel lives.
                            for group in std::iter::once(None).chain(categories().into_iter().map(Some)) {
                                {
                                    let group_id = group.as_ref().map(|c: &shared::ChannelCategory| c.id);
                                    let in_group: Vec<Channel> = channels()
                                        .into_iter()
                                        .filter(|c| c.kind == "text" && c.category_id == group_id)
                                        .collect();
                                    let shut = group_id.is_some_and(|id| collapsed().contains(&id));
                                    rsx! {
                                        if let Some(category) = group.clone() {
                                            {
                                                let id = category.id;
                                                let chevron: &'static str =
                                                    if shut { "chevron-down" } else { "chevron-up" };
                                                rsx! {
                                                    button {
                                                        class: "section-label group-btn",
                                                        onclick: move |_| {
                                                            let mut list = collapsed();
                                                            match list.iter().position(|c| *c == id) {
                                                                Some(at) => { list.remove(at); }
                                                                None => list.push(id),
                                                            }
                                                            let _ = gloo_storage::LocalStorage::set(COLLAPSED_KEY, &list);
                                                            collapsed.set(list);
                                                        },
                                                        span { class: "grow", "{category.name}" }
                                                        Icon { name: chevron, size: 12 }
                                                    }
                                                }
                                            }
                                        } else if !in_group.is_empty() {
                                            div { class: "section-label", "Text" }
                                        }
                                        // Shut hides the quiet ones. Anything
                                        // unread, or the channel you're in,
                                        // stays put.
                                        for channel in in_group.into_iter().filter(|c| {
                                            !shut || unread().get(&c.id).is_some_and(|(n, _, _)| *n > 0) || selected_id == Some(c.id)
                                        }) {
                                            button {
                                                key: "{channel.id}",
                                                class: if unread().get(&channel.id).is_some_and(|(n, _, _)| *n > 0) { "chan-row unread" } else { "chan-row" },
                                                onclick: {
                                                    let channel = channel.clone();
                                                    move |_| open_channel(channel.clone())
                                                },
                                                span { class: "chan-hash", "#" }
                                                span { class: "chan-name ellipsis", "{channel.name}" }
                                                if let Some((n, m)) = unread().get(&channel.id).map(|(n, m, _)| (*n, *m)).filter(|(n, _)| *n > 0) {
                                                    span { class: if m > 0 { "badge mention" } else { "badge" }, "{n}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            div { class: "section-label", "Direct messages" }
                            if dm_list.is_empty() {
                                div { class: "empty-note",
                                    "No DMs yet. Open Members from the header and message someone."
                                }
                            }
                            for channel in dm_list {
                                {
                                    let peer_name = dm_peer(&channel, me_id);
                                    let peer = channel.dm_members.iter().find(|u| u.id != me_id).cloned();
                                    let roster = members().into_iter().find(|m| m.user.username == peer_name);
                                    let online = roster.as_ref().is_some_and(|m| m.online);
                                    let mode = roster.as_ref().map(|m| m.presence.clone()).unwrap_or_default();
                                    // The app has no last-message preview, so
                                    // the second line carries presence, plus
                                    // their status when they've set one. In
                                    // words, not just the dot's colour — and
                                    // the row is never blank, which it was for
                                    // anyone who'd never set a status.
                                    let sub = presence_line(online, &mode, roster.and_then(|m| m.status));
                                    let unread_here = unread().get(&channel.id).map(|(n, m, _)| (*n, *m)).filter(|(n, _)| *n > 0);
                                    let for_open = channel.clone();
                                    rsx! {
                                        button {
                                            key: "d{channel.id}",
                                            class: "dm-row",
                                            onclick: move |_| open_channel(for_open.clone()),
                                            if let Some(user) = peer {
                                                span { class: "avatar-wrap",
                                                    Avatar { user }
                                                    // Reinforces the words above; the
                                                    // line underneath is what actually
                                                    // says it, so this is decoration.
                                                    span {
                                                        class: presence_dot(online, &mode),
                                                        aria_hidden: "true",
                                                    }
                                                }
                                            }
                                            span { class: "dm-col",
                                                span { class: "dm-name", "{peer_name}" }
                                                span { class: "dm-sub ellipsis", "{sub}" }
                                            }
                                            if let Some((n, m)) = unread_here {
                                                span { class: if m > 0 { "badge mention" } else { "badge" }, "{n}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ---------------- Voice ----------------
                if tab() == "voice" && over.is_none() {
                    div { class: "screen",
                        div { class: "screen-head tight",
                            div { class: "grow",
                                div { class: "screen-title", "Voice" }
                                div { class: "screen-sub",
                                    if in_call { "you're in {call_name}" } else { "nobody's waiting for you" }
                                }
                            }
                        }
                        div { class: "scroll grow",
                            style: "padding: 4px 12px 14px; display: flex; flex-direction: column; gap: 10px",
                            if voice_list.is_empty() {
                                div { class: "empty-note", "No voice channels on this server yet." }
                            }
                            for channel in voice_list {
                                {
                                    let here = voice_conn().map(|(id, _)| id) == Some(channel.id);
                                    let people: Vec<UserStatus> = voice_users()
                                        .iter()
                                        .filter(|(_, chan)| **chan == channel.id)
                                        .filter_map(|(uid, _)| {
                                            members().iter().find(|m| m.user.id == *uid).cloned()
                                        })
                                        .collect();
                                    let count = if people.is_empty() {
                                        "empty".to_string()
                                    } else {
                                        format!("{} here", people.len())
                                    };
                                    let for_join = channel.clone();
                                    // Only this device knows its own mic state.
                                    let muted_here = voice_glue().muted;
                                    rsx! {
                                        div {
                                            key: "v{channel.id}",
                                            class: if here { "voice-card here" } else { "voice-card" },
                                            button {
                                                class: "voice-card-head",
                                                onclick: move |_| {
                                                    if voice_conn.peek().as_ref().map(|(id, _)| *id) == Some(for_join.id) {
                                                        // Already in it: show the call rather
                                                        // than reconnecting on top of yourself.
                                                        call_open.set(true);
                                                    } else {
                                                        join_voice(for_join.clone());
                                                    }
                                                },
                                                span { class: "voice-card-icon", Icon { name: "volume", size: 18 } }
                                                span { class: "voice-card-name ellipsis", "{channel.name}" }
                                                span { class: "voice-card-count", "{count}" }
                                            }
                                            if !people.is_empty() {
                                                div { class: "voice-people",
                                                    for person in people {
                                                        div { key: "p{person.user.id}", class: "voice-person",
                                                            {
                                                                let colour = name_color(person.user.id, &members(), &tags());
                                                                rsx! {
                                                                    Avatar { user: person.user.clone() }
                                                                    span {
                                                                        class: "voice-person-name ellipsis",
                                                                        style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                                        "{person.user.username}"
                                                                    }
                                                                }
                                                            }
                                                            {
                                                                // Everyone else reads as open: the
                                                                // server doesn't publish mic state.
                                                                let off = person.user.id == me_id && muted_here;
                                                                let icon: &'static str = if off { "mic-off" } else { "mic" };
                                                                rsx! {
                                                                    span {
                                                                        class: if off { "mic off" } else { "mic" },
                                                                        Icon { name: icon, size: 15 }
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
                            div { class: "empty-note",
                                "Screen share and webcam are desktop-only for now. On the phone you get voice, and you can watch someone else's share."
                            }
                        }
                    }
                }

                // ---------------- Music ----------------
                if tab() == "music" && over.is_none() {
                    div { class: "screen",
                        div { class: "screen-head tight",
                            div { class: "grow",
                                div { class: "screen-title", "Music" }
                                div { class: "screen-sub", "you're the remote" }
                            }
                        }
                        div { class: "scroll grow", style: "padding: 4px 12px 14px",
                            {
                                let state = music();
                                let play_icon: &'static str = if state.paused { "play" } else { "pause" };
                                let track = state.now_playing.clone();
                                let duration = track.as_ref().and_then(|t| t.duration).unwrap_or(0.0);
                                let pct = if duration > 0.0 {
                                    (state.position / duration * 100.0).clamp(0.0, 100.0)
                                } else {
                                    0.0
                                };
                                let elapsed = format!(
                                    "{}:{:02}",
                                    state.position as i64 / 60,
                                    state.position as i64 % 60,
                                );
                                let total = if duration > 0.0 {
                                    format!("{}:{:02}", duration as i64 / 60, duration as i64 % 60)
                                } else {
                                    "--:--".to_string()
                                };
                                rsx! {
                                    div { class: "np-card",
                                        div { class: "np-top",
                                            if let Some(art) = track.as_ref().and_then(|t| t.art.clone()) {
                                                img { class: "np-art", src: "{art}" }
                                            } else {
                                                div { class: "np-art", Icon { name: "music", size: 26 } }
                                            }
                                            div { class: "np-text",
                                                if let Some(track) = track.clone() {
                                                    div { class: "np-title", "{track.title}" }
                                                    if !track.artist.is_empty() {
                                                        div { class: "np-artist", "{track.artist}" }
                                                    }
                                                } else {
                                                    div { class: "np-empty",
                                                        "Nothing playing. Paste a link below and the bot picks it up."
                                                    }
                                                }
                                            }
                                        }
                                        if track.is_some() {
                                            div {
                                                div { class: "np-progress",
                                                    div { class: "np-progress-fill", style: "width: {pct}%" }
                                                }
                                                div { class: "np-times",
                                                    span { "{elapsed}" }
                                                    span { "{total}" }
                                                }
                                            }
                                        }
                                        div { class: "music-controls",
                                            button {
                                                class: "mbtn primary",
                                                aria_label: "Play or pause",
                                                onclick: move |_| {
                                                    let action = if music.peek().paused { "resume" } else { "pause" };
                                                    spawn(async move {
                                                        let _ = api::music_control(&sess(), action).await;
                                                        if let Ok(s) = api::music_state(&sess()).await {
                                                            music.set(s);
                                                        }
                                                    });
                                                },
                                                Icon { name: play_icon, size: 18 }
                                            }
                                            button {
                                                class: "mbtn",
                                                aria_label: "Skip",
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
                                                aria_label: "Stop",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let _ = api::music_control(&sess(), "stop").await;
                                                        if let Ok(s) = api::music_state(&sess()).await {
                                                            music.set(s);
                                                        }
                                                    });
                                                },
                                                Icon { name: "stop", size: 16 }
                                            }
                                        }
                                    }

                                    div { class: "queue-head",
                                        span { class: "section-label grow", style: "padding: 0", "Up next" }
                                        button {
                                            class: "queue-shuffle",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    let _ = api::music_queue(&sess(), shared::MusicQueueRequest {
                                                        action: "shuffle".into(),
                                                        id: None,
                                                        offset: None,
                                                        ids: Vec::new(),
                                                    }).await;
                                                    if let Ok(s) = api::music_state(&sess()).await {
                                                        music.set(s);
                                                    }
                                                });
                                            },
                                            Icon { name: "shuffle", size: 12 }
                                            "Shuffle"
                                        }
                                    }
                                    div { style: "display: flex; flex-direction: column; gap: 6px",
                                        if state.queue.is_empty() {
                                            div { class: "np-empty", "queue's empty" }
                                        }
                                        for (n, track) in state.queue.clone().into_iter().enumerate() {
                                            div { key: "{track.id}", class: "queue-row",
                                                span { class: "queue-n", "{n + 1}" }
                                                span { class: "queue-title ellipsis", "{track.title}" }
                                                button {
                                                    class: "queue-x",
                                                    aria_label: "Remove",
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
                            // Queueing has its own field now. It used to ride
                            // on the composer, which meant the music tab and
                            // the channel you were reading shared one box.
                            div { class: "queue-add",
                                input {
                                    class: "text-input",
                                    placeholder: "paste a link to queue",
                                    value: "{music_link}",
                                    oninput: move |e| music_link.set(e.value()),
                                    onkeydown: move |e| {
                                        if e.key() == Key::Enter {
                                            queue_link(());
                                        }
                                    },
                                }
                                button {
                                    class: "btn btn-primary",
                                    onclick: move |_| queue_link(()),
                                    "Queue"
                                }
                            }
                        }
                    }
                }

                // ---------------- You ----------------
                if tab() == "you" && over.is_none() {
                    div { class: "screen",
                        div { class: "scroll grow", style: "padding: 18px 16px 14px; padding-top: calc(18px + var(--safe-top))",
                            div { class: "you-head",
                                {
                                    let me = sess().user.clone();
                                    let colour = name_color(me_id, &members(), &tags());
                                    let my_status = members()
                                        .iter()
                                        .find(|m| m.user.id == me_id)
                                        .and_then(|m| m.status.clone())
                                        .unwrap_or_default();
                                    rsx! {
                                        span { class: "avatar-wrap",
                                            Avatar { user: me.clone(), variant: "big" }
                                            span {
                                                class: "{presence_dot(true, &my_presence)} big",
                                                aria_hidden: "true",
                                            }
                                        }
                                        div { class: "grow",
                                            div { style: "display: flex; align-items: center; gap: 7px",
                                                span {
                                                    class: "you-name",
                                                    style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                    "{me.username}"
                                                }
                                                if me.role == "admin" {
                                                    span { class: "tag-badge", "ADMIN" }
                                                }
                                            }
                                            {
                                                let label = shared::presence_label(&my_presence);
                                                rsx! {
                                                    if my_status.is_empty() {
                                                        div { class: "you-status", "{label}" }
                                                    } else {
                                                        div { class: "you-status ellipsis", "{label} · {my_status}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // Four taps, no menu: the whole point of do not
                            // disturb is reaching it the moment you need it.
                            div { class: "presence-picker",
                                for mode in shared::PRESENCE_MODES {
                                    button {
                                        key: "pm-{mode}",
                                        class: if my_presence == mode { "presence-chip on" } else { "presence-chip" },
                                        onclick: move |_| {
                                            spawn(async move {
                                                if let Err(e) = api::set_presence(&sess(), mode).await {
                                                    status.set(e);
                                                    return;
                                                }
                                                // The roster is what draws it
                                                // everywhere else, your own
                                                // row included.
                                                if let Ok(list) = api::users(&sess()).await {
                                                    members.set(list);
                                                }
                                            });
                                        },
                                        span { class: "presence {mode}", aria_hidden: "true" }
                                        span { "{shared::presence_label(mode)}" }
                                    }
                                }
                            }
                            if my_presence == "dnd" {
                                div { class: "note",
                                    "Notifications are held while you're on do not disturb — no push, no sound."
                                }
                            }
                            if my_presence == "invisible" {
                                div { class: "note",
                                    "You'll show as offline to everyone else. Messages still reach you."
                                }
                            }

                            div { class: "you-actions",
                                button {
                                    class: "btn btn-primary",
                                    onclick: move |_| {
                                        // Seed the fields from what the server
                                        // currently holds, not from whatever
                                        // was typed and abandoned last time.
                                        status_draft.set(
                                            members()
                                                .iter()
                                                .find(|m| m.user.id == me_id)
                                                .and_then(|m| m.status.clone())
                                                .unwrap_or_default(),
                                        );
                                        profile_saved.set(false);
                                        overlay.set(Some("profile"));
                                        load_profile();
                                    },
                                    "Edit profile"
                                }
                                button {
                                    class: "btn btn-quiet",
                                    onclick: move |_| {
                                        notify_msg.set(String::new());
                                        // A password half-typed and abandoned
                                        // must not still be sitting there next
                                        // time — this is a phone.
                                        pw_current.set(String::new());
                                        pw_new.set(String::new());
                                        pw_confirm.set(String::new());
                                        pw_message.set((String::new(), false));
                                        overlay.set(Some("settings"));
                                        load_notify();
                                    },
                                    "Settings"
                                }
                            }

                            div { class: "section-label", style: "padding: 22px 2px 8px", "This device" }
                            div { class: "row-stack",
                                button {
                                    class: "row-card",
                                    onclick: move |_| {
                                        notify_msg.set(String::new());
                                        // A password half-typed and abandoned
                                        // must not still be sitting there next
                                        // time — this is a phone.
                                        pw_current.set(String::new());
                                        pw_new.set(String::new());
                                        pw_confirm.set(String::new());
                                        pw_message.set((String::new(), false));
                                        overlay.set(Some("settings"));
                                        load_notify();
                                    },
                                    span { class: "row-label", "Notifications" }
                                    span { class: "row-value on", "{notify_label}" }
                                }
                                div { class: "row-card",
                                    span { class: "row-label", "Voice" }
                                    span { class: "row-value",
                                        if in_call { "connected to {call_name}" } else { "not connected" }
                                    }
                                }
                                div { class: "row-card",
                                    span { class: "row-label", "Server" }
                                    span { class: "row-value ellipsis", "{host_name()}" }
                                }
                            }

                            div { class: "section-label", style: "padding: 22px 2px 8px", "Members" }
                            for member in members() {
                                {
                                    let colour = name_color(member.user.id, &members(), &tags());
                                    let sub = presence_line(member.online, &member.presence, member.status.clone());
                                    rsx! {
                                        div { key: "m{member.user.id}", class: "member-row",
                                            span { class: "avatar-wrap",
                                                Avatar { user: member.user.clone() }
                                                span {
                                                    class: presence_dot(member.online, &member.presence),
                                                    aria_hidden: "true",
                                                }
                                            }
                                            div { class: "member-col",
                                                div { class: "member-line",
                                                    span {
                                                        class: "member-name ellipsis",
                                                        style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                        "{member.user.username}"
                                                    }
                                                    if member.user.role == "admin" {
                                                        span { class: "tag-badge", "ADMIN" }
                                                    }
                                                    if voice_users().contains_key(&member.user.id) {
                                                        span { class: "member-voice", Icon { name: "volume", size: 12 } }
                                                    }
                                                }
                                                div { class: "member-status ellipsis", "{sub}" }
                                            }
                                        }
                                    }
                                }
                            }

                            button {
                                class: "logout-btn",
                                onclick: move |_| {
                                    save_session(&None);
                                    session.set(None);
                                },
                                "Log out ({sess().user.username})"
                            }
                        }
                    }
                }

                // ---------------- Channel ----------------
                if over == Some("channel") {
                    div { class: "overlay",
                        div { class: "overlay-head",
                            button {
                                class: "hbtn",
                                aria_label: "Back",
                                onclick: move |_| overlay.set(None),
                                // Backing out only closes the screen: the
                                // channel stays selected, so its half-typed
                                // message stays in the composer where it
                                // belongs. Parking it here as well would let
                                // the next `open_channel` re-park the emptied
                                // composer over the copy this just saved.
                                Icon { name: "chevron-left", size: 20 }
                            }
                            div { class: "chan-head-main",
                                div { class: "chan-head-name ellipsis", "{chan_title}" }
                                if !chan_topic.is_empty() {
                                    div { class: "chan-head-topic ellipsis", "{chan_topic}" }
                                }
                            }
                            if chan_is_dm {
                                button {
                                    class: "hbtn accent",
                                    aria_label: "Call",
                                    onclick: move |_| {
                                        let Some(channel) = selected.peek().clone() else { return };
                                        if voice_conn.peek().as_ref().map(|(id, _)| *id) == Some(channel.id) {
                                            // Already on the line: show it rather
                                            // than dialling on top of yourself.
                                            call_open.set(true);
                                        } else {
                                            join_voice(channel);
                                        }
                                    },
                                    Icon { name: "phone", size: 18 }
                                }
                            }
                            button {
                                class: "hbtn",
                                aria_label: "Members",
                                onclick: move |_| sheet.set(Some("members")),
                                Icon { name: "user", size: 18 }
                            }
                        }

                        main { class: "messages",
                            {
                                // The first message after where you left off,
                                // and the messages that open a new day. Both
                                // are worked out once per render, not per row.
                                // Read for the subscription: the day labels
                                // are relative to now, and this is what
                                // re-renders them when the date changes.
                                let _ = day_tick();
                                let first_unread = divider_at().and_then(|last_read| {
                                    messages().iter().map(|m| m.id).filter(|id| *id > last_read).min()
                                });
                                let day_starts: std::collections::HashSet<i64> = {
                                    let list = messages();
                                    list.iter()
                                        .enumerate()
                                        .filter(|(i, m)| *i == 0 || different_day(list[i - 1].created_at, m.created_at))
                                        .map(|(_, m)| m.id)
                                        .collect()
                                };
                                rsx! {
                                    // Grouped in reading order, then reversed:
                                    // column-reverse pins the view to the newest.
                                    // A divider is emitted AFTER its row, which
                                    // in this flipped list puts it above.
                                    for (msg, compact) in group_messages(&messages()).into_iter().rev() {
                                        {
                                        let msg_id = msg.id;
                                        let divider_here = Some(msg_id) == first_unread;
                                        let day_here = day_starts.contains(&msg_id).then(|| day_label(msg.created_at));
                                        rsx! {
                                        div {
                                            key: "{msg_id}",
                                            class: if highlight() == Some(msg_id) { "hit-wrap" } else { "" },
                                            MessageRow {
                                                failed: failed_sends().contains(&msg.id),
                                                msg,
                                                compact,
                                                me_id,
                                                me_admin,
                                            }
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
                                button { class: "load-older", onclick: load_older, "Load older messages" }
                            }
                            // An empty list means one of two things, and the
                            // screen should say which.
                            if messages().is_empty() {
                                if loading_channel() {
                                    div { class: "list-note", "loading…" }
                                } else {
                                    div { class: "list-note", "Nothing here yet. Say something." }
                                }
                            }
                        }

                        {
                            let names: Vec<String> = typing()
                                .values()
                                .filter(|(channel_id, _, _)| Some(*channel_id) == selected_id)
                                .map(|(_, name, _)| name.clone())
                                .collect();
                            let line = match names.as_slice() {
                                [] => String::new(),
                                [a] => format!("{a} is typing\u{2026}"),
                                [a, b] => format!("{a} and {b} are typing\u{2026}"),
                                _ => "several people are typing\u{2026}".into(),
                            };
                            rsx! {
                                if !line.is_empty() {
                                    div { class: "typing-line", "{line}" }
                                }
                            }
                        }

                        if let Some(target) = replying() {
                            div { class: "reply-bar",
                                Icon { name: "reply", size: 12 }
                                span { class: "grow ellipsis",
                                    "Replying to {target.author.username}: {target.content.chars().take(50).collect::<String>()}"
                                }
                                button {
                                    class: "hbtn",
                                    style: "width: 28px; height: 28px",
                                    aria_label: "Cancel reply",
                                    onclick: move |_| replying.set(None),
                                    Icon { name: "x", size: 13 }
                                }
                            }
                        }

                        {
                            // Recomputed from the draft on every keystroke;
                            // it's a prefix match over a handful of people,
                            // so there is nothing to memoise.
                            let suggestions = shared::mention_suggestions(&draft(), &members());
                            rsx! {
                                if !suggestions.is_empty() {
                                    div { class: "mention-pop",
                                        for name in suggestions {
                                            {
                                                let member = members()
                                                    .into_iter()
                                                    .find(|m| m.user.username == name);
                                                let colour = member
                                                    .as_ref()
                                                    .map(|m| name_color(m.user.id, &members(), &tags()))
                                                    .unwrap_or_default();
                                                let for_tap = name.clone();
                                                rsx! {
                                                    button {
                                                        key: "mention-{name}",
                                                        class: "mention-row",
                                                        onclick: move |_| {
                                                            // Read out first: the peek guard would
                                                            // still be alive at the set() below.
                                                            let current = draft.peek().clone();
                                                            draft.set(shared::complete_mention(&current, &for_tap));
                                                            focus_composer();
                                                        },
                                                        if let Some(member) = member.clone() {
                                                            Avatar { user: member.user.clone(), variant: "sm" }
                                                        } else {
                                                            // @everyone has no face.
                                                            span { class: "mention-all", Icon { name: "at-sign", size: 14 } }
                                                        }
                                                        span {
                                                            class: "mention-name ellipsis",
                                                            style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                            "{name}"
                                                        }
                                                        if member.is_none() {
                                                            span { class: "mention-hint", "notifies the whole channel" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        footer { class: if in_call { "composer" } else { "composer floor" },
                            button {
                                class: "cbtn",
                                aria_label: "Attach",
                                onclick: move |_| sheet.set(Some("attach")),
                                if uploading() { "…" } else { Icon { name: "plus", size: 18 } }
                            }
                            // A textarea, so a message can have more than one
                            // line. Return makes a newline, the way every phone
                            // chat app works; the arrow sends, and so does
                            // Ctrl/Cmd+Return for anyone on a real keyboard.
                            textarea {
                                id: COMPOSER_ID,
                                class: "draft",
                                rows: "1",
                                // Just "message": the channel's name is in
                                // the header directly above, and a long one
                                // here wrapped to a second line.
                                placeholder: "message",
                                value: "{draft}",
                                oninput: move |e| {
                                    draft.set(e.value());
                                    // Throttled: one event every few seconds
                                    // holds the indicator up, and a phone
                                    // keyboard fires a lot of these.
                                    if let Some(channel) = selected() {
                                        let now = now_ms();
                                        if now - last_typing_sent() >= TYPING_SEND_INTERVAL_MS {
                                            last_typing_sent.set(now);
                                            ws.send(ClientEvent::Typing { channel_id: channel.id });
                                        }
                                    }
                                },
                                onkeydown: move |e| {
                                    let mods = e.modifiers();
                                    if e.key() == Key::Enter
                                        && (mods.contains(Modifiers::CONTROL) || mods.contains(Modifiers::META))
                                    {
                                        e.prevent_default();
                                        send(());
                                    }
                                },
                            }
                            button {
                                class: "cbtn plain",
                                aria_label: "Stickers",
                                onclick: move |_| sheet.set(Some("stickers")),
                                Icon { name: "smile", size: 19 }
                            }
                            button {
                                class: if armed { "send-btn armed" } else { "send-btn" },
                                aria_label: "Send",
                                onclick: move |_| send(()),
                                Icon { name: "send", size: 17 }
                            }
                        }
                    }
                }

                // ---------------- Search ----------------
                if over == Some("search") {
                    div { class: "overlay",
                        div { class: "overlay-head",
                            button {
                                class: "hbtn",
                                aria_label: "Back",
                                onclick: move |_| overlay.set(None),
                                Icon { name: "chevron-left", size: 20 }
                            }
                            input {
                                class: "search-input",
                                placeholder: "search every channel",
                                value: "{query}",
                                autocapitalize: "none",
                                oninput: move |e| query.set(e.value()),
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter {
                                        run_search(());
                                    }
                                },
                            }
                            button {
                                class: "hbtn accent",
                                aria_label: "Search",
                                onclick: move |_| run_search(()),
                                Icon { name: "search", size: 18 }
                            }
                        }
                        div { class: "scroll grow", style: "padding: 10px 12px 14px",
                            div { class: "search-count",
                                if searching() {
                                    "searching…"
                                } else if query().trim().is_empty() {
                                    "type something and hit search"
                                } else if results().is_empty() {
                                    "nothing matched"
                                } else if results().len() == 1 {
                                    "1 message"
                                } else {
                                    "{results().len()} messages"
                                }
                            }
                            div { style: "display: flex; flex-direction: column; gap: 8px",
                                for hit in results() {
                                    {
                                        let where_label = if hit.channel_kind == "dm" {
                                            format!("@{}", hit.channel_name)
                                        } else {
                                            format!("#{}", hit.channel_name)
                                        };
                                        let when = format_time(hit.message.created_at);
                                        let colour = name_color(hit.message.author.id, &members(), &tags());
                                        let target = hit.message.channel_id;
                                        let text = hit.message.content.clone();
                                        rsx! {
                                            button {
                                                key: "s{hit.message.id}",
                                                class: "search-hit",
                                                onclick: move |_| {
                                                    // Jump to the room it was
                                                    // said in. Landing on the
                                                    // exact message needs
                                                    // history paging the API
                                                    // doesn't offer yet.
                                                    if let Some(channel) =
                                                        channels.peek().iter().find(|c| c.id == target).cloned()
                                                    {
                                                        overlay.set(None);
                                                        open_channel(channel);
                                                    }
                                                },
                                                span { class: "search-where",
                                                    span { class: "place", "{where_label}" }
                                                    "· {when}"
                                                }
                                                span {
                                                    class: "search-author",
                                                    style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                    "{hit.message.author.username}"
                                                }
                                                span { class: "search-text", "{text}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ---------------- Settings ----------------
                if over == Some("settings") {
                    div { class: "overlay",
                        div { class: "overlay-head",
                            button {
                                class: "hbtn",
                                aria_label: "Back",
                                onclick: move |_| overlay.set(None),
                                Icon { name: "chevron-left", size: 20 }
                            }
                            div { class: "overlay-title", "Settings" }
                        }
                        div { class: "scroll overlay-body",
                            // Installing comes first: on iPhone it's what
                            // makes notifications possible at all, and on
                            // Android the browser's own banner only ever
                            // appears once.
                            {
                                let inst = install_glue();
                                rsx! {
                                    if !inst.installed {
                                        div { class: "section-label", style: "padding: 14px 2px 8px", "Install" }
                                        if inst.available {
                                            button {
                                                class: "row-card",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let _ = wasm_bindgen_futures::JsFuture::from(install_prompt_js()).await;
                                                        if let Ok(state) = serde_json::from_str::<InstallGlue>(&install_state_js()) {
                                                            install_glue.set(state);
                                                        }
                                                    });
                                                },
                                                Icon { name: "download", size: 16 }
                                                span { class: "row-label", "Install NotDiscord on this device" }
                                            }
                                        } else if inst.ios {
                                            div { class: "note",
                                                "To install: tap the Share button, then \"Add to Home Screen\". Notifications only work once it's installed."
                                            }
                                        } else {
                                            div { class: "note",
                                                "To install: open your browser's menu and pick \"Install app\" or \"Add to Home screen\". Your browser only offers to do this on its own once, so the menu is the reliable way back."
                                            }
                                        }
                                        if inst.outcome == "dismissed" {
                                            div { class: "note good", "maybe next time" }
                                        }
                                    }
                                }
                            }

                            div { class: "section-label", style: "padding: 18px 2px 8px", "Notifications" }
                            {
                                let glue = push_glue();
                                let prefs = notify();
                                let on = prefs.as_ref().is_some_and(|p| p.subscribed) && glue.permission == "granted";
                                let level = prefs.as_ref().map(|p| p.level.clone()).unwrap_or_else(|| "mentions".into());
                                let vapid = prefs.as_ref().map(|p| p.vapid_key.clone()).unwrap_or_default();
                                let push_icon: &'static str = if on { "check" } else { "user" };
                                rsx! {
                                    if !glue.supported {
                                        div { class: "note",
                                            "This browser can't do notifications. On iPhone, add NotDiscord to your Home Screen first."
                                        }
                                    } else {
                                        button {
                                            class: if on { "row-card picked" } else { "row-card" },
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
                                            Icon { name: push_icon, size: 16 }
                                            span { class: "row-label",
                                                if on {
                                                    "Notifications are on for this device"
                                                } else {
                                                    "Turn on notifications for this device"
                                                }
                                            }
                                        }
                                    }

                                    div { class: "section-label", style: "padding: 18px 2px 8px", "Notify me about" }
                                    div { class: "row-stack",
                                        for (value, label, hint) in [
                                            ("all", "Everything", "every message in every channel"),
                                            ("mentions", "Mentions & DMs", "when someone @s you or messages you directly"),
                                            ("none", "Nothing", "no notifications at all"),
                                        ] {
                                            button {
                                                key: "{value}",
                                                class: if level == value { "row-card picked" } else { "row-card" },
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
                                                span { class: "row-col",
                                                    span { style: "font-size: 14px", "{label}" }
                                                    span { class: "row-hint", "{hint}" }
                                                }
                                                if level == value {
                                                    span { style: "color: var(--accent); display: flex",
                                                        Icon { name: "check", size: 15 }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    div { class: "note",
                                        "Notifications only arrive while the app is closed — if you're already looking, you'd just see the message."
                                    }
                                    if !notify_msg().is_empty() {
                                        div { class: "note good", "{notify_msg}" }
                                    }
                                }
                            }

                            div { class: "section-label", style: "padding: 22px 2px 8px", "Password" }
                            {
                                let me_name = sess().user.username.clone();
                                // The same rule the server applies, run as you
                                // type, so the problem shows before you submit.
                                let problem = if pw_new().is_empty() {
                                    None
                                } else {
                                    shared::password_problem(&pw_new(), &me_name)
                                };
                                let mismatch = !pw_confirm().is_empty() && pw_confirm() != pw_new();
                                let ready = !pw_current().is_empty()
                                    && !pw_new().is_empty()
                                    && pw_confirm() == pw_new()
                                    && problem.is_none();
                                rsx! {
                                    div { class: "row-stack",
                                        div { class: "field",
                                            label { "Current password" }
                                            input {
                                                r#type: "password",
                                                autocomplete: "current-password",
                                                value: "{pw_current}",
                                                oninput: move |e| pw_current.set(e.value()),
                                            }
                                        }
                                        div { class: "field",
                                            label { "New password" }
                                            input {
                                                r#type: "password",
                                                autocomplete: "new-password",
                                                value: "{pw_new}",
                                                oninput: move |e| {
                                                    pw_new.set(e.value());
                                                    pw_message.set((String::new(), false));
                                                },
                                            }
                                        }
                                        div { class: "field",
                                            label { "Confirm new password" }
                                            input {
                                                r#type: "password",
                                                autocomplete: "new-password",
                                                value: "{pw_confirm}",
                                                oninput: move |e| pw_confirm.set(e.value()),
                                            }
                                        }
                                    }
                                    if let Some(problem) = problem {
                                        div { class: "note bad", "{problem}" }
                                    } else if mismatch {
                                        div { class: "note bad", "passwords don't match" }
                                    } else if !pw_confirm().is_empty() && !pw_new().is_empty() {
                                        div { class: "note good", "looks good" }
                                    }
                                    if !pw_message().0.is_empty() {
                                        div { class: if pw_message().1 { "note good" } else { "note bad" }, "{pw_message().0}" }
                                    }
                                    button {
                                        class: "btn btn-primary",
                                        style: "width: 100%; margin-top: 10px",
                                        disabled: !ready,
                                        onclick: move |_| {
                                            let (current, new) = (pw_current(), pw_new());
                                            spawn(async move {
                                                match api::change_password(&sess(), current, new).await {
                                                    Ok(()) => {
                                                        pw_current.set(String::new());
                                                        pw_new.set(String::new());
                                                        pw_confirm.set(String::new());
                                                        // This device keeps its
                                                        // session by the server's
                                                        // design; the others don't.
                                                        pw_message.set((
                                                            "changed — your other devices were signed out".into(),
                                                            true,
                                                        ));
                                                    }
                                                    Err(e) => pw_message.set((e, false)),
                                                }
                                            });
                                        },
                                        "Change password"
                                    }
                                    div { class: "note",
                                        "Changing it signs out every other device you're logged in on. This one stays."
                                    }
                                }
                            }
                        }
                    }
                }

                // ---------------- Edit profile ----------------
                if over == Some("profile") {
                    div { class: "overlay",
                        div { class: "overlay-head",
                            button {
                                class: "hbtn",
                                aria_label: "Back",
                                onclick: move |_| overlay.set(None),
                                Icon { name: "chevron-left", size: 20 }
                            }
                            div { class: "overlay-title", "Edit profile" }
                            button {
                                class: "btn btn-primary",
                                style: "min-height: 34px; padding: 6px 13px",
                                onclick: move |_| save_profile(()),
                                if profile_saved() { "Saved" } else { "Save" }
                            }
                        }
                        div { class: "scroll overlay-body", style: "padding: 18px 16px",
                            div { style: "display: flex; align-items: center; gap: 14px",
                                Avatar { user: sess().user.clone(), variant: "big" }
                                label { class: "btn btn-quiet",
                                    input {
                                        r#type: "file",
                                        accept: "image/*",
                                        style: "display: none",
                                        onchange: move |evt| {
                                            spawn(async move {
                                                let Some(file) = evt.files().into_iter().next() else { return };
                                                let name = file.name();
                                                uploading.set(true);
                                                if let Ok(bytes) = file.read_bytes().await {
                                                    match api::upload(&sess(), &name, bytes.to_vec()).await {
                                                        Ok(url) => {
                                                            match api::update_profile(&sess(), Some(url), None).await {
                                                                // The roster carries the avatar
                                                                // everywhere else in the app, so
                                                                // refresh it rather than guessing.
                                                                Ok(()) => {
                                                                    if let Ok(list) = api::users(&sess()).await {
                                                                        members.set(list);
                                                                    }
                                                                    if let Ok(user) = api::me(&sess()).await {
                                                                        // Cloned out first: the read
                                                                        // guard would still be alive
                                                                        // at the set() below.
                                                                        let current = session.peek().clone();
                                                                        if let Some(current) = current {
                                                                            let updated = api::Session { user, ..current };
                                                                            save_session(&Some(updated.clone()));
                                                                            session.set(Some(updated));
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => status.set(e),
                                                            }
                                                        }
                                                        Err(e) => status.set(e),
                                                    }
                                                }
                                                uploading.set(false);
                                            });
                                        },
                                    }
                                    if uploading() { "Uploading…" } else { "Change avatar" }
                                }
                            }

                            div { class: "field", style: "margin-top: 22px",
                                label { "Display name" }
                                input { value: "{sess().user.username}", disabled: true }
                                span { class: "field-hint",
                                    "Your username is how everyone here is addressed, so it isn't editable from the phone."
                                }
                            }
                            div { class: "field", style: "margin-top: 16px",
                                label { "Status" }
                                input {
                                    value: "{status_draft}",
                                    placeholder: "what you're up to",
                                    oninput: move |e| {
                                        status_draft.set(e.value());
                                        profile_saved.set(false);
                                    },
                                }
                                span { class: "field-hint", "Everyone on the server sees this next to your name." }
                            }
                            div { class: "field", style: "margin-top: 16px",
                                label { "About you" }
                                textarea {
                                    rows: "3",
                                    value: "{bio_draft}",
                                    oninput: move |e| {
                                        bio_draft.set(e.value());
                                        profile_saved.set(false);
                                    },
                                }
                            }
                            div { class: "field", style: "margin-top: 16px",
                                label { "Name colour" }
                                div { class: "swatch-row",
                                    {
                                        let colour = name_color(me_id, &members(), &tags());
                                        let shown = if colour.is_empty() { "var(--text)".to_string() } else { colour };
                                        rsx! {
                                            span { class: "swatch", style: "background: {shown}" }
                                            for tag in my_tags() {
                                                span {
                                                    key: "t{tag.id}",
                                                    class: "tag-badge",
                                                    style: "background: {tag.color}; color: var(--bg)",
                                                    "{tag.name}"
                                                }
                                            }
                                        }
                                    }
                                }
                                span { class: "field-hint", "Colours come from tags an admin gives you." }
                            }
                        }
                    }
                }
            }

            // ---------------- the call, minimised ----------------
            if in_call && !call_open() {
                div { class: if over.is_some() { "callbar floor" } else { "callbar" },
                    button {
                        class: "callbar-open",
                        onclick: move |_| call_open.set(true),
                        span { class: "wave", span {} span {} span {} }
                        span { class: "callbar-col",
                            span { class: "callbar-name ellipsis", "{call_name}" }
                            span { class: "callbar-sub ellipsis", "{call_peers_line}" }
                        }
                    }
                    button {
                        class: if voice_glue().muted { "minibtn off" } else { "minibtn" },
                        aria_label: "Mute",
                        onclick: move |_| {
                            let now_muted = !voice_glue.peek().muted;
                            spawn(async move {
                                let _ = wasm_bindgen_futures::JsFuture::from(voice_set_muted_js(now_muted)).await;
                            });
                        },
                        Icon { name: mic_icon, size: 16 }
                    }
                    button {
                        class: "minibtn",
                        aria_label: "Leave",
                        onclick: leave_voice,
                        Icon { name: "phone-off", size: 16 }
                    }
                }
            }

            // ---------------- tabs ----------------
            if over.is_none() && !call_open() {
                nav { class: "tabbar",
                    button {
                        class: if tab() == "chats" { "tabbtn on" } else { "tabbtn" },
                        onclick: move |_| tab.set("chats"),
                        span { class: "tabpill",
                            Icon { name: "message", size: 20 }
                            if total_unread > 0 {
                                span { class: if any_mention { "tabbadge mention" } else { "tabbadge" }, "{total_unread}" }
                            }
                        }
                        span { class: "tablabel", "Chats" }
                    }
                    button {
                        class: if tab() == "voice" { "tabbtn on" } else { "tabbtn" },
                        onclick: move |_| tab.set("voice"),
                        span { class: "tabpill", Icon { name: "volume", size: 20 } }
                        span { class: "tablabel", "Voice" }
                    }
                    button {
                        class: if tab() == "music" { "tabbtn on" } else { "tabbtn" },
                        onclick: move |_| {
                            tab.set("music");
                            refresh_music();
                        },
                        span { class: "tabpill", Icon { name: "music", size: 20 } }
                        span { class: "tablabel", "Music" }
                    }
                    button {
                        class: if tab() == "you" { "tabbtn on" } else { "tabbtn" },
                        onclick: move |_| tab.set("you"),
                        span { class: "tabpill", Icon { name: "user", size: 20 } }
                        span { class: "tablabel", "You" }
                    }
                }
            }

            // ---------------- the call, full screen ----------------
            if in_call && call_open() {
                div { class: "call-screen",
                    div { class: "call-head",
                        button {
                            class: "hbtn",
                            aria_label: "Minimise",
                            onclick: move |_| call_open.set(false),
                            Icon { name: "chevron-down", size: 20 }
                        }
                        div { class: "grow",
                            div { class: "call-name ellipsis", "{call_name}" }
                            div { class: "call-sub ellipsis", "{call_clock} · {call_peers_line}" }
                        }
                    }
                    div { class: "scroll grow", style: "padding: 8px 14px",
                        div { class: "call-grid",
                            for peer in voice_glue().participants {
                                {
                                    let state_label = if peer.local {
                                        if voice_glue().muted { "you · muted" } else { "you · open mic" }
                                    } else if peer.speaking {
                                        "speaking"
                                    } else {
                                        "quiet"
                                    };
                                    // The roster has the avatar and the tag
                                    // colour; the voice glue only has a name.
                                    let member = members()
                                        .into_iter()
                                        .find(|m| m.user.username == peer.name);
                                    let colour = member
                                        .as_ref()
                                        .map(|m| name_color(m.user.id, &members(), &tags()))
                                        .unwrap_or_default();
                                    rsx! {
                                        div {
                                            key: "{peer.identity}",
                                            class: if peer.speaking { "call-tile speaking" } else { "call-tile" },
                                            if let Some(member) = member.clone() {
                                                Avatar { user: member.user.clone(), variant: "tile" }
                                            }
                                            span {
                                                class: "call-tile-name ellipsis",
                                                style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                "{peer.name}"
                                            }
                                            span { class: "call-tile-state", "{state_label}" }
                                        }
                                    }
                                }
                            }
                        }
                        if voice_glue().participants.iter().any(|p| !p.local) {
                            div { class: "call-volumes",
                                div { class: "section-label", style: "padding: 0 0 10px", "Per-person volume" }
                                for peer in voice_glue().participants.into_iter().filter(|p| !p.local) {
                                    {
                                        let identity = peer.identity.clone();
                                        let level = volumes().get(&identity).copied().unwrap_or(100);
                                        let for_input = identity.clone();
                                        rsx! {
                                            div { key: "vol{identity}", class: "volume-row",
                                                span { class: "volume-name ellipsis", "{peer.name}" }
                                                input {
                                                    r#type: "range",
                                                    class: "volume-slider",
                                                    min: "0",
                                                    max: "200",
                                                    value: "{level}",
                                                    oninput: move |e: Event<FormData>| {
                                                        if let Ok(v) = e.value().parse::<i64>() {
                                                            voice_set_volume_js(&for_input, v as f64 / 100.0);
                                                            volumes.write().insert(for_input.clone(), v);
                                                        }
                                                    },
                                                }
                                                span { class: "volume-value", "{level}%" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div { class: "call-actions",
                        button {
                            class: if voice_glue().muted { "callbtn" } else { "callbtn on" },
                            aria_label: "Mute",
                            onclick: move |_| {
                                let now_muted = !voice_glue.peek().muted;
                                spawn(async move {
                                    let _ = wasm_bindgen_futures::JsFuture::from(voice_set_muted_js(now_muted)).await;
                                });
                            },
                            Icon { name: mic_icon, size: 21 }
                        }
                        button {
                            class: if voice_glue().deafened { "callbtn active" } else { "callbtn" },
                            aria_label: "Deafen",
                            onclick: move |_| {
                                let now = !voice_glue.peek().deafened;
                                spawn(async move {
                                    let _ = wasm_bindgen_futures::JsFuture::from(voice_set_deafened_js(now)).await;
                                });
                            },
                            Icon { name: deafen_icon, size: 21 }
                        }
                        button {
                            class: "callbtn leave",
                            aria_label: "Leave call",
                            onclick: leave_voice,
                            Icon { name: "phone-off", size: 22 }
                        }
                    }
                }
            }

            // ---------------- somebody is calling ----------------
            // Above the sheets: a call arriving while you're picking a
            // sticker should still be the first thing you see.
            if let Some((ring_channel, caller)) = incoming_call() {
                div { class: "ring-screen",
                    div { class: "ring-halo",
                        span { class: "ring-wave" }
                        span { class: "ring-wave late" }
                        Avatar { user: caller.clone(), variant: "ring" }
                    }
                    div { class: "ring-name", "{caller.username}" }
                    div { class: "ring-sub", "is calling you" }
                    div { class: "grow" }
                    div { class: "ring-actions",
                        button {
                            class: "ring-btn",
                            onclick: move |_| incoming_call.set(None),
                            span { class: "ring-circle", Icon { name: "phone-off", size: 24 } }
                            span { "Decline" }
                        }
                        button {
                            class: "ring-btn answer",
                            onclick: move |_| {
                                incoming_call.set(None);
                                if let Some(channel) = channels.peek().iter().find(|c| c.id == ring_channel).cloned() {
                                    join_voice(channel);
                                }
                            },
                            span { class: "ring-circle", Icon { name: "phone", size: 24 } }
                            span { "Answer" }
                        }
                    }
                }
            }

            // ---------------- sheets ----------------
            if let Some(which) = sheet() {
                div {
                    class: "scrim",
                    onclick: move |_| {
                        // A send in flight keeps its sheet; anything else
                        // staged and not sent is let go of with the sheet.
                        if uploading() {
                            return;
                        }
                        discard_staged();
                        sheet.set(None);
                    },
                }
                div { class: "sheet",
                    div { class: "sheet-grip" }

                    if which == "preview" {
                        if let Some(file) = staged() {
                            div { class: "sheet-head", span { class: "sheet-title", "Send this?" } }
                            if file.kind == "image" {
                                img { class: "preview-img", src: "{file.url}", alt: "" }
                            } else if file.kind == "video" {
                                video { class: "preview-img", src: "{file.url}", controls: true, preload: "metadata" }
                            } else {
                                div { class: "preview-file",
                                    Icon { name: "file", size: 20 }
                                    span { class: "grow", "{file.name}" }
                                }
                            }
                            div { class: "preview-meta", "{file.name} · {human_size(file.size)}" }
                            if uploading() {
                                div { class: "upload-bar", div { style: "width: {upload_percent}%" } }
                                div { class: "preview-meta", "sending… {upload_percent}%" }
                            }
                            div { class: "preview-actions",
                                // Never disabled: mid-upload it aborts the
                                // request. It used to grey out while sending,
                                // which on a send that had stalled made it "a
                                // lie" (Jon) — the one moment you want it.
                                button {
                                    class: "btn",
                                    onclick: move |_| {
                                        discard_staged();
                                        uploading.set(false);
                                        sheet.set(None);
                                    },
                                    "Cancel"
                                }
                                button {
                                    class: "btn btn-primary",
                                    disabled: uploading(),
                                    onclick: send_staged,
                                    if uploading() { "Sending…" } else { "Send" }
                                }
                            }
                        }
                    }

                    if which == "attach" {
                        div { class: "sheet-head", span { class: "sheet-title", "Send something" } }
                        div { class: "attach-grid",
                            label { class: "attach-opt",
                                input {
                                    id: "nd-pick-photo",
                                    r#type: "file",
                                    accept: "image/*",
                                    onchange: move |_| stage_file("nd-pick-photo"),
                                }
                                Icon { name: "image", size: 20 }
                                span { "Gallery" }
                            }
                            label { class: "attach-opt",
                                input {
                                    id: "nd-pick-camera",
                                    r#type: "file",
                                    accept: "image/*",
                                    capture: "environment",
                                    onchange: move |_| stage_file("nd-pick-camera"),
                                }
                                Icon { name: "camera", size: 20 }
                                span { "Camera" }
                            }
                            label { class: "attach-opt",
                                input {
                                    id: "nd-pick-file",
                                    r#type: "file",
                                    onchange: move |_| stage_file("nd-pick-file"),
                                }
                                Icon { name: "file", size: 20 }
                                span { "File" }
                            }
                            button {
                                class: "attach-opt",
                                onclick: move |_| sheet.set(Some("stickers")),
                                Icon { name: "smile", size: 20 }
                                span { "Sticker" }
                            }
                            button {
                                class: "attach-opt",
                                onclick: move |_| {
                                    gif_query.set(String::new());
                                    sheet.set(Some("gifs"));
                                    // Trending, so there's something to tap
                                    // before you've typed a letter.
                                    search_gifs(String::new());
                                },
                                span { class: "gif-glyph", "GIF" }
                                span { "GIF" }
                            }
                        }
                        div { class: "note",
                            "Uploads keep for as long as this server's retention allows, then the file goes and the message stays."
                        }
                    }

                    if which == "stickers" {
                        div { class: "sheet-head",
                            span { class: "sheet-title", "Stickers" }
                            span { class: "sheet-note", "added from the desktop app" }
                        }
                        if stickers().is_empty() {
                            div { class: "note", "Nobody has added a sticker to this server yet." }
                        }
                        div { class: "sticker-grid",
                            for sticker in stickers() {
                                img {
                                    key: "st{sticker.id}",
                                    class: "sticker-cell",
                                    src: "{sticker.url}",
                                    alt: "{sticker.name}",
                                    onclick: {
                                        let url = sticker.url.clone();
                                        move |_| {
                                            if let Some(channel) = selected.peek().clone() {
                                                ws.send(ClientEvent::SendMessage {
                                                    channel_id: channel.id,
                                                    content: url.clone(),
                                                    reply_to: None,
                                                });
                                            }
                                            sheet.set(None);
                                        }
                                    },
                                }
                            }
                        }
                    }

                    if which == "gifs" {
                        div { class: "sheet-head",
                            span { class: "sheet-title", "GIFs" }
                            span { class: "sheet-note", "via GIPHY" }
                        }
                        div { class: "gif-search",
                            input {
                                class: "text-input",
                                placeholder: "search, or browse what's trending",
                                value: "{gif_query}",
                                autocapitalize: "none",
                                oninput: move |e| gif_query.set(e.value()),
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter {
                                        search_gifs(gif_query.peek().clone());
                                    }
                                },
                            }
                            button {
                                class: "hbtn accent",
                                aria_label: "Search GIFs",
                                onclick: move |_| search_gifs(gif_query.peek().clone()),
                                Icon { name: "search", size: 18 }
                            }
                        }
                        if !gif_status().is_empty() {
                            div { class: "note", style: "margin: 4px 0 10px", "{gif_status}" }
                        }
                        div { class: "gif-grid",
                            for (i, gif) in gif_results().into_iter().enumerate() {
                                img {
                                    key: "g{i}",
                                    class: "gif-cell",
                                    src: "{gif.preview}",
                                    loading: "lazy",
                                    alt: "GIF",
                                    onclick: {
                                        // The full-size URL goes in the message;
                                        // the preview was only for the grid.
                                        let url = gif.url.clone();
                                        move |_| {
                                            if let Some(channel) = selected.peek().clone() {
                                                ws.send(ClientEvent::SendMessage {
                                                    channel_id: channel.id,
                                                    content: url.clone(),
                                                    reply_to: None,
                                                });
                                            }
                                            sheet.set(None);
                                        }
                                    },
                                }
                            }
                        }
                    }

                    if which == "members" {
                        div { class: "sheet-head",
                            span { class: "sheet-title", "Members" }
                            span { class: "sheet-note", "{members().len()} · {online_now} online" }
                        }
                        for member in members() {
                            {
                                let colour = name_color(member.user.id, &members(), &tags());
                                let sub = presence_line(member.online, &member.presence, member.status.clone());
                                let user_id = member.user.id;
                                let is_me = user_id == me_id;
                                rsx! {
                                    div { key: "sm{member.user.id}", class: "member-row",
                                        span { class: "avatar-wrap",
                                            Avatar { user: member.user.clone() }
                                            span {
                                                class: presence_dot(member.online, &member.presence),
                                                aria_hidden: "true",
                                            }
                                        }
                                        div { class: "member-col",
                                            div { class: "member-line",
                                                span {
                                                    class: "member-name ellipsis",
                                                    style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                                    "{member.user.username}"
                                                }
                                                if member.user.role == "admin" {
                                                    span { class: "tag-badge", "ADMIN" }
                                                }
                                                if voice_users().contains_key(&member.user.id) {
                                                    span { class: "member-voice", Icon { name: "volume", size: 12 } }
                                                }
                                            }
                                            div { class: "member-status ellipsis", "{sub}" }
                                        }
                                        if !is_me {
                                            button {
                                                class: "member-dm",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        match api::create_dm(&sess(), user_id).await {
                                                            Ok(channel) => {
                                                                if !channels.peek().iter().any(|c| c.id == channel.id) {
                                                                    channels.write().push(channel.clone());
                                                                }
                                                                sheet.set(None);
                                                                open_channel(channel);
                                                            }
                                                            Err(e) => status.set(e),
                                                        }
                                                    });
                                                },
                                                "Message"
                                            }
                                        }
                                    }
                                }
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
                        if let Some(mut view) = lightbox() {
                            view.step(delta);
                            lightbox.set(Some(view));
                        }
                    };
                    rsx! {
                        div {
                            class: "lightbox",
                            onclick: move |_| lightbox.set(None),
                            // A drag across the photo pages it, the way every
                            // other phone gallery works. Anything under a
                            // third of the screen is a tap, not a swipe.
                            ontouchstart: move |e: Event<TouchData>| {
                                if let Some(t) = e.touches().first() {
                                    swipe_from.set(Some(t.client_coordinates().x));
                                }
                            },
                            ontouchend: move |e: Event<TouchData>| {
                                let Some(from) = swipe_from() else { return };
                                swipe_from.set(None);
                                let Some(t) = e.touches_changed().first().map(|t| t.client_coordinates().x) else {
                                    return;
                                };
                                let travelled = t - from;
                                let enough = web_sys::window()
                                    .and_then(|w| w.inner_width().ok())
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(360.0)
                                    / 3.0;
                                if travelled.abs() > enough {
                                    // Drag left to go forward, like paper.
                                    step(if travelled < 0.0 { 1 } else { -1 });
                                }
                            },
                            img {
                                class: "lightbox-img",
                                src: "{url}",
                                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                            }
                            if many {
                                button {
                                    class: "lightbox-arrow left",
                                    onclick: move |e: Event<MouseData>| {
                                        e.stop_propagation();
                                        step(-1);
                                    },
                                    Icon { name: "chevron-down", size: 22 }
                                }
                                button {
                                    class: "lightbox-arrow right",
                                    onclick: move |e: Event<MouseData>| {
                                        e.stop_propagation();
                                        step(1);
                                    },
                                    Icon { name: "chevron-down", size: 22 }
                                }
                            }
                            div { class: "lightbox-bar",
                                if many {
                                    span { class: "lightbox-count", "{position}" }
                                }
                                a {
                                    class: "lightbox-open",
                                    href: "{url}",
                                    target: "_blank",
                                    onclick: move |e: Event<MouseData>| e.stop_propagation(),
                                    "Open full size"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Avatar(user: User, #[props(default = "")] variant: &'static str) -> Element {
    let hue = (user.id * 137) % 360;
    let class = if variant.is_empty() {
        "avatar".to_string()
    } else {
        format!("avatar {variant}")
    };
    match user.avatar.clone() {
        Some(url) => rsx! { img { class: "{class} sized", src: "{url}" } },
        None => rsx! {
            div {
                class: "{class} initial",
                style: "background: hsl({hue}, 55%, 45%)",
                {user.username.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}
            }
        },
    }
}

/// An OpenGraph card under a message. Everything here came from the server,
/// so nobody's IP reaches the linked site — and where the host can be
/// embedded (Spotify, YouTube, SoundCloud, Vimeo), the play button swaps in
/// a real player in place of the card's art.
#[component]
fn LinkCard(url: String) -> Element {
    let sess = use_context::<Signal<api::Session>>();
    let mut card = use_signal(|| None::<shared::LinkPreview>);
    let mut playing = use_signal(|| false);

    use_future({
        let url = url.clone();
        move || {
            let url = url.clone();
            async move {
                let hit = PREVIEW_CACHE.with(|c| c.borrow().get(&url).cloned());
                let preview = match hit {
                    Some(cached) => cached,
                    None => {
                        let fetched = api::link_preview(&sess(), &url).await;
                        PREVIEW_CACHE.with(|c| c.borrow_mut().insert(url.clone(), fetched.clone()));
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
    let embed = preview.embed.clone();
    // Players are tall enough to read; the card's own art is not.
    let height = preview.embed_height.max(120);

    rsx! {
        div { class: "link-card",
            if playing() {
                if let Some(src) = embed.clone() {
                    iframe {
                        class: "link-card-player",
                        src: "{src}",
                        height: "{height}",
                        allow: "autoplay; encrypted-media; clipboard-write; picture-in-picture",
                        // A player is a stranger's page: no scripts of ours,
                        // no reaching back out of the frame.
                        "sandbox": "allow-scripts allow-same-origin allow-popups allow-presentation",
                    }
                }
            } else {
                if let Some(image) = preview.image.clone() {
                    div { class: "link-card-media",
                        img { class: "link-card-img", src: "{image}", loading: "lazy" }
                        if embed.is_some() {
                            button {
                                class: "link-card-play",
                                onclick: move |_| playing.set(true),
                                Icon { name: "play", size: 22 }
                            }
                        }
                    }
                }
            }
            div { class: "link-card-body",
                if !preview.site_name.is_empty() {
                    div { class: "link-card-site", "{preview.site_name}" }
                }
                if !preview.title.is_empty() {
                    a {
                        class: "link-card-title",
                        href: "{url}",
                        target: "_blank",
                        rel: "noopener noreferrer",
                        "{preview.title}"
                    }
                }
                if !preview.description.is_empty() {
                    div { class: "link-card-desc", "{preview.description}" }
                }
                if embed.is_some() && !playing() && preview.image.is_none() {
                    button {
                        class: "link-card-playrow",
                        onclick: move |_| playing.set(true),
                        Icon { name: "play", size: 14 }
                        span { "Play here" }
                    }
                }
            }
        }
    }
}

/// The quick-react strip: the crew's high-traffic emojis.
const QUICK_REACTIONS: [&str; 6] = ["👍", "😂", "❤️", "😮", "😢", "🔥"];

/// How long a finger has to stay on a reaction pill before it asks who
/// reacted instead of toggling. A phone has no hover, so the press is the
/// tooltip; short enough not to feel like a wait, long enough that a
/// deliberate tap never trips it.
const LONG_PRESS_MS: i64 = 400;

#[component]
fn MessageRow(msg: Message, compact: bool, failed: bool, me_id: i64, me_admin: bool) -> Element {
    // Negative id: shown but not yet confirmed by the server.
    let pending = msg.id < 0;
    let ws = use_coroutine_handle::<ClientEvent>();
    let mut replying = use_context::<Signal<Option<Message>>>();
    let mut lightbox = use_context::<Signal<Option<Lightbox>>>();
    let gallery = use_context::<Gallery>().0;
    let stickers_ctx = try_consume_context::<Signal<Vec<shared::Sticker>>>();
    let members_ctx = try_consume_context::<Signal<Vec<UserStatus>>>();
    let tags_ctx = try_consume_context::<Signal<Vec<shared::Tag>>>();
    // Tapping the words opens the actions. The old always-visible strip
    // reserved 104px on the right of every line for four icons, which is a
    // quarter of a phone's width spent on controls nobody was using.
    let mut actions_open = use_signal(|| false);
    // A finger down on this row: (which press, where it started). The
    // timer that opens the actions checks the press is still the same one
    // when it fires, so a released or scrolled press can't open anything.
    let mut press = use_signal(|| None::<(u32, f64, f64)>);
    let mut press_seq = use_signal(|| 0u32);
    // When a long-press last fired, so the click the browser sends on
    // release doesn't also open the image it happened to be over.
    let mut long_pressed_at = use_signal(|| 0i64);
    // Which pill's reactors are being shown, and when the finger went down.
    let mut reactors_for = use_signal(|| None::<String>);
    let mut pressed_at = use_signal(|| None::<i64>);
    // Editing and deleting your own words, the way the desktop app allows.
    // Deleting asks first: these are thumb-sized targets on a phone.
    let mut editing = use_signal(|| None::<String>);
    let mut confirming_delete = use_signal(|| false);
    let msg_id = msg.id;
    let channel_id = msg.channel_id;
    let mine = msg.author.id == me_id;
    let public_url = try_consume_context::<PublicUrl>();

    // The bot's music-player card is a desktop thing.
    if msg.content.starts_with(shared::PLAYER_MARKER) {
        return rsx! {
            div { class: "msg",
                div { class: "msg-body dim", "🎵 music player — open the desktop app for controls" }
            }
        };
    }
    let (images, videos, files, text) = extract_media(&msg.content);
    // Counted now: the render loops below consume the lists, and the
    // actions sheet's preview line wants the numbers after that.
    let (n_images, n_videos, n_files) = (images.len(), videos.len(), files.len());
    // A message that is nothing but one of the server's stickers renders
    // small. Full width is right for a photo someone took and absurd for a
    // reaction sticker, and a sticker arrives as a bare URL so this is the
    // only way to tell the two apart.
    let is_sticker = text.is_empty()
        && images.len() == 1
        && videos.is_empty()
        && files.is_empty()
        && stickers_ctx.is_some_and(|list| {
            list.read().iter().any(|s| images[0].ends_with(&s.url))
        });

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
        div {
            // What a permalink scrolls to.
            id: "msg-{msg_id}",
            class: {
                let base = match (compact, pending, failed) {
                    (_, _, true) => "msg failed",
                    (true, true, _) => "msg compact pending",
                    (true, false, _) => "msg compact",
                    (false, true, _) => "msg pending",
                    (false, false, _) => "msg",
                };
                if press().is_some() { format!("{base} pressing") } else { base.to_string() }
            },
            // Hold anywhere on the message for its actions — words, picture,
            // or the empty gutter. Until now the actions lived on the words
            // alone, so a GIF or a screenshot could not be replied to,
            // reacted to, or deleted from the phone at all.
            ontouchstart: move |e: Event<TouchData>| {
                // Nothing to do to a message the server hasn't confirmed yet.
                if pending {
                    return;
                }
                // Bound first: touches() is a fresh Vec, and a let-else
                // would drop it before t is read.
                let touches = e.touches();
                let Some(t) = touches.first() else { return };
                let (x, y) = (t.client_coordinates().x, t.client_coordinates().y);
                let seq = press_seq() + 1;
                press_seq.set(seq);
                press.set(Some((seq, x, y)));
                spawn(async move {
                    gloo_timers::future::TimeoutFuture::new(LONG_PRESS_MS as u32).await;
                    // Still the same finger, still down, hasn't scrolled.
                    if (*press.peek()).map(|(s, _, _)| s) == Some(seq) {
                        press.set(None);
                        long_pressed_at.set(now_ms());
                        haptic(25);
                        actions_open.set(true);
                    }
                });
            },
            ontouchmove: move |e: Event<TouchData>| {
                // A finger that travels is scrolling, not pressing.
                if let (Some((_, x0, y0)), Some(t)) = (press(), e.touches().first()) {
                    let (x, y) = (t.client_coordinates().x, t.client_coordinates().y);
                    if (x - x0).abs() > 10.0 || (y - y0).abs() > 10.0 {
                        press.set(None);
                    }
                }
            },
            ontouchend: move |_| press.set(None),
            ontouchcancel: move |_| press.set(None),
            // A right-click in a desktop browser is the same gesture; and on
            // Android the native long-press menu must not fight the sheet.
            oncontextmenu: move |e: Event<MouseData>| {
                e.prevent_default();
                if !pending {
                    actions_open.set(true);
                }
            },
            // The gutter holds the avatar on the first message of a block and
            // stays empty (but present) on the rest, so every line in a block
            // shares one left edge.
            div { class: "msg-gutter",
                if !compact {
                    Avatar { user: msg.author.clone() }
                }
            }
            div { class: "msg-main",
            // Only the first message of a block names its author; the rest
            // are plainly hers by position.
            if !compact {
                div { class: "msg-head",
                    {
                        let colour = match (members_ctx, tags_ctx) {
                            (Some(m), Some(t)) => name_color(msg.author.id, &m.read(), &t.read()),
                            // No context is not worth a panic; the default
                            // colour is perfectly readable.
                            _ => String::new(),
                        };
                        rsx! {
                            span {
                                class: "msg-author",
                                style: if colour.is_empty() { String::new() } else { format!("color: {colour}") },
                                "{msg.author.username}"
                            }
                        }
                    }
                    span { class: "msg-time", {format_time(msg.created_at)} }
                }
            }
            // A centred dialog, not a bar inside the row: on a tall picture
            // the row starts above the screen, and so did the question (Jon).
            // Touches are stopped at the card so the row underneath doesn't
            // start a long-press timer.
            if confirming_delete() {
                div { class: "confirm-scrim", onclick: move |_| confirming_delete.set(false) }
                div {
                    class: "confirm-card",
                    ontouchstart: move |e| e.stop_propagation(),
                    div { class: "confirm-title", "Delete this message?" }
                    div { class: "confirm-sub", "Everyone loses it. There is no undo." }
                    div { class: "confirm-actions",
                        button {
                            class: "msg-confirm-no",
                            onclick: move |_| confirming_delete.set(false),
                            "Cancel"
                        }
                        button {
                            class: "msg-confirm-yes danger",
                            onclick: move |_| {
                                confirming_delete.set(false);
                                ws.send(ClientEvent::DeleteMessage { message_id: msg_id });
                            },
                            "Delete"
                        }
                    }
                }
            }
            if let Some(draft) = editing() {
                div { class: "msg-edit",
                    textarea {
                        class: "msg-edit-box",
                        rows: "3",
                        value: "{draft}",
                        oninput: move |e| editing.set(Some(e.value())),
                    }
                    div { class: "msg-edit-actions",
                        button {
                            class: "msg-confirm-yes",
                            onclick: move |_| {
                                let text = editing().unwrap_or_default().trim().to_string();
                                // An empty edit is a delete you didn't ask for;
                                // the server ignores it, so we do too.
                                if !text.is_empty() {
                                    ws.send(ClientEvent::EditMessage { message_id: msg_id, content: text });
                                }
                                editing.set(None);
                            },
                            "Save"
                        }
                        button { class: "msg-confirm-no", onclick: move |_| editing.set(None), "Cancel" }
                    }
                }
            }
            if let Some(preview) = msg.reply_preview.clone() {
                div { class: "reply-ref", "↩ {preview.author}: {preview.content.chars().take(60).collect::<String>()}" }
            }
            // Pinning is still a desktop action, but the phone should show
            // which messages someone thought were worth keeping — including
            // on a compact message, which has no header to hang it off.
            if msg.pinned {
                div { class: "reply-ref",
                    Icon { name: "pin", size: 11 }
                    "pinned"
                }
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
                    // When the server said how big it is, the box is drawn at
                    // that size now and the picture fills it when it lands,
                    // so nothing below it moves. A picture from elsewhere
                    // still arrives at whatever size it is.
                    let boxed = msg
                        .media
                        .iter()
                        .find(|m| m.url == src)
                        .map(|m| image_box_style(m.width, m.height, is_sticker));
                    let img_class = match (is_sticker, boxed.is_some()) {
                        (true, _) => "msg-img sticker-msg",
                        (false, true) => "msg-img boxed",
                        (false, false) => "msg-img",
                    };
                    let open = {
                        let src = src.clone();
                        move |_| {
                            // The browser sends a click when the finger
                            // lifts, long-press or not; that one is not
                            // a request to see the picture.
                            if now_ms() - long_pressed_at() < 800 {
                                return;
                            }
                            lightbox.set(Some(Lightbox::within(src.clone(), gallery())))
                        }
                    };
                    rsx! {
                        if let Some(style) = boxed {
                            div { key: "{i}", class: "msg-img-box", style: "{style}",
                                img { class: "{img_class}", src: "{shown}", loading: "lazy", onclick: open }
                            }
                        } else {
                            img { key: "{i}", class: "{img_class}", src: "{shown}", loading: "lazy", onclick: open }
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
            // Deliberately not a retry button. An unsent message is still
            // queued, and now survives a reconnect, so it goes out on its own
            // — offering "try again" would send a second copy the moment the
            // connection came back. Say what is actually true instead.
            if failed {
                div { class: "msg-failed-row", "still sending…" }
            }
            for link in preview_urls(&text) {
                LinkCard { key: "{link}", url: link }
            }
            if !reaction_groups.is_empty() {
                div { class: "reaction-row",
                    for (emoji, count, mine) in reaction_groups {
                        button {
                            key: "{emoji}",
                            class: if mine { "reaction-pill mine" } else { "reaction-pill" },
                            ontouchstart: move |e: Event<TouchData>| {
                                // The pill has its own hold (who reacted); the
                                // row must not open the actions on top of it.
                                e.stop_propagation();
                                pressed_at.set(Some(now_ms()));
                            },
                            onclick: {
                                let emoji = emoji.clone();
                                move |_| {
                                    let held = pressed_at().is_some_and(|at| now_ms() - at >= LONG_PRESS_MS);
                                    pressed_at.set(None);
                                    if held {
                                        reactors_for.set(Some(emoji.clone()));
                                    } else {
                                        ws.send(ClientEvent::ToggleReaction { message_id: msg_id, emoji: emoji.clone() });
                                    }
                                }
                            },
                            "{emoji} {count}"
                        }
                    }
                }
            }
            // Tapped a message: everything you can do to it, in one sheet.
            if actions_open() {
                {
                    // A picture's "text" is its URL, which is not what you
                    // held your finger on. Say what the thing is instead.
                    let preview: String = if text.is_empty() {
                        match (n_images, n_videos, n_files) {
                            (n, _, _) if n > 0 => if n == 1 { "a picture".into() } else { format!("{n} pictures") },
                            (_, n, _) if n > 0 => "a video".into(),
                            (_, _, n) if n > 0 => "a file".into(),
                            _ => msg.content.chars().take(46).collect(),
                        }
                    } else {
                        text.chars().take(46).collect()
                    };
                    let author = msg.author.username.clone();
                    let for_reply = msg.clone();
                    let original = msg.content.clone();
                    let to_copy = msg.content.clone();
                    rsx! {
                        div { class: "scrim", onclick: move |_| actions_open.set(false) }
                        div { class: "sheet",
                            div { class: "sheet-grip" }
                            div { class: "sheet-preview ellipsis", "{author}: {preview}" }
                            div { class: "quick-row",
                                for quick in QUICK_REACTIONS {
                                    button {
                                        key: "q{quick}",
                                        class: "quick-react",
                                        onclick: move |_| {
                                            actions_open.set(false);
                                            ws.send(ClientEvent::ToggleReaction {
                                                message_id: msg_id,
                                                emoji: quick.to_string(),
                                            });
                                        },
                                        "{quick}"
                                    }
                                }
                            }
                            button {
                                class: "sheet-action",
                                onclick: move |_| {
                                    actions_open.set(false);
                                    replying.set(Some(for_reply.clone()));
                                },
                                Icon { name: "reply", size: 18 }
                                span { class: "grow", "Reply" }
                            }
                            button {
                                class: "sheet-action",
                                onclick: move |_| {
                                    actions_open.set(false);
                                    copy_text(&to_copy);
                                },
                                Icon { name: "copy", size: 18 }
                                span { class: "grow", "Copy text" }
                            }
                            button {
                                class: "sheet-action",
                                onclick: move |_| {
                                    actions_open.set(false);
                                    let public = public_url.and_then(|p| p.0());
                                    copy_text(&shared::message_link(
                                        &permalink_base(&public),
                                        channel_id,
                                        msg_id,
                                    ));
                                },
                                Icon { name: "link", size: 18 }
                                span { class: "grow", "Copy link" }
                            }
                            if mine {
                                button {
                                    class: "sheet-action",
                                    onclick: move |_| {
                                        actions_open.set(false);
                                        confirming_delete.set(false);
                                        editing.set(Some(original.clone()));
                                    },
                                    Icon { name: "edit", size: 18 }
                                    span { class: "grow", "Edit" }
                                }
                            }
                            if mine || me_admin {
                                button {
                                    class: "sheet-action danger",
                                    onclick: move |_| {
                                        actions_open.set(false);
                                        editing.set(None);
                                        confirming_delete.set(true);
                                    },
                                    Icon { name: "trash", size: 18 }
                                    span { class: "grow", "Delete" }
                                }
                            }
                        }
                    }
                }
            }
            // Held a pill down: who reacted. The ids came with the message, so
            // this is a lookup in the roster, not a fetch.
            if let Some(emoji) = reactors_for() {
                {
                    let roster = members_ctx.map(|m| m.read().clone()).unwrap_or_default();
                    let names = shared::reactor_names(&msg.reactions, &emoji, &roster, me_id);
                    let people: Vec<(Option<User>, String)> = msg
                        .reactions
                        .iter()
                        .filter(|r| r.emoji == emoji)
                        .map(|r| roster.iter().find(|m| m.user.id == r.user_id).map(|m| m.user.clone()))
                        .zip(names)
                        .collect();
                    rsx! {
                        div { class: "scrim", onclick: move |_| reactors_for.set(None) }
                        div { class: "sheet",
                            div { class: "sheet-grip" }
                            div { class: "sheet-head",
                                span { class: "sheet-title", "{emoji}  {people.len()}" }
                                button {
                                    class: "hbtn",
                                    style: "width: 32px; height: 32px",
                                    onclick: move |_| reactors_for.set(None),
                                    Icon { name: "x", size: 14 }
                                }
                            }
                            for (user, name) in people {
                                div { key: "{name}", class: "member-row",
                                    if let Some(user) = user {
                                        Avatar { user }
                                    }
                                    div { class: "member-col",
                                        div { class: "member-name", "{name}" }
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
