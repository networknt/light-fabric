# syntax=docker/dockerfile:1
FROM rust:1.98.0-bookworm AS build
ARG CARGO_CACHE_ID=warm
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=light-fabric-rust198-registry-${CARGO_CACHE_ID},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-git-${CARGO_CACHE_ID},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=light-fabric-rust198-target-${CARGO_CACHE_ID}-src-gnu,target=/src/target,sharing=locked \
    cargo build --locked --release -p light-knowledge-worker \
    && mkdir -p /out \
    && cp /src/target/release/light-knowledge-worker /out/light-knowledge-worker

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install --yes --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && groupadd --system --gid 10001 lightknowledge \
  && useradd --system --uid 10002 --gid 10001 --home /nonexistent lightknowledge \
  && mkdir -p /var/lib/light-knowledge/checkouts \
    /var/lib/light-knowledge/objects /var/lib/light-knowledge/projection \
  && chown -R 10002:10001 /var/lib/light-knowledge
COPY --from=build /out/light-knowledge-worker \
  /usr/local/bin/light-knowledge-worker
COPY apps/light-knowledge-worker/config /etc/light-knowledge-worker
USER 10002
ENV LIGHT_KNOWLEDGE_CONFIG_DIR=/etc/light-knowledge-worker
ENTRYPOINT ["/usr/local/bin/light-knowledge-worker"]
