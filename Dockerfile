# ─── Stage 1: build ───────────────────────────────────────────────────────────
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies — copy manifests first, dummy src, build deps only
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release
RUN rm src/main.rs

# Now build the real source
COPY src ./src
# Touch main.rs so cargo knows it changed
RUN touch src/main.rs && cargo build --release

# ─── Stage 2: runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    postgresql-client \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN useradd -r -s /bin/false -m -d /app postback

COPY --from=builder /build/target/release/postback /usr/local/bin/postback

# Temp dir for dumps
RUN mkdir -p /tmp/postback && chown postback:postback /tmp/postback

# Mount point for service account key
RUN mkdir -p /secrets && chown postback:postback /secrets

USER postback
WORKDIR /app

ENTRYPOINT ["/usr/local/bin/postback"]
