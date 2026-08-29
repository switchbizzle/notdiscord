//! Per-server credentials: the bot's LLM key, GIF search, SoundCloud HD, and
//! Spotify. Every one of these used to be environment-only, which meant
//! configuring a server required SSH and a restart — fine for us, a wall for
//! anyone self-hosting.
//!
//! Settings stored in `server_meta` win; the environment is the fallback, so
//! an existing deployment keeps working with nothing typed in.

use crate::SharedState;

/// One knob: the settings key it lives under, its environment fallback, and
/// how it introduces itself in the UI.
pub struct Credential {
    pub key: &'static str,
    pub meta_key: &'static str,
    pub env: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const CREDENTIALS: &[Credential] = &[
    Credential {
        key: "openrouter",
        meta_key: "openrouter_key",
        env: "NOTDISCORD_OPENROUTER_KEY",
        label: "OpenRouter API key",
        hint: "Lets the bot answer questions, describe images, and generate art. Get one at openrouter.ai — usage is billed to whoever's key this is.",
    },
    Credential {
        key: "giphy",
        meta_key: "giphy_key",
        env: "NOTDISCORD_GIPHY_KEY",
        label: "GIPHY API key",
        hint: "Powers the GIF button. Free from developers.giphy.com.",
    },
    Credential {
        key: "soundcloud",
        meta_key: "soundcloud_cookie",
        env: "NOTDISCORD_SOUNDCLOUD_COOKIE",
        label: "SoundCloud cookie file",
        hint: "Optional. Paste a cookies.txt from a logged-in SoundCloud account to stream the artist's original file where it's offered. Without it, music plays at SoundCloud's public quality.",
    },
    // Two boxes, because developer.spotify.com shows two values. Asking an
    // admin to join them with a colon was one more thing to get wrong, and
    // it was got wrong the first time it was used.
    Credential {
        key: "spotify_id",
        meta_key: "spotify_client_id",
        env: "NOTDISCORD_SPOTIFY_ID",
        label: "Spotify client ID",
        hint: "Optional. From your app's page on developer.spotify.com — the long code under the app name. Lets Spotify links resolve to the right song; the audio still streams from SoundCloud, because Spotify's API doesn't allow playback through a bot.",
    },
    Credential {
        key: "spotify_secret",
        meta_key: "spotify_client_secret",
        env: "NOTDISCORD_SPOTIFY_SECRET",
        label: "Spotify client secret",
        hint: "The value behind \"View client secret\" on the same page. Saved together with the ID above, and checked against Spotify the moment you save.",
    },
];

fn find(key: &str) -> Option<&'static Credential> {
    CREDENTIALS.iter().find(|c| c.key == key)
}

fn env_value(cred: &Credential) -> Option<String> {
    std::env::var(cred.env).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

async fn stored(state: &SharedState, cred: &Credential) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM server_meta WHERE key = ?")
        .bind(cred.meta_key)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// The value in force: what an admin typed, else what the process was
/// started with, else nothing.
pub async fn get(state: &SharedState, key: &str) -> Option<String> {
    let cred = find(key)?;
    match stored(state, cred).await {
        Some(value) => Some(value),
        None => env_value(cred),
    }
}

/// Store (or clear, with an empty value) one credential.
pub async fn set(state: &SharedState, key: &str, value: &str) -> anyhow::Result<()> {
    let Some(cred) = find(key) else {
        anyhow::bail!("unknown credential");
    };
    let value = value.trim();
    if value.is_empty() {
        sqlx::query("DELETE FROM server_meta WHERE key = ?")
            .bind(cred.meta_key)
            .execute(&state.db)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO server_meta (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(cred.meta_key)
        .bind(value)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// What the settings pane shows: set-or-not and where it came from, never
/// the value itself.
pub async fn statuses(state: &SharedState) -> Vec<shared::CredentialStatus> {
    let mut out = Vec::with_capacity(CREDENTIALS.len());
    for cred in CREDENTIALS {
        let stored = stored(state, cred).await;
        let from_env = stored.is_none() && env_value(cred).is_some();
        out.push(shared::CredentialStatus {
            key: cred.key.to_owned(),
            label: cred.label.to_owned(),
            hint: cred.hint.to_owned(),
            set: stored.is_some() || from_env,
            from_env,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_credential_is_addressable_and_distinct() {
        // The UI addresses these by key and the storage by meta_key; a
        // duplicate in either would silently overwrite another's value.
        for cred in CREDENTIALS {
            assert!(find(cred.key).is_some(), "{} not findable", cred.key);
            assert!(!cred.hint.is_empty(), "{} has no explanation", cred.key);
        }
        let keys: std::collections::HashSet<_> = CREDENTIALS.iter().map(|c| c.key).collect();
        assert_eq!(keys.len(), CREDENTIALS.len(), "duplicate credential key");
        let metas: std::collections::HashSet<_> = CREDENTIALS.iter().map(|c| c.meta_key).collect();
        assert_eq!(metas.len(), CREDENTIALS.len(), "two credentials share a storage key");
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(find("please_give_me_your_secrets").is_none());
    }
}
