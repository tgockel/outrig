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

WORKDIR /workspace
CMD ["sleep", "infinity"]
