#!/bin/sh
# Copy the release binary to ./rust-xpra-$VERSION-r$REVISION.exe, where
# VERSION is the crate version from Cargo.toml and REVISION is the number
# of commits since the most recent tag reachable from HEAD.
set -eu

cd "$(dirname "$0")"

cargo build -r

SRC="target/release/xpra.exe"
if [ ! -f "$SRC" ]; then
    echo "error: $SRC not found after 'cargo build -r'" >&2
    exit 1
fi

# first `version = "..."` line, which is the [package] one
VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
if [ -z "$VERSION" ]; then
    echo "error: could not find the version in Cargo.toml" >&2
    exit 1
fi

if TAG=$(git describe --tags --abbrev=0 2>/dev/null); then
    REVISION=$(git rev-list --count "$TAG..HEAD")
else
    # no tag at all: count every commit
    REVISION=$(git rev-list --count HEAD)
fi

DEST="rust-xpra-$VERSION-r$REVISION.exe"
cp "$SRC" "$DEST"
echo "$DEST"
