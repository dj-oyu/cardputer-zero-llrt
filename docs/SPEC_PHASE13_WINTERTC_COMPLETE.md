# Cardputer Zero LLRT — Phase 13 Technical Specification (WinterTC Completion: `EventTarget`/`Event`/`CustomEvent`, `structuredClone`, `performance.now()`, `queueMicrotask`, `node:os`, Minimal `Buffer`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
**Mandatory reading first**: `/workspace/docs/ZERO_ALLOC_GUIDELINES.md`.
All existing Phase 1–12 tests and `phase10-alloc-audit-regression` MUST continue to pass without regression.

---

## 1. Goal

Complete the remaining WinterTC / modern JS standard globals and lightweight Node compatibility primitives needed for `cz-js` to stand completely on its own (AWS-LLRT Free).

---

## 2. Global Web Standard Primitives

Install from `cz-runner/src/main.rs` (via a helper in `cz-modules`, e.g., `install_web_globals(&ctx)`):

1. **`Event`, `CustomEvent`, `EventTarget`**:
   - `new Event(type, { bubbles?, cancelable? })`: `.type`, `.defaultPrevented`, `.preventDefault()`, `.stopPropagation()`, `.stopImmediatePropagation()`, `.target`, `.currentTarget`, `.timeStamp`.
   - `new CustomEvent(type, { detail?, bubbles?, cancelable? }) extends Event`: `.detail`.
   - `class EventTarget`: `addEventListener(type, listener, options?)` (supports `{ once: true }`), `removeEventListener(type, listener)`, `dispatchEvent(event)` (sets `target`/`currentTarget`, respects `stopImmediatePropagation()`, returns `!event.defaultPrevented`).
   - Make **`AbortSignal` and `WebSocket` inherit from `EventTarget`** (`signal instanceof EventTarget === true`, `ws instanceof EventTarget === true`) so all existing `addEventListener` / `removeEventListener` / `on*` semantics remain 100% compatible!
2. **`structuredClone(value)`**:
   - Deep-clones primitives, plain Objects, Arrays, `Date`, `RegExp`, `Map`, `Set`, `Uint8Array` / `ArrayBuffer` (copying backing bytes), and handles **circular references** via a `Map` of seen objects!
   - Throws a `DataCloneError` (`Error` with `name = "DataCloneError"`) on functions / symbols.
3. **`performance.now()`**:
   - Global `performance` object with monotonic high-resolution `performance.now(): number` (milliseconds as `f64` since process start via `std::time::Instant` — NEVER wall clock!) and `performance.timeOrigin: number`.
4. **`queueMicrotask(fn)`**:
   - Schedules `fn` on the QuickJS microtask queue (`Promise.resolve().then(fn)`), throwing `TypeError` if `fn` is not a function.

---

## 3. `node:os` (and `"os"` alias)

Create `crates/cz-modules/src/os_module.rs` and register `OsModule` (`ModuleKind::Os`, re-exported as `pub use os_module::OsModule;`) for `"node:os"` and `"os"` (named + `default` exports):
- `platform()` (`"linux"`), `arch()` (`"arm64"` / `"x64"`), `EOL` (`"\n"`), `tmpdir()` (`std::env::temp_dir()`), `homedir()` (`$HOME` fallback `"/root"` / `"/"`), `hostname()` (`libc::gethostname`), `cpus()` (array of `{ model, speed }` with length `std::thread::available_parallelism()`), `uptime()` (seconds).

---

## 4. Minimal `Buffer` Global & `"node:buffer"` / `"buffer"`

Provide a lightweight `Buffer extends Uint8Array` global (plus `"node:buffer"` / `"buffer"` exporting `{ Buffer }` + `default`) backed by native UTF-8 / Base64 / Hex helpers (following `ZERO_ALLOC_GUIDELINES.md` — no slow char-by-char JS loops!):
- `Buffer.from(input, encoding?)`:
  - `string` with `"utf8"`/`"utf-8"` (default), `"base64"`, `"base64url"`, `"hex"`, `"latin1"`/`"binary"`
  - `Uint8Array` / `ArrayBuffer` / `number[]`
- `Buffer.alloc(size, fill?)` and `Buffer.allocUnsafe(size)`
- `Buffer.concat(list, totalLength?)`
- `Buffer.isBuffer(val)` → `val instanceof Buffer`
- `Buffer.byteLength(str, encoding?)`
- Instance methods:
  - `buf.toString(encoding?)` (`"utf8"`/`"utf-8"`, `"base64"`, `"base64url"`, `"hex"`, `"latin1"`/`"binary"`)
  - `buf.equals(other)`, `buf.subarray(start?, end?)` (returns `Buffer`), `buf.copy(target, targetStart?, sourceStart?, sourceEnd?)`

---

## 5. TypeScript Declarations (`types/cardputer.d.ts`)

Add declarations for `Event`, `CustomEvent`, `EventTarget`, `structuredClone`, `performance`, `queueMicrotask`, `Buffer`, `declare module "node:buffer"`, `declare module "buffer"`, `declare module "node:os"`, and `declare module "os"`.

---

## 6. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase13/verify_phase13.mjs` passes all 6 E2E checks.
3. `node /refs/phase10/verify_phase10_final.mjs` (`#[global_allocator]` audit) passes.
4. `node /refs/phase12/verify_phase12.mjs` and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` pass.
