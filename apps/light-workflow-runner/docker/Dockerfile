FROM rust:1.94.1-bookworm AS builder
WORKDIR /src/light-fabric
COPY light-fabric/Cargo.toml light-fabric/Cargo.lock ./
COPY light-fabric/crates/ crates/
COPY light-fabric/frameworks/ frameworks/
COPY light-fabric/apps/ apps/
COPY controller-rs/ /src/controller-rs/
RUN cargo build --locked --release -p light-workflow-runner -p light-agent-worker

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 runner \
    && useradd --system --uid 10001 --gid runner --home-dir /var/lib/light-workflow-runner --shell /usr/sbin/nologin runner \
    && install -d -o runner -g runner -m 0700 /var/lib/light-workflow-runner
COPY --from=builder /src/light-fabric/target/release/light-workflow-runner /usr/local/bin/light-workflow-runner
COPY --from=builder /src/light-fabric/target/release/light-agent-worker /usr/local/bin/light-agent-worker
USER 10001:10001
EXPOSE 9444
ENTRYPOINT ["/usr/bin/tini","--"]
CMD ["/usr/local/bin/light-workflow-runner"]
