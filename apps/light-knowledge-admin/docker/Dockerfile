# Build in the same Debian generation as the runtime image so the binary never
# inherits a newer host GLIBC requirement.
FROM rust:1.98.0-bookworm AS builder
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/
COPY contracts/ contracts/
RUN cargo build --locked --release --package light-knowledge-admin --bin light-knowledge-admin

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home lightknowledge
COPY --from=builder /usr/src/app/target/release/light-knowledge-admin /usr/local/bin/light-knowledge-admin
COPY apps/light-knowledge-admin/config /app/config
WORKDIR /app
USER 10001
EXPOSE 8093
ENTRYPOINT ["/usr/local/bin/light-knowledge-admin"]
