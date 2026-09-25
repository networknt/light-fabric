# syntax=docker/dockerfile:1
FROM rust:1.98.0-bookworm AS build
ARG CARGO_CACHE_ID=warm
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=light-fabric-rust198-registry-${CARGO_CACHE_ID},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-git-${CARGO_CACHE_ID},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-target-${CARGO_CACHE_ID}-src-gnu,target=/src/target,sharing=locked \
    cargo build --locked --release -p light-knowledge \
    && mkdir -p /out \
    && cp /src/target/release/light-knowledge /out/light-knowledge

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install --yes --no-install-recommends ca-certificates curl git \
  && rm -rf /var/lib/apt/lists/* \
  && groupadd --system --gid 10001 lightknowledge \
  && useradd --system --uid 10001 --gid 10001 --home /nonexistent lightknowledge \
  && mkdir -p /var/lib/light-knowledge/objects \
    /var/lib/light-knowledge/checkouts /var/lib/light-knowledge/projection \
  && chown -R 10001:10001 /var/lib/light-knowledge
COPY --from=build /out/light-knowledge /usr/local/bin/light-knowledge
COPY apps/light-knowledge/config /etc/light-knowledge
WORKDIR /var/lib/light-knowledge
USER 10001
ENV LIGHT_KNOWLEDGE_CONFIG_FILE=/etc/light-knowledge/knowledge.yml
EXPOSE 8092
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=6 \
  CMD curl --fail --silent http://127.0.0.1:8092/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/light-knowledge"]
