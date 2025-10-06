# --- STAGE 1: Builder ---
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y git pkg-config libssl-dev protobuf-compiler curl && \
    rm -rf /var/lib/apt/lists/*

ARG GIT_COMMIT="unknown"
ARG BUILD_DATE="unknown"
ARG SERVICE_VERSION="0.0.0"

WORKDIR /app

COPY . .

ENV GIT_COMMIT=${GIT_COMMIT}
ENV BUILD_DATE=${BUILD_DATE}
ENV SERVICE_VERSION=${SERVICE_VERSION}

RUN cargo build --release --bin sentiric-proxy-service

# --- STAGE 2: Final (Minimal) Image ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    netcat-openbsd \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG GIT_COMMIT
ARG BUILD_DATE
ARG SERVICE_VERSION
ENV GIT_COMMIT=${GIT_COMMIT}
ENV BUILD_DATE=${BUILD_DATE}
ENV SERVICE_VERSION=${SERVICE_VERSION}

WORKDIR /app

COPY --from=builder /app/target/release/sentiric-proxy-service .

RUN useradd -m -u 1001 appuser
USER appuser
ENTRYPOINT ["./sentiric-proxy-service"]