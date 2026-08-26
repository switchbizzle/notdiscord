# NotDiscord server image. Build from the repo root:
#   docker build -t notdiscord-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/server /usr/local/bin/notdiscord-server
ENV NOTDISCORD_DB=/data/notdiscord.db \
    NOTDISCORD_UPLOADS=/data/uploads \
    NOTDISCORD_CLIENT_DIR=/data/client \
    NOTDISCORD_ADDR=0.0.0.0:3000
VOLUME /data
EXPOSE 3000
CMD ["notdiscord-server"]
