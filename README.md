# NotDiscord

Self-hosted, Discord-style chat for a private group of friends. Full-Rust stack:

- **Client**: [Dioxus](https://dioxuslabs.com) desktop app (Rust UI components, rendered in the system WebView)
- **Server**: [axum](https://github.com/tokio-rs/axum) with plain WebSockets for real-time events, SQLite via sqlx
- **Shared**: one crate of protocol types (REST DTOs + WebSocket events) used by both sides, so client and server can never disagree about the wire format
- **Voice (planned)**: [LiveKit](https://livekit.io) self-hosted SFU via the official LiveKit Rust SDK

## Repository layout

```text
crates/
|-- shared/    # Wire types: entities, REST bodies, ClientEvent/ServerEvent enums
|-- server/    # axum REST + WebSocket server, SQLite storage, auth
`-- client/    # Dioxus desktop client
phases/        # Original phase-by-phase planning docs (written for the old
               # Electron/FastAPI stack -- feature checklist still applies)
```

## Development

Prerequisites: Rust (stable, via [rustup](https://rustup.rs)). On Windows you also need MSVC Build Tools and the WebView2 runtime (preinstalled on Windows 11).

Run the server (defaults to `127.0.0.1:3000`, SQLite file `notdiscord.db` in the working directory):

```bash
cargo run -p server
```

Run the desktop client:

```bash
cargo run -p client
```

Register an account in the client, then chat. Open a second client to see real-time delivery.

### Server configuration

| Env var | Default | Purpose |
|---|---|---|
| `NOTDISCORD_ADDR` | `127.0.0.1:3000` | Listen address |
| `NOTDISCORD_DB` | `notdiscord.db` | SQLite database path |

### Client configuration

| Env var | Default | Purpose |
|---|---|---|
| `NOTDISCORD_SERVER` | `http://127.0.0.1:3000` | Prefilled server URL on the login screen |

## Status

- [x] Accounts (argon2 password hashing, opaque session tokens)
- [x] Channels (create, list)
- [x] Real-time text chat over WebSockets with message history
- [ ] DMs, typing indicators, presence
- [ ] Message editing/deletion, reactions, mentions, attachments
- [ ] Voice channels (LiveKit Rust SDK + self-hosted LiveKit/coturn)
- [ ] Deployment guide for the Virtualmin server (single static binary + nginx reverse proxy for WSS)

## Notes

- Intended for private/self-hosted deployment; registration is open, so keep the server behind your own domain/firewall and share the URL only with friends.
- For remote use, terminate TLS at nginx and the client will speak `https`/`wss` automatically when given an `https://` server URL.
