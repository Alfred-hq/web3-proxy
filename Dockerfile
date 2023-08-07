FROM debian:bullseye-slim as rust_build

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


RUN set -eux -o pipefail; \
    \
    cargo binstall -y cargo-nextest


RUN set -eux -o pipefail; \
    \
    curl -L https://foundry.paradigm.xyz | bash && foundryup

# changing our features doesn't change any of the steps above
ENV WEB3_PROXY_FEATURES "deadlock_detection,rdkafka-src"

ENV CARGO_TARGET_DIR = /app/target

# copy the app
COPY . .

# fetch deps
RUN cargo --verbose fetch


FROM debian:bullseye-slim AS runtime


ENV CARGO_TARGET_DIR = /app/target

# TODO: lower log level when done with prototyping
ENV RUST_LOG "warn,ethers_providers::rpc=off,web3_proxy=debug,web3_proxy::rpcs::consensus=info,web3_proxy_cli=debug"

COPY --from=rust_build /app/target/* /app/target

# we copy something from build_tests just so that docker actually builds it
COPY --from=rust_build /usr/local/bin/* /usr/local/bin/

ENTRYPOINT ["web3_proxy_cli"]
CMD [ "--config", "/web3-proxy.toml", "proxyd" ]

# make sure the app works
RUN web3_proxy_cli --help
