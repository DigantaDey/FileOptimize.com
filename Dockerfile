# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Stage 1 — build
# ---------------------------------------------------------------------------
FROM rust:1.82-slim-bookworm AS builder

WORKDIR /app

# Manifest first: dependencies are cached independently of source changes.
COPY Cargo.toml rust-toolchain.toml ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --bin fileoptimize \
    && rm -rf src

COPY src ./src
COPY web ./web
RUN cargo build --release --bin fileoptimize && rm -rf target/release/incremental

# ---------------------------------------------------------------------------
# Stage 2 — runtime
#
# Ghostscript + poppler-utils provide the heavy PDF engine (compression and
# rasterisation) without pulling a TeX/LaTeX stack into the image.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        ghostscript \
        poppler-utils \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --shell /usr/sbin/nologin app \
    && mkdir -p /var/tmp/fileoptimize \
    && chown -R app:app /var/tmp/fileoptimize

WORKDIR /srv
COPY --from=builder /app/target/release/fileoptimize /srv/fileoptimize
RUN chown app:app /srv/fileoptimize

USER app

ENV HOST=0.0.0.0 \
    PORT=8080 \
    TMP_DIR=/var/tmp/fileoptimize \
    MAX_UPLOAD_SIZE_MB=50 \
    MAX_IMAGE_PIXELS=25000000 \
    MAX_CONCURRENT=4 \
    CLEANUP_TTL_SECS=1800 \
    TOOL_TIMEOUT_SECS=120 \
    RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

ENTRYPOINT ["/srv/fileoptimize"]
