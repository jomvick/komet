# Self-hosted komet sync server.
# Images are pinned by digest; update the tag and digest together.
FROM rust:1.98.1-slim-bookworm@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS builder
WORKDIR /app
# Use the image's toolchain instead of letting rust-toolchain.toml download one.
ENV RUSTUP_TOOLCHAIN=1.98.1
COPY . .
RUN cargo build --locked -p komet-sync-server --release --bin komet-sync-server

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
RUN useradd --system --uid 10001 --home-dir /data komet \
    && mkdir -p /data \
    && chown komet:komet /data
COPY --from=builder /app/target/release/komet-sync-server /usr/local/bin/komet-sync-server
USER komet
ENV KOMET_DATA_DIR=/data \
    KOMET_SYNC_PORT=8787
EXPOSE 8787
CMD ["komet-sync-server"]
