# Cardputer Zero LLRT — Phase 10 Final Technical Specification (`#[global_allocator]` Allocation Audit & Full System Verification)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
Execution environment: `pi-sandbox` with `-WithAwsEmulator`.

---

## 1. Goal

Verify with **real `#[global_allocator]` heap allocation counters** that every Phase 10A–10D zero-copy / zero-clone architectural optimization actually achieves its target allocation bounds, and run the complete Phase 1–10D + `aws-emulator` regression suite.

If any allocation assertion reveals a residual hidden allocation in any hot path, fix that hot path cleanly so the audit passes honestly (never weaken the assertions!).

---

## 2. `crates/cz-runner/tests/alloc_audit.rs`

Add `[dev-dependencies]` (`cz-hal`, `cz-gfx`, `cz-json`) to `crates/cz-runner/Cargo.toml` and create `crates/cz-runner/tests/alloc_audit.rs` with a process-wide counting `#[global_allocator]` wrapping `std::alloc::System`:

```rust
struct CountingAlloc;
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
```
- Count both `alloc` and `realloc` (incrementing `ALLOC_COUNT` by 1 and `ALLOC_BYTES` by requested size).
- Provide `fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, usize)` (resets counters, runs `f`, returns `(val, alloc_count, alloc_bytes)`). Run with `--test-threads=1`.

### Required Allocation Audit Tests (all MUST print measured counts and assert strict bounds!)

1. **`alloc_audit_lazy_json_clean_reads_zero`**
   - Build a `cz_json::LazyDoc` from an `>= 8 KiB` JSON document.
   - Perform **1,000** clean reads (`get` / `has` on nested object/array nodes).
   - **Assert**: `allocs == 0`!
   - Print: `ALLOC_AUDIT_PASS lazy_json_clean_1000 allocs=0 bytes=0`

2. **`alloc_audit_kv_existing_updates_and_visitor_zero`**
   - Open an in-memory `KvStore`, seed `"counter"` (scalar) and `"rec"` (record with fields `"n"`, `"hits"`).
   - Measure **1,000** iterations of: `set("counter", ...)`, `set_field("rec", "n", ...)`, `incr("rec", "hits", 1)`, `with_value("counter", ...)`, `with_field("rec", "n", ...)`, and `visit_record("rec", ...)`.
   - **Assert**: `allocs == 0`!
   - Print: `ALLOC_AUDIT_PASS kv_existing_update_visitor_1000 allocs=0 bytes=0`

3. **`alloc_audit_canvas_as_le_bytes_zero`**
   - Create a `cz_gfx::Canvas`, draw primitives, then measure **1,000** calls to `canvas.as_le_bytes()` (summing a byte to prevent dead-code elimination).
   - **Assert**: `allocs == 0 && bytes == 0`!
   - Print: `ALLOC_AUDIT_PASS canvas_as_le_bytes_1000 allocs=0 bytes=0`

4. **`alloc_audit_font_draw_text_cached_zero`**
   - Load a `cz_gfx::FontFace` (e.g., from the minimal valid TTF fixture or system DejaVu font if available) and warm up the glyph cache with 1 call to `draw_text("Hello123", ...)`.
   - Measure **200** repeated `draw_text("Hello123", ...)` calls hitting the warm glyph cache.
   - **Assert**: `allocs == 0`!
   - Print: `ALLOC_AUDIT_PASS font_draw_text_cached_200 allocs=0 bytes=0`

5. **`alloc_audit_http_head_lookups_zero`**
   - Parse a 12-header HTTP/1.1 request with `RequestParser`.
   - Measure **1,000** iterations of `head.method()`, `head.target()`, `head.version()`, `head.header("host")`, `head.keep_alive()`, `head.chunked()`, `head.content_length()`, `head.expects_continue()`.
   - **Assert**: `allocs == 0`!
   - Print: `ALLOC_AUDIT_PASS http_head_lookups_1000 allocs=0 bytes=0`

6. **`alloc_audit_http_parse_and_tx_head_bounds`**
   - Warm up a `RequestParser` on one request so its linear `buf` has capacity.
   - Feed a 12-header HTTP/1.1 Keep-Alive request and measure `parser.step()`.
   - **Assert**: `allocs <= 3` (proves span-backed `RequestHead` replaced ~30–40 small `String` allocations with ≤ 3 allocations!).
   - Also pre-allocate `let mut tx_head = Vec::with_capacity(512);`, warm once, and measure **1,000** calls to `serialize_response_head_into(&mut tx_head, 200, None, &headers, 128, true)` (`tx_head.clear()` each iteration).
   - **Assert**: `tx_allocs == 0`!
   - Print: `ALLOC_AUDIT_PASS http_parse_and_tx parse_allocs=<N> tx_head_allocs=0`

---

## 3. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase10/verify_phase10_final.mjs` passes (runs `alloc_audit` with all 6 `ALLOC_AUDIT_PASS` lines verified + `verify_phase10.mjs` for `10a`, `10b`, `10c`, `10d`).
3. `node /refs/phase9a/verify_phase9a_aws.mjs` passes against live `aws-emulator`.
4. `verify_phase8a.mjs`, `verify_phase8b.mjs`, `verify_phase8c.mjs`, and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` all pass.
