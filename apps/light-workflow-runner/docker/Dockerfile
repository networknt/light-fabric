FROM rust:1.94.1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/
RUN cargo build --locked --release -p light-workflow-runner -p light-agent-worker

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 runner \
    && useradd --system --uid 10001 --gid runner --home-dir /var/lib/light-workflow-runner --shell /usr/sbin/nologin runner \
    && install -d -o runner -g runner -m 0700 /var/lib/light-workflow-runner
COPY --from=builder /src/target/release/light-workflow-runner /usr/local/bin/light-workflow-runner
COPY --from=builder /src/target/release/light-agent-worker /usr/local/bin/light-agent-worker
USER 10001:10001
EXPOSE 9444
ENTRYPOINT ["/usr/bin/tini","--"]
CMD ["/usr/local/bin/light-workflow-runner"]
