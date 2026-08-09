FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p light-knowledge

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install --yes --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/* \
  && groupadd --system --gid 10001 lightknowledge \
  && useradd --system --uid 10001 --gid 10001 --home /nonexistent lightknowledge \
  && mkdir -p /var/lib/light-knowledge/objects \
  && chown -R 10001:10001 /var/lib/light-knowledge
COPY --from=build /src/target/release/light-knowledge /usr/local/bin/light-knowledge
COPY apps/light-knowledge/config /etc/light-knowledge
USER 10001
ENV LIGHT_KNOWLEDGE_CONFIG_FILE=/etc/light-knowledge/knowledge.yml
EXPOSE 8092
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=6 \
  CMD curl --fail --silent http://127.0.0.1:8092/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/light-knowledge"]
