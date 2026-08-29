//! Spotify links, turned into something the music bot can actually play.
//!
//! Spotify's API will tell you everything about a song and then refuse to
//! hand over a single sample of it — playback through a bot isn't a thing
//! their licensing allows. So this module uses Spotify only for what it's
//! good at (knowing what a link *is*) and leaves the audio to SoundCloud:
//! every track comes back as a search phrase the sidecar resolves at play
//! time, carrying Spotify's title, artist, art, and duration so the queue
//! reads right and the match can be checked against the real length.

use serde::Deserialize;

use crate::SharedState;

/// How many tracks one link may add. Playlists run to the thousands; nobody
/// means to queue that from one message.
const MAX_TRACKS: usize = 100;

/// What a link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Playlist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: Kind,
    pub id: String,
}

/// One track, ready for the sidecar: what to search SoundCloud for, and the
/// metadata to show while it plays.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Planned {
    pub query: String,
    pub title: String,
    pub artist: String,
    pub art: Option<String>,
    pub duration: Option<f64>,
}

/// True for anything we'd try to resolve, so callers can branch before
/// spending a network round trip.
pub fn is_spotify_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("spotify:")
        || ["open.spotify.com", "play.spotify.com", "spotify.link", "spotify.app.link"]
            .iter()
            .any(|host| lower.starts_with(&format!("https://{host}/")) || lower.starts_with(&format!("http://{host}/")))
}

/// Pull the kind and id out of a share link or URI. Returns None for links
/// that need a redirect first (see `expand_short_link`) and for the parts of
/// Spotify we can't play — artists, shows, episodes.
pub fn parse_link(url: &str) -> Option<Link> {
    // The URI form the desktop app copies: spotify:track:ID
    if let Some(rest) = url.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        let kind = kind_of(parts.next()?)?;
        let id = clean_id(parts.next()?)?;
        return Some(Link { kind, id });
    }

    let after_host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split_once('/')
        .map(|(_, path)| path)?;
    // Localised links carry a segment we don't care about: /intl-de/track/ID
    let mut segments = after_host.split('/').filter(|s| !s.is_empty() && !s.starts_with("intl-"));
    let kind = kind_of(segments.next()?)?;
    let id = clean_id(segments.next()?)?;
    Some(Link { kind, id })
}

fn kind_of(segment: &str) -> Option<Kind> {
    match segment {
        "track" => Some(Kind::Track),
        "album" => Some(Kind::Album),
        "playlist" => Some(Kind::Playlist),
        _ => None,
    }
}

/// Spotify ids are 22 base62 characters; anything else is a link we've
/// misread, and asking the API about it just wastes a round trip.
fn clean_id(segment: &str) -> Option<String> {
    let id = segment.split(['?', '#']).next().unwrap_or(segment);
    let ok = id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| id.to_owned())
}

/// Phone shares are short links that only reveal themselves on a redirect.
async fn expand_short_link(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.contains("spotify.link/") || lower.contains("spotify.app.link/")) {
        return None;
    }
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    Some(resp.url().to_string())
}

// ---------- The API ----------

/// Spotify's two hosts. Overridable so the tests can stand up an API of
/// their own and check what we actually send and parse.
fn accounts_base() -> String {
    std::env::var("NOTDISCORD_SPOTIFY_ACCOUNTS").unwrap_or_else(|_| "https://accounts.spotify.com".into())
}

fn api_base() -> String {
    std::env::var("NOTDISCORD_SPOTIFY_API").unwrap_or_else(|_| "https://api.spotify.com/v1".into())
}

/// Client-credentials token, kept until it expires. Re-fetched whenever the
/// admin changes the credentials, since a token minted with the old pair
/// would keep working right up until it didn't.
struct Token {
    creds: String,
    value: String,
    expires_at: std::time::Instant,
}

static TOKEN: std::sync::Mutex<Option<Token>> = std::sync::Mutex::new(None);

