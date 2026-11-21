#!/usr/bin/env bash
# Build cardputer-zero-llrt_<version>_arm64.deb containing:
#   /usr/local/bin/cz-llrt (multicall LLRT v0.10.0-beta + cardputer:* + cz-zpm)
#   /usr/local/bin/node    (symlink -> cz-llrt)
#   /usr/local/bin/zpm     (symlink -> cz-llrt)
#   /usr/local/bin/npm     (symlink -> cz-llrt)
#   /usr/local/bin/esbuild (@esbuild/linux-arm64 static binary)
#
# Usage: bash scripts/build-llrt-deb.sh [version] [path-to-cz-llrt-binary]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-0.1.0}"
VERSION="${VERSION#v}"
VERSION="${VERSION#llrt-v}"
LLRT_BIN="${2:-$ROOT_DIR/dist/cz-llrt-linux-a53}"
ESBUILD_VERSION="${ESBUILD_VERSION:-0.28.2}"

[ -f "$LLRT_BIN" ] || { echo "FAIL: cz-llrt binary not found at $LLRT_BIN" >&2; exit 1; }

DIST_DIR="$ROOT_DIR/dist"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/llrt-deb.XXXXXX")"
trap 'rm -rf "$STAGE_DIR"' EXIT

echo "=== [1/4] Verifying Cortex-A53 ARMv8.0 instruction safety (LSE <= 200, RCpc == 0) ==="
OBJDUMP="$(command -v aarch64-linux-gnu-objdump || command -v objdump)"
"$OBJDUMP" -d --no-show-raw-insn "$LLRT_BIN" > "$STAGE_DIR/disasm.txt"
LSE_COUNT="$(grep -cE '\s(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas)(a|l|al)?(b|h)?\s' "$STAGE_DIR/disasm.txt" || true)"
RCPC_COUNT="$(grep -cE '\s(sdot|udot|ldapr|ldaprb|ldaprh)\s' "$STAGE_DIR/disasm.txt" || true)"
echo "CZ_LLRT_LSE=$LSE_COUNT CZ_LLRT_RCPC=$RCPC_COUNT"
if [ "$LSE_COUNT" -gt 200 ] || [ "$RCPC_COUNT" -gt 0 ]; then
  echo "FAIL: incompatible Cortex-A53 instructions (LSE=$LSE_COUNT RCPC=$RCPC_COUNT)" >&2
  exit 1
fi

echo "=== [2/4] Fetching @esbuild/linux-arm64@${ESBUILD_VERSION} ==="
EB_URL="https://registry.npmjs.org/@esbuild/linux-arm64/-/linux-arm64-${ESBUILD_VERSION}.tgz"
curl -fsSL "$EB_URL" -o "$STAGE_DIR/esbuild.tgz"
tar -xzf "$STAGE_DIR/esbuild.tgz" -C "$STAGE_DIR" package/bin/esbuild
[ -f "$STAGE_DIR/package/bin/esbuild" ] || { echo "FAIL: esbuild binary missing in tarball" >&2; exit 1; }

echo "=== [3/4] Assembling Debian package root ==="
PKG_ROOT="$STAGE_DIR/pkg"
mkdir -p "$PKG_ROOT/DEBIAN" "$PKG_ROOT/usr/local/bin"

install -m 0755 "$LLRT_BIN" "$PKG_ROOT/usr/local/bin/cz-llrt"
ln -s cz-llrt "$PKG_ROOT/usr/local/bin/node"
ln -s cz-llrt "$PKG_ROOT/usr/local/bin/zpm"
ln -s cz-llrt "$PKG_ROOT/usr/local/bin/npm"
install -m 0755 "$STAGE_DIR/package/bin/esbuild" "$PKG_ROOT/usr/local/bin/esbuild"

INSTALLED_KB="$(du -sk "$PKG_ROOT/usr" | awk '{print $1}')"
cat > "$PKG_ROOT/DEBIAN/control" <<EOF
Package: cardputer-zero-llrt
Version: ${VERSION}
Section: devel
Priority: optional
Architecture: arm64
Installed-Size: ${INSTALLED_KB}
Replaces: cardputer-zero-zpm
Conflicts: cardputer-zero-zpm
Maintainer: dj-oyu <dj-oyu@users.noreply.github.com>
Homepage: https://github.com/dj-oyu/cardputer-zero-llrt
Description: CardputerZero LLRT runtime + multicall node/zpm/npm + esbuild
 Full static Cortex-A53 LLRT v0.10.0-beta runtime with cardputer:* HAL,
 node:http/SSE, cz-json NEON lazy parser, native multicall zpm/npm/node
 aliases, and @esbuild/linux-arm64.
EOF

echo "=== [4/4] Building .deb package ==="
mkdir -p "$DIST_DIR"
OUT_DEB="$DIST_DIR/cardputer-zero-llrt_${VERSION}_arm64.deb"
dpkg-deb --build --root-owner-group "$PKG_ROOT" "$OUT_DEB"
( cd "$DIST_DIR" && sha256sum "$(basename "$OUT_DEB")" > "$(basename "$OUT_DEB").sha256" )

dpkg-deb -I "$OUT_DEB"
dpkg-deb -c "$OUT_DEB"
ls -lh "$OUT_DEB"
echo "LLRT_DEB_BUILD_OK: $OUT_DEB"
