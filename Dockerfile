# syntax=docker/dockerfile:1
#
# pesto CLI image for `pesto --watch` (issue #185).
#
# Default target `runtime` builds from source:
#   docker build -t pesto .
#   docker run --rm pesto --version
#
# Target `runtime-prebuilt` copies a glibc binary from the build context
# (used by `.github/workflows/release-pesto.yml` so GHCR matches the
# GitHub Release linux-gnu artifact bit-for-bit). That artifact is built
# on Ubuntu 22.04 (glibc 2.35) so it loads on this bookworm runtime
# (glibc 2.36). A binary from Ubuntu 24.04 / glibc 2.39 will not.
#   docker build --target runtime-prebuilt -t pesto .
#
# v1 is linux/amd64 only. Credentials are never baked in.

ARG RUST_VERSION=1.96
ARG DEBIAN_VERSION=bookworm

FROM debian:${DEBIAN_VERSION}-slim AS runtime-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        p7zip-full \
        tini \
    && rm -rf /var/lib/apt/lists/*

# uid 1000 matches a typical host user so bind-mounted incoming/nzb/archive
# directories are writable without a root container. Override at run time
# with `user:` in Compose if the host uid differs.
RUN groupadd --gid 1000 pesto \
    && useradd --uid 1000 --gid 1000 --create-home --home-dir /home/pesto \
        --shell /usr/sbin/nologin pesto \
    && mkdir -p /config/pesto /data/incoming /data/nzb /data/archive \
    && chown -R pesto:pesto /config /data /home/pesto

ENV XDG_CONFIG_HOME=/config \
    TMPDIR=/tmp \
    PATH="/usr/local/bin:${PATH}"

LABEL org.opencontainers.image.source="https://github.com/franzopl/pesto" \
      org.opencontainers.image.url="https://github.com/franzopl/pesto" \
      org.opencontainers.image.title="pesto" \
      org.opencontainers.image.description="Fast Usenet poster — watch-daemon image" \
      org.opencontainers.image.licenses="MIT"

# ── Pre-built glibc binary (release workflow) ───────────────────────────────

FROM runtime-base AS runtime-prebuilt

# File name in the build context; the release job downloads the linux-gnu
# artifact as `pesto`.
ARG PESTO_BIN=pesto
COPY ${PESTO_BIN} /usr/local/bin/pesto
RUN chmod 755 /usr/local/bin/pesto

USER pesto
ENTRYPOINT ["tini", "--", "pesto"]
CMD ["--help"]

# ── Build from source (default `docker build`) ──────────────────────────────

FROM rust:${RUST_VERSION}-bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p pesto-poster --bin pesto \
    && cp target/release/pesto /pesto

FROM runtime-base AS runtime

COPY --from=builder /pesto /usr/local/bin/pesto
RUN chmod 755 /usr/local/bin/pesto

USER pesto
ENTRYPOINT ["tini", "--", "pesto"]
CMD ["--help"]
