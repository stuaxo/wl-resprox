#!/usr/bin/env bash
# Builds the wayland-headless-harness .deb: stages scripts/ (the
# harness's own files) plus this directory's CLI dispatcher into a
# tree matching the target filesystem layout, and runs `dpkg-deb
# --build` against it.
#
# Deliberately NOT dpkg-buildpackage/debhelper -- this package is pure
# scripts + data files, no compilation, and debian/control here is a
# single flat stanza (no separate "Source:" preamble) fed straight to
# dpkg-deb, not the two-stanza source-package format debhelper expects.
# A hand-built DEBIAN/control + dpkg-deb --build produces an equally
# real, equally installable .deb (verified via dpkg -i) with far less
# machinery that could go subtly wrong for a first pass at this.
#
# Usage: ./packaging/build-harness-deb.sh
# Output: target/harness-deb/wayland-headless-harness_<version>_all.deb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
PKG_NAME="wayland-headless-harness"
# No independent version scheme exists yet for the harness (unlike the
# proxy, which has Cargo.toml) -- date-stamped for now. Revisit if this
# ever needs to track compatibility with a specific wayland-proxy
# release, rather than being versioned entirely on its own.
VERSION="$(date +%Y.%m.%d)"

OUT_DIR="$PROJECT_ROOT/target/harness-deb"
STAGING="$OUT_DIR/staging"
LIB_DIR="$STAGING/usr/lib/$PKG_NAME"

rm -rf "$STAGING"
mkdir -p "$STAGING/DEBIAN" "$LIB_DIR" "$STAGING/usr/bin"

cp "$PROJECT_ROOT"/scripts/*.sh "$LIB_DIR/"
cp -r "$PROJECT_ROOT/scripts/containers" "$LIB_DIR/"
chmod +x "$LIB_DIR"/*.sh

install -m 0755 "$SCRIPT_DIR/harness/wayland-headless-harness" "$STAGING/usr/bin/wayland-headless-harness"

{
    cat "$PROJECT_ROOT/debian/control"
    echo "Version: ${VERSION}"
} > "$STAGING/DEBIAN/control"

mkdir -p "$OUT_DIR"
DEB_PATH="$OUT_DIR/${PKG_NAME}_${VERSION}_all.deb"
dpkg-deb --build --root-owner-group "$STAGING" "$DEB_PATH"
echo "Built: $DEB_PATH"
