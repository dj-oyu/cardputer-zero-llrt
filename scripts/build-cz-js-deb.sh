#!/usr/bin/env bash
# AWS-LLRT Free Cortex-A53 .deb packaging for the unified multicall `cz-js` runtime
# (standalone QuickJS + Rust modules on rustls/ring — zero awslabs/llrt, zero Tokio).
#
# Builds cardputer-zero-js_<version>_arm64.deb containing:
#   /usr/local/bin/cz-js   (0755 multicall runtime: cz-js / node / zpm / npm)
#   /usr/local/bin/node    (symlink -> cz-js)
#   /usr/local/bin/zpm     (symlink -> cz-js)
#   /usr/local/bin/npm     (symlink -> cz-js)
#   /usr/share/doc/cardputer-zero-js/types/cardputer.d.ts
#
# Usage: bash scripts/build-cz-js-deb.sh [version]
#   CZ_JS_BIN=/path/to/cz-js  skip the cross-build and package a prebuilt binary
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"
fi
VERSION="${VERSION#v}"
VERSION="${VERSION#cz-js-v}"
[ -n "$VERSION" ] || { echo "FAIL: could not determine package version" >&2; exit 1; }

DIST_DIR="$ROOT_DIR/dist"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cz-js-deb.XXXXXX")"
trap 'rm -rf "$STAGE_DIR"' EXIT

TARGET=aarch64-unknown-linux-gnu
CZ_BIN="${CZ_JS_BIN:-$ROOT_DIR/target/$TARGET/release/cz-js}"

if [ ! -f "$CZ_BIN" ]; then
  echo "=== [1/3] Cross-building cz-js for Cortex-A53 (stable Rust, -C target-cpu=cortex-a53) ==="
  RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=cortex-a53"
  export RUSTFLAGS
  CC_aarch64_unknown_linux_gnu="${CC_aarch64_unknown_linux_gnu:-aarch64-linux-gnu-gcc}"
  AR_aarch64_unknown_linux_gnu="${AR_aarch64_unknown_linux_gnu:-aarch64-linux-gnu-ar}"
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}"
  export CC_aarch64_unknown_linux_gnu AR_aarch64_unknown_linux_gnu CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER
  rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release -p cz-runner --target "$TARGET"
else
  echo "=== [1/3] Using prebuilt cz-js binary: $CZ_BIN ==="
fi
[ -f "$CZ_BIN" ] || { echo "FAIL: cz-js binary not found at $CZ_BIN" >&2; exit 1; }

echo "=== [2/3] Cortex-A53 ARMv8.0 instruction audit (LSE <= 200, RCpc/dotprod == 0) ==="
OBJDUMP="$(command -v aarch64-linux-gnu-objdump || command -v objdump || true)"
if [ -n "$OBJDUMP" ]; then
  "$OBJDUMP" -d --no-show-raw-insn "$CZ_BIN" > "$STAGE_DIR/disasm.txt"
  LSE_COUNT="$(grep -cE '\s(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas)(a|l|al)?(b|h)?\s' "$STAGE_DIR/disasm.txt" || true)"
  RCPC_COUNT="$(grep -cE '\s(sdot|udot|ldapr|ldaprb|ldaprh)\s' "$STAGE_DIR/disasm.txt" || true)"
  echo "CZ_JS_LSE=$LSE_COUNT CZ_JS_RCPC=$RCPC_COUNT"
  if [ "$LSE_COUNT" -gt 200 ] || [ "$RCPC_COUNT" -gt 0 ]; then
    echo "FAIL: incompatible Cortex-A53 instructions (LSE=$LSE_COUNT RCPC=$RCPC_COUNT)" >&2
    exit 1
  fi
else
  echo "NOTE: objdump unavailable — skipping static ARMv8.0 audit (build still pins -C target-cpu=cortex-a53)"
fi

echo "=== [3/3] Assembling Debian package root + building .deb ==="
PKG_ROOT="$STAGE_DIR/pkg"
mkdir -p "$PKG_ROOT/DEBIAN" "$PKG_ROOT/usr/local/bin" "$PKG_ROOT/usr/share/doc/cardputer-zero-js/types"

install -m 0755 "$CZ_BIN" "$PKG_ROOT/usr/local/bin/cz-js"
ln -s cz-js "$PKG_ROOT/usr/local/bin/node"
ln -s cz-js "$PKG_ROOT/usr/local/bin/zpm"
ln -s cz-js "$PKG_ROOT/usr/local/bin/npm"
install -m 0644 "$ROOT_DIR/types/cardputer.d.ts" "$PKG_ROOT/usr/share/doc/cardputer-zero-js/types/cardputer.d.ts"

INSTALLED_KB="$(du -sk "$PKG_ROOT/usr" | awk '{print $1}')"
cat > "$PKG_ROOT/DEBIAN/control" <<EOF
Package: cardputer-zero-js
Version: ${VERSION}
Section: devel
Priority: optional
Architecture: arm64
Installed-Size: ${INSTALLED_KB}
Replaces: cardputer-zero-llrt, cardputer-zero-zpm
Conflicts: cardputer-zero-llrt, cardputer-zero-zpm
Maintainer: dj-oyu <dj-oyu@users.noreply.github.com>
Homepage: https://github.com/dj-oyu/cardputer-zero-llrt
Description: CardputerZero standalone AWS-LLRT Free cz-js runtime
 Standalone multicall JavaScript runtime (cz-js / node / zpm / npm) for
 M5Stack CardputerZero (Cortex-A53 ARMv8.0): QuickJS + Rust modules on
 rustls/ring, 100% Tokio-free, Web Standard / WinterTC globals, node:*
 compatibility and cardputer:* hardware bindings.
EOF

mkdir -p "$DIST_DIR"
OUT_DEB="$DIST_DIR/cardputer-zero-js_${VERSION}_arm64.deb"
dpkg-deb --build --root-owner-group "$PKG_ROOT" "$OUT_DEB"
( cd "$DIST_DIR" && sha256sum "$(basename "$OUT_DEB")" > "$(basename "$OUT_DEB").sha256" )

dpkg-deb -I "$OUT_DEB"
dpkg-deb -c "$OUT_DEB"
ls -lh "$OUT_DEB"
echo "CZ_JS_DEB_BUILD_OK: $OUT_DEB"
