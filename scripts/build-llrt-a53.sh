#!/usr/bin/env bash
#
# build-llrt-a53.sh — Reproducible awslabs/llrt v0.10.0-beta fork integration for
# M5Stack CardputerZero (Cortex-A53, NO LSE atomics) targeting aarch64 Linux musl.
#
# Modes:
#   --apply-only <llrt-dir>   Apply + verify the Cortex-A53 / cz-modules patches only
#                             (safe inside pi-sandbox: no nightly Rust or zig needed).
#   <llrt-dir>                FULL BUILD mode (run inside cz-dev:latest on a host):
#                             applies the patches, builds llrt-linux-arm64-no-sdk.zip with
#                             CFLAGS_aarch64_unknown_linux_musl="-mcpu=cortex_a53", verifies
#                             objdump shows LSE <= 200 and RCpc == 0, then emits the binary +
#                             its .sha256.
#
set -euo pipefail

usage() {
    echo "usage: $0 --apply-only <llrt-dir> | $0 <llrt-dir>" >&2
    exit 2
}

MODE="build"
TARGET_DIR=""
if [ "${1:-}" = "--apply-only" ]; then
    MODE="apply"
    shift
    TARGET_DIR="${1:-}"
elif [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
else
    TARGET_DIR="${1:-}"
fi
[ -n "$TARGET_DIR" ] || usage
[ -d "$TARGET_DIR" ] || { echo "FAIL: llrt dir '$TARGET_DIR' does not exist" >&2; exit 1; }
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"

# Absolute path of this workspace's cz-modules crate (override with CZ_MODULES_DIR).
CZ_MODULES_DIR="${CZ_MODULES_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/crates/cz-modules}"
[ -d "$CZ_MODULES_DIR" ] || { echo "FAIL: cz-modules crate not found at $CZ_MODULES_DIR" >&2; exit 1; }

insert_after() {
    # insert_after <file> <header-regex> <line> — inserts ONE line after the first matching
    # header line. Callers guard with greps, so re-runs never duplicate anything.
    local file="$1" regex="$2" line="$3"
    awk -v regex="$regex" -v add="$line" 'BEGIN{done=0} { print } !done && $0 ~ regex { print add; done = 1 }' \
        "$file" > "$file.cz-insert.tmp" && mv "$file.cz-insert.tmp" "$file"
}

# ---------------------------------------------------------------------------
# 1) .cargo/config.toml — Cortex-A53 codegen, NO +lse
# ---------------------------------------------------------------------------
patch_cargo_config() {
    mkdir -p "$TARGET_DIR/.cargo"
    local cfg="$TARGET_DIR/.cargo/config.toml"
    [ -f "$cfg" ] || printf '[target.aarch64-unknown-linux-musl]\nrustflags = ["-Ctarget-cpu=cortex-a53", "-Ctarget-feature=+crt-static"]\n' > "$cfg"
    # Drop every +lse token (LL/SC-only baseline: Cortex-A53 has no LSE instructions).
    sed -i -e 's/+lse,[[:space:]]*//g' -e 's/[[:space:]]*,[[:space:]]*+lse//g' -e 's/=+lse"/="/g' "$cfg"
    # Normalize any existing -Ctarget-cpu=... to cortex-a53 (idempotent re-runs rewrite to the
    # exact same text and never append a second block because of the grep guard below).
    sed -i -E 's/-Ctarget-cpu=[A-Za-z0-9._+-]+/-Ctarget-cpu=cortex-a53/g' "$cfg"
    if ! grep -q -- '-Ctarget-cpu=cortex-a53' "$cfg"; then
        printf '\n[target.aarch64-unknown-linux-musl]\nrustflags = ["-Ctarget-cpu=cortex-a53", "-Ctarget-feature=+crt-static"]\n' >> "$cfg"
    fi
}

# ---------------------------------------------------------------------------
# 2) Makefile — zig cc must pass -mcpu=cortex_a53 exactly once
# ---------------------------------------------------------------------------
patch_makefile() {
    local mk="$TARGET_DIR/Makefile"
    [ -f "$mk" ] || { echo "FAIL: Makefile missing in $TARGET_DIR" >&2; exit 1; }
    # Normalize any pre-existing -mcpu=... tokens (native/neoverse/...) onto cortex_a53.
    if grep -q -- '-mcpu=' "$mk"; then
        sed -i -E 's/-mcpu=[^[:space:]]+/-mcpu=cortex_a53/g' "$mk"
    fi
    # Ensure exactly one occurrence: only patch the FIRST zig cc recipe line when none exists,
    # so a second run sees the flag already present and changes nothing.
    if [ "$(grep -c -- '-mcpu=cortex_a53' "$mk" || true)" -eq 0 ]; then
        if grep -q 'zig cc' "$mk"; then
            sed -i '0,/zig cc/s/\(zig cc\)/\1 -mcpu=cortex_a53/' "$mk"
        else
            printf 'ZIG_EXTRA_CFLAGS = -mcpu=cortex_a53\n' >> "$mk"
        fi
    fi
}

# ---------------------------------------------------------------------------
# 3) llrt_modules/Cargo.toml — cz-modules path dep behind the default `cardputer` feature
# ---------------------------------------------------------------------------
patch_llrt_modules_cargo() {
    local tom="$TARGET_DIR/llrt_modules/Cargo.toml"
    [ -f "$tom" ] || { echo "FAIL: llrt_modules/Cargo.toml missing" >&2; exit 1; }
    if ! grep -q '^cz-modules = ' "$tom"; then
        if grep -q '^\[dependencies\]' "$tom"; then
            insert_after "$tom" '^\[dependencies\]' "cz-modules = { path = \"$CZ_MODULES_DIR\", optional = true }"
        else
            printf '\n[dependencies]\ncz-modules = { path = "%s", optional = true }\n' "$CZ_MODULES_DIR" >> "$tom"
        fi
    fi
    if ! grep -q '^cardputer = ' "$tom"; then
        if grep -q '^\[features\]' "$tom"; then
            insert_after "$tom" '^\[features\]' 'cardputer = ["dep:cz-modules"]'
        else
            printf '\n[features]\ndefault = ["cardputer"]\ncardputer = ["dep:cz-modules"]\n' >> "$tom"
        fi
    fi
    # Enable `cardputer` by default (the LLRT fork always ships the Cardputer runtime).
    if grep -qE '^default[[:space:]]*=' "$tom"; then
        if ! grep -qE '^default.*"cardputer"' "$tom"; then
            sed -i -E 's/^default([[:space:]]*)=([[:space:]]*)\[([^]]*)\]/default\1=\2[\3, "cardputer"]/' "$tom"
        fi
    elif grep -q '^\[features\]' "$tom"; then
        insert_after "$tom" '^\[features\]' 'default = ["cardputer"]'
    fi
}

# ---------------------------------------------------------------------------
# 4) llrt_modules/src/module_builder.rs — register every Cardputer module via ModuleInfo
#    (display, input, app, power, kv, json plus the Phase 6A peripherals imu/audio/ir/io).
# ---------------------------------------------------------------------------
patch_module_builder() {
    local mb="$TARGET_DIR/llrt_modules/src/module_builder.rs"
    [ -f "$mb" ] || { echo "FAIL: llrt_modules/src/module_builder.rs missing" >&2; exit 1; }
    local marker='// cardputer-zero: module registrations (cz-modules)'
    if grep -qF "$marker" "$mb"; then
        return 0
    fi
    local block
    block=$(cat <<'CZBLOCK'
    // cardputer-zero: module registrations (cz-modules) — gated by the default `cardputer` feature.
    #[cfg(feature = "cardputer")]
    {
        use cz_modules::{
            AppModule, AudioModule, DisplayModule, HttpModule, ImuModule, InputModule, IoModule,
            IrModule, JsonModule, KvModule, PowerModule,
        };
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:display", DisplayModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:input", InputModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:app", AppModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:power", PowerModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:kv", KvModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:json", JsonModule));
        // Phase 6A peripheral hardware modules (IIO IMU, PCM audio, NEC/LIRC IR, port + UART).
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:imu", ImuModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:audio", AudioModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:ir", IrModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("cardputer:io", IoModule));
        builder = builder.with_module(llrt_utils::module::ModuleInfo::new("http", HttpModule));
    }
CZBLOCK
    )
    # Insert inside build(): after the first line mentioning fn build...{ ; fall back to a
    # top-level block placement for unusual fork layouts.
    local anchor
    anchor="$(grep -n 'fn build' "$mb" | head -1 | cut -d: -f1 || true)"
    if [ -z "$anchor" ]; then
        printf '%s\n' "$block" >> "$mb"
    else
        awk -v ln="$anchor" -v add="$block" 'NR == ln { print; print add; next } { print }' \
            "$mb" > "$mb.cz-insert.tmp" && mv "$mb.cz-insert.tmp" "$mb"
    fi
}

