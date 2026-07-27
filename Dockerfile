# init-pro — single multicall binary.
# Multi-stage: build a stripped release binary, then copy it into a small
# runtime image. argv[0] selects behavior (server | agent | kubectl | ctr |
# crictl | containerd | etcd), so one image serves every role.
#
# Build:  docker build -t init-pro .
# Run:    docker run --rm -p 6443:6443 init-pro server
# (Bundled peers are NOT embedded by default; set --build-arg EMBED=1 to bake
#  containerd/runc/cni into the binary, which needs network during build.)
ARG EMBED=0

FROM rust:1.89-bookworm AS builder
ARG EMBED
WORKDIR /src
# Copy the manifest + lock first for layer caching, then the sources.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
COPY vendor/versions.toml ./vendor/versions.toml
# Replicate the gitignored vendor cache layout expected by build.rs when
# EMBED=1. Provide an empty cache when EMBED=0 (no network, fast build).
RUN mkdir -p vendor/bin vendor/cache
ENV INIT_PRO_VENDOR=${EMBED} \
    INIT_PRO_EMBED=${EMBED}
RUN cargo build --workspace --locked --release

FROM debian:bookworm-slim AS runtime
# The binary is a multicall dispatcher; install it under every alias so
# `docker exec <ctr> kubectl ...` and symlink-style invocation both work.
COPY --from=builder /src/target/release/init-pro /usr/local/bin/init-pro
RUN for a in server agent kubectl ctr crictl containerd etcd; do \
        ln -s init-pro /usr/local/bin/$a; \
    done
# apiserver (6443). The Router data plane and bundled CRI listen on other
# ports once those layers land; document 6443 now, the stable default.
EXPOSE 6443
ENTRYPOINT ["init-pro"]
CMD ["server", "--bind-address", "0.0.0.0", "--https-listen-port", "6443"]
