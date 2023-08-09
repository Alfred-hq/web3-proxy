FROM debian:bullseye-slim as rust

WORKDIR /app
# sccache cannot cache incrementals, but we use --mount=type=cache and import caches so it should be helpful
ENV CARGO_INCREMENTAL true
ENV CARGO_UNSTABLE_SPARSE_REGISTRY true
ENV CARGO_TERM_COLOR always
SHELL [ "/bin/bash", "-c" ]
ENV SHELL /bin/bash
ENV PATH "/root/.foundry/bin:/root/.cargo/bin:${PATH}"

# install rustup dependencies
# also install web3-proxy system dependencies. most things are rust-only, but not everything
RUN set -eux -o pipefail; \
    \
    apt-get update; \
    apt-get install --no-install-recommends --yes \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    liblz4-dev \
    libpthread-stubs0-dev \
    libsasl2-dev \
    libzstd-dev \
    make \
    pkg-config \
    ;

# install rustup
RUN set -eux -o pipefail; \
    \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile=minimal

# run a cargo command to install our desired version of rust
# it is expected to exit code 101 since no Cargo.toml exists
COPY rust-toolchain.toml ./
RUN set -eux -o pipefail; \
    \
    cargo check || [ "$?" -eq 101 ]

# cargo binstall
RUN set -eux -o pipefail; \
    \
    curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh >/tmp/install-binstall.sh; \
    bash /tmp/install-binstall.sh; \
    rm -rf /tmp/*

FROM rust as rust_with_env

# changing our features doesn't change any of the steps above
ENV WEB3_PROXY_FEATURES "deadlock_detection,rdkafka-src"

# copy the app
COPY . .

# fetch deps
RUN set -eux -o pipefail; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo --locked --verbose fetch

FROM rust_with_env as build_app

# build the release application
# using a "release" profile (which install does by default) is **very** important
# TODO: use the "faster_release" profile which builds with `codegen-units = 1` (but compile is SLOW)

# RUN apt-get update && apt install libssl-dev -y

RUN set -eux -o pipefail; \
    \
    [ -e "$(pwd)/payment-contracts/src/contracts/mod.rs" ] || touch "$(pwd)/payment-contracts/build.rs"; \
    cargo install \
    --features "$WEB3_PROXY_FEATURES" \
    --frozen \
    --no-default-features \
    --offline \
    --path ./web3_proxy \
    --root /usr/local \
    ; \
    /usr/local/bin/web3_proxy_cli --help | grep 'Usage: web3_proxy_cli'

# RUN cargo build --release --frozen

# copy this file so that docker actually creates the build_tests container
# without this, the runtime container doesn't need build_tests and so docker build skips it
# COPY --from=build_tests /test_success /

FROM ubuntu:latest as ub
RUN apt-get update && apt-get install -y ca-certificates

#
# We do not need the Rust toolchain or any deps to run the binary!
#
FROM debian:bullseye AS runtime

# Create llama user to avoid running container with root
RUN set -eux; \
    \
    mkdir /llama; \
    adduser --home /llama --shell /sbin/nologin --gecos '' --no-create-home --disabled-password --uid 1001 llama; \
    chown -R llama /llama

USER llama

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy::rpcs::consensus=info,web3_proxy_cli=debug"

# we copy something from build_tests just so that docker actually builds it
COPY --from=build_app /usr/local/bin/* /usr/local/bin/
COPY --from=ub /etc/ssl/certs /etc/ssl/certs

# make sure the app works
RUN web3_proxy_cli --help
