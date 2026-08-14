FROM rust:bookworm AS builder

WORKDIR /app
COPY . .
RUN cargo build --release -p collecta-server

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false collecta

COPY --from=builder /app/target/release/collecta-server /usr/local/bin/collecta-server

# sqlite database and attachment files both live under /data
RUN mkdir -p /data && chown collecta /data

USER collecta

ENV RUST_LOG=info
ENV COLLECTA_DB=/data/collecta.db
ENV COLLECTA_DATA_DIR=/data
ENV COLLECTA_ADDR=0.0.0.0:3000

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:3000/health || exit 1

ENTRYPOINT ["collecta-server"]
