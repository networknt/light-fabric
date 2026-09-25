# syntax=docker/dockerfile:1
# Stage 1: Build the Rust binary
FROM rust:1.98.0-bookworm AS builder
ARG CARGO_CACHE_ID=warm

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

WORKDIR /usr/src/app/apps/light-workflow
RUN --mount=type=cache,id=light-fabric-rust198-registry-${CARGO_CACHE_ID},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-git-${CARGO_CACHE_ID},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-target-${CARGO_CACHE_ID}-app-gnu,target=/usr/src/app/target,sharing=locked \
    cargo build --locked --release \
    && mkdir -p /out \
    && cp /usr/src/app/target/release/light-workflow /out/light-workflow

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl git && \
    groupadd --system workflow && \
    useradd --system --gid workflow --home-dir /app --shell /usr/sbin/nologin workflow && \
    mkdir -p /app /var/lib/light-workflow/evidence && \
    chown -R workflow:workflow /app /var/lib/light-workflow && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder --chown=workflow:workflow /out/light-workflow /app/light-workflow

ENV WORKFLOW_LOG_ANSI=false

EXPOSE 8436

USER workflow:workflow
CMD ["/app/light-workflow"]
