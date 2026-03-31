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
    cargo build --release --locked && \
    install -Dm755 /app/target/release/stratumbee /out/stratumbee

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libzmq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/stratumbee

COPY --from=build /out/stratumbee /usr/local/bin/stratumbee
COPY config ./config
COPY assets ./assets

EXPOSE 3333 8080

ENTRYPOINT ["stratumbee"]
CMD ["--config", "/opt/stratumbee/config/stratumbee.toml"]
