# syntax=docker/dockerfile:1.7
# Multi-stage build for edge-control binary.
# Output: minimal Debian image with edge-control + native deps.
#
# BuildKit cache mounts speed up incremental rebuilds dramatically:
#  - /usr/local/cargo/registry  ← downloaded crate sources
#  - /usr/local/cargo/git        ← git deps
#  - /build/target               ← compiled artifacts
#
# First build = full compile (~8-12 min on dev machines).
# Subsequent builds with only Rust source changes = 30-90s.
# Cache survives `docker system prune` unless `--volumes` is passed.

FROM rust:1.88-bookworm AS builder
WORKDIR /build

# Native deps required by mediasoup-sys (worker C++ compile + flatbuffers codegen)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       python3 python3-pip python3-invoke \
       meson ninja-build \
       build-essential cmake pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt

# Copy workspace manifest + sources together. We rely on BuildKit's
# `--mount=type=cache` to keep the cargo registry + target dir warm
# across rebuilds, so there's no need to fake a deps-only first pass.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

ENV CARGO_BUILD_JOBS=2
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo clean -p edge-bandwidth -p edge-control \
    && cargo build --release -p edge-control --bin edge-control \
    && cp /build/target/release/edge-control /usr/local/bin/edge-control \
    && find /build/target/release -name 'mediasoup-worker' -type f -executable \
         -exec cp {} /usr/local/bin/mediasoup-worker \; \
    && test -x /usr/local/bin/mediasoup-worker \
       || (echo "ERROR: mediasoup-worker not found in target/" && exit 1)

# Runtime stage — slim Debian
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libstdc++6 \
       libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/edge-control /usr/local/bin/edge-control
# mediasoup spawn child process `mediasoup-worker` ở runtime. Binary
# này build cùng cargo target và copy ra `/usr/local/bin/` ở stage
# builder để runtime stage thấy được (BuildKit cache mount target/
# không persist giữa stages).
COPY --from=builder /usr/local/bin/mediasoup-worker /usr/local/bin/mediasoup-worker
ENV MEDIASOUP_WORKER_BIN=/usr/local/bin/mediasoup-worker

# Pin UID/GID = 1500 để host bind-mount chown thuận tiện
# (`sudo chown -R 1500:1500 ../state` trên host).
RUN groupadd --system --gid 1500 edge \
    && useradd --system --uid 1500 --gid 1500 \
       --no-create-home --shell /usr/sbin/nologin edge \
    && mkdir -p /state/rathole /state/queue \
    && chown -R edge:edge /state

USER edge
EXPOSE 9443
ENTRYPOINT ["/usr/local/bin/edge-control"]
