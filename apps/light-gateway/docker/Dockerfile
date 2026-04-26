# Stage 1: Build the Rust binary
FROM rust:1.94.1-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake g++ pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

# This Dockerfile expects the workspace root as the build context.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/

WORKDIR /usr/src/app/apps/light-gateway
RUN cargo build --locked --release

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    groupadd --system gateway && \
    useradd --system --gid gateway --home-dir /app --shell /usr/sbin/nologin gateway && \
    mkdir -p /app /config && \
    ln -s /config /app/config && \
    chown -R gateway:gateway /app /config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder --chown=gateway:gateway /usr/src/app/target/release/light-gateway /app/light-gateway

EXPOSE 8080 8443

USER gateway:gateway
CMD ["/app/light-gateway"]
