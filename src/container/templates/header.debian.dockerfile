FROM docker.io/library/{IMAGE}

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential passwd \
 && rm -rf /var/lib/apt/lists/*
