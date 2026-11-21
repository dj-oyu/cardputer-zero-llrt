# Cardputer Zero LLRT — Phase 11 Technical Specification (Global `URL` Class + RFC 6455 `WebSocket` Client)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, 512 MB RAM).
**Mandatory reading first**: `/workspace/docs/ZERO_ALLOC_GUIDELINES.md` — all new code MUST follow the zero-allocation guidelines and add a `#[global_allocator]` audit test to `crates/cz-runner/tests/alloc_audit.rs`.
All existing Phase 1–10 Final tests MUST continue to pass without regression.

---

## 1. Global `URL` Class

Install `globalThis.URL` (alongside `URLSearchParams` in `fetch_module.rs` or a dedicated `url_module.rs` installed by `cz-runner`, and also expose `"node:url"` / `"url"` exporting `{ URL, URLSearchParams }` + `default`).

### Required `URL` Surface
- `new URL(input: string | URL, base?: string | URL)`:
  - Supports `http:`, `https:`, `ws:`, `wss:`, `file:`, and custom schemes.
  - Relative URL resolution against `base` (e.g., `new URL("../b?x=1#h", "https://example.com:8443/a/c")`).
  - Throws `TypeError` on invalid URLs or invalid `base`.
  - Normalizes default ports (`http`/`ws` → `80` omitted from `.port`/`.host`; `https`/`wss` → `443` omitted).
  - Supports IPv6 host literals (`http://[::1]:8080/path`).
- Properties (readable & writable where standard):
  - `href`, `origin` (read-only), `protocol`, `username`, `password`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`
  - `searchParams`: live `URLSearchParams` instance — mutating `url.searchParams.set("k", "v")` updates `url.search` and `url.href`, and assigning `url.search = "?a=1"` updates `url.searchParams`!
- Methods & Statics:
  - `toString()` / `toJSON()` → `this.href`
  - `URL.canParse(input, base?)` → `boolean`
  - `URL.parse(input, base?)` → `URL | null`
- **`fetch()` integration**: `fetch(new URL("https://..."))` and `new Request(new URL(...))` MUST work seamlessly.

---

## 2. Global `WebSocket` Client (RFC 6455, `ws://` & `wss://`)

### 2.1 Engine (`crates/cz-net/src/ws.rs`, exported from `cz-net`)
- **Zero new crates**: Use `rustls 0.23` + `ring 0.17` (already in `Cargo.lock`) for:
  - `wss://` TLS via `cz_net::tls::build_client_config` (supports system CA `SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS` AND optional per-socket `tls: { ca, cert, key, rejectUnauthorized }`!)
  - RFC 6455 `Sec-WebSocket-Accept` verification using `ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, ...)` + standard base64 encoding
  - Client 4-byte masking keys using `ring::rand::SystemRandom`
- **Zero-allocation frame helpers (MUST be `pub` so `alloc_audit.rs` can test them!)**:
  - `pub fn encode_frame_header(out: &mut [u8; 14], fin: bool, opcode: u8, payload_len: usize, mask_key: Option<[u8; 4]>) -> usize`
    - Writes RFC 6455 frame header (2, 4, or 10 bytes + optional 4-byte mask key) into a **caller-owned 14-byte stack array** — **0 heap allocations**!
  - `pub fn apply_mask_in_place(buf: &mut [u8], mask_key: [u8; 4])` — **0 heap allocations**!
- **Worker thread & protocol compliance**:
  - Stack size `256 * 1024`, wakeup via `cz_hal::http_server::notify_event_loop()`.
  - Full HTTP/1.1 Upgrade handshake (`GET <path+query> HTTP/1.1`, `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Key`, `Sec-WebSocket-Version: 13`, optional `Sec-WebSocket-Protocol`). Validates `101` status and exact `Sec-WebSocket-Accept` hash!
  - Handles:
    - Text (`0x1`) and Binary (`0x2`) frames, including **fragmented messages** (`fin=0` + continuation `0x0` frames)
    - Extended payload lengths (`126` 16-bit and `127` 64-bit, tested up to >65 KiB)
    - **Automatic `Pong` (`0xA`) reply** echoing payload when server sends `Ping` (`0x9`)
    - Orderly `Close` (`0x8`) frame handshake (parses 2-byte big-endian close code + UTF-8 reason; replies with Close frame when peer initiates close)
    - Outbound client frames are always masked per RFC 6455, reusing a per-connection write buffer (zero `Vec::new()` per frame header).
  - Liveness: `pub fn ws_active_count() -> usize` and `pub fn ws_has_pending() -> bool`.

### 2.2 JS Global Surface (`crates/cz-modules/src/ws_module.rs` & `cz-runner`)
- Install `globalThis.WebSocket` from `cz-runner/src/main.rs` via `cz_modules::install_ws(&ctx)` and include `ws_active_count()` / `ws_has_pending()` + `__czWsDrainLoop` in `loop_pass` / `loop_liveness` / `run_event_loop`.
- Transfer inbound binary payloads via `crate::vec_to_uint8array(ctx, bytes)` (zero-copy `ArrayBuffer::new` move — NO `TypedArray::new_copy`!) and decode text frames via native `__czUtf8Decode` / `String::from_utf8`.
- `new WebSocket(url: string | URL, protocolsOrOptions?: string | string[] | { protocols?: string | string[], tls?: CzFetchTlsOptions })`:
  - Static constants & instance constants: `CONNECTING = 0`, `OPEN = 1`, `CLOSING = 2`, `CLOSED = 3`
  - Properties: `readyState`, `url`, `protocol`, `binaryType` (default `"arraybuffer"`, supports `"uint8array"` / `"arraybuffer"`), `bufferedAmount`
  - Methods: `send(data: string | Uint8Array | ArrayBuffer)`, `close(code?: number, reason?: string)`, `addEventListener(type, fn)`, `removeEventListener(type, fn)`
  - Handlers: `onopen`, `onmessage` (`{ data }`), `onerror` (`{ message }`), `onclose` (`{ code, reason, wasClean }`)

---

## 3. Allocation Audit & Types

1. **`crates/cz-runner/tests/alloc_audit.rs`**:
   - Add `alloc_audit_ws_frame_encode_zero`: runs `encode_frame_header` + `apply_mask_in_place` 1,000 times on stack buffers, asserts **`allocs == 0 && bytes == 0`**, and prints:
     `ALLOC_AUDIT_PASS ws_frame_encode_1000 allocs=0 bytes=0`
2. **`types/cardputer.d.ts`**:
   - Add `URL`, `declare module "node:url"`, `declare module "url"`, `WebSocket`, `MessageEvent`, `CloseEvent`, and update `fetch(input: string | URL | Request, ...)`.

---

## 4. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase11/verify_phase11.mjs` passes all checks (including `docs/ZERO_ALLOC_GUIDELINES.md` contract, `URL`, `ws://` & `wss://` WebSocket E2E, and `ws_frame_encode_1000 allocs=0`).
3. `node /refs/phase10/verify_phase10_final.mjs` (permanent `#[global_allocator]` regression gate!) passes.
4. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` and Phase 8A/8B/8C regressions pass.
