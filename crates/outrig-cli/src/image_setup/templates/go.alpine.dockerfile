RUN case "$(uname -m)" in \
      x86_64)  arch=amd64 sum=63d339f0da5ab53635a56f2490a7984dfe12dfcff22ad749f63edaf590168445 ;; \
      aarch64) arch=arm64 sum=3450b45a3f9ee8568792736a5c5e70a1f2e9b36c35a8f74958c03e51d7d92bec ;; \
      *) echo "go toolchain: unsupported architecture $(uname -m)" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/go.tgz "https://go.dev/dl/go1.27.1.linux-$arch.tar.gz" \
 && echo "$sum  /tmp/go.tgz" | sha256sum -c - \
 && tar -C /usr/local -xzf /tmp/go.tgz \
 && rm /tmp/go.tgz
ENV PATH=/usr/local/go/bin:$PATH
