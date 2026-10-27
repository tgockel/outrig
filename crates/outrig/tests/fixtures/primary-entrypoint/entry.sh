#!/bin/sh
# A primary image's ENTRYPOINT that touches the network, which `containers.md`
# asks nobody to set and which podman hands `sleep infinity` as its arguments.
# The CLI e2e suite uses it to prove the interceptor is attached before this
# runs (#395): the request below has to land in the audit log, attributed to
# the primary. Its target is the host listener the test opened, whose port it
# left in the workspace -- the only input a primary's entrypoint is given.
wget -q -T 5 -O /dev/null "http://host.containers.internal:$(cat /workspace/port)/"
# What an entrypoint wrapper does when it has nothing more to say: hand over to
# the command it was given, here `sleep infinity`.
exec "$@"
