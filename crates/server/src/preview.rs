//! Link previews. The server fetches a posted URL, scrapes its OpenGraph
//! tags, caches the result (thumbnail included) and hands clients a card —
//! so nobody's IP ever reaches the site they linked, and a link posted in a
//! busy channel is fetched once, not once per person.

use std::net::IpAddr;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use serde::Deserialize;
use sqlx::Row;
use tokio::sync::Semaphore;

use shared::LinkPreview;

use crate::auth::{err, internal, ApiResult, AuthUser};
use crate::{now_ms, SharedState};

/// Cards are refetched after this long (comfortably shorter than the shortest
/// upload retention window, so a cached row never outlives its thumbnail).
const CACHE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_HTML_BYTES: usize = 512 * 1024;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// A handful of slow remote hosts shouldn't be able to tie up the server.
static FETCH_GATE: Semaphore = Semaphore::const_new(4);

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub url: String,
}

/// GET /api/preview?url=… — the card for one link, or 404 if it has none.
pub async fn preview(
    State(state): State<SharedState>,
    _user: AuthUser,
    Query(q): Query<PreviewQuery>,
) -> ApiResult<Json<LinkPreview>> {
    let url = q.url.trim().to_owned();
    if url.len() > 2048 {
        return Err(err(StatusCode::BAD_REQUEST, "url too long"));
    }
    let parsed = reqwest::Url::parse(&url).map_err(|_| err(StatusCode::BAD_REQUEST, "not a url"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(err(StatusCode::BAD_REQUEST, "unsupported scheme"));
    }

    if let Some(cached) = load_cached(&state, &url).await? {
        return respond(cached);
    }

    let _permit = FETCH_GATE.acquire().await.map_err(internal)?;
    match build_preview(&state, &parsed).await {
        Ok(fetched) => {
            store_cached(&state, &url, fetched.as_ref()).await?;
            respond(fetched)
        }
        // Couldn't reach the site: don't cache the failure, it may be a blip.
        Err(e) => {
            tracing::debug!("no preview for {url}: {e}");
            respond(None)
        }
    }
}

fn respond(preview: Option<LinkPreview>) -> ApiResult<Json<LinkPreview>> {
    match preview {
        Some(preview) => Ok(Json(preview)),
        None => Err(err(StatusCode::NOT_FOUND, "no preview for that link")),
    }
}

// ---------- Cache ----------

/// Outer None: nothing usable cached. Inner None: cached "this link has no
/// preview", so we don't hammer the site on every render.
async fn load_cached(state: &SharedState, url: &str) -> ApiResult<Option<Option<LinkPreview>>> {
    let row = sqlx::query(
        "SELECT ok, title, description, image, site_name, embed, embed_height \
         FROM link_previews WHERE url = ? AND fetched_at > ?",
    )
    .bind(url)
    .bind(now_ms() - CACHE_TTL_MS)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;

    let Some(row) = row else { return Ok(None) };
    let ok: i64 = row.get(0);
    if ok == 0 {
        return Ok(Some(None));
    }

    // Retention could have swept the thumbnail already; drop it rather than
    // handing out a broken image.
    let image: Option<String> = row.get(3);
    let image = match image {
        Some(path) if thumbnail_exists(&path).await => Some(path),
        _ => None,
    };

    Ok(Some(Some(LinkPreview {
        url: url.to_owned(),
        title: row.get(1),
        description: row.get(2),
        image,
        site_name: row.get(4),
        embed: row.get(5),
        embed_height: row.get(6),
    })))
}

async fn thumbnail_exists(path: &str) -> bool {
    let mut file = crate::uploads_dir();
    for part in path.trim_start_matches("/files/").split('/') {
        file.push(part);
    }
    tokio::fs::metadata(file).await.is_ok()
}

async fn store_cached(state: &SharedState, url: &str, preview: Option<&LinkPreview>) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO link_previews \
           (url, ok, title, description, image, site_name, embed, embed_height, fetched_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(url) DO UPDATE SET ok = excluded.ok, title = excluded.title, \
           description = excluded.description, image = excluded.image, \
           site_name = excluded.site_name, embed = excluded.embed, \
           embed_height = excluded.embed_height, fetched_at = excluded.fetched_at",
    )
    .bind(url)
    .bind(i64::from(preview.is_some()))
    .bind(preview.map(|p| p.title.clone()).unwrap_or_default())
    .bind(preview.map(|p| p.description.clone()).unwrap_or_default())
    .bind(preview.and_then(|p| p.image.clone()))
    .bind(preview.map(|p| p.site_name.clone()).unwrap_or_default())
    .bind(preview.and_then(|p| p.embed.clone()))
    .bind(preview.map(|p| p.embed_height).unwrap_or_default())
    .bind(now_ms())
    .execute(&state.db)
    .await
    .map_err(internal)?;
    Ok(())
}

