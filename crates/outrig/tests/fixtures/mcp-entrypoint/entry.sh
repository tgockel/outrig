#!/bin/sh
# Entrypoint-stdio fixture: touch the network BEFORE serving MCP, so the e2e
# suite can prove session policy was attached ahead of the entrypoint's first
# packet (the audit log must carry this lookup attributed to this container).
# Retried because parallel e2e image builds can saturate the network long
# enough for a single short attempt to give up before opening a connection.
for _ in 1 2 3; do
    wget -q -T 5 -O /dev/null http://example.com/ && break
done
exec mcp-server-filesystem /tmp
