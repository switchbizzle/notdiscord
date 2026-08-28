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
nano .env          # two domains, two random strings for LiveKit
docker compose up -d --build
```

The first build takes a while — it compiles the server and the web app from
source — and Caddy fetches TLS certificates the first time your domains are
hit, so give it a minute after that.

Then open `https://chat.example.com/app` in a browser. Nobody has an account
yet, so it offers to **set the server up**: name it, create your account —
which is the admin — and choose the invite code everyone else will need.
That's the whole setup. There is no config file to edit afterwards.

Everything else is configured from inside the app, under **Server settings**:
the invite code, how long uploads are kept, the storage cap, the bot's
personality and model, and the bot's keys (see below).

## What's included

- **The server** — chat, voice, video, files, the works
- **The web app** at `/app`, built into the image. It installs to a phone's
  home screen and does push notifications, so your people don't need to
  install anything from a store
- **The music bot**, which joins a voice channel and streams SoundCloud (and
  Spotify links, resolved to their audio) on request
- **LiveKit** for voice and video, and **Caddy** for automatic HTTPS

## The bot's keys

All optional, all set in the app under **Server settings → Bot** — no file
editing, no restart:

| Key | What it turns on |
| --- | --- |
| OpenRouter | `@NotBot` answering questions, describing images, drawing |
| GIPHY | the GIF button |
| SoundCloud | a `cookies.txt` for higher-quality music |
| Spotify | `id:secret`, so Spotify links resolve to the right song |

Each feature simply stays quiet until its key is there. They're stored on
your server and never shown again once saved.

## The desktop client

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
- `version.txt` — its version, e.g. `0.68.0`
- `changelog.json` — copied from the repo root

Without them the server runs fine; the web app works either way, and clients
just won't see update banners from your instance.

## Voice/video ports

LiveKit needs UDP `50000-50100` and TCP `7881` open in your firewall/cloud
security group, in addition to `80`/`443` for the web tier. If a
participant's NAT is hostile they'll fall back to TCP 7881 automatically.

## Backups

All state lives in `deploy/data/` — the SQLite database and the uploads.
Back up that folder and you've backed up the server.

## Updating

```bash
cd notdiscord && git pull
cd deploy && docker compose up -d --build
```

Database migrations run automatically on boot.
