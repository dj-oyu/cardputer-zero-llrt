# Cardputer Zero LLRT — Phase 3A Technical Specification (`cardputer:kv` Native Memory Store)

Target goal: Provide a **Rust-native, JS-heap-offloaded key-value & structured record store** (`cardputer:kv`) for M5Stack CardputerZero (512MB RAM, SD card with power-loss risk) so multi-user web servers and apps do NOT bloat QuickJS heap / GC with `Map` / `JSObject` / `JSON.parse` overhead.

All Phase 1, Phase 2, and Phase 2.5 tests MUST continue to pass without regression.

---

## 1. Core Architectural Proof Requirements (Non-Negotiable)

1. **True Native Storage (No JS Map/Object Cheating)**:
   - All stored keys, field maps, strings, byte buffers, and numeric values MUST live in Rust heap memory inside `crates/cz-hal` (or `cz-kv`), NEVER in JS `Map` or JS objects.
   - Storing **2 MiB** of data (e.g. 256 entries × 8 KiB) into `cardputer:kv` MUST show `store.stats().bytes >= 2_000_000` while QuickJS `app.memoryUsage().mallocSize` increases by **less than 128 KiB** (proving payloads are offloaded from the JS heap).
2. **Zero-Copy Native Row Handle (`store.row(key)`)**:
   - `const r = store.row("u1")` returns a thin handle (e.g. a JS `Proxy` over an empty target backed by native getter/setter traps, or an `rquickjs::Class` handle) referencing the Rust-native record for `"u1"`.
   - Regular JS arithmetic and property access MUST work naturally: `r.views = 0; r.views++; r.views += 5; r.name = "alice";`.
   - **No property copying**: `Object.getOwnPropertyNames(r)` MUST NOT contain stored data field names (data lives in Rust, not copied onto the JS object).
   - **Shared live view**: Two handles `r1 = store.row("u1")` and `r2 = store.row("u1")` MUST reflect mutations immediately (`r1.views = 42` ⇒ `r2.views === 42`), and native `store.incr("u1", "views", 8)` MUST immediately be visible via `r1.views === 50`.
3. **Compact Tagged Native Values**:
   - Rust value enum: `KvValue { Null, Bool(bool), Int(i64), Float(f64), Str(Box<str>), Bytes(Box<[u8]>) }`.
   - No `JSON.parse` / `JSON.stringify` roundtrips for reading or incrementing fields.
4. **Memory Bounds (LRU Eviction) & TTL Expiration**:
   - Every store enforces `maxBytes` (default `1_048_576` = 1 MiB) and `maxEntries` (default `1024`).
   - When inserting/updating exceeds `maxEntries` or `maxBytes`, evict least-recently-used entries until within limits and increment `evictions`.
   - Optional `ttlSec` (positive number, fractional seconds allowed e.g. `0.15` for 150ms): expired entries are automatically treated as absent and reclaimed on access/stats/keys.
5. **Power-Loss-Safe Persistence (`tmp -> fsync -> rename`)**:
   - If `persistPath` is provided in `kv.open(name, { persistPath })`:
     - If the file already exists on `open`, load all non-expired entries from disk.
     - `store.flush()` writes a compact binary (or deterministic length-prefixed) snapshot to `<persistPath>.tmp`, calls `sync_all()` (`fsync`), and atomically `rename`s it over `<persistPath>`.
     - Never write on every `set()`; only write when `flush()` is explicitly called (SD card longevity rule).

---

## 2. Module API Specifications

### 2.1 Module `"cardputer:kv"` (named exports AND `default` export)
- `open(name: string, options?: { maxBytes?: number, maxEntries?: number, persistPath?: string }): KvStore`
  - Opens or returns the named Rust-native store.
- `drop(name: string): boolean`
  - Removes and frees the named store from the native registry (returns `true` if existed).

#### `KvStore` object methods
- `set(key: string, value: null | boolean | number | string | Uint8Array, ttlSec?: number): void`
- `get(key: string): null | boolean | number | string | Uint8Array | undefined`
- `has(key: string): boolean`
- `del(key: string): boolean`
- `keys(prefix?: string): string[]`
- `setField(key: string, field: string, value: null | boolean | number | string | Uint8Array, ttlSec?: number): void`
- `getField(key: string, field: string): null | boolean | number | string | Uint8Array | undefined`
- `incr(key: string, field: string, delta?: number, ttlSec?: number): number` (default `delta = 1`; treats missing field as `0`)
- `row(key: string, ttlSec?: number): Record<string, any>` (thin native handle with live get/set/deleteProperty traps; no data copied onto JS object)
- `stats(): { name: string, entries: number, bytes: number, maxBytes: number, maxEntries: number, evictions: number, hits: number, misses: number }`
- `flush(): boolean` (returns `true` if persisted to `persistPath`, `false` if in-memory only)
- `clear(): void`

### 2.2 Diagnostic Extension on `"cardputer:app"`
- `memoryUsage(): { mallocSize: number, mallocCount: number, memoryUsedSize: number, objCount: number, strCount: number }`
  - Backed directly by QuickJS `Runtime::memory_usage()` (`JS_ComputeMemoryUsage`).

### 2.3 Runner & Types
- Register `"cardputer:kv"` in `crates/cz-runner/src/main.rs`.
- Add `"cardputer:kv"` and `app.memoryUsage()` declarations to `types/cardputer.d.ts`.
