FROM docker.io/library/python:3.13-slim

# The SDK pin is load-bearing, not tidiness: mcp-server-git 2026.7.10 declares `mcp>=1.0.0`,
# and mcp 2.0.0 dropped the low-level `Server.list_tools` decorator it builds its tool list
# with, so an unpinned install crashes on the first request.
# The `test` turns a moved install path into a build failure rather than a session-start one.
RUN pip install --no-cache-dir mcp-server-git==2026.7.10 mcp==1.29.0 \
 && test -x /usr/local/bin/mcp-server-git

# `outrig-enter` opens the ENTRYPOINT program before joining the primary's mount namespace
# and refuses anything that is not an ELF64 binary, so the console script cannot be named
# directly -- its `#!` line would resolve against the primary's rootfs. Name the interpreter
# and hand it the same script. No `git` binary is installed here on purpose: the server runs
# in the primary's mount namespace, which has one.
ENTRYPOINT ["/usr/local/bin/python3", "/usr/local/bin/mcp-server-git"]
