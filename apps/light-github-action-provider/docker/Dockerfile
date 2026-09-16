FROM rust:1.98.0-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends cmake g++ pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
COPY crates/ crates/
COPY contracts/ contracts/
COPY frameworks/ frameworks/
COPY apps/ apps/
RUN cargo build --locked --release -p light-github-action-provider

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git && \
    groupadd --system workflow && \
    useradd --system --gid workflow --home-dir /app --shell /usr/sbin/nologin workflow && \
    mkdir -p /app /var/lib/light-github-action-provider/work && \
    chown -R workflow:workflow /app /var/lib/light-github-action-provider && \
    chmod 700 /var/lib/light-github-action-provider /var/lib/light-github-action-provider/work && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder --chown=workflow:workflow /usr/src/app/target/release/light-github-action-provider /app/light-github-action-provider
WORKDIR /app
USER workflow:workflow
CMD ["/app/light-github-action-provider"]
