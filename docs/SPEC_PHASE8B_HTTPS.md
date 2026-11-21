# Cardputer Zero LLRT — Phase 8B Technical Specification (`node:https` Server TLS)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM, Debian trixie).
All existing Phase 1–8A tests (`cargo test --workspace -- --test-threads=1`, Phase 8A `fetch`+TLS E2E, and Phase 2 HTTP E2E) MUST continue to pass without regression.

---

## 1. Goal & Architecture

Add Node-compatible **`node:https`** (and alias **`https`**) TLS server support to `cz-js` while keeping `cz-hal` strictly `libc`-only (zero circular dependency with `cz-net`):

1. **`crates/cz-hal/src/http_server.rs`** (stays `libc`-only!):
   - Abstract the per-connection socket inside `http_server.rs` behind a `pub trait ConnStream: Read + Write + Send` (plus helpers `set_read_timeout`, `set_write_timeout`, `shutdown_both`, `peer_addr_str`, `is_encrypted(&self) -> bool`).
   - Implement `ConnStream` for `TcpStream` (`is_encrypted() -> false`).
   - Define `pub type StreamAcceptor = Arc<dyn Fn(TcpStream) -> std::io::Result<Box<dyn ConnStream>> + Send + Sync>;`.
   - Add `pub fn start_listener_with_acceptor(..., acceptor: Option<StreamAcceptor>) -> std::io::Result<Listener>` (keeping `start_listener(...)` calling `start_listener_with_acceptor(..., None)` so existing `node:http` callers need zero changes).
   - **Important**: Run `acceptor(stream)` inside the spawned per-connection worker thread (NOT inside `accept_loop`!) so a slow or malicious client that stalls during TLS handshake can NEVER block other incoming connections.
   - Surface `encrypted: bool` on each dispatched `IncomingRequest` / connection metadata so JS sets `req.socket.encrypted = true` for HTTPS connections (`false` for plain HTTP).

2. **`crates/cz-net/src/https.rs`** (new module exported from `cz-net`):
   - Reuse `cz_net::tls::{TlsServerOptions, build_server_config}` (already implemented in Phase 8A!).
   - Provide `pub fn make_tls_acceptor(opts: &TlsServerOptions) -> Result<cz_hal::http_server::StreamAcceptor, String>`:
     - Builds `Arc<rustls::ServerConfig>` once at server startup (failing fast on invalid PEM/key!).
     - For each accepted `TcpStream`, sets `nodelay(true)` and a bounded handshake timeout (e.g., 5s read/write timeout), creates `rustls::ServerConnection::new(cfg.clone())`, drives the TLS handshake to completion (`complete_io`), wraps it in a `TlsConnStream` implementing `cz_hal::http_server::ConnStream` (`is_encrypted() -> true`), and restores normal socket timeouts.
     - If handshake fails (e.g., plaintext HTTP client hitting HTTPS port, or missing/invalid client cert under mTLS), returns `Err(io::Error)` so the worker thread exits cleanly without crashing or wedging the server.
     - On `shutdown_both()`, sends TLS `close_notify` best-effort before shutting down the underlying `TcpStream`.
   - Do **NOT** add `tokio`, `openssl`, `native-tls`, `aws-lc-rs`, or `webpki-roots` to `cz-net`.

3. **`crates/cz-modules/src/https_module.rs`** (new module):
   - Register `HttpsModule` in `crates/cz-modules/src/lib.rs` for both `"node:https"` and `"https"` (`ModuleKind::Https`), so `cz-runner` resolves them automatically.
   - Re-export `pub use https_module::HttpsModule;` at the `cz-modules` crate root.
   - Export named symbols AND a `default` object containing:
     - `createServer(options, requestListener?)`
     - `Server`
     - `STATUS_CODES`, `METHODS` (reused from `node:http`)
   - **`options` accepted by `https.createServer(options, handler?)`**:
     - `key`: `string | Uint8Array` (required PEM private key)
     - `cert`: `string | Uint8Array` (required PEM certificate chain)
     - `ca?`: `string | Uint8Array` (optional PEM CA for client certificate verification / mTLS)
     - `requestCert?`: `boolean` (default `false`)
     - `rejectUnauthorized?`: `boolean` (default `true` when `requestCert` is `true`)
     - Missing/invalid `key` or `cert` throws a descriptive `Error` immediately (no panic).
   - **Server & Request/Response semantics**:
     - Reuses the exact `node:http` engine & JS glue (`listen(port, host?, cb?)` with default host `"0.0.0.0"`, `address()`, `close(cb?)`, `on("request" | "listening" | "close" | "error")`, HTTP/1.1 Keep-Alive, chunked, 1 MiB body cap, streaming `res.write()` / SSE + client-close detection).
     - `req.socket.encrypted === true` on HTTPS requests (`false` on plain HTTP).
     - Counts toward `http_listening_count()` / `http_has_pending()` so `cz-runner`'s event loop stays alive while any HTTPS server is listening and exits with status `0` once closed.

4. **TypeScript Declarations (`types/cardputer.d.ts`)**:
   - Add `CzHttpsServerOptions` and `declare module "node:https"` + `declare module "https"` declarations.

---

## 2. Verification Requirements

1. **`cz-hal` purity**: `crates/cz-hal/Cargo.toml` MUST remain `libc`-only (no `rustls` or `cz-net`).
2. **Rust tests**: `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 6` new unit tests covering `StreamAcceptor`, `make_tls_acceptor`, invalid PEM rejection, TLS handshake failure isolation, and mTLS config.
3. **Phase 8B offline E2E suite**: `node /refs/phase8b/verify_phase8b.mjs` passes all 8 scenarios (Node HTTPS client GET/POST/Keep-Alive/SSE, `cz-js` self-loopback `node:https` + `fetch()`, mTLS accept/reject, plaintext-on-HTTPS resilience, graceful `close()`, and `types/cardputer.d.ts`).
4. **No regressions**: `bash /refs/verify/check_integrity.sh`, `node /refs/phase8a/verify_phase8a.mjs`, and `bash /refs/verify/check_e2e_p2.sh` all pass.
