# Cardputer Zero LLRT — Phase 15B Technical Specification (Code Review Medium/Low Hardening & Polish)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM, SD card).
**Mandatory reading first**: `/workspace/docs/ZERO_ALLOC_GUIDELINES.md`.
All existing Phase 1–15 tests and `phase10-alloc-audit-regression` MUST continue to pass without regression.

---

## 1. Goal

Resolve the remaining Medium/Low code-review findings across `cz-modules`, `cz-hal`, `cz-runner`, and `cz-zpm` so every Web/Node API edge case, security check, and SD power-loss invariant is rock-solid before real-device deployment.

---

## 2. Required Fixes (9 Concrete Items)

1. **`crates/cz-modules/src/web_crypto.rs`**:
   - Enforce `extractable` on the Rust `RegKey` struct in `exportKey` (mutating `key.extractable = true` in JS MUST NOT allow exporting a non-extractable key!).
   - Enforce `key.usages` in `sign` (`"sign"`), `verify` (`"verify"`), `encrypt` (`"encrypt"`), and `decrypt` (`"decrypt"`), rejecting with `InvalidAccessError` / `Error` when the usage is not permitted.
   - Reject `Float32Array`, `Float64Array`, and `DataView` in `crypto.getRandomValues()` with `TypeMismatchError` / `TypeError` (accept integer typed arrays only per Web Crypto spec).

2. **`crates/cz-modules/src/path_module.rs`**:
   - Fix `path.dirname("/") === "/"` and `path.dirname("///") === "/"` (currently returns `"."`).
   - Fix `path.parse("/").dir === "/"` (currently returns `""`).

3. **`crates/cz-modules/src/buffer_module.rs`**:
   - Support `Buffer.from(new DataView(ab, off, len))`.
   - In `Buffer.from(uint16Array)` (non-`Uint8Array` TypedArray): copy its byte view into a fresh `Buffer` — NEVER mutate `uint16Array`'s prototype to `Buffer.prototype`!
   - Fix `Buffer.byteLength(uint8Array)` / `Buffer.byteLength(arrayBuffer)` to return `.byteLength` (NOT `"[object Uint8Array]".length`).

4. **`crates/cz-modules/src/web_module.rs`**:
   - Sanitize/strip `\r` and `\n` from `v.type` in `FormData._serialize` to prevent multipart header CRLF injection.
   - Make `Blob.prototype.arrayBuffer()` return a fresh copy of the bytes so mutating the returned `ArrayBuffer` cannot corrupt the `Blob`'s internal data.

5. **`crates/cz-modules/src/url_module.rs` & `crates/cz-modules/src/fetch_module.rs`**:
   - `URLSearchParams`: do NOT throw `URIError` on bare `%` (e.g., `new URLSearchParams("?q=100%").get("q") === "100%"`).
   - Fix the regex `/!'()*/g` in `fetch_module.rs` to `/[!'()*]/g`.
   - Ensure `Response.clone()` allows independent consumption (e.g. reading `res.text()` on one clone and `cloned.bytes()` on the other works cleanly).

6. **`crates/cz-modules/src/web_globals.rs`**:
   - In `EventTarget.prototype.dispatchEvent`, when a listener is an object with `{ handleEvent(ev) }`, invoke `listener.handleEvent(ev)` with `this === listener` (NOT `this === eventTarget`).

7. **`crates/cz-hal/src/http.rs`**:
   - In `ChunkPhase::DataEol`: reject trailing non-empty garbage (`line_len != 0`) with `HttpError::BadRequest`.
   - In `write_filtered_headers`: allow multiple `Set-Cookie` headers (do NOT collapse/drop duplicate `Set-Cookie` headers!).

8. **`crates/cz-runner/src/main.rs`**:
   - Install `SIGINT`/`SIGTERM` handlers unconditionally before entering `run_event_loop` (so servers started inside top-level `await`, `setTimeout`, or `node:tls`/`WebSocket` daemons also handle signals cleanly).
   - Remove the hardcoded 3600s kill in `run_event_loop` (only enforce a max loop timeout if `CZ_MAX_LOOP_SECS` env var is explicitly set).

9. **`crates/cz-zpm/src/lib.rs`**:
   - Fix `^0.x.y` semver matching in `satisfies`: `^0.2.3` MUST match `0.2.9` and MUST NOT match `0.3.0` (standard npm caret rules: `^0.y.z` locks minor `y` when `y > 0`, `^0.0.z` locks patch `z`).
   - Support multi-hash SRI strings in `verify_integrity` (space-separated tokens like `"sha256-... sha512-..."` pass if any valid `sha512-...` matches).
   - Write `package.json` and `zpm-lock.json` atomically (`tmp` file in same dir → `sync_all()` → `rename`) for SD card power-loss safety.

---

## 3. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase15b/verify_phase15b.mjs` passes all 9 checks.
3. `node /refs/phase10/verify_phase10_final.mjs` (`#[global_allocator]` audit) passes.
4. `node /refs/phase15/verify_phase15.mjs` and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` pass.
