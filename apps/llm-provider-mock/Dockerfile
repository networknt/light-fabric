FROM rust:1-bookworm AS builder
WORKDIR /usr/src/light-fabric
COPY . .
RUN cargo build --locked --release -p llm-provider-mock --bin llm-provider-mock

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --create-home mock
COPY --from=builder /usr/src/light-fabric/target/release/llm-provider-mock /usr/local/bin/llm-provider-mock
USER mock
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/llm-provider-mock"]

