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

WORKDIR /usr/src/app/apps/light-workflow
RUN cargo build --locked --release

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    groupadd --system workflow && \
    useradd --system --gid workflow --home-dir /app --shell /usr/sbin/nologin workflow && \
    mkdir -p /app && \
    chown -R workflow:workflow /app && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder --chown=workflow:workflow /usr/src/app/target/release/light-workflow /app/light-workflow

ENV WORKFLOW_LOG_ANSI=false

EXPOSE 8080

USER workflow:workflow
CMD ["/app/light-workflow"]
