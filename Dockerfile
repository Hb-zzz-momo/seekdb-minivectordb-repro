# MiniVectorDB evaluation image.
#
#   docker build -t minivectordb .
#   docker run -i --rm -v mvdb-data:/data minivectordb
#
# The database reads SQL on stdin, writes results to stdout and logs to stderr.
# All persistent files live in /data. No network access is needed at runtime.
# Base images are pinned by digest; the crate has no third-party dependencies.

FROM rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --release --locked

FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
COPY --from=build /src/target/release/minivectordb /usr/local/bin/minivectordb
VOLUME /data
ENTRYPOINT ["/usr/local/bin/minivectordb", "--data-dir", "/data"]
