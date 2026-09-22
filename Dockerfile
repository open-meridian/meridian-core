# syntax=docker/dockerfile:1
#
# The instrument store image.
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
 && cp target/release/meridian-street target/release/meridian-instrument \
       target/release/meridian-conductor target/release/meridian-sidecar \
       target/release/meridian-dashboard target/release/meridian-group-hook \
       target/release/meridian-first-run target/release/meridian-broker-config \
       /usr/local/bin/

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The private key lives here, on a volume, and never leaves the container.
RUN mkdir -p /var/lib/meridian
VOLUME /var/lib/meridian

# Where plugins dial.

# One image, three components. They share a build and a base, and differ in
# which one an orchestrator starts: a deployment upgrades them on their own
# schedules by moving one Deployment's tag, not by pulling three images that
# were built from different commits.
COPY --from=build /usr/local/bin/meridian-street /usr/local/bin/meridian-street
COPY --from=build /usr/local/bin/meridian-instrument /usr/local/bin/meridian-instrument
COPY --from=build /usr/local/bin/meridian-conductor /usr/local/bin/meridian-conductor
COPY --from=build /usr/local/bin/meridian-sidecar /usr/local/bin/meridian-sidecar
COPY --from=build /usr/local/bin/meridian-dashboard /usr/local/bin/meridian-dashboard
COPY --from=build /usr/local/bin/meridian-group-hook /usr/local/bin/meridian-group-hook
COPY --from=build /usr/local/bin/meridian-first-run /usr/local/bin/meridian-first-run
COPY --from=build /usr/local/bin/meridian-broker-config /usr/local/bin/meridian-broker-config

# No default: a component is chosen, never inherited. An image that starts
# something when nobody said which is an image that starts the wrong thing.
ENTRYPOINT []
