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
        builder = builder
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:display", module: DisplayModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:input", module: InputModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:app", module: AppModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:power", module: PowerModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:kv", module: KvModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:json", module: JsonModule })
            // Phase 6A peripheral hardware modules (IIO IMU, PCM audio, NEC/LIRC IR, port + UART).
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:imu", module: ImuModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:audio", module: AudioModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:ir", module: IrModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "cardputer:io", module: IoModule })
            .with_module(llrt_utils::module::ModuleInfo { name: "http", module: HttpModule });
    }
CZBLOCK
    )
    # Insert right before the trailing `builder` return inside Default::default() / build(),
    # falling back to after `fn build` or EOF for minimal mock layouts.
    local anchor
    anchor="$(grep -n '^[[:space:]]*builder[[:space:]]*$' "$mb" | tail -1 | cut -d: -f1 || true)"
    if [ -n "$anchor" ]; then
        awk -v ln="$anchor" -v add="$block" 'NR == ln { print add } { print }' \
            "$mb" > "$mb.cz-insert.tmp" && mv "$mb.cz-insert.tmp" "$mb"
    else
        anchor="$(grep -n 'fn build' "$mb" | head -1 | cut -d: -f1 || true)"
        if [ -z "$anchor" ]; then
            printf '%s\n' "$block" >> "$mb"
        else
            awk -v ln="$anchor" -v add="$block" 'NR == ln { print; print add; next } { print }' \
                "$mb" > "$mb.cz-insert.tmp" && mv "$mb.cz-insert.tmp" "$mb"
        fi
    fi
}

# ---------------------------------------------------------------------------
# 5) llrt_core (when present) — install cz-json AFTER LLRT's json::redefine_static_methods
# ---------------------------------------------------------------------------
patch_llrt_core() {
    local cc="$TARGET_DIR/llrt_core/Cargo.toml"
    local vm="$TARGET_DIR/llrt_core/src/vm.rs"
    [ -f "$cc" ] && [ -f "$vm" ] || return 0
    if ! grep -q '^cz-modules = ' "$cc"; then
        insert_after "$cc" '^\[dependencies\]' "cz-modules = { path = \"$CZ_MODULES_DIR\" }"
    fi
    if ! grep -q 'cz_modules::install_json(ctx)?;' "$vm"; then
        if grep -q 'json::redefine_static_methods(ctx)?;' "$vm"; then
            insert_after "$vm" 'json::redefine_static_methods\(ctx\)\?;' '    cz_modules::install_json(ctx)?;'
        fi
    fi
}

# ---------------------------------------------------------------------------
# 6) llrt binary (when present) — wire cz-zpm multicall (`zpm` / `npm` / `cz-llrt zpm`)
# ---------------------------------------------------------------------------
patch_llrt_multicall() {
    local lc="$TARGET_DIR/llrt/Cargo.toml"
    local lm="$TARGET_DIR/llrt/src/main.rs"
    local zpm_dir="${CZ_ZPM_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/crates/cz-zpm}"
    [ -f "$lc" ] && [ -f "$lm" ] && [ -d "$zpm_dir" ] || return 0
    if ! grep -q '^cz-zpm = ' "$lc"; then
        insert_after "$lc" '^\[dependencies\]' "cz-zpm = { path = \"$zpm_dir\" }"
    fi
    if ! grep -q 'cz_zpm::maybe_run_multicall_async' "$lm"; then
        if grep -q 'async fn main()' "$lm"; then
            insert_after "$lm" 'async fn main\(\)' '    if let Some(code) = cz_zpm::maybe_run_multicall_async().await { return Ok(ExitCode::from(code)); }'
        fi
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
        spec_n="$(grep -cF "name: \"$spec\"" "$mb" || true)"
        [ "$spec_n" -eq 1 ] || { echo "FAIL: $spec registered $spec_n times (want exactly 1)" >&2; exit 1; }
    done
    grep -qF '// cardputer-zero: module registrations (cz-modules)' "$mb" || {
        echo "FAIL: module_builder.rs registration marker missing" >&2; exit 1; }
    if [ -f "$TARGET_DIR/llrt_core/src/vm.rs" ]; then
        grep -qF 'cz_modules::install_json(ctx)?;' "$TARGET_DIR/llrt_core/src/vm.rs" || {
            echo "FAIL: llrt_core/src/vm.rs missing cz_modules::install_json(ctx)?;" >&2; exit 1; }
    fi
    echo "[build-llrt-a53] patches verified OK in $TARGET_DIR (LSE removed, cortex-a53 active, cz-modules wired, all 10 Cardputer modules + http alias registered)"
}

apply_patches() {
    patch_cargo_config
    patch_makefile
    patch_llrt_modules_cargo
    patch_module_builder
    patch_llrt_core
    patch_llrt_multicall
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
RUSTFLAGS="-Ctarget-cpu=cortex-a53 -Ctarget-feature=-lse,-aes,-sha2,+crt-static" \
    cargo +nightly build --release --target aarch64-unknown-linux-musl --features cardputer

BIN="$TARGET_DIR/target/aarch64-unknown-linux-musl/release/llrt"
[ -f "$BIN" ] || { echo "FAIL: release binary not produced at $BIN" >&2; exit 1; }

OBJDUMP="$(command -v aarch64-linux-gnu-objdump || command -v objdump)"
"$OBJDUMP" -d --no-show-raw-insn "$BIN" > /tmp/cz-llrt-disasm.txt
LSE_COUNT="$(grep -cE '\s(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp|cas)(a|l|al)?(b|h)?\s' /tmp/cz-llrt-disasm.txt || true)"
RCPC_COUNT="$(grep -cE '\s(sdot|udot|ldapr|ldaprb|ldaprh)\s' /tmp/cz-llrt-disasm.txt || true)"
rm -f /tmp/cz-llrt-disasm.txt
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
