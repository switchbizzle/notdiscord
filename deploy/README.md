# Self-hosting NotDiscord

Run your own NotDiscord server with Docker. You need:

- A Linux box reachable from the internet (any small VPS works)
- Docker with the compose plugin
- A domain with **two DNS A records** pointing at the box, e.g.
  `chat.example.com` (the server) and `livekit.example.com` (voice/video)

## Quickstart

```bash
git clone https://github.com/switchbizzle/notdiscord.git
cd notdiscord/deploy
cp .env.example .env
nano .env          # domains, invite code, two random LiveKit strings
docker compose up -d --build
```

That's it. Caddy fetches TLS certificates automatically the first time the
domains are hit, so give it a minute on first boot.

- **First account to register becomes the admin/owner.** Sign up promptly.
- Signup requires the invite code from `.env` — share it with your people.
- All state lives in `deploy/data/` (SQLite database + uploads). Back up
  that folder and you've backed up the server.

## Getting the client

The desktop client is a single `NotDiscord.exe`. Grab it from any existing
NotDiscord server's `https://<server>/download`, or build it yourself on
Windows with `cargo build --release -p client`.

In the client's server picker, add your server as `https://chat.example.com`
(your `DOMAIN`). The client's multi-server rail means people can be on your
server and their friends' servers at once.

### Serving client auto-updates (optional)

If you drop these three files into `deploy/client/`, your server will offer
downloads and auto-updates to its members:

- `NotDiscord.exe` — the client build
- `version.txt` — its version, e.g. `0.24.0`
- `changelog.json` — copied from the repo root

Without them the server runs fine; clients just won't see update banners
from your instance.

## Voice/video ports

LiveKit needs UDP `50000-50100` and TCP `7881` open in your firewall/cloud
security group, in addition to `80`/`443` for the web tier. If a
participant's NAT is hostile they'll fall back to TCP 7881 automatically.

## Updating the server

```bash
cd notdiscord && git pull
cd deploy && docker compose up -d --build
```

Database migrations run automatically on boot.
