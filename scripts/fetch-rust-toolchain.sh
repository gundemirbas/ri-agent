#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────
# Provision the sandbox's OWN Rust toolchain (not the system rustup).
#
# Downloads and assembles a self-contained, musl-HOST Rust toolchain into
# rootfs/toolchain/ (gitignored):
#   - rustc + rust-std for x86_64-unknown-linux-musl (rust-lang dist; host
#     triple IS musl, so rustc itself runs on musl with no glibc),
#   - the musl dynamic loader (ld-musl) and libgcc_s (Alpine official
#     packages) — the two runtime libs the musl-host rustc needs.
#
# The sandbox image then BINDs this toolchain (read-only) at /toolchain,
# so the agent can compile custom tools inside the sandbox (see the `rustc`
# tool + docs/CONTAINER-RUNTIME-SPEC.md §6/§16). Nothing touches the user's
# rustup; pin the versions below for reproducible builds.
# ─────────────────────────────────────────────────────────────────────────
set -euo pipefail

RUST_VERSION="${RI_RUST_VERSION:-1.97.1}"
TRIPLE="x86_64-unknown-linux-musl"
MUSL_VERSION="${RI_MUSL_VERSION:-1.2.5-r12}"
LIBGCC_VERSION="${RI_LIBGCC_VERSION:-14.2.0-r6}"
ALPINE_BASE="${RI_ALPINE_BASE:-https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64}"
DIST_BASE="${RI_DIST_BASE:-https://static.rust-lang.org/dist}"
DEST="${1:-rootfs/toolchain}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "rust: $RUST_VERSION ($TRIPLE)  musl: $MUSL_VERSION  libgcc: $LIBGCC_VERSION"

# 1. rustc + rust-std (musl-host)
curl -fsSL "$DIST_BASE/rustc-${RUST_VERSION}-${TRIPLE}.tar.xz"  -o "$TMPDIR/rustc.tar.xz"
curl -fsSL "$DIST_BASE/rust-std-${RUST_VERSION}-${TRIPLE}.tar.xz" -o "$TMPDIR/std.tar.xz"
tar -xJf "$TMPDIR/rustc.tar.xz" -C "$TMPDIR"
tar -xJf "$TMPDIR/std.tar.xz"  -C "$TMPDIR"
TC="rustc-${RUST_VERSION}-${TRIPLE}/rustc"
STD="rust-std-${RUST_VERSION}-${TRIPLE}/rust-std-${TRIPLE}"

# 2. musl loader + libgcc_s (Alpine official)
curl -fsSL "$ALPINE_BASE/musl-${MUSL_VERSION}.apk"   -o "$TMPDIR/musl.apk"
curl -fsSL "$ALPINE_BASE/libgcc-${LIBGCC_VERSION}.apk" -o "$TMPDIR/libgcc.apk"
tar -xzf "$TMPDIR/musl.apk"   -C "$TMPDIR"
tar -xzf "$TMPDIR/libgcc.apk" -C "$TMPDIR"

# 3. Assemble the trimmed toolchain (no rustdoc/gdb/lldb/share).
rm -rf "$DEST"
mkdir -p "$DEST/bin" "$DEST/lib/lib"
cp "$TC/bin/rustc" "$DEST/bin/rustc"
cp "$TC"/lib/librustc_driver-*.so "$DEST/lib/"
cp -r "$TC/lib/rustlib" "$DEST/lib/rustlib"
# std rlibs (only where the rustc component did not already ship them)
cp -rn "$STD/lib/rustlib/x86_64-unknown-linux-musl/lib" "$DEST/lib/rustlib/x86_64-unknown-linux-musl/" 2>/dev/null || true
# runtime libs (musl loader + gcc runtime for the driver)
cp "$TMPDIR/lib/ld-musl-x86_64.so.1" "$DEST/lib/ld-musl-x86_64.so.1"
cp "$TMPDIR/usr/lib/libgcc_s.so.1"   "$DEST/lib/libgcc_s.so.1"
chmod +x "$DEST/bin/rustc"

echo "toolchain provisioned at $DEST ($(du -sh "$DEST" | cut -f1)) — rustc $RUST_VERSION $TRIPLE"
