FROM docker.io/library/node:22-slim

RUN npm install -g mcp-server-commands@0.8.2 \
 && npm cache clean --force \
 && test -f /usr/local/lib/node_modules/mcp-server-commands/build/index.js

# Same constraint as the mcp-git image: the launcher refuses a `#!` console script, so the
# interpreter is named and the server's entry module is its argument. Absolute, so that the
# `PATH` this server's config sets -- the primary's -- is never searched for it.
ENTRYPOINT ["/usr/local/bin/node", \
            "/usr/local/lib/node_modules/mcp-server-commands/build/index.js"]
