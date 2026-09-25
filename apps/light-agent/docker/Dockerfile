# syntax=docker/dockerfile:1
# Stage 1: Build the Rust binary
# Pin to a specific version for reproducibility as suggested by Copilot
FROM rust:1.98.0-bookworm AS builder
ARG CARGO_CACHE_ID=warm

# Install musl tools for static linking
RUN apt-get update && \
    apt-get install -y --no-install-recommends musl-tools pkg-config && \
    rm -rf /var/lib/apt/lists/* && \
    rustup target add x86_64-unknown-linux-musl

WORKDIR /usr/src/app

# Copy workspace root configuration
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/

# Copy the required crates, frameworks, and apps
# Note: This Dockerfile expects to be built from the workspace root context.
# Use specific directory copies to avoid bringing in unnecessary files
COPY contracts/ contracts/
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/

# Build the release binary within the light-agent directory
WORKDIR /usr/src/app/apps/light-agent
RUN --mount=type=cache,id=light-fabric-rust198-registry-${CARGO_CACHE_ID},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-git-${CARGO_CACHE_ID},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-target-${CARGO_CACHE_ID}-app-musl,target=/usr/src/app/target,sharing=locked \
    cargo build --locked --release --target x86_64-unknown-linux-musl \
    && mkdir -p /out \
    && cp /usr/src/app/target/x86_64-unknown-linux-musl/release/light-agent /out/light-agent

# Stage 2: Create the minimal runtime image
FROM debian:bookworm-slim

# Deployment preflight requires independent session cleanup and structured WS errors.
LABEL io.lightapi.agent.session-lifecycle="1"
LABEL io.lightapi.agent.claude-personal="phase2"

# Install runtime dependencies and create a non-root user
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    groupadd --system agent && \
    useradd --system --gid agent --home-dir /app --shell /usr/sbin/nologin agent && \
    # Create /config and symlink it to /app/config so the binary can find it.
    # Bundled templates live in /app/config-defaults and are overlaid by /config.
    mkdir -p /app /config /app/config-defaults && \
    ln -s /config /app/config && \
    chown -R agent:agent /app /config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder --chown=agent:agent /out/light-agent /app/light-agent
COPY --from=builder --chown=agent:agent /usr/src/app/apps/light-agent/config/*.yml /app/config-defaults/

# Expose the correct agent port (aligned with config/server.yml or values.yml)
EXPOSE 8083

# Run the binary
USER agent:agent
CMD ["/app/light-agent"]