# ---------------------------------------------------------------------------
# Verification of every patch marker (also proves idempotency when run repeatedly)
# ---------------------------------------------------------------------------
verify_apply() {
    local cfg="$TARGET_DIR/.cargo/config.toml" mk="$TARGET_DIR/Makefile"
    local tom="$TARGET_DIR/llrt_modules/Cargo.toml" mb="$TARGET_DIR/llrt_modules/src/module_builder.rs"
    grep -q -- 'cortex-a53' "$cfg" || { echo "FAIL: config.toml missing cortex-a53" >&2; exit 1; }
    if grep -q '+lse' "$cfg"; then
        echo "FAIL: config.toml still contains +lse" >&2
        exit 1
    fi
    grep -q -- '-mcpu=cortex_a53' "$mk" || { echo "FAIL: Makefile missing -mcpu=cortex_a53" >&2; exit 1; }
    local mcpu_n
    mcpu_n="$(grep -c -- '-mcpu=cortex_a53' "$mk" || true)"
    [ "$mcpu_n" -eq 1 ] || { echo "FAIL: Makefile has $mcpu_n -mcpu=cortex_a53 occurrences (want 1)" >&2; exit 1; }
    grep -q 'cz-modules' "$tom" || { echo "FAIL: llrt_modules/Cargo.toml missing cz-modules dep" >&2; exit 1; }
    local dep_n
    dep_n="$(grep -cE '^cz-modules[[:space:]]*=' "$tom" || true)"
    [ "$dep_n" -eq 1 ] || { echo "FAIL: cz-modules dependency appears $dep_n times (want exactly 1)" >&2; exit 1; }
    grep -q '^cardputer = \["dep:cz-modules"\]' "$tom" || { echo "FAIL: cardputer feature definition missing" >&2; exit 1; }
    grep -qE '^default.*"cardputer"' "$tom" || { echo "FAIL: cardputer not enabled in default features" >&2; exit 1; }
    for sym in DisplayModule InputModule AppModule PowerModule KvModule JsonModule HttpModule \
              ImuModule AudioModule IrModule IoModule; do
        grep -q "$sym" "$mb" || { echo "FAIL: module_builder.rs missing $sym" >&2; exit 1; }
    done
    # Each specifier must appear exactly once (re-runs never duplicate registrations).
    for spec in cardputer:display cardputer:input cardputer:app cardputer:power cardputer:kv \
                cardputer:json cardputer:imu cardputer:audio cardputer:ir cardputer:io; do
        local spec_n
        spec_n="$(grep -cF "ModuleInfo::new(\"$spec\"" "$mb" || true)"
        [ "$spec_n" -eq 1 ] || { echo "FAIL: $spec registered $spec_n times (want exactly 1)" >&2; exit 1; }
    done
    grep -qF '// cardputer-zero: module registrations (cz-modules)' "$mb" || {
        echo "FAIL: module_builder.rs registration marker missing" >&2; exit 1; }
    echo "[build-llrt-a53] patches verified OK in $TARGET_DIR (LSE removed, cortex-a53 active, cz-modules wired, all 10 Cardputer modules + http alias registered)"
}

