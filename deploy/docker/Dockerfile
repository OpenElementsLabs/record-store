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

ENV OES_S3_BIND=0.0.0.0:7600 \
    OES_API_BIND=0.0.0.0:7601 \
    OES_STORAGE_DATA_DIRECTORY=/var/lib/oes \
    OES_LOG_JSON=true

USER oes:oes
VOLUME ["/var/lib/oes"]
EXPOSE 7600 7601
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["oes", "status", "--endpoint", "http://127.0.0.1:7601"]
ENTRYPOINT ["oes", "server"]
