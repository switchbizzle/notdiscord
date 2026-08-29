//! Web Push for the phone app: RFC 8291 message encryption (aes128gcm) and
//! RFC 8292 VAPID authentication, spoken directly to whatever push service
//! the browser hands us — Google's for Chrome, Mozilla's for Firefox, Apple's
//! for Safari. No third-party service, no account, no SDK: the server signs
//! with a keypair it generates once and keeps in server_meta.
//!
//! The `web-push` crate would do this too, but it pulls in OpenSSL; these are
//! all RustCrypto crates, so the server stays pure Rust and portable.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdh::EphemeralSecret;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use p256::{PublicKey, SecretKey};
use sha2::Sha256;
use sqlx::Row;

use crate::SharedState;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// One browser's push channel, straight from the PushSubscription JSON.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub endpoint: String,
    /// The browser's public key (uncompressed P-256 point, 65 bytes).
    pub p256dh: Vec<u8>,
    /// 16-byte shared auth secret.
    pub auth: Vec<u8>,
}

/// The server's VAPID identity, created on first use and reused forever —
/// browsers tie existing subscriptions to the public key that made them.
pub struct Vapid {
    secret: SecretKey,
    /// Uncompressed public point, base64url — the browser needs this to
    /// subscribe, and it travels in the Authorization header.
    pub public_b64: String,
}

pub async fn vapid(state: &SharedState) -> anyhow::Result<Vapid> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'vapid_key'")
            .fetch_optional(&state.db)
            .await?;
    let secret = match existing {
        Some(pem) => SecretKey::from_pkcs8_pem(&pem)?,
        None => {
            let secret = SecretKey::random(&mut rand_core::OsRng);
            let pem = secret.to_pkcs8_pem(Default::default())?.to_string();
            sqlx::query("INSERT OR REPLACE INTO server_meta (key, value) VALUES ('vapid_key', ?)")
                .bind(&pem)
                .execute(&state.db)
                .await?;
            secret
        }
    };
    let public_b64 = B64.encode(secret.public_key().to_sec1_bytes());
    Ok(Vapid { secret, public_b64 })
}

/// The signed token a push service demands before it will deliver anything.
fn vapid_header(vapid: &Vapid, endpoint: &str, subject: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(endpoint)?;
    let audience = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().ok_or_else(|| anyhow::anyhow!("endpoint has no host"))?
    );
    let claims = serde_json::json!({
        "aud": audience,
        // Twelve hours: long enough to be reused, short enough to matter.
        "exp": crate::now_ms() / 1000 + 12 * 3600,
        "sub": subject,
    });
    let key = jsonwebtoken::EncodingKey::from_ec_der(vapid.secret.to_pkcs8_der()?.as_bytes());
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    let token = jsonwebtoken::encode(&header, &claims, &key)?;
    Ok(format!("vapid t={token}, k={}", vapid.public_b64))
}

/// RFC 8291: encrypt `plaintext` for one subscription, producing the
/// aes128gcm body the push service forwards verbatim.
fn encrypt(sub: &Subscription, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ua_public = PublicKey::from_sec1_bytes(&sub.p256dh)?;
    let ephemeral = EphemeralSecret::random(&mut rand_core::OsRng);
    let as_public = ephemeral.public_key().to_sec1_bytes().to_vec();
    let shared = ephemeral.diffie_hellman(&ua_public);

    // The pseudo-random key mixes the ECDH secret with both public keys, so
    // a message can only be read by the browser that subscribed.
    let mut key_info = Vec::from(&b"WebPush: info\0"[..]);
    key_info.extend_from_slice(&sub.p256dh);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&sub.auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("hkdf ikm"))?;

    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("os rng: {e}"))?;
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow::anyhow!("hkdf cek"))?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow::anyhow!("hkdf nonce"))?;

    // 0x02 marks the last (only) record; everything is one record here.
    let mut padded = plaintext.to_vec();
    padded.push(0x02);
    let ciphertext = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow::anyhow!("aes key"))?
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &padded, aad: &[] })
        .map_err(|_| anyhow::anyhow!("aes encrypt"))?;

    // Header: salt | record size | key id length | key id (our public key).
    let mut body = Vec::with_capacity(21 + as_public.len() + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Deliver one notification. Returns Ok(false) when the subscription is dead
/// (the browser was uninstalled, permission revoked) so the caller can forget
/// it rather than retrying forever.
pub async fn send(
    vapid: &Vapid,
    sub: &Subscription,
    payload: &serde_json::Value,
    subject: &str,
) -> anyhow::Result<bool> {
    let body = encrypt(sub, payload.to_string().as_bytes())?;
    let auth = vapid_header(vapid, &sub.endpoint, subject)?;
    let resp = reqwest::Client::new()
        .post(&sub.endpoint)
        .header("Authorization", auth)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", "86400")
        .header("Urgency", "high")
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(false);
    }
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        anyhow::bail!("push service said {status}: {}", detail.chars().take(200).collect::<String>());
    }
    Ok(true)
}

