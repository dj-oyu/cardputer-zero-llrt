# Cardputer Zero LLRT — Phase 8A Technical Specification (`cz-net`: Shared TLS Foundation + `fetch` API)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM, Debian trixie).
All existing Phase 1–7C tests (`cargo test --workspace -- --test-threads=1` and existing E2E suites) MUST continue to pass without regression.

---

## 1. Goal & Multi-Phase Roadmap

Phase 8 introduces outbound HTTP/HTTPS (`fetch`) and TLS to `cz-js` without bringing Tokio or C-library TLS stacks into the runtime:

- **Phase 8A (this phase)**: New `crates/cz-net` crate with shared TLS configuration (`tls.rs`, `rustls` + `ring` + system CA via `rustls-native-certs`) and sync-threaded HTTP/1.1 client (`fetch.rs`, `ureq` v3) wired into `cz-modules` (`install_fetch`) and `cz-runner`'s existing Condvar `Waker` event loop.
- **Phase 8B (planned)**: `node:https` server TLS by adding a `StreamAcceptor` hook to `cz-hal::http_server` and supplying a `rustls::ServerConnection` acceptor from `cz-net`.
- **Phase 8C (planned)**: `node:tls` client/server raw TLS sockets built on `cz-net::tls`.

Designing `cz-net` cleanly now ensures `cz-hal` stays `libc`-only forever and avoids circular dependencies in Phases 8B/8C.

---

## 2. Crate & Dependency Constraints (`crates/cz-net`)

Add `"crates/cz-net"` to workspace `Cargo.toml`.

### Allowed Dependencies in `crates/cz-net/Cargo.toml`
Align versions with the existing workspace `Cargo.lock` (`rustls 0.23`, `ring 0.17`):
- `cz-hal = { path = "../cz-hal" }` (for `cz_hal::http_server::notify_event_loop()`)
- `ureq = { version = "3", default-features = false, features = ["rustls", "gzip"] }`
- `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }`
- `rustls-native-certs = "0.8"`
- `rustls-pemfile = "2"`
- `http = "1"`

### Forbidden Dependencies in `crates/cz-net`
Do **NOT** add any of the following to `cz-net` (or `cz-runner` / `cz-modules` / `cz-hal`):
- `tokio` (must remain synchronous worker threads + existing `Waker`)
- `openssl` / `native-tls` (C library dependency breaks musl/A53 portability)
- `aws-lc-rs` (heavy C/CMake cross-build; use `ring` only)
- `webpki-roots` (system CA only; avoids ~250–300 KiB static binary bloat)

---

## 3. `crates/cz-net/src/tls.rs` — Shared TLS Foundation

Provide reusable TLS builders designed for 8A (`fetch`), 8B (`node:https`), and 8C (`node:tls`):

### 3.1 System Root CA Loading (Lazy, Zero Startup Cost)
- Load system root certificates **lazily** via `OnceLock<Result<Arc<RootCertStore>, String>>` on first TLS use — `cz-js` startup for scripts that never call `fetch` MUST NOT touch `/etc/ssl/certs`.
- Use `rustls_native_certs::load_native_certs()`, which natively respects `SSL_CERT_FILE` and `SSL_CERT_DIR`.
- Also inspect `NODE_EXTRA_CA_CERTS`: if set to a readable PEM file path, parse and add those certificates into the root store as well.
- If the resulting `RootCertStore` is empty (no certificates loaded), return a clear error:
  `"TLS error: no root certificates found (install ca-certificates or set SSL_CERT_FILE)"`.
- Note for tests: when `SSL_CERT_FILE` or `SSL_CERT_DIR` is set, ensure the loader respects the current process env (since each `cz-js` invocation is a separate process, `OnceLock` per process is safe; unit tests that vary env vars can call a non-cached `load_system_roots_uncached()` helper).

### 3.2 `TlsClientOptions` & `build_client_config`
```rust
#[derive(Clone, Debug, Default)]
pub struct TlsClientOptions {
    /// Optional PEM-encoded CA certificate(s) (replaces system roots when provided).
    pub ca_pem: Option<Vec<u8>>,
    /// Optional PEM-encoded client certificate chain (for mTLS).
    pub cert_pem: Option<Vec<u8>>,
    /// Optional PEM-encoded client private key (for mTLS, SEC1/PKCS#8/PKCS#1).
    pub key_pem: Option<Vec<u8>>,
    /// Default `true`. When `false`, skips server certificate verification (dev/local use only).
    pub reject_unauthorized: bool, // default true via Default impl or Option<bool>
}
```
- When `ca_pem` is `Some(bytes)`, parse certificates with `rustls_pemfile::certs` into a fresh `RootCertStore` (return a clear error if 0 valid certs).
- When `cert_pem` + `key_pem` are both `Some`, configure client auth via `with_client_auth_cert(chain, key)`.
- When `reject_unauthorized == false`, attach a custom `rustls::client::danger::ServerCertVerifier` that accepts any certificate chain and valid TLS 1.2/1.3 signature (still performs real TLS encryption).
- Explicitly ensure `rustls::crypto::ring::default_provider()` is used (never panic on missing process-wide CryptoProvider).
- Provide `TlsServerOptions` / `build_server_config` stubs or ready helpers (`cert_pem`, `key_pem`, optional `ca_pem`) cleanly if helpful, without `todo!()`/`unimplemented!()`.