// ---------- Fetching ----------

async fn build_preview(state: &SharedState, url: &reqwest::Url) -> anyhow::Result<Option<LinkPreview>> {
    // A page that won't be scraped isn't fatal: media hosts (YouTube shows
    // bots a consent wall) still answer their oEmbed endpoint.
    let scraped = fetch_html(url).await;
    let failure = scraped.as_ref().err().map(ToString::to_string);
    let (final_url, meta) = match scraped {
        Ok((final_url, html)) => (final_url, parse_meta(&html)),
        Err(_) => (url.clone(), Meta::default()),
    };

    let (embed, embed_height) = match embed_for(&final_url) {
        Some((embed, height)) => (Some(embed), height),
        None => (None, 0),
    };
    let host = final_url.host_str().unwrap_or_default().trim_start_matches("www.");
    let mut title = clamp(pick(&[&meta.og_title, &meta.title]), 200);
    let mut description = clamp(pick(&[&meta.og_description, &meta.description]), 400);
    let mut site_name = clamp(pick(&[&meta.site_name, host]), 80);
    let mut image_src = meta.image;

    if title.is_empty() {
        if let Some(oembed) = fetch_oembed(&final_url).await {
            title = clamp(&oembed.title, 200);
            if description.is_empty() && !oembed.author.is_empty() {
                description = clamp(&oembed.author, 400);
            }
            if !oembed.provider.is_empty() {
                site_name = clamp(&oembed.provider, 80);
            }
            image_src = image_src.or(oembed.thumbnail);
        }
    }

    let image = match image_src.as_deref() {
        Some(src) => cache_thumbnail(state, &final_url, src).await,
        None => None,
    };

    if title.is_empty() && description.is_empty() && image.is_none() && embed.is_none() {
        // Distinguish "nothing to show" (worth caching) from "couldn't look"
        // (a blip shouldn't stick for a week).
        return match failure {
            Some(e) => Err(anyhow::anyhow!(e)),
            None => Ok(None),
        };
    }
    Ok(Some(LinkPreview {
        url: url.to_string(),
        title,
        description,
        image,
        site_name,
        embed,
        embed_height,
    }))
}

struct OEmbed {
    title: String,
    author: String,
    thumbnail: Option<String>,
    provider: String,
}

