# NotDiscord

Discord-style chat app focused on self-hosted text and voice communication.

This repository currently contains the Electron desktop client and project planning docs. The FastAPI backend and infra services are planned in upcoming phases.

## Project Status

- Current: Electron + React desktop client scaffolded and buildable
- Next: Auth, real-time text chat, and backend API integration
- Planned: LiveKit voice chat and optional web client

## Stack

- Desktop client: Electron, React, TypeScript, Vite
- Real-time client transport: Socket.IO client
- Voice client SDK: LiveKit
- Planned backend: FastAPI, python-socketio, MariaDB, Redis

## Repository Layout

```text
.
|-- chat-client/               # Electron + React app
|-- phases/                    # Phase-by-phase implementation docs
|-- project-overview.md        # Architecture and scope
`-- server-setup.sh            # Server bootstrap notes/scripts
```

## Quick Start (Desktop Client)

### Prerequisites

- Node.js 20 LTS+
- npm 10+

### Install

```bash
cd chat-client
npm install
```

### Run in development

```bash
npm run dev
```

### Build

```bash
# Windows installer
npm run build:win

# macOS
npm run build:mac

# Linux
npm run build:linux
```

## Planned Features

- Account auth (register/login with JWT)
- Server/channel model
- Real-time text chat
- DMs, typing indicators, reactions, mentions
- Voice channels via LiveKit + coturn

## Docs

- Architecture: [project-overview.md](project-overview.md)
- Phase plan: [phases/phase-1-project-setup.md](phases/phase-1-project-setup.md)

## Notes

- This project is intended for private/self-hosted deployment.
- Keep secrets out of git (API keys, DB credentials, TURN secrets, cert files).