---

## 4. `crates/cz-net/src/fetch.rs` — Non-Blocking Worker Engine

### 4.1 Threading, Limits & Event-Loop Wakeup
- **No polling loops**: whenever headers arrive, a body chunk arrives, a stream finishes, or an error/abort occurs, call `cz_hal::http_server::notify_event_loop()` so `cz-runner`'s `idle_wait` wakes immediately.
- **Worker stack size**: spawn worker threads with `std::thread::Builder::new().stack_size(256 * 1024)`.
- **Concurrency cap**: maximum `16` concurrent in-flight `fetch` requests (`MAX_CONCURRENT_FETCHES = 16`). Exceeding the cap rejects cleanly with a `TypeError` / resource limit error rather than spawning unbounded threads.
- **Chunk queue backpressure**: each streaming response uses a bounded `Mutex<VecDeque<StreamEvent>> + Condvar` queue with max `8` chunks (`CHUNK_SIZE = 16 * 1024` bytes, max buffered ~128 KiB per stream). If full, the worker blocks on the Condvar until JS drains chunks or cancels/aborts.
- **Buffered body cap**: `text()` / `json()` / `bytes()` / `arrayBuffer()` enforce a default cap of **`16 * 1024 * 1024` bytes (16 MiB)**. Exceeding this cap cancels the underlying stream and rejects the Promise cleanly without OOM. Streaming via `res.body` has no total-size cap (bounded only by the 128 KiB backpressure queue).

### 4.2 Immediate Cancellation (`AbortSignal` & `AbortSignal.timeout`)
- Synchronous `read()` calls block until data arrives or the socket closes. To make `AbortController.abort()` cancel an idle SSE stream immediately (within milliseconds, NOT waiting for server timeout):
  - Wrap `ureq`'s TCP connector in a thin custom `Transport` (or `Connector`) that stores a cloned `TcpStream` handle (or `Arc<Mutex<Option<TcpStream>>>`) associated with the request's `AbortHandle`.
  - When `abort()` is called:
    1. Set `aborted: AtomicBool` to `true`.
    2. Call `stream.shutdown(std::net::Shutdown::Both)` on the active `TcpStream` (if connected).
    3. Notify the backpressure `Condvar` (in case the worker is parked on a full chunk queue).
    4. Call `notify_event_loop()` so JS sees the `AbortError` immediately.
- **Header-first resolve**: as soon as `ureq` finishes receiving the HTTP status line and headers, push a `FetchEvent::Headers { status, status_text, url, redirected, headers }` and wake the event loop — do **NOT** wait for the response body to finish before resolving `fetch()`!

### 4.3 HTTP Semantics
- Methods: `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS` (and any valid token).
- Redirects (`init.redirect`):
  - `"follow"` (default): follow 301/302/303/307/308 up to 10 hops; set `response.redirected = true` and `response.url` to the final URL.
  - `"manual"`: do not follow; return the 3xx `Response` as-is (`redirected = false`).
  - `"error"`: reject `fetch()` with a `TypeError` on any 3xx redirect response.
- Compression: automatic `gzip` decompression via `ureq`'s `gzip` feature.
- Non-2xx responses (404, 500, etc.): resolve `Response` with `ok = false` and the actual `status` (matching Web Fetch spec — do NOT reject on HTTP 4xx/5xx!).

---

## 5. JS Global Surface (`crates/cz-modules/src/fetch_module.rs` & `cz-runner`)

Expose `pub fn install_fetch<'js>(ctx: &Ctx<'js>) -> Result<()>` in `cz-modules` and call it from `crates/cz-runner/src/main.rs` right after `install_json(&ctx)`.

Do **NOT** add `fetch` to `CARDPUTER_MODULE_NAMES` (the `awslabs/llrt` fork already ships its own `fetch`; `install_fetch` is specifically for `cz-js`).

### 5.1 Globals Installed by `install_fetch`
1. **`Headers`**
   - `new Headers(init?)` (accepts plain object, `Array<[string, string]>`, or another `Headers`)
   - Case-insensitive lowercase header names
   - `get(name)` (returns `string | null`; multiple values joined with `", "`), `set(name, value)`, `append(name, value)`, `has(name)`, `delete(name)`, `forEach(cb)`, `keys()`, `values()`, `entries()`, `[Symbol.iterator]()`