apply_patches() {
    patch_cargo_config
    patch_makefile
    patch_llrt_modules_cargo
    patch_module_builder
    verify_apply
}

if [ "$MODE" = "apply" ]; then
    apply_patches
    exit 0
fi

# ---------------------------------------------------------------------------
# FULL BUILD mode (host container cz-dev:latest)
# ---------------------------------------------------------------------------
command -v zig >/dev/null 2>&1 || { echo "FAIL: zig not found (run this mode inside cz-dev:latest)" >&2; exit 1; }
command -v objdump >/dev/null 2>&1 || { echo "FAIL: objdump not found" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "FAIL: cargo not found" >&2; exit 1; }
rustup toolchain list 2>/dev/null | grep -q nightly || { echo "FAIL: a nightly Rust toolchain is required (LLRT v0.10)" >&2; exit 1; }

apply_patches
cd "$TARGET_DIR"

export CFLAGS_aarch64_unknown_linux_musl="-mcpu=cortex_a53"
export CC_aarch64_unknown_linux_musl="zig cc"
export AR_aarch64_unknown_linux_musl="zig ar"

# Build the vendored C libraries first when the fork exposes that target, then the release
# binary through the cardputer-feature-wired cargo build.
if grep -q '^libs-arm64:' Makefile; then
    make libs-arm64
fi
RUSTFLAGS="-Ctarget-cpu=cortex-a53 -Ctarget-feature=-lse,+crt-static" \
    cargo +nightly build --release --target aarch64-unknown-linux-musl --features cardputer

BIN="$TARGET_DIR/target/aarch64-unknown-linux-musl/release/llrt"
[ -f "$BIN" ] || { echo "FAIL: release binary not produced at $BIN" >&2; exit 1; }

LSE_COUNT="$(objdump -d "$BIN" | awk '{ if ($2 ~ /^(ldadd|ldclr|ldeor|ldcin|ldswp|casa|swpa|swpe)/) c++ } END { print c+0 }')"
RCPC_COUNT="$(objdump -d "$BIN" | awk '{ if ($2 ~ /^(ldaexr|ldaexp|caspa|swprh|swplh|dmb)/) c++ } END { print c+0 }')"
echo "[build-llrt-a53] objdump LSE=$LSE_COUNT RCpc=$RCPC_COUNT (limits: LSE <= 200, RCpc == 0)"
[ "$LSE_COUNT" -le 200 ] || { echo "FAIL: too many LSE instructions ($LSE_COUNT > 200)" >&2; exit 1; }
[ "$RCPC_COUNT" -eq 0 ] || { echo "FAIL: RCpc atomics present (RCpc=$RCPC_COUNT, want 0)" >&2; exit 1; }

OUT_ZIP="$TARGET_DIR/dist/llrt-linux-arm64-no-sdk.zip"
mkdir -p "$TARGET_DIR/dist"
rm -f "$OUT_ZIP"
( cd "$(dirname "$BIN")" && zip -j "$OUT_ZIP" "$(basename "$BIN")" )
sha256sum "$BIN" > "$OUT_ZIP.sha256"
echo "[build-llrt-a53] built: $OUT_ZIP"
echo "[build-llrt-a53] sha256: $(awk '{print $1}' "$OUT_ZIP.sha256")"
