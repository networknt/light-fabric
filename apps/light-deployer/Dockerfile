# syntax=docker/dockerfile:1

FROM rust:1.94.1-bookworm AS builder
WORKDIR /usr/src/light-fabric

RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY frameworks frameworks
COPY apps/light-deployer apps/light-deployer

RUN cargo build --release -p light-deployer

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd -r -u 10001 -g root light

COPY --from=builder /usr/src/light-fabric/target/release/light-deployer /usr/local/bin/light-deployer
COPY --from=builder /usr/src/light-fabric/apps/light-deployer/config /app/config

USER 10001
WORKDIR /app
EXPOSE 7088
ENTRYPOINT ["/usr/local/bin/light-deployer"]
