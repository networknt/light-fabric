FROM rust:1.94.1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p light-knowledge-worker

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install --yes --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && groupadd --system --gid 10001 lightknowledge \
  && useradd --system --uid 10002 --gid 10001 --home /nonexistent lightknowledge \
  && mkdir -p /var/lib/light-knowledge/checkouts \
    /var/lib/light-knowledge/objects /var/lib/light-knowledge/projection \
  && chown -R 10002:10001 /var/lib/light-knowledge
COPY --from=build /src/target/release/light-knowledge-worker \
  /usr/local/bin/light-knowledge-worker
COPY apps/light-knowledge-worker/config /etc/light-knowledge-worker
USER 10002
ENV LIGHT_KNOWLEDGE_CONFIG_DIR=/etc/light-knowledge-worker
ENTRYPOINT ["/usr/local/bin/light-knowledge-worker"]
