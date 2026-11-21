# Cardputer Zero LLRT — Phase 12 Technical Specification (Modern Web & WinterCG APIs: Web Crypto, `Blob`/`File`/`FormData`, `atob`/`btoa`, `node:path`, `node:fs/promises`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
**Mandatory reading first**: `/workspace/docs/ZERO_ALLOC_GUIDELINES.md` — all new code MUST obey the zero-allocation rules (`vec_to_uint8array` ownership move, borrowed `&[u8]` slices, native UTF-8/Base64, `ring` only, `cz-hal` strictly `libc`-only).
All existing Phase 1–11 tests and `phase10-alloc-audit-regression` MUST continue to pass without regression.

---

## 1. Web Crypto (`globalThis.crypto` + `crypto.subtle`)

Expose `globalThis.crypto` (installed via `cz_modules::install_web_crypto(&ctx)` from `cz-runner/src/main.rs`) backed by **`ring 0.17`** (already in `Cargo.lock` — **zero new crates**!):

- **`crypto.getRandomValues(typedArray)`**: fills `Uint8Array` / `Int8Array` / `Uint16Array` / `Uint32Array` in-place via `ring::rand::SystemRandom` and returns the same `typedArray` (throws `QuotaExceededError` / `RangeError` if `byteLength > 65536`).
- **`crypto.randomUUID()`**: RFC 4122 v4 UUID string (reusing `crypto_module`).
- **`crypto.subtle`** (all methods return `Promise`s):
  - **`digest(algorithm, data)`**: `"SHA-256"` / `"SHA-512"` (string or `{ name }`), `data` is `ArrayBuffer | Uint8Array` → returns `Promise<ArrayBuffer>`.
  - **`importKey(format, keyData, algorithm, extractable, keyUsages)`**:
    - Formats: `"raw"` (`ArrayBuffer | Uint8Array`) and `"jwk"` (`{ kty: "oct", k: base64url }`).
    - Algorithms: `{ name: "HMAC", hash: "SHA-256" | "SHA-512" | { name } }` and `{ name: "AES-GCM" }` (128-bit = 16 B or 256-bit = 32 B raw key).
    - Returns a `CryptoKey` object (`type: "secret"`, `extractable`, `algorithm`, `usages`).
  - **`exportKey(format, key)`**: `"raw"` → `Promise<ArrayBuffer>` (throws if `!key.extractable`).
  - **`sign(algorithm, key, data)`** & **`verify(algorithm, key, signature, data)`**:
    - `"HMAC"` / `{ name: "HMAC" }` with `SHA-256` or `SHA-512` (`sign` → `Promise<ArrayBuffer>`, `verify` → `Promise<boolean>` using constant-time verification via `ring::hmac::verify`).
  - **`encrypt(algorithm, key, data)`** & **`decrypt(algorithm, key, data)`**:
    - `{ name: "AES-GCM", iv: Uint8Array | ArrayBuffer (12 bytes), additionalData?: Uint8Array | ArrayBuffer, tagLength?: 128 }` backed by `ring::aead::{AES_128_GCM, AES_256_GCM, LessSafeKey, Nonce, Aad}` → returns `Promise<ArrayBuffer>` (ciphertext || 16-byte tag on encrypt; plaintext on decrypt, rejecting on tampered ciphertext/tag/AAD!).

---

## 2. `atob` / `btoa` & `Blob` / `File` / `FormData`

Install globally (e.g., in `fetch_module.rs` or `web_module.rs` called by `cz-runner`):
- **Native `atob(b64)` & `btoa(binStr)`**: standard Base64 decode/encode (latin1 binary string semantics per HTML spec, native Rust implementation — no pure-JS byte loops!).
- **`Blob`**:
  - `new Blob(parts?, options?)` (`parts`: array of `string | Uint8Array | ArrayBuffer | Blob`; `options`: `{ type?: string }`)
  - `.size`, `.type`
  - `.arrayBuffer(): Promise<ArrayBuffer>`, `.bytes(): Promise<Uint8Array>`, `.text(): Promise<string>`, `.slice(start?, end?, contentType?): Blob`
