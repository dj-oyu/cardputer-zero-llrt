# Cardputer Zero LLRT — Phase 5 Technical Specification (`crates/cz-json`: Cortex-A53 NEON JSON Engine + Transparent Lazy Materialization)

Target goal: Replace `globalThis.JSON.parse` and `globalThis.JSON.stringify` with a Cortex-A53-optimized Rust engine (`crates/cz-json`) that delivers **transparent high-speed execution for unmodified third-party JS code**:
1. **Small JSON (`< 4096` bytes)**: Zero-copy 1-pass NEON parse (no input buffer copy, no Tape) + 1-pass NEON escape & fast-int stringify.
2. **Large JSON (`>= 4096` bytes)**: Automatic **16-byte arena `TapeNode`** + `Arc<[u8]>` transparent Lazy handle (zero JS objects materialized until touched, Copy-on-Write on mutation, zero-reparse passthrough on `JSON.stringify`).
3. **Native-to-Native Shortcut**: `JSON.stringify(kv.row(key))` serializes directly from Rust `EntryPayload::Record` without walking JS properties.

All Phase 1, 2, 2.5, 3A, and 4A tests (156+ Rust tests, 30 E2E tests) MUST continue to pass without regression.

---

## 1. Crate Structure: `crates/cz-json`

Add `"crates/cz-json"` to workspace `Cargo.toml` and wire it into `crates/cz-modules` and `crates/cz-runner`.

### 1.1 Layer 0: Cortex-A53 ARMv8.0 NEON & SWAR Primitives
- Implement `scan_unescaped_run(bytes: &[u8], start: usize) -> usize`:
  - Returns the index of the first byte `>= start` that is `b'"'`, `b'\\'`, or `< 0x20` (or `bytes.len()`).
  - Under `#[cfg(target_arch = "aarch64")]`, use ARMv8.0 NEON intrinsics (`core::arch::aarch64::{vld1q_u8, vdupq_n_u8, vceqq_u8, vcltq_u8, vorrq_u8, vmaxvq_u8}`) — 16 bytes per iteration, NO LSE/RCpc instructions!
  - Under non-aarch64 (x86_64 tests), provide an identical-semantics fast loop so all unit/E2E tests verify the exact same behavior.
- Implement `write_escaped_str(out: &mut Vec<u8>, s: &str)`:
  - Wraps in `"`...`"`, uses `scan_unescaped_run` to bulk-copy clean runs via `out.extend_from_slice`, and escapes `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, and control chars `\u00XX`.
- Implement fast integer formatting (`write_i64` using a 200-byte 2-digit lookup table) and integer-first parsing (`parse_int_fast`: pure integer digits without `.` / `e` / `E` bypass `f64` parser).

### 1.2 Layer 1: 16-Byte Arena `TapeNode` & Adaptive Indexing
- Exact C-layout struct (MUST assert `std::mem::size_of::<TapeNode>() == 16` in unit tests!):
  ```rust
  #[repr(C)]
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct TapeNode {
      pub kind: u8,     // Object=1, Array=2, String=3, Int=4, Float=5, Bool=6, Null=7
      pub flags: u8,    // bit 0: HAS_ESCAPE, bit 1: BOOL_TRUE
      pub key_fp: u16,  // 16-bit fingerprint of key (len + first/last bytes) for fast mismatch rejection
      pub start: u32,   // byte start offset in Arc<[u8]>
      pub end: u32,     // byte end offset (for scalars) OR direct child count (for Object/Array)
      pub skip_to: u32, // next sibling Tape index (O(1) subtree skip)
  }
  ```
- Stored in a single contiguous `Vec<TapeNode>` pre-allocated with `Vec::with_capacity(input.len() / 8)` (arena allocation: ~1 malloc for the entire document).
- Adaptive object key lookup:
  - `child_count <= 16`: linear scan over Tape children using `key_fp: u16` pre-filter (0 extra memory, L1 cache friendly).
  - `child_count > 16`: lazily build and cache a `HashMap<u64, u32>` index on first access to that object.
- Strict JSON syntax validation during Tape build (invalid JSON MUST return `Err(...)` -> JS `SyntaxError`, never panic!).

### 1.3 Layer 2: Transparent `globalThis.JSON.parse` / `globalThis.JSON.stringify` Replacement
Install `cz_json::install_json(&ctx)` at startup in `cz-runner` (and expose `pub fn install_json` in `cz-modules` for LLRT integration):

#### `JSON.parse(text, reviver?)`
- If `reviver` is a function: delegate to saved original QuickJS `JSON.parse(text, reviver)`.
- Coerce `text` to string (ECMA-262).
- If `text.len() < 4096`:
  - Run the 1-pass zero-copy NEON parser directly constructing `JSValue` (unescaped strings passed straight from `&bytes[start..end]` without intermediate `Vec<u8>`).
- If `text.len() >= 4096`:
  - Build the `Arc<TapeDoc>` (`Arc<[u8]>` + `Vec<TapeNode>`) and return a **transparent Lazy Proxy handle**:
    - For Object root: `Proxy` over `{}` (tagged with hidden Symbol `Symbol.for("cz.lazyJson")`).
    - For Array root: `Proxy` over `[]` so `Array.isArray(x) === true`!
    - Property reads (`doc.meta.seq`, `doc.items[2499].score`): read from Tape without materializing sibling nodes. Nested Object/Array returns a child Lazy handle; scalars return immediate JS primitives.
    - `Object.keys`, `for..in`, `in`, `length` (on arrays) work transparently.
    - **Copy-on-Write (CoW)**: On any mutation (`set` or `deleteProperty`), materialize that node into a real JS object/array (preserving all existing fields + applying the mutation) and mark the handle dirty so subsequent reads and `JSON.stringify` reflect the mutation accurately!
- Invalid JSON MUST throw a JS `SyntaxError`.

#### `JSON.stringify(value, replacer?, space?)`
- If `replacer` is non-null/defined OR `space` is non-null/defined: delegate to saved original QuickJS `JSON.stringify(value, replacer, space)`.
- Fast native dispatch:
  1. **Clean (unmodified) Lazy JSON handle**: return the exact slice `&raw[start..end]` directly (zero re-parse, zero re-serialize!).
  2. **`cardputer:kv` row handle** (tagged with hidden Symbol `Symbol.for("cz.kvRow")`): serialize directly from Rust `EntryPayload::Record` into `&mut Vec<u8>` without invoking JS `Proxy` traps!
  3. **Regular JS values**: serialize directly into `&mut Vec<u8>` using NEON `write_escaped_str` and `write_i64` (skipping `undefined`/function object properties, emitting `null` for `undefined`/NaN/Infinity in arrays, throwing `TypeError` on circular references — ECMA-262 compliant).

### 1.4 Module `"cardputer:json"` (named + `default` exports)
- `parseLazy(text: string): any` — forces Lazy Tape handle regardless of size (useful for testing & explicit use).
- `isLazy(value: any): boolean` — returns `true` if `value` is a clean (unmaterialized) Lazy JSON handle.
- `lazyStats(value: any): { nodeCount: number, rawBytes: number, materialized: boolean } | null`.
- Register `"cardputer:json"` in `cz-modules` (`JsonModule`), `cz-runner`, and `types/cardputer.d.ts`.
