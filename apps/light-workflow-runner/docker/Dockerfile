FROM rust:1.98.0-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
COPY crates/ crates/
COPY frameworks/ frameworks/
COPY apps/ apps/
RUN cargo build --locked --release -p light-workflow-runner -p light-agent-worker

FROM node:24-bookworm AS codex
RUN mkdir /codex \
    && cd /codex \
    && npm install --ignore-scripts @openai/codex@0.153.4 \
    && echo "56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da  /codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex" | sha256sum --check --strict

FROM rust:1.98.0-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 runner \
    && useradd --system --uid 10001 --gid runner --home-dir /var/lib/light-workflow-runner --shell /usr/sbin/nologin runner \
    && install -d -o runner -g runner -m 0700 /var/lib/light-workflow-runner
COPY --from=builder /src/target/release/light-workflow-runner /usr/local/bin/light-workflow-runner
COPY --from=builder /src/target/release/light-agent-worker /usr/local/bin/light-agent-worker
COPY --from=codex /codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex /usr/local/bin/codex
COPY --from=codex /codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex-code-mode-host /usr/local/bin/codex-code-mode-host
COPY --from=codex /codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex-resources /usr/local/codex-resources
COPY --from=codex /codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex-path /usr/local/codex-path
USER 10001:10001
EXPOSE 9444
ENTRYPOINT ["/usr/bin/tini","--"]
CMD ["/usr/local/bin/light-workflow-runner"]
