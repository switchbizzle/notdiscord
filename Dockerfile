# NotDiscord server image. Build from the repo root:
#   docker build -t notdiscord-server .
#
# Two build stages, because a self-hosted server is expected to serve both the
# API and the phone web app. The wasm bundle is baked into the image rather
# than mounted, so there's nothing for an operator to build by hand.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p server

FROM rust:1-bookworm AS web
# Pinned to the version this repo is developed against; dx and dioxus move
# together and a mismatch fails at build time rather than at runtime.
RUN rustup target add wasm32-unknown-unknown \
    && cargo install dioxus-cli --version 0.7.10 --locked
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cd crates/webclient && dx build --platform web --release
# Stage exactly what the deploy script ships: the bundle, plus the hand-written
# PWA files (service worker, manifest, glue) that dx doesn't know about.
RUN mkdir -p /webapp \
    && cp -r target/dx/webclient/release/web/public/. /webapp/ \
    && cp crates/webclient/pwa/* /webapp/ \
    # The install prompt fires once and won't wait for wasm to boot, so it's
    # caught in the document head — same line release-webapp.ps1 injects.
    && sed -i 's|<head>|<head>\n        <script>window.__ndInstallEvent=null;window.addEventListener("beforeinstallprompt",function(e){e.preventDefault();window.__ndInstallEvent=e;});</script>|' /webapp/index.html

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/server /usr/local/bin/notdiscord-server
COPY --from=web /webapp /usr/share/notdiscord/webapp
ENV NOTDISCORD_DB=/data/notdiscord.db \
    NOTDISCORD_UPLOADS=/data/uploads \
    NOTDISCORD_CLIENT_DIR=/data/client \
    NOTDISCORD_WEBAPP_DIR=/usr/share/notdiscord/webapp \
    NOTDISCORD_ADDR=0.0.0.0:3000
VOLUME /data
EXPOSE 3000
CMD ["notdiscord-server"]
