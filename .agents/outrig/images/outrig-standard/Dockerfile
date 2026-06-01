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
      nodejs \
      npm \
      passwd \
      pkg-config \
      procps \
      python3 \
      python3-pip \
      python3-venv \
      ripgrep \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

RUN cargo install mdbook mdbook-mermaid lychee --locked

RUN npm install -g @modelcontextprotocol/server-filesystem \
 && pip install --break-system-packages mcp-server-git mcp-shell-server

WORKDIR /workspace
CMD ["sleep", "infinity"]
