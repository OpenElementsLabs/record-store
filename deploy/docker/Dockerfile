# The toolchain rust-toolchain.toml pins, so the image digest pins the compiler
# and the build needs no toolchain download. Bump the two together.
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder
WORKDIR /src

# The build context has no .git, so the commit is passed in; the binaries
# report it (record-store version, /api/v1/system/info, backup manifests).
ARG RECORD_STORE_BUILD_COMMIT=unknown
COPY . .
RUN cargo build --locked --release --bin record-store-server --bin record-store

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime
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

# glibc gives each thread that contends for the allocator an arena of its own,
# up to eight per core, and keeps what every arena once held. Under steady load
# the server's resident memory then keeps climbing while its live heap stays
# flat; two arenas held it at half, with no difference in throughput (see
# docs/operations/capacity-planning.md#memory).
ENV RECORD_STORE_S3_BIND=0.0.0.0:7600 \
    RECORD_STORE_API_BIND=0.0.0.0:7601 \
    RECORD_STORE_STORAGE_DATA_DIRECTORY=/var/lib/record-store \
    RECORD_STORE_LOG_JSON=true \
    MALLOC_ARENA_MAX=2

USER record-store:record-store
VOLUME ["/var/lib/record-store"]
EXPOSE 7600 7601 7603
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["record-store", "status", "--endpoint", "http://127.0.0.1:7601"]
ENTRYPOINT ["record-store", "server"]
