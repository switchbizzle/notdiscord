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

## Get the app

**Windows:** download `NotDiscord.exe` from your server's `/download` page
(the crew's is [notdiscord.switchbhost.com/download](https://notdiscord.switchbhost.com/download)).
It's a single exe. Run it and it offers to install itself — a copy in
`%LOCALAPPDATA%\Programs\NotDiscord`, a Start Menu shortcut, and an entry in
Apps & features, which is also how you uninstall it. Say *Not now* and it
runs standalone from wherever you saved it; `NotDiscord.exe --portable` never
asks. Updates arrive in-app either way. Login and settings live in
`%APPDATA%\NotDiscord` and survive both installing and uninstalling.

**Phone:** open `/app` on your server in the phone's browser and add it to the
home screen. It's a PWA with push notifications, voice, uploads and the
lot; the desktop app is the full experience.

### Code signing

Releases are not yet signed, so on first run Windows SmartScreen shows
"Windows protected your PC" (More info → Run anyway), and Defender's
heuristics occasionally quarantine the exe outright. The project has been
opened up under the MIT licence and builds on GitHub Actions
(`.github/workflows/client.yml`) so that releases can be signed for free
through [SignPath Foundation](https://signpath.org); that application is in
progress. If Defender quarantines a copy, report it as a false positive at
<https://www.microsoft.com/en-us/wdsi/filesubmission> — it usually clears in
a day or two.

### Privacy

The desktop client connects only to the NotDiscord server you sign in to
and to the voice server that server hands it. It sends nothing to anyone
else and phones home to no one; the update check asks your own server.
This program will not transfer any information to other networked systems
unless specifically requested by the user or the person installing or
operating it.

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
| `NOTDISCORD_INVITE` | *(unset)* | Invite code seed for the first boot; after that admins manage it in Settings → Server |
| `NOTDISCORD_GIPHY_KEY` | *(unset)* | GIPHY API key for GIF search (optional) |
| `NOTDISCORD_OPENROUTER_KEY` | *(unset)* | OpenRouter API key — lets @NotBot answer questions (optional; release announcements work without it) |
| `NOTDISCORD_BOT_MODEL` | `google/gemini-2.5-flash-lite` | OpenRouter model id NotBot thinks with (Settings → Bot wins over this) |
| `NOTDISCORD_BOT_IMAGE_MODEL` | `google/gemini-2.5-flash-image` | OpenRouter model id `/image` draws with (Settings → Bot wins over this) |
| `NOTDISCORD_PUBLIC_URL` | *(unset)* | Public base URL of this instance — required for the bot to post generated images |
| `NOTDISCORD_MUSIC_URL` | `http://127.0.0.1:3001` | The music sidecar's address (see `Dockerfile.music`); unset it to nowhere = music commands politely fail |
| `NOTDISCORD_LIVEKIT_API_URL` | derived from `LIVEKIT_URL` | LiveKit's HTTP API, used to reconcile the voice roster. Set this when the public URL only proxies `/rtc` (e.g. `http://127.0.0.1:7880`) |
| `LIVEKIT_URL` | *(unset)* | LiveKit websocket URL, e.g. `wss://livekit.example.com` |
| `LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` | *(unset)* | LiveKit credentials (voice/video off without them) |
