#!/usr/bin/env bash
# Build cardputer-zero-zpm_<version>_arm64.deb containing:
#   /usr/local/bin/zpm     (native Cortex-A53 cz-zpm multicall binary)
#   /usr/local/bin/npm     (symlink -> zpm)
#   /usr/local/bin/esbuild (@esbuild/linux-arm64 static binary)
#
# Usage: bash scripts/build-zpm-deb.sh [version]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-0.1.0}"
VERSION="${VERSION#v}"
VERSION="${VERSION#zpm-v}"
ESBUILD_VERSION="${ESBUILD_VERSION:-0.28.2}"

DIST_DIR="$ROOT_DIR/dist"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/zpm-deb.XXXXXX")"
trap 'rm -rf "$STAGE_DIR"' EXIT

echo "=== [1/5] Building cz-zpm for aarch64-unknown-linux-gnu (Cortex-A53, no LSE) ==="
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}"
export CC_aarch64_unknown_linux_gnu="${CC_aarch64_unknown_linux_gnu:-aarch64-linux-gnu-gcc}"
export RUSTFLAGS="-Ctarget-cpu=cortex-a53 -Ctarget-feature=-lse ${RUSTFLAGS:-}"

cd "$ROOT_DIR"
cargo build --release -p cz-zpm --target aarch64-unknown-linux-gnu

TARGET_BASE="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
ZPM_BIN="$TARGET_BASE/aarch64-unknown-linux-gnu/release/cz-zpm"
[ -f "$ZPM_BIN" ] || { echo "FAIL: binary not found at $ZPM_BIN" >&2; exit 1; }

cp "$ZPM_BIN" "$STAGE_DIR/cz-zpm"
if command -v aarch64-linux-gnu-strip >/dev/null 2>&1; then
  aarch64-linux-gnu-strip "$STAGE_DIR/cz-zpm"
fi

echo "=== [2/5] Verifying Cortex-A53 ARMv8.0 instruction safety (LSE <= 200, RCpc == 0) ==="
OBJDUMP="$(command -v aarch64-linux-gnu-objdump || command -v objdump)"
"$OBJDUMP" -d --no-show-raw-insn "$STAGE_DIR/cz-zpm" > "$STAGE_DIR/disasm.txt"
LSE_COUNT="$(grep -cE '\s(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas)(a|l|al)?(b|h)?\s' "$STAGE_DIR/disasm.txt" || true)"
RCPC_COUNT="$(grep -cE '\s(sdot|udot|ldapr|ldaprb|ldaprh)\s' "$STAGE_DIR/disasm.txt" || true)"
echo "CZ_ZPM_LSE=$LSE_COUNT CZ_ZPM_RCPC=$RCPC_COUNT"
if [ "$LSE_COUNT" -gt 200 ] || [ "$RCPC_COUNT" -gt 0 ]; then
  echo "FAIL: incompatible Cortex-A53 instructions (LSE=$LSE_COUNT RCPC=$RCPC_COUNT)" >&2
  exit 1
fi

echo "=== [3/5] Fetching @esbuild/linux-arm64@${ESBUILD_VERSION} ==="
EB_URL="https://registry.npmjs.org/@esbuild/linux-arm64/-/linux-arm64-${ESBUILD_VERSION}.tgz"
curl -fsSL "$EB_URL" -o "$STAGE_DIR/esbuild.tgz"
tar -xzf "$STAGE_DIR/esbuild.tgz" -C "$STAGE_DIR" package/bin/esbuild
[ -f "$STAGE_DIR/package/bin/esbuild" ] || { echo "FAIL: esbuild binary missing in tarball" >&2; exit 1; }

echo "=== [4/5] Assembling Debian package root ==="
PKG_ROOT="$STAGE_DIR/pkg"
mkdir -p "$PKG_ROOT/DEBIAN" "$PKG_ROOT/usr/local/bin"

install -m 0755 "$STAGE_DIR/cz-zpm" "$PKG_ROOT/usr/local/bin/zpm"
ln -s zpm "$PKG_ROOT/usr/local/bin/npm"
install -m 0755 "$STAGE_DIR/package/bin/esbuild" "$PKG_ROOT/usr/local/bin/esbuild"

INSTALLED_KB="$(du -sk "$PKG_ROOT/usr" | awk '{print $1}')"
cat > "$PKG_ROOT/DEBIAN/control" <<EOF
Package: cardputer-zero-zpm
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: arm64
Installed-Size: ${INSTALLED_KB}
Maintainer: dj-oyu <dj-oyu@users.noreply.github.com>
Homepage: https://github.com/dj-oyu/cardputer-zero-llrt
Description: Zero Package Manager (zpm / npm shim + esbuild) for CardputerZero
 Native Rust multicall package manager (zpm / npm) bundled with
 @esbuild/linux-arm64 for M5Stack CardputerZero (Cortex-A53).
EOF

echo "=== [5/5] Building .deb package ==="
mkdir -p "$DIST_DIR"
OUT_DEB="$DIST_DIR/cardputer-zero-zpm_${VERSION}_arm64.deb"
dpkg-deb --build --root-owner-group "$PKG_ROOT" "$OUT_DEB"
( cd "$DIST_DIR" && sha256sum "$(basename "$OUT_DEB")" > "$(basename "$OUT_DEB").sha256" )

dpkg-deb -I "$OUT_DEB"
dpkg-deb -c "$OUT_DEB"
ls -lh "$OUT_DEB"
echo "ZPM_DEB_BUILD_OK: $OUT_DEB"
