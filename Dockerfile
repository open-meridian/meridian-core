# syntax=docker/dockerfile:1
#
# The replica image.
#
# Separate from Dockerfile.rust, which is the gate runner. This one produces the
# thing that runs; that one produces answers about it.

ARG RUST_VERSION=1.90

FROM rust:${RUST_VERSION}-slim-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /w
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/w/target \
    cargo build --release --locked -p meridian-runtime \
 && cp target/release/meridian-runtime /usr/local/bin/meridian-runtime

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The private key lives here, on a volume, and never leaves the container.
RUN mkdir -p /var/lib/meridian
VOLUME /var/lib/meridian

# Where plugins dial.
EXPOSE 9191

COPY --from=build /usr/local/bin/meridian-runtime /usr/local/bin/meridian-runtime
ENTRYPOINT ["/usr/local/bin/meridian-runtime"]
