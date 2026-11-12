FROM docker.io/library/rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      coreutils \
      curl \
      findutils \
      git \
      jq \
      libssl-dev \
      pkg-config \
      procps \
      python3 \
      ripgrep \
 && rm -rf /var/lib/apt/lists/*

# The musl target is not optional here: `crates/outrig/build.rs` compiles the `outrig-enter`
# launcher for it, and without it embeds an empty artifact and warns. An `outrig` built in
# this container would then fail to start its own `view = "primary"` sidecars -- which is
# every MCP server this repo's config declares.
RUN rustup component add clippy rustfmt \
 && rustup target add "$(uname -m)-unknown-linux-musl"

RUN cargo install mdbook mdbook-mermaid lychee --locked

# After the `cargo install` above, so those binaries stay in `/usr/local/cargo/bin` on
# `PATH`. The base image's `/usr/local/cargo` is not writable by the session user and holds
# none of this repo's dependencies, so a build with it fails either on a permission denied
# under `registry/` or, offline, on a missing crate. The checkout's gitignored
# `/workspace/.cargo` is the cargo home that works; rustup still finds its toolchain through
# `RUSTUP_HOME=/usr/local/rustup`.
ENV CARGO_HOME=/workspace/.cargo

WORKDIR /workspace
CMD ["sleep", "infinity"]