/// oEmbed gives media hosts a machine-readable answer even when their HTML is
/// a bot wall.
async fn fetch_oembed(url: &reqwest::Url) -> Option<OEmbed> {
    let host = url.host_str()?.to_lowercase();
    let target = percent_encode(url.as_str());
    let endpoint = match host.trim_start_matches("www.") {
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be" => {
            format!("https://www.youtube.com/oembed?format=json&url={target}")
        }
        "soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com" => {
            format!("https://soundcloud.com/oembed?format=json&url={target}")
        }
        "vimeo.com" | "player.vimeo.com" => {
            format!("https://vimeo.com/api/oembed.json?url={target}")
        }
        "open.spotify.com" => format!("https://open.spotify.com/oembed?url={target}"),
        _ => return None,
    };

    let (_, resp) = fetch_guarded(&reqwest::Url::parse(&endpoint).ok()?, "application/json")
        .await
        .ok()?;
    let body = read_capped(resp, 64 * 1024).await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some(OEmbed {
        title: json["title"].as_str().unwrap_or_default().to_owned(),
        author: json["author_name"].as_str().unwrap_or_default().to_owned(),
        thumbnail: json["thumbnail_url"].as_str().map(str::to_owned),
        provider: json["provider_name"].as_str().unwrap_or_default().to_owned(),
    })
}

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            // Redirects are followed by hand so every hop gets address-checked.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Mozilla/5.0 (compatible; NotDiscordBot/1.0; +link preview)")
            .build()
            .expect("http client")
    })
}

/// GET with hand-followed redirects, refusing to touch private addresses at
/// every hop (a redirect policy can't run async DNS, so it can't do this).
async fn fetch_guarded(
    start: &reqwest::Url,
    accept: &str,
) -> anyhow::Result<(reqwest::Url, reqwest::Response)> {
    let mut url = start.clone();
    for _ in 0..MAX_REDIRECTS {
        guard_public(&url).await?;
        let resp = client().get(url.clone()).header(ACCEPT, accept).send().await?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect with no location"))?;
            url = url.join(location)?;
            continue;
        }
        if !status.is_success() {
            anyhow::bail!("remote returned {status}");
        }
        return Ok((url, resp));
    }
    anyhow::bail!("too many redirects")
}

async fn fetch_html(url: &reqwest::Url) -> anyhow::Result<(reqwest::Url, String)> {
    let (final_url, resp) = fetch_guarded(url, "text/html,application/xhtml+xml").await?;
    let content_type = header(&resp, CONTENT_TYPE);
    if !content_type.is_empty() && !content_type.contains("html") {
        anyhow::bail!("not a web page ({content_type})");
    }
    let bytes = read_capped(resp, MAX_HTML_BYTES).await?;
    Ok((final_url, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Download the card image and keep it with the normal uploads, so it counts
/// against the storage cap and expires with everything else.
async fn cache_thumbnail(state: &SharedState, base: &reqwest::Url, src: &str) -> Option<String> {
    let url = base.join(src).ok()?;
    let (_, resp) = fetch_guarded(&url, "image/*").await.ok()?;

    let content_type = header(&resp, CONTENT_TYPE);
    let ext = match content_type.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };

    let bytes = read_capped(resp, MAX_IMAGE_BYTES).await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    if crate::routes::uploads_size().await + bytes.len() as i64
        > crate::routes::storage_cap_bytes(state).await
    {
        return None;
    }

    let mut id = [0u8; 16];
    getrandom::fill(&mut id).ok()?;
    let id = hex::encode(id);
    let dir = crate::uploads_dir().join(&id);
    tokio::fs::create_dir_all(&dir).await.ok()?;
    tokio::fs::write(dir.join(format!("preview.{ext}")), &bytes).await.ok()?;
    Some(format!("/files/{id}/preview.{ext}"))
}

async fn read_capped(mut resp: reqwest::Response, cap: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() >= cap {
            break;
        }
    }
    out.truncate(cap);
    Ok(out)
}

fn header(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase()
}

// ---------- Address guard (SSRF) ----------

/// Refuse to fetch anything that resolves inside the server's own network.
async fn guard_public(url: &reqwest::Url) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("unsupported scheme");
    }
    let host = url.host_str().unwrap_or_default();
    if host.is_empty() {
        anyhow::bail!("no host");
    }

    // IPv6 literals arrive bracketed.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match is_public(ip) {
            true => Ok(()),
            false => Err(anyhow::anyhow!("refusing to fetch a private address")),
        };
    }

    let lower = host.to_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        anyhow::bail!("refusing to fetch a local host name");
    }

    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<_> = tokio::net::lookup_host((lower.as_str(), port)).await?.collect();
    if addrs.is_empty() {
        anyhow::bail!("host does not resolve");
    }
    if !addrs.iter().all(|addr| is_public(addr.ip())) {
        anyhow::bail!("refusing to fetch a private address");
    }
    Ok(())
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                // carrier-grade NAT and reserved space
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80) // link local
        }
    }
}

