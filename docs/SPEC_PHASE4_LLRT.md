# Cardputer Zero LLRT — Phase 4A Specification (KV Memory Optimization + `awslabs/llrt` Integration Bridge)

All Phase 1, 2, 2.5, and 3A tests (147+ Rust tests, 28 E2E tests) MUST continue to pass without regression.

---

## 1. `cardputer:kv` Memory & Allocation Optimization (from Code Review)

### 1.1 `crates/cz-hal/src/kv.rs`: Unified `EntryPayload` Enum
- Replace the dual `scalar: Option<KvValue>` + `fields: HashMap<Box<str>, KvValue>` fields inside `Entry` with:
  ```rust
  #[derive(Clone, Debug)]
  pub enum EntryPayload {
      Scalar(KvValue),
      Record(HashMap<Box<str>, KvValue>),
  }
  ```
- This eliminates the empty `HashMap` (~48 bytes) overhead on every scalar entry.
- Add a Rust unit test asserting `std::mem::size_of::<Entry>() <= 72` (on 64-bit targets).

### 1.2 `crates/cz-modules/src/kv_module.rs`: Zero Per-Call Native Closure Allocation in `store.row(key)`
- Previously, every `store.row(key, ttlSec?)` call created 4 new Rust closures (`get_f`, `set_f`, `del_f`, `has_f`) with `Arc::clone` + `key.clone()`.
- Refactor so the 4 native closures `(key: String, field: String, ...)` are created **once per `kv.open()` store**, and `__czKvMakeRow(key, ttl, getF, setF, delF, hasF)` wraps them in a thin JS `Proxy` over `{}` without creating any new Rust `Function` objects per `row()` call.
- Prove in E2E: calling `store.row("u1")` 2,000 times inside a loop does not leak Rust closures or bloat QuickJS heap (`jsHeapDelta` stays small).

---

## 2. Clean `awslabs/llrt` v0.10.0-beta Integration Surface (`crates/cz-modules`)

- Expose public `rquickjs::module::ModuleDef` structs directly from `cz_modules`:
  - `DisplayModule` (`"cardputer:display"`)
  - `InputModule` (`"cardputer:input"`)
  - `AppModule` (`"cardputer:app"`)
  - `PowerModule` (`"cardputer:power"`)
  - `KvModule` (`"cardputer:kv"`)
  - `HttpModule` (`"node:http"` / `"http"`)
  - `FsModule` (`"node:fs"` / `"fs"`)
- Expose a clean, documented `pub fn pump_llrt_http<'js>(ctx: &Ctx<'js>) -> Result<bool>` and `pub fn has_active_http_servers() -> bool` so `llrt_core` (Tokio async runtime) can drive `node:http` + SSE servers cleanly alongside LLRT's native event loop.
- Provide `pub const CARDPUTER_MODULE_NAMES: &[&str]` listing all Cardputer modules.

---

## 3. Reproducible LLRT Fork Integration Script (`scripts/build-llrt-a53.sh`)

Create `/workspace/scripts/build-llrt-a53.sh` (LF line endings, executable):
- Supports `--apply-only <llrt-dir>` mode (for testing patch application cleanly without needing nightly/zig inside `pi-sandbox`) AND full build mode (for running inside `cz-dev:latest` container on the host).
- In `--apply-only <llrt-dir>` mode:
  1. Applies Cortex-A53 CPU configuration to `<llrt-dir>/.cargo/config.toml` (removes `+lse`, sets `target-cpu=cortex-a53`) and `<llrt-dir>/Makefile` (adds `-mcpu=cortex_a53` to `zig cc`).
  2. Wires `cz-modules` as a path dependency into `<llrt-dir>/llrt_modules/Cargo.toml` (behind a `cardputer` feature enabled by default) and registers `cardputer:display`, `cardputer:input`, `cardputer:app`, `cardputer:power`, `cardputer:kv`, and `http` (`HttpModule`) in `<llrt-dir>/llrt_modules/src/module_builder.rs` via `llrt_utils::module::ModuleInfo`.
  3. Verifies all patch markers are present and idempotent (running `--apply-only` twice on the same directory succeeds cleanly without duplicating lines).
- In full build mode: runs `--apply-only`, builds `llrt-linux-arm64-no-sdk.zip` with `CFLAGS_aarch64_unknown_linux_musl="-mcpu=cortex_a53"`, verifies `objdump` has `LSE <= 200` and `RCpc == 0`, and outputs the binary + `.sha256`.
