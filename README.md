# NotDiscord

Self-hosted, Discord-style chat for a private group of friends. Full-Rust stack:

- **Client**: [Dioxus](https://dioxuslabs.com) desktop app (Windows), with voice, video, and screen share via the [LiveKit](https://livekit.io) Rust SDK
- **Server**: [axum](https://github.com/tokio-rs/axum) with plain WebSockets for real-time events, SQLite via sqlx
- **Shared**: one crate of protocol types (REST DTOs + WebSocket events) used by both sides, so client and server can never disagree about the wire format
- **Voice/video**: self-hosted LiveKit SFU; RNNoise noise suppression, push-to-talk, and voice-activity gating are done client-side

## What it does

Text channels, DMs (with real privacy — scoped broadcasts), replies, mentions
with autocomplete, full-text search, reactions, custom stickers and tags, GIF
search, file/image/video uploads with retention, profiles and avatars, roles
and moderation, voice channels, private DM calls with ringing, screen share
with a monitor picker, webcam video, in-app auto-updates with a changelog
screen, and a system tray. Built rapidly by a crew that wanted their own
Discord — see [TODO.md](TODO.md) for where it's headed.

## Run your own server

See **[deploy/README.md](deploy/README.md)** — a Docker Compose stack
(server + LiveKit + Caddy with automatic HTTPS) that goes from VPS to
working server in about five minutes.

## Repository layout

```text
crates/
|-- shared/    # Wire types: entities, REST bodies, ClientEvent/ServerEvent enums
|-- server/    # axum REST + WebSocket server, SQLite storage, auth
`-- client/    # Dioxus desktop client (voice.rs, share.rs, camera.rs for A/V)
deploy/        # Self-hosting kit: docker-compose, Caddy, LiveKit config
scripts/       # Release tooling
```

## Development

Prerequisites: Rust (stable, via [rustup](https://rustup.rs)). On Windows you
also need MSVC Build Tools and the WebView2 runtime (preinstalled on
Windows 11).

```bash
cargo run -p server            # defaults to 127.0.0.1:3000, ./notdiscord.db
cargo build --release -p client && target/release/client.exe
```

The client links LiveKit's prebuilt libwebrtc (static CRT), so client debug
builds don't link — use `--release` (the repo's `.cargo/config.toml` sets
`+crt-static`).

Register an account in the client, then chat. The first account on a fresh
server becomes the admin. Open a second client to see real-time delivery.

### Server configuration

| Env var | Default | Purpose |
|---|---|---|
| `NOTDISCORD_ADDR` | `127.0.0.1:3000` | Listen address |
| `NOTDISCORD_DB` | `notdiscord.db` | SQLite database path |
| `NOTDISCORD_UPLOADS` | `uploads` | Upload storage directory |
| `NOTDISCORD_CLIENT_DIR` | `client` | Client build served at `/download` (optional) |
| `NOTDISCORD_INVITE` | *(unset)* | Invite code required to register |
| `NOTDISCORD_GIPHY_KEY` | *(unset)* | GIPHY API key for GIF search (optional) |
| `LIVEKIT_URL` | *(unset)* | LiveKit websocket URL, e.g. `wss://livekit.example.com` |
| `LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` | *(unset)* | LiveKit credentials (voice/video off without them) |
