# Cardputer Zero LLRT — Phase 15 Technical Specification (AWS-LLRT Free Consolidation, A53 `.deb` CI, README Refresh & Load/Leak Stress Verification)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
All existing Phase 1–14 tests and `phase10-alloc-audit-regression` MUST continue to pass without regression.

---

## 1. Goal

Complete the **AWS-LLRT Free** transition: retire the legacy `awslabs/llrt` patch/packaging scripts and workflows, ship a single clean **stable-Rust Cortex-A53 `.deb` pipeline** for the unified multicall `cz-js` binary (`cz-js` / `node` / `zpm` / `npm`), refresh `README.md`, and pass a real **Load, RSS/FD Leak & Hostile-Traffic Stress Suite**.

---

## 2. Packaging & CI Consolidation (AWS-LLRT Free)

1. **Create `scripts/build-cz-js-deb.sh`** (executable `0755`, LF line endings):
   - Builds `cz-runner` (`cz-js`) with `RUSTFLAGS="-C target-cpu=cortex-a53"` for `aarch64-unknown-linux-gnu` (or accepts prebuilt binary path via `CZ_JS_BIN` for fast packaging).
   - Runs static ARMv8.0 Cortex-A53 instruction audit (`objdump -d` when available: `LSE <= 200`, `RCpc/dotprod == 0`).
   - Packages `cardputer-zero-js_<version>_arm64.deb` containing:
     - `/usr/local/bin/cz-js` (0755)
     - Symlinks: `/usr/local/bin/node -> cz-js`, `/usr/local/bin/zpm -> cz-js`, `/usr/local/bin/npm -> cz-js`
     - `/usr/share/doc/cardputer-zero-js/types/cardputer.d.ts`
2. **Create `.github/workflows/cz-js-deb.yml`**:
   - Stable Rust only (zero `awslabs/llrt` clone, zero nightly Rust, zero yarn, zero zstd).
   - Runs `cargo test --workspace -- --test-threads=1`, `alloc_audit`, cross-builds `aarch64-unknown-linux-gnu` with `-C target-cpu=cortex-a53`, runs `scripts/build-cz-js-deb.sh`, and uploads the `.deb` artifact.
3. **Retire legacy AWS-LLRT packaging files**:
   - Remove `.github/workflows/llrt-deb.yml`, `.github/workflows/zpm-deb.yml`, `scripts/build-llrt-a53.sh`, `scripts/build-llrt-deb.sh`, and `scripts/build-zpm-deb.sh`.
4. **Refresh `README.md`**:
   - Document `cz-js` as a standalone, **AWS-LLRT Free, 100% Tokio-Free** unified multicall JS runtime (`cz-js` / `node` / `zpm` / `npm`) for M5Stack CardputerZero.
   - Document the complete API surface (Web Standard / WinterTC: `fetch`, `WebSocket`, `URL`, `Web Crypto`, `Blob`/`File`/`FormData`, `EventTarget`, `structuredClone`, `performance`, `Buffer`; Node compat: `node:http`, `node:https`, `node:tls`, `node:fs`/`fs/promises`, `node:path`, `node:os`, `node:crypto`, `node:process`; Hardware: `cardputer:*`), `docs/ZERO_ALLOC_GUIDELINES.md`, `#[global_allocator]` audit, and `.deb` packaging.

---

## 3. Load, Leak & Resilience Stress Requirements

The Phase 15 verifier (`/refs/phase15/verify_phase15.mjs`) tests the unified `cz-js` binary under load:
1. **1,000 HTTPS Keep-Alive Requests (8 concurrent clients) + RSS/FD Leak Check**:
   - After 100 warmup requests, 1,000 HTTPS requests MUST all return `200 OK`, open FD delta MUST be `<= 4`, and RSS growth after warmup MUST be `<= 16 MiB`.
2. **Concurrent 16-client `fetch()` + 500-message `WebSocket` Stress**:
   - All 16 concurrent `fetch()` calls and 500 WebSocket echo messages succeed and `cz-js` exits `0` promptly (no leaked slots/threads).
3. **Hostile/Abnormal Traffic Resilience**:
   - 20 abrupt TCP RST disconnects + 20 plaintext garbage connections to `node:https` server MUST NOT crash, stall, or leak FDs; subsequent valid HTTPS requests return `200 OK` and open FD count returns within `<= 4` of baseline.

If any stress test uncovers a real FD leak, memory leak, or concurrency bug in `cz-js`, fix the root cause cleanly!

---

## 4. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase15/verify_phase15.mjs` passes all checks (packaging/CI/README audit + 3 stress/leak tests).
3. `node /refs/phase14/verify_phase14.mjs` passes.
4. `node /refs/phase10/verify_phase10_final.mjs` (`#[global_allocator]` audit) and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` pass.
