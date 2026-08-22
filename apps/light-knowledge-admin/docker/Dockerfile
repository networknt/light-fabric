FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home lightknowledge
COPY target/release/light-knowledge-admin /usr/local/bin/light-knowledge-admin
COPY apps/light-knowledge-admin/config /app/config
WORKDIR /app
USER 10001
EXPOSE 8093
ENTRYPOINT ["/usr/local/bin/light-knowledge-admin"]
