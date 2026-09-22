# Stage 1: Build the Rust binary
FROM rust:1.98.0-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake g++ pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

# This Dockerfile expects the workspace root as the build context.
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
COPY crates/ crates/
COPY contracts/ contracts/
COPY frameworks/ frameworks/
COPY apps/ apps/

WORKDIR /usr/src/app/apps/light-identity-issuer
RUN cargo build --locked --release --bin light-identity-issuer-service

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    groupadd --system issuer && \
    useradd --system --gid issuer --home-dir /app --shell /usr/sbin/nologin issuer && \
    mkdir -p /app /config /data && \
    chown -R issuer:issuer /app /config /data && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder --chown=issuer:issuer /usr/src/app/target/release/light-identity-issuer-service /app/light-identity-issuer-service

ENV ISSUER_CONFIG_PATH=/config/issuer.yml

EXPOSE 9443

USER issuer:issuer
CMD ["/app/light-identity-issuer-service"]
