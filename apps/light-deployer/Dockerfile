# syntax=docker/dockerfile:1

FROM rust:1.98.0-bookworm AS builder
ARG CARGO_CACHE_ID=warm
WORKDIR /usr/src/light-fabric

RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY patches patches
COPY crates crates
COPY frameworks frameworks
COPY apps apps

RUN --mount=type=cache,id=light-fabric-rust198-registry-${CARGO_CACHE_ID},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-git-${CARGO_CACHE_ID},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-target-${CARGO_CACHE_ID}-fabric-gnu,target=/usr/src/light-fabric/target,sharing=locked \
    cargo build --locked --release -p light-deployer \
    && mkdir -p /out \
    && cp /usr/src/light-fabric/target/release/light-deployer /out/light-deployer

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd -r -u 10001 -g root light

COPY --from=builder /out/light-deployer /usr/local/bin/light-deployer
COPY --from=builder /usr/src/light-fabric/apps/light-deployer/config /app/config

USER 10001
WORKDIR /app
EXPOSE 7088
ENTRYPOINT ["/usr/local/bin/light-deployer"]
