FROM public.ecr.aws/docker/library/rust:1.92-bookworm AS chef

# Build dependencies (needed by some crates that bind to native libs)
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    clang \
    libclang-dev \
    gcc \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

# Cargo Chef: caches dependency compilation across rebuilds
RUN cargo install cargo-chef --locked

FROM chef AS planner
WORKDIR /work

# IMPORTANT:
# This Dockerfile expects the build context to contain BOTH sibling directories:
# - rblib-world-chain/  (this repo)
# - rblib/             (your local rblib checkout)
#
# Example build command (run from the parent directory that contains both):
#   docker buildx build -t world-chain:latest -f rblib-world-chain/Dockerfile .
COPY rblib-world-chain ./rblib-world-chain
COPY rblib ./rblib

WORKDIR /work/rblib-world-chain
RUN cargo chef prepare --recipe-path /work/recipe.json

FROM chef AS builder
WORKDIR /work/rblib-world-chain

ARG WORLD_CHAIN_BUILDER_BIN="examples/pipeline"

COPY --from=planner /work/recipe.json /work/recipe.json

# `cargo-chef cook` must be able to resolve local path dependencies.
# The recipe references `rblib` at `/work/rblib`, so copy it in before cooking.
COPY --from=planner /work/rblib /work/rblib

# BuildKit cache mounts speed up rebuilds dramatically (registry + git + target).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/rblib-world-chain/target \
    cargo chef cook --recipe-path /work/recipe.json

# Copy sources after deps are cached
WORKDIR /work
COPY rblib-world-chain ./rblib-world-chain
COPY rblib ./rblib

WORKDIR /work/rblib-world-chain
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/rblib-world-chain/target \
      cargo build --example pipeline;

# Deployments depend on sh wget and awscli v2
FROM public.ecr.aws/docker/library/debian:bookworm-slim
WORKDIR /app

# Install wget in the final image
RUN apt-get update && \
  apt-get install -y --no-install-recommends \
  ca-certificates \
  unzip \
  curl \
  lz4 \
  wget \
  jq \
  netcat-traditional \
  tzdata && \
  apt-get clean && \
  rm -rf /var/lib/apt/lists/*

ARG WORLD_CHAIN_BUILDER_BIN="examples/pipeline"

COPY --from=builder /work/rblib-world-chain/target/debug/${WORLD_CHAIN_BUILDER_BIN} /usr/local/bin/

EXPOSE 30303 30303/udp 9001 8545 8546

ENTRYPOINT ["/usr/local/bin/pipeline"]
