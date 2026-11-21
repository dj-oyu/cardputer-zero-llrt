# Cardputer Zero LLRT — Phase 10A Technical Specification (Data Layer Zero-Clone: `cz-json` & `cz-hal::kv`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
All existing Phase 1–9A Rust workspace tests and Phase 3A/5 E2E suites MUST continue to pass without regression.

---

## 1. Goal

Eliminate hot-path cloning, dead work, and O(N) full-map scans in the data layer (`cz-json`, `cz-hal::kv`, `cz-modules/{json_module,kv_module}`) so downstream Phase 10C (`Response.json()` direct) and Phase 10D (HTTP) build on a zero-clone foundation.

---

## 2. `cz-json` (`crates/cz-json/src/lazy.rs`)

1. **Clean fast-path (`is_clean()`)**:
   - In `get()`, `has()`, and `own_keys()`, check `self.is_clean()` BEFORE acquiring the `edits` `Mutex`. Untouched Lazy documents MUST perform zero mutex locking and zero edit lookups.
2. **Eliminate full `Edit` Vec deep-clones (`edits_of`)**:
   - Remove `fn edits_of(&self, node: u32) -> Edit` (which cloned the entire `Vec<(String, Option<Val<'static>>)>` on every lookup!).
   - Replace with under-lock reference lookup (`with_edits` / holding `MutexGuard` while reading `latest_in_edit(&edit, key)`), returning only the needed borrowed/lightweight result without cloning unrelated edits.
3. **Fix `serialize()` dead work & duplicate lookups**:
   - Remove the dead `let keys = self.own_keys(node);` + `let _ = &keys;` in `serialize()`.
   - In object/array serialization, lock the `edits` map at most once per dirty node (instead of calling `latest()` twice per child + `edits_of()` + `own_keys()`).
   - Avoid temporary `i.to_string()` inside tight array loops where numeric index comparison suffices, and avoid allocating a temporary `Vec<u8>` in `write_key()`.

---

## 3. `cz-modules/src/json_module.rs`

1. **Hoist shared `LAZY_HANDLER` singleton**:
   - In `__czLazyMake`, hoist the 6-trap Proxy handler object into a single shared `LAZY_HANDLER` constant (mirroring `ROW_HANDLER` in `kv_module.rs`), reading the tag from the proxy target rather than allocating 6 new JS closures per Lazy handle.
2. **Cache global symbols & avoid redundant clones**:
   - Avoid redundant `String` allocations in `js_to_val` and `write_kv_record` (use the new `KvStore::visit_record` Visitor API below!).

---

## 4. `cz-hal::kv` (`crates/cz-hal/src/kv.rs`) & `crates/cz-modules/src/kv_module.rs`

1. **Visitor API (zero-clone reads under lock)**:
   - Add `pub fn with_value<R>(&mut self, key: &str, f: impl FnOnce(Option<&KvValue>) -> R) -> R`
   - Add `pub fn with_field<R>(&mut self, key: &str, field: &str, f: impl FnOnce(Option<&KvValue>) -> R) -> R`
   - Add `pub fn visit_record(&mut self, key: &str, f: impl FnMut(&str, &KvValue)) -> bool` (visits record fields in sorted key order without cloning `KvValue`s or allocating `Vec<(String, KvValue)>`!).
   - Keep existing `get` / `get_field` / `record_fields` methods for backward compatibility with unit tests, while switching `kv_module.rs` and `json_module.rs::write_kv_record` to use the Visitor API!
2. **Zero-alloc updates on existing keys/fields**:
   - In `set`, `set_field`, and `incr`, check `self.entries.get_mut(key)` FIRST before allocating `key.to_string()` or `Box<str>` for existing keys/fields. Mutate scalar/record entries in place when the key already exists!
3. **Eliminate O(N) full-map scans on every operation**:
   - Track `ttl_count: usize` (or `earliest_expiry: Option<Instant>`) so `purge_expired()` is an **O(1) no-op** when no TTL entries exist or none have expired yet.
   - Maintain `total_bytes` incrementally via byte deltas on `set_field` / `del_field` / `incr` instead of calling `recount_total_bytes()` (which walked every entry and nested map) on every mutation.

---

## 5. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase10/verify_phase10.mjs --phase=10a` passes.
3. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 6` new unit tests covering `LazyDoc` clean/dirty serialize, `KvStore` Visitor API, zero-recount byte accounting, and TTL expiry fast-path.
4. Phase 3A KV E2E (`bash /refs/verify/check_e2e_p3a.sh`) and Phase 5 JSON E2E (`bash /refs/verify/check_e2e_p5.sh`) regressions pass.
