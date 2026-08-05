FROM docker.io/library/node:22-slim AS mcp

RUN npm install -g mcp-searxng@1.14.0 \
 && npm cache clean --force \
 && test -f /usr/local/lib/node_modules/mcp-searxng/dist/cli.js

FROM docker.io/searxng/searxng:2026.7.19-6da6eee26

COPY --from=mcp /usr/local/bin/node /usr/local/bin/node
COPY --from=mcp /usr/local/lib/node_modules/mcp-searxng /usr/local/lib/node_modules/mcp-searxng

# Settings live outside `/etc/searxng` because the base image declares it a `VOLUME`:
# anything written there during a build is discarded. Upstream's fallback -- copy the
# template in on first start -- does not save us either, since `--userns=keep-id` runs
# this container as the host uid, which cannot write a `searxng`-owned directory.
# `secret_key` gets a build-time random value for the same reason upstream seds one in:
# searxng exits rather than serve with the literal `ultrasecretkey`.
RUN mkdir -p /usr/local/etc/searxng \
 && printf '%s\n' \
  'use_default_settings: true' \
  'server:' \
  '  secret_key: "ultrasecretkey"' \
  '  limiter: false' \
  '  image_proxy: false' \
  'search:' \
  '  formats:' \
  '    - html' \
  '    - json' \
  > /usr/local/etc/searxng/settings.yml \
 && sed -i "s/ultrasecretkey/$(head -c 24 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9')/" \
  /usr/local/etc/searxng/settings.yml \
 && chown -R searxng:searxng /usr/local/etc/searxng

# Two names for one file, and they have to agree: the entrypoint checks (and would seed)
# `$__SEARXNG_CONFIG_PATH/settings.yml`, while searxng itself reads the path in
# `SEARXNG_SETTINGS_PATH`. The `searxng` ownership above keeps the entrypoint's
# ownership check quiet -- it warns on anything that is not `searxng:searxng`.
ENV __SEARXNG_CONFIG_PATH=/usr/local/etc/searxng
ENV SEARXNG_SETTINGS_PATH=/usr/local/etc/searxng/settings.yml
ENV SEARXNG_URL=http://localhost:8080

# `searx` is imported from the source tree rather than installed into the venv, so the
# search half only resolves it with that tree as its working directory. Held in its own
# subshell rather than left to the image's `WORKDIR`, which a sidecar declaring a
# `workspace` would lose to outrig's `-w`.
ENTRYPOINT ["/bin/sh", "-c", \
            "(cd /usr/local/searxng && exec ./entrypoint.sh) 1>&2 & exec /usr/local/bin/node /usr/local/lib/node_modules/mcp-searxng/dist/cli.js"]
