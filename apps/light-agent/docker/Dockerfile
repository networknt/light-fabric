# Stage 1: Build the Rust binary
# Pin to a specific version for reproducibility as suggested by Copilot
FROM rust:1.94.1-bookworm AS builder

# Install musl tools for static linking
RUN apt-get update && \
    apt-get install -y --no-install-recommends musl-tools pkg-config && \
    rm -rf /var/lib/apt/lists/* && \
    rustup target add x86_64-unknown-linux-musl

WORKDIR /usr/src/app

# Copy the required crates from the workspace
# Note: This Dockerfile expects to be built from the workspace root context.
# Use specific directory copies to avoid bringing in unnecessary files
COPY light-rs/ light-rs/
COPY light-axum/ light-axum/
COPY light-agent/ light-agent/

# Build the release binary within the light-agent directory
WORKDIR /usr/src/app/light-agent
RUN cargo build --locked --release --target x86_64-unknown-linux-musl

# Stage 2: Create the minimal runtime image
FROM debian:bookworm-slim

# Install runtime dependencies and create a non-root user
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    groupadd --system agent && \
    useradd --system --gid agent --home-dir /app --shell /usr/sbin/nologin agent && \
    # Create /config and symlink it to /app/config so the binary can find it
    mkdir -p /app /config && \
    ln -s /config /app/config && \
    chown -R agent:agent /app /config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder --chown=agent:agent /usr/src/app/light-agent/target/x86_64-unknown-linux-musl/release/agent /app/light-agent

# Expose the correct agent port (aligned with config/server.yml)
EXPOSE 8081

# Run the binary
USER agent:agent
CMD ["/app/light-agent"]
