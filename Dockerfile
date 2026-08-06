# syntax=docker/dockerfile:1.7

ARG RUST_IMAGE=rust:1.97.1-bookworm
ARG CARGO_CHEF_VERSION=0.1.77

FROM ${RUST_IMAGE} AS chef

ARG CARGO_CHEF_VERSION
RUN --mount=type=cache,id=crowdrelay-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=crowdrelay-cargo-git,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"

WORKDIR /workspace

# Generate a dependency-only recipe. Source edits that do not change manifests
# keep the expensive `cargo chef cook` layer reusable through the GHA cache.
FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

COPY --from=planner /workspace/recipe.json recipe.json
RUN --mount=type=cache,id=crowdrelay-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=crowdrelay-cargo-git,target=/usr/local/cargo/git/db,sharing=locked \
    cargo chef cook \
        --locked \
        --release \
        --recipe-path recipe.json \
        --package crowdrelay-api \
        --package crowdrelay-worker

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations

# API and worker are compiled in one Cargo invocation. Both runtime targets then
# reuse this same builder graph instead of compiling the workspace twice.
RUN --mount=type=cache,id=crowdrelay-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=crowdrelay-cargo-git,target=/usr/local/cargo/git/db,sharing=locked \
    cargo build \
        --locked \
        --release \
        --package crowdrelay-api \
        --package crowdrelay-worker \
    && install --directory /out \
    && install --mode 0755 target/release/crowdrelay-api /out/crowdrelay-api \
    && install --mode 0755 target/release/crowdrelay-worker /out/crowdrelay-worker

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/wojciechbator/crowdrelay" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 crowdrelay \
    && useradd \
        --uid 10001 \
        --gid crowdrelay \
        --no-create-home \
        --home-dir /nonexistent \
        --shell /usr/sbin/nologin \
        crowdrelay

WORKDIR /app

ENV RUST_LOG=info

FROM runtime AS api

RUN apt-get update \
    && apt-get install --yes --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/crowdrelay-api /usr/local/bin/crowdrelay-api

USER crowdrelay
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/crowdrelay-api"]

FROM runtime AS worker

COPY --from=builder /out/crowdrelay-worker /usr/local/bin/crowdrelay-worker

USER crowdrelay

ENTRYPOINT ["/usr/local/bin/crowdrelay-worker"]
