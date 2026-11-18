# deploy/Dockerfile — production image for muster-cloud (multi-stage).
#
# Build from the repository root (the whole workspace is the context):
#
#   docker build -f deploy/Dockerfile -t muster-cloud:local .
#
# Context exclusions live in deploy/Dockerfile.dockerignore (BuildKit
# per-Dockerfile ignore file: .git, target/, .hillclimb/, local env/secret
# files). With a legacy (non-BuildKit) builder those exclusions are skipped:
# the context is only larger, the build itself stays correct. This file never
# starts or stops a Docker daemon.
#
# Reproducibility: the dependency graph is pinned by --locked against the
# committed workspace Cargo.lock (a lockfile mismatch fails the build instead
# of silently upgrading). Base images are pinned Debian tags; before serving
# real traffic, pin them by digest (@sha256:...) — see deploy/README.md.
#
# Secrets: none are baked in. All configuration, including the database URL
# and the OIDC client secret, is injected at runtime (see deploy/README.md).
# There is no development-auth fallback in this image: MUSTER_ENV defaults to
# production and config.rs rejects insecure production settings.

ARG RUST_IMAGE=rust:1-slim-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# --- build stage ---------------------------------------------------------
FROM ${RUST_IMAGE} AS build

# The rust:slim image already carries gcc and libc-dev (rustc needs the
# linker); pkg-config and build-essential cover sys-crate probes and
# make-based builds.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Workspace manifests first so the dependency layers cache independently of
# source edits (the warm-up build below compiles only dependencies plus
# placeholder sources; the real sources overwrite them in the next layer).
COPY Cargo.toml Cargo.lock ./
COPY cloud/Cargo.toml cloud/Cargo.toml

# Placeholder lib/bin sources: cargo target-validates EVERY workspace member
# manifest at load time (a member with no autodiscoverable targets is a hard
# parse error), so the root member needs placeholders too even though only
# `muster-cloud` is built. The real sources from the next COPY replace all
# source files.
RUN set -eux; \
    mkdir -p cloud/src src deploy/harness; \
    printf 'fn main() {}\n' > deploy/harness/drill.rs; \
    echo 'pub fn __placeholder() {}' > cloud/src/lib.rs; \
    printf 'fn main() {}\n' > cloud/src/main.rs; \
    printf 'fn main() {}\n' > cloud/src/ops.rs; \
    echo 'pub fn __placeholder() {}' > src/lib.rs; \
    printf 'fn main() {}\n' > src/main.rs

RUN cargo build --release --locked -p muster-cloud --bin muster-cloud --bin muster-cloud-ops

# Real workspace sources (context excludes target/, .git/, .hillclimb/ and
# env/secret files). cloud/src/lib.rs includes ../../src/cloud_protocol.rs,
# so the whole tree must be present.
COPY . .

RUN set -eux; \
    # The warm-up layer compiled the placeholder sources, and its artifacts
    # are NEWER than the real sources copied in the previous step (Docker
    # COPY preserves mtimes), so cargo would consider everything fresh and
    # ship placeholder binaries. Drop the artifacts of BOTH internal
    # workspace members — external dependency artifacts stay cached —
    # forcing a real rebuild from the actual sources. (If a new internal
    # path dependency ever joins the workspace, it must be listed here too.)
    cargo clean --release -p muster-cloud -p muster; \
    cargo build --release --locked -p muster-cloud --bin muster-cloud --bin muster-cloud-ops; \
    cp target/release/muster-cloud /usr/local/bin/muster-cloud; \
    cp target/release/muster-cloud-ops /usr/local/bin/muster-cloud-ops

# --- runtime stage -------------------------------------------------------
FROM ${RUNTIME_IMAGE}

# git: the cloud service validates and stores Git object history.
# ca-certificates: TLS trust for OIDC discovery and provider webhooks.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates git; \
    rm -rf /var/lib/apt/lists/*

# Dedicated non-root account with fixed uid/gid and no login shell.
RUN set -eux; \
    groupadd --system --gid 10001 muster; \
    useradd --system --uid 10001 --gid muster \
      --home-dir /var/lib/muster --no-create-home --shell /usr/sbin/nologin muster

COPY --from=build /usr/local/bin/muster-cloud /usr/local/bin/muster-cloud
# Operator tool for the lifecycle authority (init on pristine targets,
# status, register, activate, restore-check). Run it in this same image,
# e.g. `docker run --entrypoint muster-cloud-ops ... init`; pg-restore.sh
# finds it on PATH.
COPY --from=build /usr/local/bin/muster-cloud-ops /usr/local/bin/muster-cloud-ops

# Data and Git object stores live under one volume-ready root, owned by the
# service account with no access for other accounts. Mount a volume here at
# runtime; the exact subpaths are service config (see deploy/README.md).
RUN set -eux; \
    mkdir -p /var/lib/muster; \
    chown muster:muster /var/lib/muster; \
    chmod 0700 /var/lib/muster

USER muster
WORKDIR /var/lib/muster

# Plain HTTP inside the container; TLS termination belongs to a reverse
# proxy in front of it (see deploy/README.md).
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/muster-cloud"]
