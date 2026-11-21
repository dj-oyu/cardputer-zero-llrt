# Cardputer Zero LLRT — Phase 8C Technical Specification (`node:tls` Client & Server Raw TLS Sockets)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM, Debian trixie).
All existing Phase 1–8B tests (`cargo test --workspace -- --test-threads=1`, Phase 8B `node:https`, Phase 8A `fetch`+TLS, and Phase 2 `node:http`) MUST continue to pass without regression.

---

## 1. Goal & Architecture

Add Node-compatible **`node:tls`** (and alias **`tls`**) raw TLS client and server sockets to `cz-js`, built on the shared `cz-net::tls` foundation (`rustls 0.23` + `ring` + system CA) and existing Condvar `Waker` event loop — **zero Tokio, zero C TLS libraries, `cz-hal` strictly `libc`-only**.

### 1.1 `crates/cz-net/src/tls.rs` (small ALPN extension)
- Add optional `pub alpn_protocols: Vec<Vec<u8>>` to both `TlsClientOptions` and `TlsServerOptions` (default empty).
- In `build_client_config` and `build_server_config`, if `!opts.alpn_protocols.is_empty()`, set `cfg.alpn_protocols = opts.alpn_protocols.clone()`.
- Keep all existing callers compatible (default `Vec::new()`).

### 1.2 `crates/cz-net/src/tls_socket.rs` (new module exported from `cz-net`)
Provide a clean, thread-based full-duplex TLS socket & server engine:
- **Wakeup**: On every event (`SecureConnect`, `ServerConnection`, `Data(Vec<u8>)`, `End`, `Close`, `Error(String)`), push to the shared event queue and call `cz_hal::http_server::notify_event_loop()`.
- **Worker stack size**: `256 * 1024` bytes (`std::thread::Builder::new().stack_size(256 * 1024)`).
- **Client socket (`connect_tls`)**:
  - Connects `TcpStream::connect((host, port))`, sets `nodelay(true)`.
  - Performs `rustls::ClientConnection` handshake using `servername` (defaults to `host`) and `build_client_config(&opts)`.
  - On handshake error (e.g., untrusted CA, mTLS cert rejected), emits `Error(msg)` + `Close` and exits thread cleanly.
  - On handshake success, records negotiated `alpn_protocol: Option<String>` and `authorized: bool` (`opts.reject_unauthorized`), emits `SecureConnect`, and enters a full-duplex I/O loop:
    - Uses short non-blocking / poll-based interleaving (`set_read_timeout(Some(Duration::from_millis(20)))` or `libc::poll` + outbound write channel) so `write(bytes)` and `end()` / `destroy()` are flushed promptly without deadlocking on a blocking `read()`.
    - On `end()`: flushes pending outbound writes, sends TLS `close_notify` (`conn.send_close_notify()`), flushes, and shuts down write half (`Shutdown::Write`).
    - On `destroy()`: sends best-effort `close_notify`, calls `TcpStream::shutdown(Shutdown::Both)`, and exits thread cleanly.
    - When peer closes cleanly (EOF / `close_notify`), emits `End` then `Close` and removes socket from active registry.
- **Server listener (`create_tls_server` / `listen` / `close`)**:
  - Builds `Arc<rustls::ServerConfig>` up front via `build_server_config(&opts)` (invalid PEM/key fails fast).
  - Binds `TcpListener` with `SO_REUSEADDR` (default host `"0.0.0.0"`, `port = 0` ephemeral port supported).
  - Accept loop polls with short timeout so `close()` stops accepting and joins cleanly.
  - Each accepted `TcpStream` spawns a per-connection worker thread (256 KiB stack) that completes the TLS handshake with a bounded timeout (5s) so malformed/plaintext clients never stall the server.
  - On handshake success, registers a server-side `TlsSocket` handle, emits `ServerConnection { server_id, socket_id, alpn_protocol, authorized, remote_address, remote_port }`, and runs the same full-duplex I/O loop.
- **Liveness**:
  - `pub fn tls_active_count() -> usize` (open client sockets + open server sockets + listening TLS servers).
  - `pub fn tls_has_pending() -> bool` (queued events awaiting JS drain).

### 1.3 `crates/cz-modules/src/tls_module.rs` (new module)
- Register `TlsModule` in `crates/cz-modules/src/lib.rs` for `"node:tls"` and `"tls"` (`ModuleKind::Tls`), and re-export `pub use tls_module::TlsModule;`.
- Export `pub fn tls_active_count() -> usize` and `pub fn tls_has_pending() -> bool` from `cz-modules`.
- Export named symbols AND `default` object:
  - `connect(options, callback?)` and `connect(port, host?, options?, callback?)`
  - `createServer(options, secureConnectionListener?)`
  - `TLSSocket`, `Server`
- **`TLSSocket` JS surface**:
  - Properties: `encrypted` (`true`), `authorized` (`boolean`), `alpnProtocol` (`string | false`), `remoteAddress` (`string`), `remotePort` (`number`), `destroyed` (`boolean`).
  - Methods: `write(chunk, cb?)` (`string | Uint8Array`), `end(chunk?, cb?)`, `destroy()`, `setEncoding(enc)`, `on(event, fn)`, `once(event, fn)`, `off(event, fn)` / `removeListener(event, fn)`.
  - Events: `"secureConnect"`, `"data"` (`string` if `setEncoding("utf8")`, else `Uint8Array`), `"end"`, `"close"`, `"error"`.
  - Note on TLS 1.3 mTLS: remember TLS 1.3 may deliver client-cert rejection alerts as a post-handshake error on the first read/write; ensure any post-handshake TLS error emits `"error"` and `"close"` cleanly.
- **`Server` JS surface**:
  - `listen(port, host?, cb?)` (default host `"0.0.0.0"`), `address()` (`{ address, family, port } | null`), `close(cb?)`, `on("secureConnection" | "listening" | "close" | "error", fn)`, `once(...)`.

### 1.4 Event Loop Integration (`crates/cz-runner/src/main.rs`)
- In `loop_pass`: pump `__czTlsDrainLoop` alongside HTTP/fetch drains (and ALWAYS run `drain_jobs(rt)` OUTSIDE `ctx.with(...)` per the rquickjs `RefCell` rule!).
- In `loop_liveness` and `run_event_loop`: include `cz_modules::tls_active_count()` and `cz_modules::tls_has_pending()` so scripts using `node:tls` stay alive while sockets or servers are open and exit cleanly with status `0` once closed.

### 1.5 TypeScript Declarations (`types/cardputer.d.ts`)
- Add `CzTlsConnectOptions`, `CzTlsServerOptions`, `CzTlsSocket`, `CzTlsServer`, `declare module "node:tls"`, and `declare module "tls"`.

---

## 2. Verification Requirements

1. **Crate purity**: `crates/cz-hal/Cargo.toml` stays `libc`-only; `crates/cz-net/Cargo.toml` has no `tokio`/`openssl`/`native-tls`/`aws-lc-rs`/`webpki-roots`.
2. **Rust tests**: `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 6` new unit tests for `tls_socket` and ALPN.
3. **Phase 8C offline E2E suite**: `node /refs/phase8c/verify_phase8c.mjs` passes all 7 scenarios.
4. **No regressions**: `check_integrity.sh`, `verify_phase8b.mjs`, `verify_phase8a.mjs`, and `check_e2e_p2.sh` all pass.
