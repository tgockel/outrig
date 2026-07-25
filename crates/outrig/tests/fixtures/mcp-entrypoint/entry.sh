#!/bin/sh
# Entrypoint-stdio fixture. The served directories come from the container
# argv, i.e. an MCP entry's or sidecar block's `args`.
#
# Fail loudly when the argv is empty. mcp-server-filesystem does not: it starts
# with no allowed directory and waits for the client to supply roots over the
# MCP protocol, which outrig's proxy never does. That would still fail the e2e
# assertions -- every path is then "outside allowed directories" -- but it would
# fail as a denied tool call rather than as a missing argument, which is a
# slower thing to read. Refusing here keeps "the args did not arrive" a
# startup error.
if [ "$#" -eq 0 ]; then
    echo "entry.sh: no directory to serve; the container argv was empty" >&2
    exit 64
fi

# Touch the network BEFORE serving MCP, so the e2e suite can prove session
# policy was attached ahead of the entrypoint's first packet (the audit log
# must carry this lookup attributed to this container). Retried because
# parallel e2e image builds can saturate the network long enough for a single
# short attempt to give up before opening a connection.
for _ in 1 2 3; do
    wget -q -T 5 -O /dev/null http://example.com/ && break
done
exec mcp-server-filesystem "$@"
