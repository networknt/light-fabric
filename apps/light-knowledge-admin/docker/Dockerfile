FROM debian:bookworm-slim

RUN useradd --system --uid 10001 --create-home lightknowledge
COPY target/release/light-knowledge-admin /usr/local/bin/light-knowledge-admin
COPY apps/light-knowledge-admin/config /app/config
WORKDIR /app
USER 10001
EXPOSE 8493
ENTRYPOINT ["/usr/local/bin/light-knowledge-admin"]
