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
# container may use. See HOW-TO-USE.md § 4.

FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p addok-cli

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/WildBenji/addok-rs" \
      org.opencontainers.image.description="Batch geocoding of French addresses against the BAN: addok rewritten in Rust" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /src/target/release/addok-cli /usr/local/bin/addok-cli
RUN useradd --system --no-create-home addok
USER addok
EXPOSE 7878
# docker stop sends SIGTERM, which addok-cli, as PID 1 without a handler for
# it, would ignore until the kill; it shuts down cleanly on SIGINT (Ctrl-C).
STOPSIGNAL SIGINT
ENTRYPOINT ["addok-cli", "serve", "/data/ban.addok", "--host", "0.0.0.0", "--port", "7878"]
