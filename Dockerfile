# syntax=docker/dockerfile:1.7

FROM rust:1.85-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libzmq3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin stratumbee && \
    install -Dm755 /app/target/release/stratumbee /out/stratumbee

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    gosu \
    libzmq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/stratumbee

RUN useradd --system --home /opt/stratumbee --shell /usr/sbin/nologin stratumbee \
    && mkdir -p /opt/stratumbee/var \
    && chown -R stratumbee:stratumbee /opt/stratumbee

COPY --from=build /out/stratumbee /usr/local/bin/stratumbee
COPY --chown=stratumbee:stratumbee config ./config
COPY --chown=stratumbee:stratumbee assets ./assets
COPY deploy/docker-entrypoint.sh /usr/local/bin/stratumbee-entrypoint

RUN chmod 0755 /usr/local/bin/stratumbee-entrypoint

ENV RUST_LOG=info

EXPOSE 3333 3334 8080

ENTRYPOINT ["stratumbee-entrypoint"]
CMD ["--config", "/opt/stratumbee/config/stratumbee.toml"]
