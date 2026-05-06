# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin v8-session-manager \
    && cp target/release/v8-session-manager /usr/local/bin/v8-session-manager

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m -d /var/lib/v8-session-manager v8 \
    && mkdir -p /var/lib/v8-session-manager/work /etc/v8-session-manager \
    && chown -R v8:v8 /var/lib/v8-session-manager

COPY --from=builder /usr/local/bin/v8-session-manager /usr/local/bin/v8-session-manager
COPY docker/v8project.yaml /etc/v8-session-manager/v8project.yaml

USER v8
WORKDIR /var/lib/v8-session-manager
EXPOSE 4000 4001

ENTRYPOINT ["/usr/local/bin/v8-session-manager", \
    "--config", "/etc/v8-session-manager/v8project.yaml", \
    "--bind", "0.0.0.0:4000", \
    "--mcp-http", "0.0.0.0:4001"]