2. **`AbortSignal` & `AbortController`**
   - `new AbortController()` with `.signal` and `.abort(reason?)`
   - `signal.aborted: boolean`, `signal.reason: any`, `signal.throwIfAborted()`, `signal.addEventListener("abort", fn)`, `signal.removeEventListener("abort", fn)`, `signal.onabort`
   - Default abort reason: an `Error` with `err.name = "AbortError"`
   - Static `AbortSignal.abort(reason?)` and `AbortSignal.timeout(ms)` (uses existing `setTimeout`, sets reason `Error` with `name = "TimeoutError"`)
3. **`ReadableStream`** (minimal Web Streams surface for `res.body`)
   - `stream.locked: boolean`
   - `stream.getReader()` → `{ read(): Promise<{ value?: Uint8Array, done: boolean }>, cancel(reason?): Promise<void>, releaseLock(): void }`
   - `stream.cancel(reason?): Promise<void>`
   - `[Symbol.asyncIterator]()` supporting `for await (const chunk of res.body)` (yields `Uint8Array` chunks and releases/cancels cleanly on `break` or error)
4. **`Request`**
   - `new Request(input, init?)` with `.url`, `.method`, `.headers`, `.body`, `.redirect`, `.signal`
5. **`Response`**
   - `.ok` (`200 <= status && status < 300`), `.status`, `.statusText`, `.url`, `.redirected`, `.headers` (`Headers`), `.body` (`ReadableStream | null`), `.bodyUsed` (`boolean`)
   - `.text(): Promise<string>`
   - `.json(): Promise<any>` — **MUST parse via `globalThis.JSON.parse` (which is `cz-json`!)** so large JSON responses transparently benefit from the NEON / Lazy tape engine
   - `.bytes(): Promise<Uint8Array>`
   - `.arrayBuffer(): Promise<ArrayBuffer>`
   - `.clone(): Response` (valid while `!bodyUsed`)
   - Reject with `TypeError` if a body consumer is called when `bodyUsed === true`.
6. **`fetch(input, init?)`**
   - `input`: `string` or `Request`
   - `init`: `{ method?, headers?, body?, redirect?, signal?, tls? }`
     - `body`: `string | Uint8Array | ArrayBuffer | ArrayBufferView | URLSearchParams | null` (if `URLSearchParams` or object with `.toString()` and no `Content-Type`, serialize as UTF-8)
     - `tls` (CardputerZero extension): `{ ca?: string | Uint8Array, cert?: string | Uint8Array, key?: string | Uint8Array, rejectUnauthorized?: boolean }`
   - Also install `URLSearchParams` if not already present in QuickJS globals (or support a lightweight `URLSearchParams` global so `new URLSearchParams({ a: "1" })` works in `fetch` body).

### 5.2 Event Loop Integration (`crates/cz-runner/src/main.rs`)
- Expose from `cz-modules`:
  - `pub fn fetch_active_count() -> usize`
  - `pub fn fetch_has_pending() -> bool`
- In `cz-runner`:
  - `loop_pass`: pump `__czFetchDrainLoop` alongside `__czHttpDrainLoop`, then drain QuickJS jobs **OUTSIDE** `ctx.with(...)` (rquickjs `RefCell` rule!).
  - `loop_liveness`: include `fetch_active_count()` and `fetch_has_pending()` so the process stays alive while any `fetch` or streaming `Response` is active, and exits cleanly with status `0` once all requests/streams/timers/servers complete or abort.
  - Ensure `has_http` guard in `run_event_loop` does NOT prematurely exit when `fetch` is active without `node:http` imported!

---

## 6. TypeScript Declarations (`types/cardputer.d.ts`)

Add complete global declarations to `types/cardputer.d.ts`:
- `CzFetchTlsOptions`, `CzFetchInit` / `RequestInit`
- `Headers`, `Request`, `Response`, `AbortController`, `AbortSignal`, `ReadableStream`, `ReadableStreamDefaultReader`, `URLSearchParams`
- `declare function fetch(input: string | Request, init?: CzFetchInit): Promise<Response>;`

---

## 7. Verification Requirements

1. **Rust unit & workspace tests**: `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 12` new unit tests in `cz-net` (TLS root loading, PEM parsing, invalid PEM error, `rejectUnauthorized: false`, chunk queue backpressure, abort wakeup, redirect modes, body cap).
2. **Phase 8A offline E2E suite**: `node /refs/phase8a/verify_phase8a.mjs` passes all 12 scenarios using local Node HTTP/HTTPS/mTLS servers and `/refs/fixtures/tls/*`.
3. **Integrity & regression**: `bash /refs/verify/check_integrity.sh` passes (`.pi/verifier.json` SHA-256 unchanged, zero `todo!/unimplemented!/#[ignore]`), and Phase 2 HTTP E2E regression passes.