// ---------- Inline players ----------

/// (player url, height) for the media hosts we can play in the chat window.
fn embed_for(url: &reqwest::Url) -> Option<(String, i64)> {
    let host = url.host_str()?.to_lowercase();
    let host = host.trim_start_matches("www.");
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();

    match host {
        "soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com" => {
            // A bare profile (/artist) has nothing to play; /artist/track and
            // /artist/sets/album do.
            if segments.len() < 2 {
                return None;
            }
            let player = format!(
                "https://w.soundcloud.com/player/?url={}&color=%23ff5500&auto_play=false\
                 &hide_related=true&show_comments=false&show_teaser=false&visual=false",
                percent_encode(url.as_str())
            );
            Some((player, if segments[1] == "sets" { 300 } else { 166 }))
        }
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be" => {
            let id = youtube_id(url, host, &segments)?;
            Some((format!("https://www.youtube-nocookie.com/embed/{id}"), 248))
        }
        "open.spotify.com" => {
            let (kind, id) = (segments.first()?, segments.get(1)?);
            if !matches!(*kind, "track" | "album" | "playlist" | "episode" | "show" | "artist") {
                return None;
            }
            if !is_id(id, 32) {
                return None;
            }
            let tall = matches!(*kind, "album" | "playlist" | "artist" | "show");
            Some((
                format!("https://open.spotify.com/embed/{kind}/{id}"),
                if tall { 352 } else { 152 },
            ))
        }
        "vimeo.com" | "player.vimeo.com" => {
            let id = segments.iter().find(|s| s.chars().all(|c| c.is_ascii_digit()))?;
            Some((format!("https://player.vimeo.com/video/{id}"), 248))
        }
        _ => None,
    }
}

fn youtube_id(url: &reqwest::Url, host: &str, segments: &[&str]) -> Option<String> {
    let id = if host == "youtu.be" {
        segments.first().map(|s| (*s).to_owned())?
    } else if matches!(segments.first().copied(), Some("shorts") | Some("embed") | Some("live")) {
        segments.get(1).map(|s| (*s).to_owned())?
    } else {
        url.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.into_owned())?
    };
    is_id(&id, 24).then_some(id)
}

fn is_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 16);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------- OpenGraph scraping ----------

#[derive(Default)]
struct Meta {
    title: String,
    og_title: String,
    description: String,
    og_description: String,
    image: Option<String>,
    site_name: String,
}

/// A deliberately small scraper: find `<meta>` tags and `<title>`, ignore the
/// rest of the document. Nothing here is rendered as markup by the client, so
/// a sloppy match can only produce a bad-looking card, never markup injection.
fn parse_meta(html: &str) -> Meta {
    let mut meta = Meta::default();
    // ASCII-lowercased so byte offsets still line up with the original.
    let lower = html.to_ascii_lowercase();

    if let Some(start) = lower.find("<title") {
        if let Some(open) = lower[start..].find('>') {
            let from = start + open + 1;
            if let Some(len) = lower[from..].find("</title>") {
                meta.title = decode_entities(html[from..from + len].trim());
            }
        }
    }

    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<meta") {
        let start = cursor + offset;
        let Some(len) = lower[start..].find('>') else { break };
        let tag = &html[start..start + len];
        cursor = start + len + 1;

        let key = attr(tag, "property")
            .or_else(|| attr(tag, "name"))
            .unwrap_or_default()
            .to_lowercase();
        let Some(content) = attr(tag, "content").filter(|c| !c.trim().is_empty()) else {
            continue;
        };

        match key.as_str() {
            "og:title" => set_once(&mut meta.og_title, content),
            "twitter:title" => set_once(&mut meta.og_title, content),
            "og:description" => set_once(&mut meta.og_description, content),
            "twitter:description" => set_once(&mut meta.og_description, content),
            "description" => set_once(&mut meta.description, content),
            "og:site_name" => set_once(&mut meta.site_name, content),
            "og:image" | "og:image:secure_url" | "og:image:url" | "twitter:image" => {
                if meta.image.is_none() {
                    meta.image = Some(content);
                }
            }
            _ => {}
        }
    }
    meta
}