fn cached(creds: &str) -> Option<String> {
    let guard = TOKEN.lock().unwrap();
    let token = guard.as_ref()?;
    (token.creds == creds && token.expires_at > std::time::Instant::now()).then(|| token.value.clone())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

/// The configured pair. Admins fill in two boxes; earlier versions had one
/// box holding "id:secret", and a server set up that way keeps working.
pub async fn credentials(state: &SharedState) -> Option<(String, String)> {
    let id = crate::creds::get(state, "spotify_id").await;
    let secret = crate::creds::get(state, "spotify_secret").await;
    if let (Some(id), Some(secret)) = (id, secret) {
        return Some((id.trim().to_owned(), secret.trim().to_owned()));
    }
    // Legacy single field, read straight from storage since it's no longer
    // in the credential registry.
    let joined: Option<String> = sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'spotify_creds'")
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .or_else(|| std::env::var("NOTDISCORD_SPOTIFY_CREDS").ok());
    let joined = joined?;
    let (id, secret) = joined.trim().split_once(':')?;
    Some((id.trim().to_owned(), secret.trim().to_owned()))
}

async fn token(state: &SharedState) -> anyhow::Result<String> {
    let Some((id, secret)) = credentials(state).await else {
        anyhow::bail!("no spotify credentials — an admin can add them in Server settings → Bot");
    };
    let creds = format!("{id}:{secret}");
    if let Some(hit) = cached(&creds) {
        return Ok(hit);
    }

    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode(&creds);
    let resp = reqwest::Client::new()
        .post(format!("{}/api/token", accounts_base()))
        .header("Authorization", format!("Basic {basic}"))
        .form(&[("grant_type", "client_credentials")])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "Spotify refused those credentials — check the client ID and secret in Server settings → Bot"
        );
    }
    let body: TokenResponse = resp.json().await?;
    // Expire a minute early so a token can't die mid-request.
    let lifetime = std::time::Duration::from_secs(body.expires_in.max(120) - 60);
    *TOKEN.lock().unwrap() = Some(Token {
        creds,
        value: body.access_token.clone(),
        expires_at: std::time::Instant::now() + lifetime,
    });
    Ok(body.access_token)
}

/// Ask Spotify whether the stored credentials work. Used when an admin saves
/// them, so a typo is caught in the settings pane instead of surfacing hours
/// later as "couldn't read that Spotify link".
pub async fn check(state: &SharedState) -> anyhow::Result<()> {
    *TOKEN.lock().unwrap() = None;
    token(state).await.map(|_| ())
}

