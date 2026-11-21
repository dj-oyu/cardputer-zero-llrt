# Cardputer Zero LLRT (`cz-js`)

A small, bundler-free JavaScript runtime for **M5Stack CardputerZero** (ESP32-S3, 512 MB RAM), built on
rquickjs 0.14 (QuickJS) with Rust modules in `crates/`. Scripts run directly as ESM — no bundler, no npm
install step — which is what makes ordinary Node-style HTTP servers work on-device (Phase 2).

Typical production use case: a plain `node:http` web server bound to `0.0.0.0` that is reachable over
Tailscale from a phone/laptop, serving static HTML through minimal `node:fs`.

## Workspace layout

| Crate | Role |
| --- | --- |
| `cz-gfx` | RGB565 canvas, 8x8 bitmap font, TTF rendering (swash/zeno) |
| `cz-hal` | Backlight/framebuffer/input sysfs + evdev, BQ27220 gauge, HTTP/1.1 parser, TCP server engine |
| `cz-modules` | JS module bindings: `cardputer:display`, `cardputer:input`, `cardputer:app`, `cardputer:power`, `node:http`, `node:fs`, global `fetch()` (Phase 8A) |
| `cz-net` | Shared TLS foundation (`rustls` + `ring`, system roots via `rustls-native-certs`) and the sync-threaded HTTP/1.1 `fetch` engine on `ureq` v3 |
| `cz-runner` | `cz-js` CLI: resolver, Node-compatible timers, event loop (top-level await pumps fetch safely) |

```bash
cargo build --release                 # target/release/cz-js
./target/release/cz-js server.mjs     # run a bundler-free HTTP server
```

## Module surface

- `cardputer:display` — 320x170 RGB565 canvas (`WIDTH`, `HEIGHT`, `rgb565`, drawing primitives, `flush`,
  `getBuffer`, backlight `getBrightness/setBrightness/sleep/wake/isAsleep`, `loadFont`).
- `cardputer:input` — keyboard `poll()/isDown()/wasPressed()/wasReleased()/getMods()/getSymMap()`,
  `setRepeat(delayMs, intervalMs)`, `reset()`.
- `cardputer:app` — `info()`, `getConfig()`, `shouldQuit()`, `requestQuit()`, `installSignalHandlers()`.
- `cardputer:power` — BQ27220 battery `status()` (capacity, mV, mA, temperature, charging state).
- `node:http` (alias `http`) — `createServer(handler)`, `STATUS_CODES`, `METHODS`, and the `Server` /
  `IncomingMessage` / `ServerResponse` classes with Node-compatible semantics:
  - `listen(port, host?, cb?)` binds immediately; **default host is `0.0.0.0`** so Tailscale clients can
    connect; `listen(0)` requests an OS ephemeral port and `address().port` reports the real one.
  - `address()` returns `{ address, family, port }`, `null` before `listen()` and after `close()`.
  - `on("request" | "listening" | "close" | "error")`; `req.method/url/httpVersion/headers` (lowercase
    names) `/socket`, `setEncoding`, late-but-reliable `"data"`/`"end"` delivery (empty bodies still fire
    `"end"`), and `res.setHeader/getHeader/hasHeader/removeHeader/writeHead/write/end` with automatic
    `Content-Length` and safe repeated `end()`.
- `node:fs` (alias `fs`) — minimal static-asset surface: `readFileSync(path, encoding?)` (`string` for
  `"utf8"`/`"utf-8"`, otherwise `Uint8Array`, throws on missing paths), `existsSync(path)`,
  `statSync(path)` → `{ size, isFile(), isDirectory() }`.
- `fetch()` (global, Phase 8A) — WHATWG surface: `fetch(input, init)` resolves **header-first** once status
  and headers arrive; `Response.status/statusText/ok/headers/url/redirected/type`, `text()/json()/bytes()/
  arrayBuffer()/clone()`, `Request`, `Headers`, `AbortController`/`AbortSignal` (+ `AbortSignal.timeout(ms)`),
  `ReadableStream` with `getReader()`, `read()`/`releaseLock()`/`cancel()` and `[Symbol.asyncIterator]`,
  plus `URLSearchParams` when absent. Redirect modes `follow`/`manual`/`error`, gzip/chunked decoding, a
  16 MiB buffered-body cap (`BodyTooLarge`), streaming mode with prefix replay, and abort that tears the
  socket down immediately (~10 ms observed).
- Per-request TLS extension `init.tls` — `{ ca, cert, key, rejectUnauthorized }` (PEM string or bytes) on real
  rustls/ring TLS 1.2/1.3: system CA roots (honoring `SSL_CERT_FILE`/`SSL_CERT_DIR` and `NODE_EXTRA_CA_CERTS`)
  cached lazily, a Node-compatible self-signed-leaf-as-CA acceptance path, mTLS client auth, and verified
  rejection of untrusted/mismatched certificates.
- Globals — `setTimeout/clearTimeout/setInterval/clearInterval` (Node-compatible ids and extra args).

## HTTP robustness (Phase 2 hardening)

The parser (`cz-hal/src/http.rs`) is an incremental state machine; the engine (`cz-hal/src/http_server.rs`)
uses a poll-based accept loop plus one reader thread per connection:

- HTTP/1.1 **keep-alive** and pipelined requests on one socket; `Connection: close` honored.
- `Content-Length` bodies, `Transfer-Encoding: chunked` (strict framing), `Expect: 100-continue` (1xx).
- **1 MiB body cap** → `413 Payload Too Large` + connection close; oversized header sections → `400 Bad
  Request`; malformed request lines never crash or stall the server (only that socket dies).
- Concurrent clients (8+ verified), unanswered requests get a prompt `500` so clients never hang, graceful
  `close()` finishes in-flight requests then emits `"close"`.
- Event loop (`crates/cz-runner/src/main.rs`) stays alive while any server listens or any timer is pending,
  with a ~5 ms idle pause, dispatching requests → draining QuickJS jobs → firing due timers, and exits with
  status `0` when idle or on SIGINT/SIGTERM. A hard lesson encoded there: Runtime APIs that lock the
  runtime (`execute_pending_job`, `is_job_pending`) must never be called inside an active `Context::with()`
  scope — rquickjs panics "RefCell already borrowed" otherwise.

## Verification

```bash
cargo test --workspace          # 262 Rust unit/integration tests (13 gfx + 126 hal + 29 json + 48 modules + 2 net + 24 net-engine + 10 runner + 28 zpm)
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets   # zero-warning gate
bash /refs/verify/check_integrity.sh    # anti-cheat / Phase 1 file integrity
bash /refs/verify/check_rust_p2.sh      # >=75 tests, -D warnings
bash /refs/verify/check_e2e_p2.sh       # Phase 1 E2E regression + 10 reference Node-style web-server cases
node /refs/phase8a/verify_phase8a.mjs    # Phase 8A fetch/TLS E2E (HTTPS, SSE streaming, abort, redirect, types)
bash /refs/verify/check_types_p2.sh     # all six module declarations present & balanced
npx --yes -p typescript tsc --noEmit --strict --target es2022 types/cardputer.d.ts  # real typecheck
```

`node` must be present on PATH for the E2E suites: the reference harnesses are plain Node scripts acting as
HTTP clients against the `cz-js` server processes.

## Docs

- `docs/SPEC.md` — Phase 1 specification.
- `docs/SPEC_PHASE2.md` — Phase 2 (`node:http` / `node:fs` / event loop & timers) specification.
- `types/cardputer.d.ts` — TypeScript declarations for all built-in modules plus the timer globals.
