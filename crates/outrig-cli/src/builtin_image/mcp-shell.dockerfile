# The shell server for outrig's built-in default image-config. Materialized into the user
# cache directory by `builtin_image::materialize` and built from there; it is never written
# into a user's repo. Kept byte-compatible in spirit with
# `.agents/outrig/images/mcp-shell/Dockerfile`, which is the copy this repo dogfoods.
FROM docker.io/library/node:22-slim

RUN npm install -g mcp-server-commands@0.8.2 \
 && npm cache clean --force \
 && test -f /usr/local/lib/node_modules/mcp-server-commands/build/index.js

# Same constraint as the mcp-git image: the launcher refuses a `#!` console script, so the
# interpreter is named and the server's entry module is its argument. Absolute, so that the
# `PATH` in effect -- the primary's -- is never searched for it.
ENTRYPOINT ["/usr/local/bin/node", \
            "/usr/local/lib/node_modules/mcp-server-commands/build/index.js"]
