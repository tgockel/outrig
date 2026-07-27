# A Debian/glibc primary with a real cargo at /usr/local/cargo/bin/cargo -- a
# path no bind mount provides. The primary-view e2e (primary_view_e2e.rs)
# asserts a `view = "primary"` sidecar, built from an Alpine/musl MCP image,
# sees it through the graft. This mirrors the prototype's check #12 target
# (https://github.com/tgockel/prototype-podman-shared-fs). The official rust
# image already ships cargo, and the user bootstrap needs nothing from the
# image at all, so the fixture adds nothing.
FROM docker.io/library/rust:1-slim