/// Everyone who should get a phone notification for this message: subscribed,
/// not the author, not currently connected (an open app is its own
/// notification), and asking for this kind of message.
pub async fn recipients(
    state: &SharedState,
    message: &shared::Message,
    mentioned: &[i64],
    dm_members: Option<&[i64]>,
) -> anyhow::Result<Vec<(i64, Subscription)>> {
    let rows = sqlx::query(
        "SELECT s.user_id, s.endpoint, s.p256dh, s.auth, COALESCE(u.notify_level, 'mentions') \
         FROM push_subscriptions s JOIN users u ON u.id = s.user_id",
    )
    .fetch_all(&state.db)
    .await?;

    // Whoever muted this channel doesn't want it on their phone either.
    let muted: std::collections::HashSet<i64> =
        sqlx::query_scalar::<_, i64>("SELECT user_id FROM channel_mutes WHERE channel_id = ?")
            .bind(message.channel_id)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect();

    let mut out = Vec::new();
    for row in rows {
        let user_id: i64 = row.get(0);
        let level: String = row.get(4);
        if muted.contains(&user_id) {
            continue;
        }
        // Presence deliberately isn't consulted. It counts a user as "here"
        // when ANY of their devices holds a socket, and the desktop client
        // lives in the tray all day — so switchb's phone was silenced from
        // the moment their PC booted, which is every waking hour. A push is
        // addressed to one device; whether it should appear is a question
        // only that device can answer, and its service worker does (it stays
        // quiet when one of its own windows is open and focused).
        if user_id == message.author.id || level == "none" {
            continue;
        }
        // A DM only notifies its participants, whatever their level says.
        if let Some(members) = dm_members {
            if !members.contains(&user_id) {
                continue;
            }
        } else if level == "mentions" && !mentioned.contains(&user_id) {
            continue;
        }
        let (endpoint, p256dh, auth): (String, String, String) = (row.get(1), row.get(2), row.get(3));
        let (Ok(p256dh), Ok(auth)) = (B64.decode(&p256dh), B64.decode(&auth)) else {
            continue;
        };
        out.push((user_id, Subscription { endpoint, p256dh, auth }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state_with_people() -> crate::SharedState {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        for (id, name, level) in [
            (1, "author", "all"),
            (2, "atdesk", "all"),
            (3, "mentions_only", "mentions"),
            (4, "silenced", "none"),
        ] {
            sqlx::query("INSERT INTO users (id, username, password_hash, created_at, notify_level) VALUES (?, ?, 'x', 0, ?)")
                .bind(id)
                .bind(name)
                .bind(level)
                .execute(&db)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO push_subscriptions (user_id, endpoint, p256dh, auth, created_at) \
                 VALUES (?, ?, 'BBBB', 'AAAA', 0)",
            )
            .bind(id)
            .bind(format!("https://push.example/{name}"))
            .execute(&db)
            .await
            .unwrap();
        }
        let (events, _) = tokio::sync::broadcast::channel(16);
        std::sync::Arc::new(crate::AppState {
            db,
            events,
            presence: std::sync::Mutex::new(std::collections::HashMap::new()),
            voice: std::sync::Mutex::new(std::collections::HashMap::new()),
            voice_left: std::sync::Mutex::new(std::collections::HashMap::new()),
            bot: std::sync::Mutex::new(shared::User {
                id: 99,
                username: "NotBot".into(),
                avatar: None,
                role: "member".into(),
            }),
            music_watch: std::sync::Mutex::new(false),
            music_player: std::sync::Mutex::new(None),
            uploads: crate::ratelimit::UploadLimits::default(),
        })
    }

    fn message_from(author_id: i64, content: &str) -> shared::Message {
        shared::Message {
            id: 1,
            channel_id: 1,
            author: shared::User {
                id: author_id,
                username: "author".into(),
                avatar: None,
                role: "member".into(),
            },
            content: content.into(),
            created_at: 0,
            edited_at: None,
            reactions: Vec::new(),
            reply_to: None,
            reply_preview: None,
            pinned: false,
        }
    }

    #[tokio::test]
    async fn another_device_being_awake_does_not_silence_the_phone() {
        let state = state_with_people().await;
        // "atdesk" has the desktop client open — a socket in presence. That
        // used to suppress every push they had, forever, because the desktop
        // sits in the tray all day (switchb's phone, silent since v0.63.0).
        state.presence.lock().unwrap().insert(2, 1);

        let got = recipients(&state, &message_from(1, "anyone about?"), &[], None).await.unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();

        assert!(ids.contains(&2), "a phone must still buzz while the desktop is open");
        assert!(!ids.contains(&1), "never notify the person who just typed it");
        assert!(!ids.contains(&3), "mentions-only, and this mentions nobody");
        assert!(!ids.contains(&4), "they asked for nothing");
    }

    #[tokio::test]
    async fn a_muted_channel_reaches_nobody_who_muted_it() {
        let state = state_with_people().await;
        // #general is channel 1, seeded by the first migration; "atdesk" has
        // had enough of it.
        sqlx::query("INSERT INTO channel_mutes (user_id, channel_id, muted_at) VALUES (2, 1, 0)")
            .execute(&state.db)
            .await
            .unwrap();

        // Even a direct mention stays quiet: silencing a channel is the whole
        // point, and a mention is exactly what would otherwise get through.
        let got = recipients(&state, &message_from(1, "oi @atdesk"), &[2, 3], None).await.unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&2), "a muted channel still pushed");
        assert!(ids.contains(&3), "and it silenced somebody who didn't mute it");
    }

    #[tokio::test]
    async fn level_and_dm_membership_still_decide() {
        let state = state_with_people().await;
        // A mention reaches the mentions-only account.
        let got = recipients(&state, &message_from(1, "oi @mentions_only"), &[3], None).await.unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&3) && ids.contains(&2));
        assert!(!ids.contains(&4), "\"nothing\" outranks being mentioned");

        // A DM reaches its participants and nobody else, whatever their level.
        let got = recipients(&state, &message_from(1, "just us"), &[], Some(&[1, 4])).await.unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, Vec::<i64>::new(), "4 asked for no notifications at all");
        let got = recipients(&state, &message_from(1, "just us"), &[], Some(&[1, 3])).await.unwrap();
        let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![3], "a DM pings its participant regardless of level");
    }

    fn fake_subscription() -> (Subscription, SecretKey) {
        let ua_secret = SecretKey::random(&mut rand_core::OsRng);
        let mut auth = [0u8; 16];
        getrandom::fill(&mut auth).unwrap();
        (
            Subscription {
                endpoint: "https://push.example.com/abc".into(),
                p256dh: ua_secret.public_key().to_sec1_bytes().to_vec(),
                auth: auth.to_vec(),
            },
            ua_secret,
        )
    }

    #[test]
    fn encrypted_body_has_the_rfc8188_shape() {
        let (sub, _) = fake_subscription();
        let body = encrypt(&sub, b"{\"title\":\"hi\"}").expect("encrypt");
        // salt(16) + rs(4) + idlen(1) + key(65) + ciphertext(>=17)
        assert!(body.len() > 16 + 4 + 1 + 65, "body too short: {}", body.len());
        assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 4096, "record size");
        assert_eq!(body[20], 65, "key id length should be an uncompressed point");
        // The embedded key must be a real point, and fresh every time.
        let key_a = &body[21..86];
        assert!(PublicKey::from_sec1_bytes(key_a).is_ok(), "embedded key isn't a P-256 point");
        let body_b = encrypt(&sub, b"{\"title\":\"hi\"}").unwrap();
        assert_ne!(key_a, &body_b[21..86], "ephemeral key was reused across messages");
        assert_ne!(body[0..16], body_b[0..16], "salt was reused across messages");
    }

    /// The browser's half of RFC 8291, used by both tests below.
    fn browser_decrypt(ua_secret: &SecretKey, sub: &Subscription, body: &[u8]) -> Vec<u8> {
        let salt = &body[0..16];
        let as_public_bytes = &body[21..86];
        let ciphertext = &body[86..];
        let as_public = PublicKey::from_sec1_bytes(as_public_bytes).unwrap();
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
        let mut key_info = Vec::from(&b"WebPush: info\0"[..]);
        key_info.extend_from_slice(&sub.p256dh);
        key_info.extend_from_slice(as_public_bytes);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&sub.auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
        let mut nonce = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
        Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad: &[] })
            .expect("browser could not decrypt our payload")
    }

    /// Stand in for Google/Mozilla's push service: capture one delivery and
    /// hand back what arrived, so the whole send path is exercised —
    /// encryption, VAPID header, HTTP shape.
    #[tokio::test]
    async fn a_push_service_receives_a_readable_delivery() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<(Vec<u8>, String, String, String)>>> = Arc::new(Mutex::new(None));
        let app = {
            let seen = seen.clone();
            axum::Router::new().route(
                "/push/device-1",
                axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let seen = seen.clone();
                    async move {
                        let get = |k: &str| {
                            headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
                        };
                        *seen.lock().unwrap() = Some((
                            body.to_vec(),
                            get("authorization"),
                            get("content-encoding"),
                            get("ttl"),
                        ));
                        axum::http::StatusCode::CREATED
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let ua_secret = SecretKey::random(&mut rand_core::OsRng);
        let mut auth = [0u8; 16];
        getrandom::fill(&mut auth).unwrap();
        let sub = Subscription {
            endpoint: format!("http://{addr}/push/device-1"),
            p256dh: ua_secret.public_key().to_sec1_bytes().to_vec(),
            auth: auth.to_vec(),
        };
        let secret = SecretKey::random(&mut rand_core::OsRng);
        let vapid = Vapid {
            public_b64: B64.encode(secret.public_key().to_sec1_bytes()),
            secret,
        };
        let payload = serde_json::json!({"title": "JunkfoodJon · #general", "body": "@tester get in here"});

        let delivered = send(&vapid, &sub, &payload, "mailto:admin@example.com").await.unwrap();
        assert!(delivered, "send reported the subscription as dead");

        let (body, auth_header, encoding, ttl) = seen.lock().unwrap().clone().expect("nothing arrived");
        assert_eq!(encoding, "aes128gcm", "push services require this exact encoding");
        assert_eq!(ttl, "86400");
        assert!(auth_header.starts_with("vapid t="), "missing VAPID token: {auth_header}");
        assert!(auth_header.contains(", k="), "missing VAPID public key");
        // A push service verifies the JWT; at minimum it must be three parts.
        let token = auth_header
            .trim_start_matches("vapid t=")
            .split(',')
            .next()
            .unwrap();
        assert_eq!(token.split('.').count(), 3, "VAPID token isn't a JWT");

        let opened = browser_decrypt(&ua_secret, &sub, &body);
        let text = String::from_utf8(opened[..opened.len() - 1].to_vec()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["title"], "JunkfoodJon · #general");
        assert_eq!(json["body"], "@tester get in here");
    }

    #[test]
    fn the_browser_can_actually_decrypt_it() {
        // Full round trip: derive the same key the browser would and open
        // the box. If this passes, the crypto matches RFC 8291.
        let (sub, ua_secret) = fake_subscription();
        let plaintext = b"{\"title\":\"JunkfoodJon\",\"body\":\"get in here\"}";
        let body = encrypt(&sub, plaintext).unwrap();

        let salt = &body[0..16];
        let as_public_bytes = &body[21..86];
        let ciphertext = &body[86..];
        let as_public = PublicKey::from_sec1_bytes(as_public_bytes).unwrap();

        // The browser's side of the ECDH.
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
        let mut key_info = Vec::from(&b"WebPush: info\0"[..]);
        key_info.extend_from_slice(&sub.p256dh);
        key_info.extend_from_slice(as_public_bytes);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&sub.auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
        let mut nonce = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();

        let opened = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad: &[] })
            .expect("browser could not decrypt our payload");
        assert_eq!(&opened[..opened.len() - 1], plaintext, "payload didn't survive the round trip");
        assert_eq!(*opened.last().unwrap(), 0x02, "missing last-record delimiter");
    }
}
