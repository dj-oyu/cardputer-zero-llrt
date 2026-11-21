# Cardputer Zero LLRT — Phase 10C Technical Specification (QuickJS FFI Binary Move + Native `TextDecoder` / `Response.json()` + `cz-net` / `cz-runner`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
All existing Phase 1–10B tests MUST continue to pass without regression.

---

## 1. Goal

Eliminate FFI double-copies between Rust `Vec<u8>` and QuickJS `Uint8Array`, replace the char-by-char pure-JS `TextEncoder`/`TextDecoder` with native Rust UTF-8 encode/decode, connect `Response.json()` directly to `cz-json` without intermediate JS string copies, and eliminate per-request TLS `ClientConfig` rebuilds and timer/loop overhead.

---

## 2. Shared FFI Binary Move Helper (`crates/cz-modules/src/lib.rs`)

- Add a shared helper:
  ```rust
  /// Transfer ownership of a Rust `Vec<u8>` into a QuickJS `Uint8Array` backed by
  /// `ArrayBuffer::new(ctx, data)` — zero copy!
  pub(crate) fn vec_to_uint8array<'js>(ctx: &Ctx<'js>, data: Vec<u8>) -> Result<Value<'js>>
  ```
  (Construct `ArrayBuffer::new(ctx.clone(), data)` and wrap it in `Uint8Array` via JS constructor or `TypedArray::from_arraybuffer` — zero memcpy of `data`!)
- Replace `TypedArray::<u8>::new_copy` with `vec_to_uint8array` in:
  - `crates/cz-modules/src/fetch_module.rs` (`EngineEvent::Chunk(bytes)`)
  - `crates/cz-modules/src/tls_module.rs` (`TlsEngineEvent::Data { bytes }`)
  - `crates/cz-modules/src/fs_module.rs` (binary `readFileSync` — AND when `want_text == true`, do NOT allocate `bytes` at all!)
  - `crates/cz-modules/src/audio_module.rs` (`generate_wav_native`)
  - `crates/cz-modules/src/io_module.rs` (`uartRead`)

---

## 3. Native `TextEncoder` / `TextDecoder` & Direct `Response.json()` (`crates/cz-modules/src/fetch_module.rs`)

1. **Native UTF-8 helpers**:
   - Bind `__czUtf8Decode(input)` (accepts `Uint8Array` or `ArrayBuffer`, borrows slice `&[u8]` directly via `as_slice()`/`as_bytes()`, decodes UTF-8 lossily via `String::from_utf8_lossy` into a QuickJS string in one native pass).
   - Bind `__czUtf8Encode(str)` (accepts JS string, encodes UTF-8 bytes and returns `Uint8Array` via `vec_to_uint8array`).
   - Replace the pure-JS `TextEncoder` / `TextDecoder` loops in `FETCH_SHIM` (`parts.push(String.fromCharCode(...))` MUST be completely removed!) with calls to `__czUtf8Encode` / `__czUtf8Decode`.
2. **Direct `Response.json()` -> `cz-json`**:
   - Bind `__czJsonParseBytes(input)` (accepts `Uint8Array` or `ArrayBuffer`, borrows `&[u8]` directly without allocating an intermediate JS `String`, and invokes `cz_modules::json_module`'s `parse_bytes` / `TapeDoc::build` + `make_lazy_handle` for `>= 4096`-byte payloads or `dom_to_js` for small payloads!).
   - Wire `Response.prototype.json()` to call `__czJsonParseBytes(bytes)` directly when consuming buffered bytes, preserving `czJson.isLazy(data) === true` for large JSON responses!
3. **Avoid defensive double-copy in `Response.bytes()` / `Response.arrayBuffer()`**:
   - Track whether a `Response` was cloned (`_shared`); when uncloned and single-use, return the received `Uint8Array` / `ArrayBuffer` directly without allocating a second copy.

---

## 4. `cz-net` & `cz-runner` Optimizations

1. **`crates/cz-net/src/tls.rs`**:
   - Cache the default `Arc<ClientConfig>` (when `ca_pem.is_none() && cert_pem.is_none() && key_pem.is_none() && reject_unauthorized && alpn_protocols.is_empty()`) in a `OnceLock<Result<Arc<ClientConfig>, String>>` so standard HTTPS `fetch()` calls reuse one `Arc<ClientConfig>` instead of rebuilding `ClientConfig` + `WebPkiServerVerifier` on every request. (Keep an uncached helper for unit tests that vary env vars!)
2. **`crates/cz-net/src/fetch.rs`**:
   - If the response has a valid `Content-Length` header (`<= MAX_BUFFERED_BYTES`), pre-size `SlotInner::buffer` with `Vec::with_capacity(cl)` to avoid repeated reallocations while reading buffered bodies.
   - Replace `stream.try_clone()` in `probe_open` with `libc::recv(stream.as_raw_fd(), ..., MSG_PEEK | MSG_DONTWAIT)` (zero fd `dup`).
3. **`crates/cz-runner/src/main.rs`**:
   - In `bind_timers`: avoid `Array.prototype.slice.call(arguments, 2)` when `arguments.length <= 2`, and mutate `t.at` in place on `setInterval` ticks instead of allocating a new timer object every tick.
   - Combine the JS drain calls in `loop_pass` to minimize redundant `ctx.with(...)` + global lookups per loop iteration (while strictly keeping `drain_jobs(rt)` OUTSIDE `ctx.with(...)`!).

---

## 5. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase10/verify_phase10.mjs --phase=10c` passes.
3. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 4` new unit tests covering `vec_to_uint8array` / native UTF-8 / direct JSON byte parse / cached default `ClientConfig`.
4. Phase 8A (`fetch`+TLS) and Phase 8C (`node:tls`) E2E regressions pass.
