# Cardputer Zero LLRT — Phase 10D Technical Specification (HTTP Pipeline Overhaul: Span-Backed `RequestHead`, Zero-Body-Copy TX, Lazy `bodyText`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
All existing Phase 1–10C tests MUST continue to pass without regression.

---

## 1. Goal

Eliminate per-request small-`String` fragmentation in HTTP header parsing, eliminate `VecDeque::make_contiguous()` memmoves, eliminate double-copying response bodies on transmit, and eliminate `HashMap<String, Vec<String>>` + eager `bodyText` allocation in `http_module`. Also clean up `cz-zpm`'s redundant tarball buffer copy & double SHA-512 hash.

---

## 2. `crates/cz-hal/src/http.rs`

### 2.1 Single-Buffer Span-Backed `RequestHead`
- Replace the 20–40 independent heap `String` allocations per request in `RequestHead` with a single owned buffer + byte offset spans:
  - Store `raw: Box<[u8]>` (or `Vec<u8>`) holding the normalized head bytes (header field names lowercased in-place, obs-fold continuations normalized into `raw`), plus:
    - `method_span: (usize, usize)` (or `Range<usize>`)
    - `target_span: (usize, usize)`
    - `version_minor: u8` (`0` for `"1.0"`, `1` for `"1.1"`, etc.)
    - `header_spans: Vec<((usize, usize), (usize, usize))>` (name span + value span into `raw`)
  - Expose clean zero-alloc borrowed accessors:
    - `pub fn method(&self) -> &str`
    - `pub fn target(&self) -> &str`
    - `pub fn version(&self) -> &str` (static `"1.0"` / `"1.1"` / etc.)
    - `pub fn headers_iter(&self) -> impl Iterator<Item = (&str, &str)>`
    - `pub fn header_count(&self) -> usize`
    - Keep `header_values(&self, name: &str)`, `header(&self, name: &str) -> Option<Cow<'_, str>>`, `keep_alive()`, `expects_continue()`, `chunked()`, `content_length()` — ALL zero-allocation for 0/1 matches!
  - Note: Update internal unit tests in `http.rs` / `http_server.rs` to use the new borrowed accessors (`head.method()`, `head.target()`, `head.version()`, `head.headers_iter()`).

### 2.2 Linear-Cursor `RequestParser`
- Replace `buf: VecDeque<u8>` in `RequestParser` with a flat linear buffer `buf: Vec<u8>` + `read_pos: usize` (compacting `buf.drain(..read_pos)` / `copy_within` only when `read_pos > 0` and either `read_pos == buf.len()` or approaching capacity). This completely eliminates `VecDeque::make_contiguous()` wrap-around memmoves!
- Keep all existing framing/security invariants intact (64 KiB head cap, 1 MiB body cap, `Expect: 100-continue`, strict chunked decoding, obs-fold, sticky errors).

### 2.3 Zero-Body-Copy Response Serialization
- Add `pub fn serialize_response_head_into(out: &mut Vec<u8>, status: u16, reason: Option<&str>, headers: &[(String, String)], body_len: usize, keep_alive: bool)` that writes **only the status line + headers** into a caller-owned reusable `out` buffer (without copying `body`!).
- Keep `serialize_response(...)` for existing unit tests, delegating to `serialize_response_head_into` + `extend_from_slice(body)`.

---

## 3. `crates/cz-hal/src/http_server.rs`

- Share `remote_ip: Arc<str>` and `local_ip: Arc<str>` per connection instead of cloning `String`s on every Keep-Alive request.
- Hold a reusable `tx_head_buf: Vec<u8>` in `spawn_connection` and send full responses by writing the formatted head (`&tx_head_buf`) + body (`&frame.body`) without allocating a combined `head + body` `Vec<u8>` (use `write_vectored` / `IoSlice` when supported, or two sequential writes on TLS streams — zero body copy either way!).
- Keep `crates/cz-hal/Cargo.toml` strictly `libc`-only!

---

## 4. `crates/cz-modules/src/http_module.rs`

- **Remove `HashMap<String, Vec<String>>` per request** in `build_descriptor`: build the JS `headers` object directly in a single pass over `ev.request.head.headers_iter()` (if a key already exists on the JS `headers` object, append `", " + value`, else set `value` directly — zero Rust `HashMap` or `Vec<String>` allocations!).
- **Move `ev.request.body` via `vec_to_uint8array(ctx, ev.request.body)`** (zero copy into QuickJS `ArrayBuffer`).
- **Lazy `bodyText`**: do NOT eagerly decode `ev.request.body` to a JS `bodyText` string in `build_descriptor`! Instead, decode UTF-8 on demand in `HTTP_SHIM` via native `__czUtf8Decode(body)` only when `req.setEncoding("utf8")` is used or text is requested.

---

## 5. Minor Cleanup in `crates/cz-zpm/src/lib.rs`

- Replace `Cursor::new(bytes.to_vec())` in `extract_tarball` with `Cursor::new(bytes)` (eliminates copying up-to-5 MiB `.tgz` buffer!).
- Avoid computing SHA-512 twice in `fetch_dep_tree` when `sri` already matched.

---

## 6. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase10/verify_phase10.mjs --phase=10d` passes.
3. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 4` new unit tests verifying span-backed `RequestHead`, linear cursor compaction across pipelined Keep-Alive requests, and `serialize_response_head_into`.
4. Phase 2 HTTP E2E (`bash /refs/verify/check_e2e_p2.sh`) and Phase 8B HTTPS E2E (`node /refs/phase8b/verify_phase8b.mjs`) regressions pass.
