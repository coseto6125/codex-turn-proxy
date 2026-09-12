FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo test --locked && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/codex-turn-proxy /usr/local/bin/codex-turn-proxy
USER 65532:65532
ENV LISTEN_ADDR=0.0.0.0:8787
EXPOSE 8787
HEALTHCHECK --interval=15s --timeout=5s --start-period=5s CMD ["codex-turn-proxy", "healthcheck"]
ENTRYPOINT ["codex-turn-proxy"]
CMD ["serve"]
