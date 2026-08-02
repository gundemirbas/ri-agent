#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────
# Fetch a static musl uutils coreutils into the sandbox provisioning dir.
#
# The sandbox image (docs/CONTAINER-RUNTIME-SPEC.md) carries self-contained
# file tools: the single multicall binary lives at rootfs/usr/bin/coreutils
# and the image assembly turns it into applet symlinks. With it present, the
# runtime does NOT bind the host's /bin,/usr (strictest isolation); without
# it the sandbox still works via read-only host binds (documented fallback).
#
# The result is gitignored (~14MB, stripped static-pie); rerun after upgrades.
# ─────────────────────────────────────────────────────────────────────────
set -euo pipefail

VERSION="${RI_UUTILS_VERSION:-0.9.0}"
ARCH="${RI_UUTILS_ARCH:-x86_64-unknown-linux-musl}"
DEST="${1:-rootfs/usr/bin/coreutils}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

URL="https://github.com/uutils/coreutils/releases/download/${VERSION}/coreutils-${VERSION}-${ARCH}.tar.gz"
echo "fetch: $URL"
curl -fsSL "$URL" -o "$TMPDIR/uu.tar.gz"
tar -xzf "$TMPDIR/uu.tar.gz" -C "$TMPDIR"

BIN="$(find "$TMPDIR" -type f -name coreutils | head -n 1)"
[ -n "$BIN" ] || { echo "error: no coreutils binary in release tarball" >&2; exit 1; }

if command -v file >/dev/null 2>&1 && ! file "$BIN" | grep -qi static; then
  echo "warning: downloaded binary does not look static (dynamic deps must be in the image)" >&2
fi

mkdir -p "$(dirname "$DEST")"
cp "$BIN" "$DEST"
chmod +x "$DEST"
echo "provisioned $DEST ($(du -h "$DEST" | cut -f1) uutils coreutils $VERSION)"
