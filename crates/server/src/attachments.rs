//! Reference counting for uploads.
//!
//! An upload belongs to whatever points at it — usually the one message it
//! was posted in, sometimes an avatar, a sticker, or a custom emoji. Deleting
//! a message used to leave its file on disk until the retention sweep noticed
//! it weeks later, which is a long time to keep something someone deleted.
//!
//! Nothing here deletes a file that anything still refers to, and the same
//! reference set is what protects an upload from the retention sweep.

use crate::SharedState;

/// Every place outside a message that can point at an upload. One statement,
/// one column named `url`: the retention sweep and the orphan check read the
/// same query, so a new kind of reference can't protect a file in one place
/// and not the other.
pub const REFERENCE_SOURCES: &str = "SELECT avatar AS url FROM users WHERE avatar IS NOT NULL      UNION ALL SELECT url FROM stickers      UNION ALL SELECT url FROM custom_emojis      UNION ALL SELECT value AS url FROM server_meta WHERE key = 'icon'";

/// The upload key a `/files/…` URL points at: either the 32-hex directory of
/// the current layout, or a legacy `{32hex}.{ext}` file from the first
/// uploads release. Anything else isn't ours to delete.
fn key_of(url_tail: &str) -> Option<&str> {
    let key = url_tail.split(['/', '?', '#', ')', ']', '"', '\'', ' ', '\n']).next()?;
    let stem = key.split('.').next()?;
    let hex = stem.len() == 32 && stem.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    // A legacy name is exactly stem.ext; a directory has no dot at all.
    let shape_ok = key == stem || key.matches('.').count() == 1;
    (hex && shape_ok).then_some(key)
}

/// Every upload a piece of text points at, in the order they appear.
pub fn keys_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, _) in text.match_indices("/files/") {
        if let Some(key) = key_of(&text[idx + "/files/".len()..]) {
            if !out.iter().any(|seen| seen == key) {
                out.push(key.to_owned());
            }
        }
    }
    out
}

/// Is anything still pointing at this upload? `excluding_message` is the
/// message being deleted, whose row may or may not be gone yet.
async fn still_referenced(
    state: &SharedState,
    key: &str,
    excluding_message: i64,
) -> anyhow::Result<bool> {
    let pattern = format!("%/files/{key}%");
    let in_a_message: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE id != ? AND content LIKE ?)",
    )
    .bind(excluding_message)
    .bind(&pattern)
    .fetch_one(&state.db)
    .await?;
    if in_a_message == 1 {
        return Ok(true);
    }
    // The rest is a short list — avatars, stickers, emojis, the icon — so
    // it's cheaper to read it than to ask the database four more questions.
    let needle = format!("/files/{key}");
    let elsewhere: Vec<String> = sqlx::query_scalar(REFERENCE_SOURCES).fetch_all(&state.db).await?;
    if elsewhere.iter().any(|url| url.contains(&needle)) {
        return Ok(true);
    }
    Ok(false)
}

