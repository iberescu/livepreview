# syntax=docker/dockerfile:1
# Builds the service as a member of the PhotoCraft workspace (pinned commit), so the engine
# crates compile with PhotoCraft's own lockfile and release profile (thin LTO, 1 codegen unit).
ARG RUST_VERSION=1.99

FROM rust:${RUST_VERSION}-slim-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ARG PHOTOCRAFT_REPO=https://github.com/storytold/photocraft.git
ARG PHOTOCRAFT_REF=ec350d64aedd019afc5eda290cc32909bc9bab7d
RUN git init -q /photocraft \
    && cd /photocraft \
    && git fetch -q --depth 1 "$PHOTOCRAFT_REPO" "$PHOTOCRAFT_REF" \
    && git checkout -q FETCH_HEAD
COPY Cargo.toml /photocraft/apps/mockup-server/Cargo.toml
COPY src /photocraft/apps/mockup-server/src
COPY web /photocraft/apps/mockup-server/web
WORKDIR /photocraft
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/photocraft/target \
    cargo build --release -p mockup-server \
    && cp target/release/mockup-server /usr/local/bin/mockup-server

FROM debian:bookworm-slim AS runtime
# Fonts for any text the engine has to re-render (Photoshop's own text pixels are used when present).
RUN apt-get update \
    && apt-get install -y --no-install-recommends fonts-dejavu-core fonts-liberation2 curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 mockup
COPY --from=builder /usr/local/bin/mockup-server /usr/local/bin/mockup-server
USER mockup
ENV PSD_DIR=/data/psds PORT=8080 RUST_LOG=info
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=120s CMD curl -fsS http://localhost:8080/health || exit 1
ENTRYPOINT ["/usr/local/bin/mockup-server"]
