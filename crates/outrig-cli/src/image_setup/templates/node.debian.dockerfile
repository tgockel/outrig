RUN node_major=24 \
 && curl -fsSL "https://deb.nodesource.com/setup_$node_major.x" | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/* \
 && case "$(node --version)" in \
      "v$node_major".*) ;; \
      *) echo "node toolchain: expected Node $node_major, found $(node --version)" >&2; exit 1 ;; \
    esac \
 && npm --version
