# Cardputer Zero LLRT — Zero-Allocation Engineering Guidelines

Target device: **M5Stack CardputerZero** (Cortex-A53 ARMv8.0 1 GHz, 512 MB RAM / ~80 MB free, single-threaded QuickJS + worker-thread I/O).

Every new crate, module, and hot-path feature MUST follow these rules and prove compliance via `crates/cz-runner/tests/alloc_audit.rs` (`#[global_allocator]`).

---

## 1. Own Once, Borrow Everywhere
- **Single-buffer + spans**: When parsing structured text/wire data (HTTP heads, URLs, frames), store ONE owned buffer (`Box<[u8]>` / `Vec<u8>`) and index fields via `(usize, usize)` byte spans instead of allocating 10–40 small `String`s per object.
- **Borrowed lookups**: Lookup helpers MUST use `eq_ignore_ascii_case` / `&str` / `Cow<'_, str>` / iterators (`header_values`) — NEVER `name.to_ascii_lowercase()` or temporary `Vec<&str>` on 0/1-match paths.
- **`Copy` state machines**: Keep parser/protocol state enums tiny and `#[derive(Copy, Clone)]` so `match self.state` never clones.

## 2. Zero-Copy Rust ↔ QuickJS FFI
- **Rust -> JS (`Vec<u8>`)**: ALWAYS transfer owned `Vec<u8>` via `crate::vec_to_uint8array(ctx, data)` (`ArrayBuffer::new` ownership move). NEVER call `TypedArray::<u8>::new_copy` on a `Vec<u8>` you already own.
- **JS -> Rust (`&[u8]`)**: For synchronous reads (e.g., `blit`, UTF-8 decode, JSON parse, hashing, frame encoding), borrow `ta.as_slice()` / `ab.as_bytes()` directly — NEVER `.to_vec()` just to pass `&[u8]`.
- **No pure-JS byte/char loops**: Use native `__czUtf8Decode` / `__czUtf8Encode` (never `String.fromCharCode` loops) and native `ring` crypto (never pure-JS SHA/HMAC loops).
- **Direct byte-to-`cz-json`**: Parse UTF-8 response/frame bytes straight into `cz-json` (`__czJsonParseBytes`) without creating intermediate JS `String` copies.
- **Singleton Proxy handlers**: Hoist JS Proxy handlers into a single module-level constant (`ROW_HANDLER`, `LAZY_HANDLER`) — NEVER allocate fresh handler closures per handle.

## 3. Under-Lock Visitor Pattern & Stack Buffers
- **Never clone out of a `Mutex` just to read or serialize**: Expose scoped closure APIs (`with_value`, `with_field`, `visit_record`) that lend `&T` under the lock (always drop the `MutexGuard` before recursing into other locked methods).
- **Fixed stack buffers for small headers/keys**: Use stack arrays (e.g., `[&str; 32]` for sorting small record keys, `[u8; 14]` for RFC 6455 WebSocket frame headers) instead of heap `Vec`s.
- **Mutate existing map entries in place**: Check `map.get_mut(key)` BEFORE allocating `key.to_string()` / `Box<str>` on updates.

## 4. Worker Threads & I/O Buffers
- **Reuse per-connection scratch buffers**: Keep a single linear `Vec<u8>` read cursor (no `VecDeque::make_contiguous` memmoves) and a reusable `tx_head_buf: Vec<u8>` per connection.
- **Zero-body-copy TX**: Use `IoSlice` / `write_vectored` (or sequential header + body writes on TLS) instead of allocating a combined `header + body` `Vec<u8>`.
- **Move across channels**: Use `std::mem::take` / `Option::take` when moving payloads across threads; share connection-immutable strings via `Arc<str>`.
- **Cache cold immutable configs**: Cache default `Arc<rustls::ClientConfig>` in `OnceLock` and font `ScaleContext` + bounded glyph cache in `FontFace`.

## 5. Mandatory Verification (`alloc_audit.rs`)
- Any new hot-path module (HTTP, WebSocket, URL, JSON, KV, GFX) MUST add a deterministic `#[global_allocator]` test in `crates/cz-runner/tests/alloc_audit.rs` asserting `allocs == 0` (or a strict small upper bound) and printing `ALLOC_AUDIT_PASS <name> allocs=<N> bytes=<B>`.
- NEVER weaken an allocation assertion; if a hidden allocation appears, fix the hot path.
