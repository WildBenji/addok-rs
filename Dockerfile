# addok-cli serve, its index mounted from outside: a new BAN edition is a new
# index file, never a new image. Plain `docker build`, no BuildKit-only
# syntax, so the legacy builder works too; it builds for the host's own
# architecture (linux/arm64 on a Mac, linux/amd64 on a Linux server). The
# published image, for both, is ghcr.io/wildbenji/addok-rs
# (.github/workflows/docker.yml).
#
#   docker build -t addok-rs .
#   docker run -p 7878:7878 -v /path/to/index-dir:/data:ro addok-rs [--cores N]
#
# The index is /data/ban.addok. Every argument after the image name is
# appended to `serve`: `--cores N` caps the cores, which default to those the
# container may use. See HOW-TO-USE.md § 5.

# Both images are pinned by digest, so that a tag rebuilt later is the same
# compiler and the same libraries; refreshed on purpose, by replacing each
# line with the digest `docker buildx imagetools inspect <image:tag>` prints.
FROM rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The tests run on the image's own platform and libm, each architecture
# natively, whoever pushed the tag.
RUN cargo test --release --locked --workspace
RUN cargo build --release --locked -p addok-cli

# glibc and libgcc, which the binary links, and nothing else of Debian's: no
# shell, no package manager, a fraction of debian:trixie-slim's packages and
# of their CVEs. Its user `nonroot` (65532) has no privileges.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cb87acc574abe719b6e5c547384defa2f7b1f24af9605d3f618cf852495d8be2
LABEL org.opencontainers.image.source="https://github.com/WildBenji/addok-rs" \
      org.opencontainers.image.description="Geocoding of French addresses against the BAN: addok rewritten in Rust" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /src/target/release/addok-cli /usr/local/bin/addok-cli
USER nonroot
EXPOSE 7878
ENTRYPOINT ["addok-cli", "serve", "/data/ban.addok", "--host", "0.0.0.0", "--port", "7878"]
