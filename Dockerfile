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

# Install pg_dump for all active PostgreSQL major versions (13–17).
# At runtime, resolve_pg_dump() queries the server version via psql and
# invokes /usr/lib/postgresql/<major>/bin/pg_dump — no version mismatch possible.
# To add support for a new major version (e.g. 18), append postgresql-client-18.
RUN apt-get update && apt-get install -y ca-certificates curl \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
       -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] \
       https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
       > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update \
    && apt-get install -y \
       postgresql-client-13 \
       postgresql-client-14 \
       postgresql-client-15 \
       postgresql-client-16 \
       postgresql-client-17 \
       age \
       rsync \
       openssh-client \
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
