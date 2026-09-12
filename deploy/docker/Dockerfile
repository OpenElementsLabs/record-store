FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS builder
WORKDIR /src

COPY . .
RUN cargo build --locked --release --bin record-store-server --bin record-store

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 record-store \
    && useradd --system --uid 10001 --gid record-store --home-dir /nonexistent --shell /usr/sbin/nologin record-store \
    && install --directory --owner record-store --group record-store /var/lib/record-store

COPY --from=builder /src/target/release/record-store-server /usr/local/bin/record-store-server
COPY --from=builder /src/target/release/record-store /usr/local/bin/record-store
# The image is labelled Apache-2.0, so it carries the licence it claims.
COPY --from=builder /src/LICENSE /usr/share/licenses/record-store/LICENSE

ENV RECORD_STORE_S3_BIND=0.0.0.0:7600 \
    RECORD_STORE_API_BIND=0.0.0.0:7601 \
    RECORD_STORE_STORAGE_DATA_DIRECTORY=/var/lib/record-store \
    RECORD_STORE_LOG_JSON=true

USER record-store:record-store
VOLUME ["/var/lib/record-store"]
EXPOSE 7600 7601 7603
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["record-store", "status", "--endpoint", "http://127.0.0.1:7601"]
ENTRYPOINT ["record-store", "server"]