async fn get(state: &SharedState, path: &str) -> anyhow::Result<serde_json::Value> {
    let token = token(state).await?;
    let resp = reqwest::Client::new()
        .get(format!("{}/{path}", api_base()))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Spotify says how long to wait; pass that on rather than making
        // somebody guess, and never retry inside the request — that is how a
        // rate limit turns into a worse rate limit.
        let wait = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("a moment");
        anyhow::bail!("Spotify is rate limiting us — try again in {wait}s");
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Spotify doesn't have that one (private playlist, or a dead link?)");
    }
    if !resp.status().is_success() {
        anyhow::bail!("spotify said {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Links resolved recently, so reposting the same track — or everyone in the
/// room pasting the same one — costs Spotify nothing. Bounded and dumb on
/// purpose: chat links repeat within minutes or never.
const CACHE_LIMIT: usize = 256;
static RESOLVED: std::sync::Mutex<Vec<(String, std::sync::Arc<Resolved>)>> =
    std::sync::Mutex::new(Vec::new());

fn cached_resolve(url: &str) -> Option<std::sync::Arc<Resolved>> {
    let cache = RESOLVED.lock().unwrap();
    cache.iter().find(|(key, _)| key == url).map(|(_, value)| value.clone())
}

fn remember(url: &str, value: &std::sync::Arc<Resolved>) {
    let mut cache = RESOLVED.lock().unwrap();
    cache.retain(|(key, _)| key != url);
    cache.push((url.to_owned(), value.clone()));
    if cache.len() > CACHE_LIMIT {
        cache.remove(0);
    }
}

// ---------- Reading their JSON ----------

/// One track object, wherever it turned up. Album tracks have no art of
/// their own, so the caller passes the album's down.
fn planned_from(track: &serde_json::Value, fallback_art: Option<&str>) -> Option<Planned> {
    let title = track["name"].as_str().filter(|t| !t.is_empty())?.to_owned();
    let artist = track["artists"]
        .as_array()
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let art = biggest_image(&track["album"]["images"])
        .or_else(|| fallback_art.map(str::to_owned));
    let duration = track["duration_ms"].as_f64().map(|ms| ms / 1000.0);
    // The artist first: SoundCloud's search leans on the uploader's name, and
    // a bare title matches every cover ever posted.
    let query = if artist.is_empty() { title.clone() } else { format!("{artist} {title}") };
    Some(Planned { query, title, artist, art, duration })
}

fn biggest_image(images: &serde_json::Value) -> Option<String> {
    images
        .as_array()?
        .iter()
        .max_by_key(|i| i["width"].as_i64().unwrap_or(0))
        .and_then(|i| i["url"].as_str())
        .map(str::to_owned)
}

/// What a link is worth queueing, in order. The name is the album or
/// playlist's own, so the bot can say what it just added.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub tracks: Vec<Planned>,
    /// True when the link had more tracks than we're willing to queue.
    pub truncated: bool,
}

pub async fn resolve(state: &SharedState, url: &str) -> anyhow::Result<Resolved> {
    // Short links say nothing until they're followed.
    let expanded = expand_short_link(url).await;
    let target = expanded.as_deref().unwrap_or(url);
    let Some(link) = parse_link(target) else {
        anyhow::bail!("that Spotify link isn't a track, album, or playlist");
    };
    // Keyed on what the link resolved to, so the short and long forms of the
    // same track share an entry.
    let cache_key = format!("{:?}:{}", link.kind, link.id);
    if let Some(hit) = cached_resolve(&cache_key) {
        return Ok((*hit).clone());
    }

    let resolved = fetch(state, &link).await?;
    remember(&cache_key, &std::sync::Arc::new(resolved.clone()));
    Ok(resolved)
}

async fn fetch(state: &SharedState, link: &Link) -> anyhow::Result<Resolved> {
    match link.kind {
        Kind::Track => {
            let track = get(state, &format!("tracks/{}", link.id)).await?;
            let planned = planned_from(&track, None)
                .ok_or_else(|| anyhow::anyhow!("spotify returned a track with no name"))?;
            Ok(Resolved { name: planned.title.clone(), tracks: vec![planned], truncated: false })
        }
        Kind::Album => {
            let album = get(state, &format!("albums/{}?limit=50", link.id)).await?;
            let name = album["name"].as_str().unwrap_or("album").to_owned();
            let art = biggest_image(&album["images"]);
            let items = album["tracks"]["items"].as_array().cloned().unwrap_or_default();
            let total = items.len();
            let tracks: Vec<Planned> = items
                .iter()
                .take(MAX_TRACKS)
                .filter_map(|t| planned_from(t, art.as_deref()))
                .collect();
            Ok(Resolved { name, tracks, truncated: total > MAX_TRACKS })
        }
        Kind::Playlist => {
            let playlist = get(state, &format!("playlists/{}?fields=name", link.id)).await?;
            let name = playlist["name"].as_str().unwrap_or("playlist").to_owned();
            let mut tracks = Vec::new();
            let mut truncated = false;
            let mut offset = 0usize;
            loop {
                let page = get(
                    state,
                    &format!("playlists/{}/tracks?limit=50&offset={offset}", link.id),
                )
                .await?;
                let items = page["items"].as_array().cloned().unwrap_or_default();
                let page_len = items.len();
                for item in &items {
                    if tracks.len() >= MAX_TRACKS {
                        truncated = true;
                        break;
                    }
                    // Local files and removed tracks come through as nulls.
                    if let Some(planned) = planned_from(&item["track"], None) {
                        tracks.push(planned);
                    }
                }
                offset += page_len;
                let more = page["next"].is_string() && page_len > 0;
                if !more || tracks.len() >= MAX_TRACKS {
                    truncated = truncated || more;
                    break;
                }
            }
            Ok(Resolved { name, tracks, truncated })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_shape_of_link_people_actually_paste() {
        let track = Link { kind: Kind::Track, id: "4cOdK2wGLETKBW3PvgPWqT".into() };
        for url in [
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc123&utm_source=copy",
            // The phone app localises the path.
            "https://open.spotify.com/intl-de/track/4cOdK2wGLETKBW3PvgPWqT",
            "http://play.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
            // The desktop app's "Copy Spotify URI".
            "spotify:track:4cOdK2wGLETKBW3PvgPWqT",
        ] {
            assert_eq!(parse_link(url).as_ref(), Some(&track), "failed on {url}");
            assert!(is_spotify_url(url), "not recognised: {url}");
        }
        assert_eq!(
            parse_link("https://open.spotify.com/album/1ATL5GLyefJaxhQzSPVrLX"),
            Some(Link { kind: Kind::Album, id: "1ATL5GLyefJaxhQzSPVrLX".into() })
        );
        assert_eq!(
            parse_link("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=x"),
            Some(Link { kind: Kind::Playlist, id: "37i9dQZF1DXcBWIGoYBM5M".into() })
        );
    }

    #[test]
    fn refuses_links_it_cannot_play() {
        // Real Spotify links, but nothing we can turn into audio.
        assert!(parse_link("https://open.spotify.com/artist/0OdUWJ0sBjDrqHygGUXeCF").is_none());
        assert!(parse_link("https://open.spotify.com/episode/512ojhOuo1ktJprKbVcKyQ").is_none());
        assert!(parse_link("https://open.spotify.com/track/short").is_none());
        assert!(parse_link("https://soundcloud.com/artist/track").is_none());
        // A short link parses to nothing until it's been followed.
        assert!(parse_link("https://spotify.link/abc123").is_none());
        assert!(is_spotify_url("https://spotify.link/abc123"), "but it's still ours to expand");
    }

    #[test]
    fn a_track_becomes_a_searchable_phrase() {
        let track = serde_json::json!({
            "name": "My Heart Has Teeth",
            "duration_ms": 240_000,
            "artists": [{ "name": "deadmau5" }, { "name": "Venture 5" }],
            "album": { "images": [
                { "url": "https://i.example/small.jpg", "width": 64 },
                { "url": "https://i.example/big.jpg", "width": 640 },
            ] },
        });
        let planned = planned_from(&track, None).unwrap();
        assert_eq!(planned.title, "My Heart Has Teeth");
        assert_eq!(planned.artist, "deadmau5, Venture 5");
        // Artist first, because a bare title matches every cover on SoundCloud.
        assert_eq!(planned.query, "deadmau5, Venture 5 My Heart Has Teeth");
        assert_eq!(planned.art.as_deref(), Some("https://i.example/big.jpg"));
        assert_eq!(planned.duration, Some(240.0));
    }

    // ---------- Against a stand-in Spotify ----------

    /// A server state with nothing in it but a database, which is all the
    /// credential lookup needs.
    async fn test_state(creds: &str) -> crate::SharedState {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE server_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO server_meta (key, value) VALUES ('spotify_creds', ?)")
            .bind(creds)
            .execute(&db)
            .await
            .unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
        std::sync::Arc::new(crate::AppState {
            db,
            events,
            presence: std::sync::Mutex::new(std::collections::HashMap::new()),
            voice: std::sync::Mutex::new(std::collections::HashMap::new()),
            voice_left: std::sync::Mutex::new(std::collections::HashMap::new()),
            bot: std::sync::Mutex::new(shared::User {
                id: 1,
                username: "NotBot".into(),
                avatar: None,
                role: "member".into(),
            }),
            music_watch: std::sync::Mutex::new(false),
            music_player: std::sync::Mutex::new(None),
            uploads: crate::ratelimit::UploadLimits::default(),
        })
    }

    fn track_json(name: &str, artist: &str, ms: u64) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "duration_ms": ms,
            "artists": [{ "name": artist }],
            "album": { "images": [{ "url": "https://i.example/art.jpg", "width": 640 }] },
        })
    }

    /// Stand in for Spotify: hand out a token, serve one track, one album, and
    /// a playlist long enough to page. Records what we asked for, so the test
    /// can check the credentials went over the wire correctly and that the
    /// token is reused rather than re-minted per request.
    #[tokio::test]
    async fn a_spotify_link_becomes_a_queue() {
        use std::sync::{Arc, Mutex};

        /// A playlist with no end, for the cap.
        const ENDLESS: &str = "3ndl3ssPl4yl1stAAAAAAA";

        #[derive(Default)]
        struct Seen {
            basic_auth: String,
            grant: String,
            tokens_issued: usize,
            bearers: Vec<String>,
            paths: Vec<String>,
        }
        let seen = Arc::new(Mutex::new(Seen::default()));

        let app = {
            let s1 = seen.clone();
            let s2 = seen.clone();
            axum::Router::new()
                .route(
                    "/api/token",
                    axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                        let seen = s1.clone();
                        async move {
                            let mut seen = seen.lock().unwrap();
                            seen.basic_auth = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            seen.grant = body;
                            seen.tokens_issued += 1;
                            axum::Json(serde_json::json!({
                                "access_token": "token-abc",
                                "token_type": "Bearer",
                                "expires_in": 3600,
                            }))
                        }
                    }),
                )
                .route(
                    "/v1/{*path}",
                    axum::routing::get(
                        move |axum::extract::Path(path): axum::extract::Path<String>,
                              raw: axum::extract::RawQuery,
                              headers: axum::http::HeaderMap| {
                            let seen = s2.clone();
                            async move {
                                let query = raw.0.unwrap_or_default();
                                {
                                    let mut seen = seen.lock().unwrap();
                                    seen.bearers.push(
                                        headers
                                            .get("authorization")
                                            .and_then(|v| v.to_str().ok())
                                            .unwrap_or_default()
                                            .to_owned(),
                                    );
                                    seen.paths.push(format!("{path}?{query}"));
                                }
                                let body = if path.starts_with("tracks/") {
                                    track_json("Strobe", "deadmau5", 637_000)
                                } else if path.starts_with("albums/") {
                                    serde_json::json!({
                                        "name": "4x4=12",
                                        "images": [{ "url": "https://i.example/cover.jpg", "width": 640 }],
                                        // Album tracks carry no album of their own.
                                        "tracks": { "items": [
                                            { "name": "Some Chords", "artists": [{ "name": "deadmau5" }], "duration_ms": 480_000 },
                                            { "name": "Raise Your Weapon", "artists": [{ "name": "deadmau5" }], "duration_ms": 400_000 },
                                        ] },
                                    })
                                } else if path.contains("/tracks") {
                                    // Playlist page: 50 the first time, 5 the second.
                                    // ENDLESS never runs out, to exercise the cap.
                                    let offset: usize = query
                                        .split('&')
                                        .find_map(|p| p.strip_prefix("offset="))
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0);
                                    let endless = path.contains(ENDLESS);
                                    let count = if endless || offset == 0 { 50 } else { 5 };
                                    let items: Vec<serde_json::Value> = (0..count)
                                        .map(|i| serde_json::json!({
                                            "track": track_json(&format!("Song {}", offset + i), "Someone", 200_000)
                                        }))
                                        // A removed track comes through as a null.
                                        .chain(std::iter::once(serde_json::json!({ "track": serde_json::Value::Null })))
                                        .collect();
                                    serde_json::json!({
                                        "items": items,
                                        "next": if endless || offset == 0 {
                                            serde_json::json!("more")
                                        } else {
                                            serde_json::Value::Null
                                        },
                                    })
                                } else {
                                    serde_json::json!({ "name": "Jon\'s mix" })
                                };
                                axum::Json(body)
                            }
                        },
                    ),
                )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        std::env::set_var("NOTDISCORD_SPOTIFY_ACCOUNTS", format!("http://{addr}"));
        std::env::set_var("NOTDISCORD_SPOTIFY_API", format!("http://{addr}/v1"));
        *TOKEN.lock().unwrap() = None;
        RESOLVED.lock().unwrap().clear();
        let state = test_state("client-id-42:shhh-secret").await;

        // A single track.
        let resolved = resolve(&state, "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT")
            .await
            .expect("track");
        assert_eq!(resolved.tracks.len(), 1);
        assert_eq!(resolved.tracks[0].title, "Strobe");
        assert_eq!(resolved.tracks[0].query, "deadmau5 Strobe");
        assert_eq!(resolved.tracks[0].duration, Some(637.0));
        assert!(!resolved.truncated);

        // The credentials went over as HTTP Basic, base64 of "id:secret".
        {
            use base64::Engine;
            let seen = seen.lock().unwrap();
            let expected = base64::engine::general_purpose::STANDARD.encode("client-id-42:shhh-secret");
            assert_eq!(seen.basic_auth, format!("Basic {expected}"));
            assert_eq!(seen.grant, "grant_type=client_credentials");
            assert_eq!(seen.bearers[0], "Bearer token-abc");
        }

        // An album: every track inherits the cover, since album tracks have none.
        let album = resolve(&state, "https://open.spotify.com/album/1ATL5GLyefJaxhQzSPVrLX")
            .await
            .expect("album");
        assert_eq!(album.name, "4x4=12");
        assert_eq!(album.tracks.len(), 2);
        assert!(album.tracks.iter().all(|t| t.art.as_deref() == Some("https://i.example/cover.jpg")));

        // A playlist longer than one page, with a removed track in it.
        let playlist = resolve(&state, "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M")
            .await
            .expect("playlist");
        assert_eq!(playlist.tracks.len(), 55, "both pages, minus the removed track on each");
        assert_eq!(playlist.tracks[0].title, "Song 0");
        assert_eq!(playlist.tracks[49].title, "Song 49");
        // The second page starts at 51, not 50: the removed track still holds
        // a position in the playlist, so the offset has to count it.
        assert_eq!(playlist.tracks[50].title, "Song 51");
        assert!(!playlist.truncated);

        // A playlist bigger than we're willing to queue stops at the cap and
        // says so, rather than dropping 3000 songs into the room.
        let huge = resolve(&state, &format!("https://open.spotify.com/playlist/{ENDLESS}"))
            .await
            .expect("endless playlist");
        assert_eq!(huge.tracks.len(), MAX_TRACKS);
        assert!(huge.truncated, "a capped playlist has to admit it");

        // One token for all of that, not one per request.
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.tokens_issued, 1, "the token should be cached");
            assert!(seen.bearers.iter().all(|b| b == "Bearer token-abc"));
            assert!(
                seen.paths.iter().any(|p| p.contains("offset=51")),
                "the second page was never fetched: {:?}",
                seen.paths
            );
        }

        // A link posted twice costs Spotify nothing the second time. This is
        // the whole answer to "are we polling their API too hard".
        let before = seen.lock().unwrap().paths.len();
        let again = resolve(&state, "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT")
            .await
            .expect("track");
        assert_eq!(again.tracks[0].title, "Strobe");
        assert_eq!(
            seen.lock().unwrap().paths.len(),
            before,
            "a repeated link went back to Spotify"
        );
        // The short form of the same track is the same cache entry.
        resolve(&state, "https://open.spotify.com/intl-de/track/4cOdK2wGLETKBW3PvgPWqT?si=x")
            .await
            .expect("track");
        assert_eq!(seen.lock().unwrap().paths.len(), before, "the same track, twice over");

        // Two separate boxes are the supported way in; the single "id:secret"
        // field above is what older servers were set up with, and both got us
        // this far in the same test.
        sqlx::query("DELETE FROM server_meta WHERE key = 'spotify_creds'")
            .execute(&state.db)
            .await
            .unwrap();
        for (key, value) in [("spotify_client_id", "other-id"), ("spotify_client_secret", "other-secret")] {
            sqlx::query("INSERT OR REPLACE INTO server_meta (key, value) VALUES (?, ?)")
                .bind(key)
                .bind(value)
                .execute(&state.db)
                .await
                .unwrap();
        }
        // Changing the credentials must not keep using the old token — and a
        // link we haven't seen has to actually go and ask.
        check(&state).await.expect("the new pair works");
        resolve(&state, "https://open.spotify.com/track/1ATL5GLyefJaxhQzSPVrLX").await.expect("track");
        {
            use base64::Engine;
            let seen = seen.lock().unwrap();
            assert_eq!(seen.tokens_issued, 2, "new credentials, new token");
            let expected = base64::engine::general_purpose::STANDARD.encode("other-id:other-secret");
            assert_eq!(seen.basic_auth, format!("Basic {expected}"));
        }

        std::env::remove_var("NOTDISCORD_SPOTIFY_ACCOUNTS");
        std::env::remove_var("NOTDISCORD_SPOTIFY_API");
        *TOKEN.lock().unwrap() = None;
    }

    #[test]
    fn album_tracks_borrow_the_album_art() {
        // Album track objects carry no album of their own.
        let track = serde_json::json!({ "name": "Track One", "artists": [{ "name": "Someone" }] });
        let planned = planned_from(&track, Some("https://i.example/cover.jpg")).unwrap();
        assert_eq!(planned.art.as_deref(), Some("https://i.example/cover.jpg"));
        assert_eq!(planned.duration, None);
        // Removed or local playlist entries are nulls, and must not queue.
        assert!(planned_from(&serde_json::Value::Null, None).is_none());
        assert!(planned_from(&serde_json::json!({ "name": "" }), None).is_none());
    }
}