/// First tag of a kind wins — pages repeat og:image for every size variant.
fn set_once(slot: &mut String, value: String) {
    if slot.is_empty() {
        *slot = value;
    }
}

/// The value of one attribute in a tag, entity-decoded.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(offset) = lower[from..].find(name) {
        let at = from + offset;
        from = at + name.len();
        let preceded_by_space = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let after = lower[from..].trim_start();
        if !preceded_by_space || !after.starts_with('=') {
            continue;
        }
        let eq = from + lower[from..].find('=')?;
        let value = tag[eq + 1..].trim_start();
        let raw = match value.chars().next()? {
            quote @ ('"' | '\'') => {
                let inner = &value[1..];
                &inner[..inner.find(quote)?]
            }
            _ => value.split_whitespace().next()?,
        };
        return Some(decode_entities(raw));
    }
    None
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity.to_ascii_lowercase().as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" => Some('\''),
            "nbsp" => Some(' '),
            other => other
                .strip_prefix('#')
                .and_then(|num| match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn pick<'a>(candidates: &[&'a str]) -> &'a str {
    candidates.iter().copied().find(|c| !c.trim().is_empty()).unwrap_or_default()
}

fn clamp(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrapes_opengraph_tags() {
        let html = r#"<html><head><title>Fallback &amp; Co</title>
            <meta property="og:title" content="Real Title">
            <meta name="description" content="A &quot;quoted&quot; blurb">
            <meta property="og:image" content="//cdn.example/art.jpg"/>
            <meta property="og:site_name" content='SoundCloud'>
            </head><body>ignored</body></html>"#;
        let meta = parse_meta(html);
        assert_eq!(meta.title, "Fallback & Co");
        assert_eq!(meta.og_title, "Real Title");
        assert_eq!(meta.description, "A \"quoted\" blurb");
        assert_eq!(meta.image.as_deref(), Some("//cdn.example/art.jpg"));
        assert_eq!(meta.site_name, "SoundCloud");
    }

    #[test]
    fn builds_player_urls() {
        let sc = reqwest::Url::parse("https://soundcloud.com/artist/some-track").unwrap();
        let (embed, height) = embed_for(&sc).unwrap();
        assert!(embed.starts_with("https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud"));
        assert_eq!(height, 166);

        let profile = reqwest::Url::parse("https://soundcloud.com/artist").unwrap();
        assert!(embed_for(&profile).is_none());

        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1s",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
        ] {
            let url = reqwest::Url::parse(link).unwrap();
            assert_eq!(
                embed_for(&url).unwrap().0,
                "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ"
            );
        }

        let spotify =
            reqwest::Url::parse("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT").unwrap();
        assert_eq!(
            embed_for(&spotify).unwrap().0,
            "https://open.spotify.com/embed/track/4cOdK2wGLETKBW3PvgPWqT"
        );

        let news = reqwest::Url::parse("https://example.com/story").unwrap();
        assert!(embed_for(&news).is_none());
    }

    #[test]
    fn blocks_private_addresses() {
        for ip in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "169.254.169.254", "::1", "::ffff:10.0.0.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }
}
