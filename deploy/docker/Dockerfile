FROM rust:1.97-bookworm AS builder
WORKDIR /src

COPY . .
RUN cargo build --locked --release --bin oes-server --bin oes

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 oes \
    && useradd --system --uid 10001 --gid oes --home-dir /nonexistent --shell /usr/sbin/nologin oes \
    && install --directory --owner oes --group oes /var/lib/oes

COPY --from=builder /src/target/release/oes-server /usr/local/bin/oes-server
COPY --from=builder /src/target/release/oes /usr/local/bin/oes

ENV OES_SERVER_BIND_ADDRESS=0.0.0.0 \
    OES_SERVER_PORT=9000 \
    OES_STORAGE_DATA_DIRECTORY=/var/lib/oes \
    OES_LOG_JSON=true

USER oes:oes
VOLUME ["/var/lib/oes"]
EXPOSE 9000
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["oes", "server", "status", "--endpoint", "http://127.0.0.1:9000"]
ENTRYPOINT ["oes-server"]

