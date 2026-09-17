#!/usr/bin/env bash
# Fetch the static CPython payload that `outrig run` mounts into the session container.
#
# Prototype tooling: a developer runs this once. It is deliberately not part of `cargo build`
# -- nothing is downloaded at build time, and nothing is downloaded at session run time.
#
# The `+static` variant is mandatory. The plain musl builds link against a musl runtime the
# *image* is expected to provide, which defeats the entire point: the payload has to run in a
# distroless image that contains nothing at all.

set -euo pipefail

RELEASE="20260901"
PY_VERSION="3.13.15"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64)
        TRIPLE="x86_64-unknown-linux-musl"
        SHA256="68606ae38cb3f4db0d0fdb75b16dde78888428161e8f82430915d405b5ea96de"
        ;;
    *)
        echo "error: no pinned payload for $ARCH (the prototype is x86_64 only)" >&2
        exit 1
        ;;
esac

ARCHIVE="cpython-${PY_VERSION}+${RELEASE}-${TRIPLE}-noopt+static-full.tar.zst"
URL="https://github.com/astral-sh/python-build-standalone/releases/download/${RELEASE}/${ARCHIVE}"
DEST="${XDG_CACHE_HOME:-$HOME/.cache}/outrig/python/${ARCH}"

if [ -x "$DEST/bin/python3" ]; then
    echo "payload already present: $DEST"
    exit 0
fi

for tool in curl sha256sum tar zstd file; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "downloading $ARCHIVE"
curl --fail --location --progress-bar --output "$WORK/$ARCHIVE" "$URL"

echo "verifying sha256"
echo "$SHA256  $WORK/$ARCHIVE" | sha256sum --check --status || {
    echo "error: sha256 mismatch for $ARCHIVE" >&2
    echo "  expected $SHA256" >&2
    echo "  actual   $(sha256sum "$WORK/$ARCHIVE" | cut -d' ' -f1)" >&2
    exit 1
}

echo "extracting"
tar --zstd -xf "$WORK/$ARCHIVE" -C "$WORK"
[ -d "$WORK/python/install" ] || { echo "error: archive has no python/install" >&2; exit 1; }

# The interpreter has to be genuinely self-contained or it will fail inside a minimal image
# with a missing-loader error that names a path nobody configured.
DESC="$(file -b "$WORK/python/install/bin/python3.13")"
case "$DESC" in
    *"ELF 64-bit"*"x86-64"*"statically linked"*) ;;
    *)
        echo "error: bin/python3.13 is not a static ELF64 x86-64 executable" >&2
        echo "  file says: $DESC" >&2
        exit 1
        ;;
esac

rm -rf "$DEST"
mkdir -p "$(dirname "$DEST")"
mv "$WORK/python/install" "$DEST"

echo
echo "payload ready: $DEST"
"$DEST/bin/python3" -I -c 'import sys; print(f"  {sys.version.split()[0]} ({sys.platform})")'
