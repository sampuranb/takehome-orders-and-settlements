# syntax=docker/dockerfile:1
#
# Two stages: a builder with the full Rust toolchain, and a runtime with neither
# a compiler nor a package manager on it. The image that ships is a binary, a
# directory of static files, and a CA bundle.
#
# Both halves of this application are built here. `cargo leptos build` runs
# `cargo build` twice — once natively for the `ssr` binary, once for
# `wasm32-unknown-unknown` for the browser bundle — and neither is optional: the
# server renders the HTML and the wasm makes it interactive.

# -----------------------------------------------------------------------------
# Builder
# -----------------------------------------------------------------------------

# Pinned to the toolchain this was developed and tested against, not `latest`.
# An image that silently follows the newest Rust release is an image whose build
# can break without a commit.
FROM rust:1.97-slim-bookworm AS builder

# `pkg-config` and `libssl-dev` are deliberately absent: this build links no
# OpenSSL. SQLx and reqwest are both configured for rustls with the ring
# provider, and `openssl-sys` appears nowhere in `Cargo.lock`.
RUN apt-get update \
    && apt-get install --no-install-recommends --assume-yes \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

# The browser half of the application does not exist without this target.
RUN rustup target add wasm32-unknown-unknown

# cargo-leptos comes from its published release, not from `cargo install`.
#
# Building it here is possible and was tried; it is a bad trade. It compiles
# swc, lightningcss and OpenSSL — twenty minutes of a dependency this project
# does not otherwise have — and its final link runs with full LTO in a single
# codegen unit, which exhausts the memory of a stock Docker Desktop VM and is
# killed. The published binary is the same artifact, in seconds.
#
# Pinned to a version and verified against a checksum recorded here rather than
# fetched alongside the file: a checksum downloaded from the same place as the
# thing it vouches for proves only that the download was not corrupted. These
# were read from the release and checked by hand.
#
# The musl build is static, so it runs on this image regardless of its glibc.
# `TARGETARCH` is supplied by BuildKit, which is what lets one Dockerfile build
# on both an arm64 laptop and an amd64 deployment host.
#
# cargo-leptos fetches a matching `wasm-bindgen` itself, so it is not installed
# separately — a hand-pinned copy is one more version to keep in step with the
# `wasm-bindgen` crate in Cargo.toml, and a mismatch between them is a confusing
# runtime failure rather than a build error.
ARG CARGO_LEPTOS_VERSION=0.3.7
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH}" in \
      arm64) target=aarch64-unknown-linux-musl; \
             sha=9b3c24b16fa3ddca29ea9e8e17df9607d864c406365dad468c6d5f6fcc37bea5 ;; \
      amd64) target=x86_64-unknown-linux-musl; \
             sha=97f9269a23837918be8a01c18c46239d060d87bb070f65625381be4a41159eda ;; \
      *) echo "no cargo-leptos release for TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/leptos-rs/cargo-leptos/releases/download/v${CARGO_LEPTOS_VERSION}/cargo-leptos-${target}.tar.gz"; \
    curl --fail --location --silent --show-error --output /tmp/cargo-leptos.tar.gz "${url}"; \
    echo "${sha}  /tmp/cargo-leptos.tar.gz" | sha256sum --check --strict; \
    tar --extract --gzip --file /tmp/cargo-leptos.tar.gz --directory /tmp; \
    install -m 0755 "/tmp/cargo-leptos-${target}/cargo-leptos" /usr/local/bin/cargo-leptos; \
    rm -rf /tmp/cargo-leptos.tar.gz "/tmp/cargo-leptos-${target}"; \
    cargo leptos --version

WORKDIR /build

# Manifests first, then a throwaway build of the dependency graph. Source
# changes then rebuild only this crate: the layer above is cached as long as
# Cargo.toml and Cargo.lock are untouched, which is most commits.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --no-default-features --features ssr \
    && cargo build --release --target wasm32-unknown-unknown \
        --no-default-features --features hydrate \
    && rm -rf src

COPY src ./src
COPY style ./style
COPY migrations ./migrations
# The web fonts, which `assets-dir` in Cargo.toml tells cargo-leptos to copy
# into target/site — the directory the runtime stage takes wholesale. Leave this
# out and the image builds, starts, serves every page, and 404s three woff2
# files, which shows up as an interface set in Helvetica and nothing in the log.
COPY assets ./assets

# `sqlx::migrate!` embeds ./migrations into the binary at compile time, so the
# runtime image needs no copy of them and cannot drift from the code that
# expects them.
#
# `touch` because the dependency-cache step above left build artifacts whose
# timestamps make Cargo believe this crate is already built.
RUN touch src/main.rs src/lib.rs \
    && cargo leptos build --release

# -----------------------------------------------------------------------------
# Runtime
# -----------------------------------------------------------------------------

FROM debian:bookworm-slim AS runtime

# `ca-certificates` is the only runtime package, and it is not optional: reqwest
# verifies TLS against the operating system's certificate store, so an https
# `BETTER_AUTH_URL` or a `sslmode=require` database fails without it. Nothing
# else is installed — no shell utilities to inherit a vulnerability from, and no
# package manager to install one with.
RUN apt-get update \
    && apt-get install --no-install-recommends --assume-yes ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 app

WORKDIR /app

# The binary and the static bundle it serves. Owned by the user that runs it,
# and by nothing else.
COPY --from=builder --chown=app:app /build/target/release/orders-and-settlements ./orders-and-settlements
COPY --from=builder --chown=app:app /build/target/site ./site

USER app

# `get_configuration(None)` reads the environment, never Cargo.toml — the
# `[package.metadata.leptos]` block only reaches the process when cargo-leptos
# is the one starting it. A bare binary that inherits none of these serves
# `/pkg/*` 404s from a path that does not exist, on an interface nothing can
# reach. Every one of them is therefore set explicitly here rather than left to
# a default.
ENV LEPTOS_OUTPUT_NAME=orders \
    LEPTOS_SITE_ROOT=/app/site \
    LEPTOS_SITE_PKG_DIR=pkg \
    # 0.0.0.0, not 127.0.0.1: a published port maps to the container's external
    # interface, and a process listening only on loopback is unreachable from it.
    LEPTOS_SITE_ADDR=0.0.0.0:8080 \
    RUST_LOG=info,orders_and_settlements=info,tower_http=info

EXPOSE 8080

# No HEALTHCHECK instruction: it would require curl or wget in an image that
# deliberately has neither. `/health` is checked by compose and by the platform
# instead, from outside — which is also where a health check that means anything
# has to run.

ENTRYPOINT ["/app/orders-and-settlements"]
