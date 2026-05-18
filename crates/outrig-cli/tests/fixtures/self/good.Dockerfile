FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends passwd \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
CMD ["sleep", "infinity"]