/// Remove the uploads a just-deleted message was the last owner of. Returns
/// how many went. Failure is never fatal — the retention sweep is still
/// there, and a file nobody can reach is not worth failing a delete over.
pub async fn drop_orphans(state: &SharedState, content: &str, message_id: i64) -> usize {
    let mut removed = 0;
    for key in keys_in(content) {
        match still_referenced(state, &key, message_id).await {
            Ok(true) => continue,
            Err(e) => {
                tracing::warn!("could not check references for {key}: {e}");
                continue;
            }
            Ok(false) => {}
        }
        let path = crate::uploads_dir().join(&key);
        // A directory in the current layout, a bare file in the legacy one.
        let gone = if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await.is_ok()
        } else {
            tokio::fs::remove_file(&path).await.is_ok()
        };
        if gone {
            removed += 1;
            tracing::info!("deleted upload {key} with its message");
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_uploads_a_message_points_at() {
        let dir = "b8130593a31d66c77a96d64e97fe26e0";
        let legacy = "0123456789abcdef0123456789abcdef.png";

        assert_eq!(keys_in(&format!("/files/{dir}/holiday.mp4")), vec![dir]);
        // Query strings, markdown, and trailing punctuation all stop the key.
        assert_eq!(keys_in(&format!("/files/{dir}/a.jpg?thumb=1")), vec![dir]);
        assert_eq!(keys_in(&format!("look: ![](/files/{dir}/a.jpg) nice")), vec![dir]);
        assert_eq!(keys_in(&format!("https://host/files/{legacy}")), vec![legacy]);
        // The same file twice is still one file.
        assert_eq!(keys_in(&format!("/files/{dir}/a.jpg /files/{dir}/a.jpg")).len(), 1);
        // Two different ones, in order.
        assert_eq!(
            keys_in(&format!("/files/{dir}/a.jpg and /files/{legacy}")),
            vec![dir.to_owned(), legacy.to_owned()]
        );
    }

    /// A state on a real schema, so the reference query is the one that ships.
    async fn test_state() -> crate::SharedState {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
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
            started_at: std::time::Instant::now(),
        })
    }

    /// Put a file on disk the way an upload does, and return its URL.
    fn place_upload(root: &std::path::Path, key: &str, name: &str) -> String {
        let dir = root.join(key);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), b"pretend this is a video").unwrap();
        format!("/files/{key}/{name}")
    }

    /// The schema's own #general is channel 1; messages need an author too.
    async fn seed(state: &crate::SharedState) {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at) VALUES (1, 'poster', 'x', 0)")
            .execute(&state.db)
            .await
            .unwrap();
    }

    async fn post(state: &crate::SharedState, id: i64, content: &str) {
        sqlx::query(
            "INSERT INTO messages (id, channel_id, author_id, content, created_at) \
             VALUES (?, 1, 1, ?, 0)",
        )
        .bind(id)
        .bind(content)
        .execute(&state.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn deleting_a_message_takes_its_attachment_with_it() {
        let root = std::env::temp_dir().join(format!("nd-orphans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("NOTDISCORD_UPLOADS", &root);
        let state = test_state().await;
        seed(&state).await;

        let lonely = place_upload(&root, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "holiday.mp4");
        let shared_twice = place_upload(&root, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "meme.jpg");
        let an_emoji = place_upload(&root, "cccccccccccccccccccccccccccccccc", "wat.png");
        let an_avatar = place_upload(&root, "dddddddddddddddddddddddddddddddd", "me.png");

        post(&state, 10, &format!("look at this {lonely} and {shared_twice}")).await;
        // Someone else posted the same image, and two other files are in use
        // as decoration — none of those may go.
        post(&state, 11, &format!("reposting {shared_twice}")).await;
        post(&state, 12, &format!("here it is again {an_emoji}")).await;
        sqlx::query("INSERT INTO custom_emojis (name, url, creator_id, created_at) VALUES ('wat', ?, 1, 0)")
            .bind(&an_emoji)
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users (id, username, password_hash, avatar, created_at) VALUES (2, 'someone', 'x', ?, 0)")
            .bind(&an_avatar)
            .execute(&state.db)
            .await
            .unwrap();

        // Delete message 10, the way the websocket handler does.
        let content = format!("look at this {lonely} and {shared_twice}");
        sqlx::query("DELETE FROM messages WHERE id = 10").execute(&state.db).await.unwrap();
        let removed = drop_orphans(&state, &content, 10).await;

        assert_eq!(removed, 1, "only the file nothing else points at");
        assert!(!root.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").exists(), "the lonely file should be gone");
        assert!(root.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").exists(), "another message still posts it");
        assert!(root.join("cccccccccccccccccccccccccccccccc").exists(), "it's an emoji");
        assert!(root.join("dddddddddddddddddddddddddddddddd").exists(), "it's an avatar");

        // Now delete the repost too: the last reference is gone, so the file is.
        sqlx::query("DELETE FROM messages WHERE id = 11").execute(&state.db).await.unwrap();
        let removed = drop_orphans(&state, &format!("reposting {shared_twice}"), 11).await;
        assert_eq!(removed, 1);
        assert!(!root.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").exists());

        // And deleting the message that used the emoji leaves the emoji alone.
        sqlx::query("DELETE FROM messages WHERE id = 12").execute(&state.db).await.unwrap();
        assert_eq!(drop_orphans(&state, &format!("here it is again {an_emoji}"), 12).await, 0);
        assert!(root.join("cccccccccccccccccccccccccccccccc").exists());

        std::env::remove_var("NOTDISCORD_UPLOADS");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_anything_that_is_not_an_upload_key() {
        // Traversal, wrong length, wrong alphabet, uppercase hex, and a
        // nested path where the key should be — none of these may ever
        // become a path we delete.
        for tail in [
            "../../etc/passwd",
            "..",
            "short/a.jpg",
            "b8130593a31d66c77a96d64e97fe26e0extra/a.jpg",
            "B8130593A31D66C77A96D64E97FE26E0/a.jpg",
            "zz130593a31d66c77a96d64e97fe26e0/a.jpg",
            "0123456789abcdef0123456789abcdef.tar.gz",
        ] {
            assert!(keys_in(&format!("/files/{tail}")).is_empty(), "accepted {tail:?}");
        }
        assert!(keys_in("no links here").is_empty());
    }
}
