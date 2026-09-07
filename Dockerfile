# Build stage
# Pinned to rust:1.96.0-bookworm (matches the project toolchain: rustc/cargo
# 1.96.0) by manifest-list digest so the build image cannot change silently.
FROM rust@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
RUN cargo build --release --package rdw-api

# Runtime stage
# Pinned to ubuntu:24.04 by manifest-list digest for the same reason.
FROM ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged, non-login user rather than root (UID 0).
RUN groupadd --gid 10001 rdwapi \
    && useradd --uid 10001 --gid rdwapi --no-create-home --shell /usr/sbin/nologin rdwapi

WORKDIR /app
COPY --from=builder /app/target/release/rdw-api /usr/local/bin/rdw-api

# VALID_API_KEYS and RDW_APP_TOKEN are supplied at runtime as environment
# variables, never baked into an image layer.
ENV RUST_LOG="info"

# CSV export staging uses the OS temp directory (see
# crates/rdw-core/src/csv_writer.rs); make sure the unprivileged user can
# write to it.
RUN mkdir -p /tmp && chown rdwapi:rdwapi /tmp && chmod 1777 /tmp

USER rdwapi

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/rdw-api"]
