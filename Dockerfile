# syntax=docker/dockerfile:1
#
# reflect-mem — multi-stage build producing a small, non-root runtime image.
#
#   docker build -t reflect-mem .
#   docker run --rm -p 127.0.0.1:8080:8080 \
#     -v "$PWD/data:/data" \
#     -e LLM_API_KEY=... -e MCP_TOKEN=... \
#     reflect-mem

# --------------------------------------------------------------------------- #
# Build                                                                       #
# --------------------------------------------------------------------------- #
FROM rust:1.96-bookworm AS build

# `lance-encoding`'s build script generates protobuf bindings: protoc plus the
# well-known types shipped in libprotobuf-dev.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Compile dependencies against a stub first, so the (slow) dependency layer
# caches across source-only changes.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release --locked

# Real sources. `touch` defeats cargo's mtime check, which would otherwise
# keep the stub artefacts (they are newer than the copied sources). Only the
# server binary is built — bench/spike are local tools and stay out of the image.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find src -name '*.rs' -exec touch {} + \
 && cargo build --release --locked --bin reflect-mem \
 && cp target/release/reflect-mem /usr/local/bin/reflect-mem

# --------------------------------------------------------------------------- #
# Runtime                                                                     #
# --------------------------------------------------------------------------- #
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="reflect-mem" \
      org.opencontainers.image.description="Long-term memory MCP server (remember / recall / forget)" \
      org.opencontainers.image.licenses="MIT"

# TLS comes from rustls and SQLite is statically bundled, so there is no
# OpenSSL/libsqlite to install. `curl` backs the container healthcheck.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home --home-dir /home/reflect-mem reflect-mem \
 && install -d -o reflect-mem -g reflect-mem /data

COPY --from=build /usr/local/bin/reflect-mem /usr/local/bin/reflect-mem

# Distribute the licence with the binary, as MIT requires.
COPY LICENSE NOTICE /usr/share/doc/reflect-mem/

# The data root holds the graph / vector / relational stores and `config.toml`.
ENV DATA_ROOT=/data \
    MCP_BIND=0.0.0.0:8080

USER reflect-mem
WORKDIR /home/reflect-mem

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${MCP_BIND##*:}/healthz" || exit 1

ENTRYPOINT ["reflect-mem"]
CMD ["mcp", "--transport", "streamable-http", "--bind", "0.0.0.0:8080"]
