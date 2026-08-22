FROM rust:1.77 as builder
WORKDIR /app
COPY . .
RUN cargo build -p komet-sync-server --release
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/komet-sync-server /usr/local/bin/komet-sync-server
EXPOSE 8787
CMD ["komet-sync-server"]
