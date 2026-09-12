# Build with the light-fabric workspace root as context.
FROM rust:1.98.0-bookworm AS builder
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/
COPY contracts/ contracts/
RUN cargo build --locked --release --package light-a2a --bin light-a2a

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && groupadd --system --gid 999 a2a \
    && useradd --system --uid 999 --gid a2a --home-dir /app --shell /usr/sbin/nologin a2a \
    && mkdir -p /app/config-defaults /app/config-cache /config /var/lib/light-a2a/artifacts \
    && ln -s /config /app/config \
    && chown -R a2a:a2a /app /config /var/lib/light-a2a \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder --chown=a2a:a2a /usr/src/app/target/release/light-a2a /app/light-a2a
COPY --from=builder --chown=a2a:a2a /usr/src/app/apps/light-a2a/config/ /app/config-defaults/
EXPOSE 8448
USER a2a:a2a
CMD ["/app/light-a2a"]