- **`File extends Blob`**:
  - `new File(bits, name, options?)` (`options`: `{ type?, lastModified? }`)
  - `.name`, `.lastModified`
- **`FormData`**:
  - `new FormData()`
  - `.append(name, value, filename?)`, `.set(name, value, filename?)`, `.get(name)`, `.getAll(name)`, `.has(name)`, `.delete(name)`, `.forEach(cb)`, `.keys()`, `.values()`, `.entries()`, `[Symbol.iterator]()`
- **`fetch()` & `Response` integration**:
  - `fetch(url, { method: "POST", body: blob })` sends `blob` bytes (setting `Content-Type: blob.type` if non-empty and not explicitly provided).
  - `fetch(url, { method: "POST", body: formData })` serializes `multipart/form-data; boundary=...` and sets the `Content-Type` header automatically.
  - `Response.prototype.blob(): Promise<Blob>`
  - `Response.prototype.formData(): Promise<FormData>` (parses both `application/x-www-form-urlencoded` and `multipart/form-data; boundary=...`, reconstructing `File` entries when `filename="..."` is present!).

---

## 3. `node:path` (and `"path"` alias)

Create `crates/cz-modules/src/path_module.rs` and register `PathModule` (`ModuleKind::Path`, re-exported as `pub use path_module::PathModule;`) for `"node:path"` and `"path"` (named + `default` exports):
- `sep` (`"/"`), `delimiter` (`":"`)
- `join(...paths)`, `resolve(...paths)`, `normalize(p)`, `isAbsolute(p)`, `dirname(p)`, `basename(p, ext?)`, `extname(p)`, `parse(p)` (`{ root, dir, base, ext, name }`), `format(obj)`.

---

## 4. `node:fs` Expansion & `node:fs/promises` (and `"fs/promises"` alias)

Extend `crates/cz-modules/src/fs_module.rs` and register `FsPromisesModule` (`ModuleKind::FsPromises`, re-exported as `pub use fs_module::FsPromisesModule;`) for `"node:fs/promises"` and `"fs/promises"` (plus `fs.promises` property on `node:fs`):
- **Additional sync functions on `node:fs` / `fs`**:
  - `writeFileSync(path, data: string | Uint8Array | ArrayBuffer)` (borrows `&[u8]` directly — no `.to_vec()`!)
  - `appendFileSync(path, data: string | Uint8Array | ArrayBuffer)`
  - `mkdirSync(path, options?: { recursive?: boolean })`
  - `readdirSync(path)` → `string[]` (sorted)
  - `unlinkSync(path)`, `rmSync(path, options?: { recursive?: boolean, force?: boolean })`, `renameSync(oldPath, newPath)`
- **`node:fs/promises` / `fs/promises`** (named + `default` exports, Promise-returning):
  - `readFile(path, encoding?)`, `writeFile(path, data)`, `appendFile(path, data)`, `mkdir(path, options?)`, `readdir(path)`, `stat(path)`, `unlink(path)`, `rm(path, options?)`, `rename(oldPath, newPath)`, `access(path)`

---

## 5. TypeScript Declarations (`types/cardputer.d.ts`)

Add declarations for:
- `CryptoKey`, `SubtleCrypto`, `Crypto`, global `crypto`, `atob`, `btoa`, `Blob`, `File`, `FormData`
- `Response.prototype.blob()` and `Response.prototype.formData()`
- `declare module "node:path"` / `declare module "path"`
- `declare module "node:fs/promises"` / `declare module "fs/promises"` + expanded `node:fs` methods.

---

## 6. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase12/verify_phase12.mjs` passes all 7 E2E checks.
3. `node /refs/phase10/verify_phase10_final.mjs` (`#[global_allocator]` audit) passes.
4. `node /refs/phase11/verify_phase11.mjs` (`URL` + `WebSocket`) and `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` pass.
