# Cardputer Zero LLRT — standalone `cz-js` runtime (AWS-LLRT Free)

A small, bundler-free JavaScript runtime for **M5Stack CardputerZero** (ESP32-S3 / Cortex-A53 ARMv8.0,
512 MB RAM), built on rquickjs 0.14 (QuickJS) with Rust modules in `crates/`. `cz-js` is **standalone and
AWS-LLRT Free**: it embeds no fork of `awslabs/llrt`, and the whole I/O stack is **100% Tokio-Free** —
rustls + ring TLS on synchronous worker threads with a poll-based event loop. Scripts run directly as ESM —
no bundler, no npm install step — which is what makes ordinary Node-style HTTP servers work on-device.

`cz-js` is a **unified multicall binary**: the same executable serves `cz-js`, `node` (run scripts / `-e`),
`zpm`, and `npm` (native package pipeline via argv0 or `cz-js zpm <cmd>`).

Typical production use case: a plain `node:http`/`node:https` web server bound to `0.0.0.0` that is
reachable over Tailscale from a phone/laptop, serving static HTML through minimal `node:fs`.

## Workspace layout

| Crate | Role |
| --- | --- |
| `cz-gfx` | RGB565 canvas, 8x8 bitmap font, TTF rendering (swash/zeno) |
| `cz-hal` | Backlight/framebuffer/input sysfs + evdev, BQ27220 gauge, HTTP/1.1 parser, TCP server engine |
| `cz-json` | NEON-accelerated lazy JSON parse/stringify engine (`__czJsonParseBytes`) |
| `cz-modules` | JS module bindings: `cardputer:*`, `node:*`, Web Standard globals (`fetch`, `WebSocket`, …) |
| `cz-net` | Shared TLS foundation (`rustls` + `ring`, system roots via `rustls-native-certs`) and the sync-threaded HTTP/1.1 `fetch` engine on `ureq` v3 — zero Tokio |
| `cz-runner` | `cz-js` CLI: resolver, Node-compatible timers, event loop (top-level await pumps fetch safely), multicall dispatch |
| `cz-zpm` | Tokio-free native package pipeline on `ureq` v3 via `cz-net` (system-CA TLS, SRI-first staging, zip-slip/native-addon guards); multicall `zpm`/`npm` dispatch inside `cz-js` |

```bash
cargo build --release                 # target/release/cz-js
./target/release/cz-js server.mjs     # run a bundler-free HTTP(S) server
ln -s cz-js target/release/zpm         # multicall: zpm/npm via argv0; node <script|-e> still runs JS
./target/release/cz-js zpm install hono    # registry install (also: npm install …)
```

## API surface

### Web Standard / WinterTC globals
- **`fetch()`** — WHATWG surface: header-first resolution; `Response.status/statusText/ok/headers/url/
  redirected/type`, `text()/json()/bytes()/arrayBuffer()/clone()`, `Request`, `Headers`,
  `AbortController`/`AbortSignal` (+ `AbortSignal.timeout(ms)`), `ReadableStream` with `getReader()`,
  `read()`/`releaseLock()`/`cancel()` and `[Symbol.asyncIterator]`. Redirect modes `follow`/`manual`/
  `error`, gzip/chunked decoding, 16 MiB buffered-body cap (`BodyTooLarge`), streaming mode with prefix
  replay, immediate abort (~10 ms socket teardown). Per-request `init.tls = { ca, cert, key,
  rejectUnauthorized }` on real rustls/ring TLS 1.2/1.3 (system CA roots honoring `SSL_CERT_FILE`/
  `SSL_CERT_DIR` and `NODE_EXTRA_CA_CERTS`, Node-compatible self-signed-leaf-as-CA path, mTLS).
- **`WebSocket` / `wss://`** — RFC 6455 client with masked client frames, ping/pong, clean close handshake.
- **`URL` / `URLSearchParams`** (also `node:url`) — WHATWG parsing/serialization.
- **Web Crypto** (`globalThis.crypto`, `crypto.subtle`) backed exclusively by `ring 0.17`: digest
  (SHA-2/SHA-3), HMAC/AES-GCM, ECDH/RSA paths, `getRandomValues()`, `randomUUID()`.
- **`Blob` / `File` / `FormData`** — WinterTC blob/file shapes with multipart encode for `fetch`.
- **`EventTarget` / `Event` / `CustomEvent`**, `structuredClone` (circular-graph safe), `performance.now()`
  (monotonic `Instant`, never wall clock) + `timeOrigin`, `queueMicrotask`.
- **`Buffer`** (also `node:buffer`) — Node-compatible byte buffer surface.
- Timers: `setTimeout/clearTimeout/setInterval/clearInterval` (Node-compatible ids and extra args).

### Node compatibility (`node:*`, bare aliases included)
- **`node:http`** — `createServer(handler)`, `STATUS_CODES`, `METHODS`, `Server`/`IncomingMessage`/
  `ServerResponse` with Node semantics: `listen(port, host?, cb?)` (**default host `0.0.0.0`**),
  `listen(0)` ephemeral ports via `address()`, `on("request"|"listening"|"close"|"error")`,
  `req.method/url/httpVersion/headers/socket`, reliable late `"data"/"end"` delivery, `res.setHeader/
  getHeader/hasHeader/removeHeader/writeHead/write/end` with automatic `Content-Length`; HTTP/1.1
  keep-alive + pipelining, chunked framing, `Expect: 100-continue`, 1 MiB body cap → `413`, graceful
  `close()`.
- **`node:https`** — TLS server on rustls/ring (`createServer({ cert, key }, handler)`), ALPN-free HTTPS,
  hostile/abnormal-traffic resilient (abrupt RSTs and garbage bytes close only the bad socket).
- **`node:tls`** — `connect()` / `TLSSocket` client surface.
- **`node:fs` / `node:fs/promises`** — `readFileSync` (utf8 string / `Uint8Array`), `existsSync`,
  `statSync` → `{ size, isFile(), isDirectory() }`, plus promise variants.
- **`node:path`**, **`node:os`**, **`node:process`** (env, argv, exit, nextTick, hrtime), **`node:crypto`**
  (`createHash`, `createHmac`, hashing primitives on `ring`).

### Hardware (`cardputer:*`)
- `cardputer:display` — 320x170 RGB565 canvas (`WIDTH`, `HEIGHT`, `rgb565`, drawing primitives, `flush`,
  `getBuffer`, backlight `getBrightness/setBrightness/sleep/wake/isAsleep`, `loadFont`).
- `cardputer:input` — keyboard `poll()/isDown()/wasPressed()/wasReleased()/getMods()/getSymMap()`,
  `setRepeat(delayMs, intervalMs)`, `reset()`.
- `cardputer:app` — `info()`, `getConfig()`, `shouldQuit()`, `requestQuit()`, `installSignalHandlers()`.
- `cardputer:power` — BQ27220 battery `status()` (capacity, mV, mA, temperature, charging state).
- `cardputer:imu`, `cardputer:audio`, `cardputer:io`, `cardputer:ir` — sensor/audio/IO/IR bindings.
- `cardputer:json` — NEON lazy JSON engine; `cardputer:kv` — persistent key/value store.

## Zero-allocation engineering & `#[global_allocator]` audit

Every hot path follows **`docs/ZERO_ALLOC_GUIDELINES.md`**: single-buffer + byte-span parsing, borrowed
lookups, `Copy` state machines, zero-copy Rust↔QuickJS FFI (`vec_to_uint8array` ownership moves, direct
`as_slice()` borrows), under-lock visitor patterns, fixed stack buffers, `IoSlice` zero-body-copy TX, and
`OnceLock` cold-config caching. Compliance is proven deterministically with a custom
**`#[global_allocator]`** in `crates/cz-runner/tests/alloc_audit.rs`: hot-path tests assert `allocs == 0`
(or strict small upper bounds) and print `ALLOC_AUDIT_PASS <name> allocs=<N> bytes=<B>`. Allocation
assertions are never weakened — hidden allocations get fixed at the root instead.

The event loop (`crates/cz-runner/src/main.rs`) stays alive while any server listens or timer is pending,
with a ~5 ms idle pause (dispatch → drain QuickJS jobs → fire due timers), exits `0` when idle or on
SIGINT/SIGTERM. Lesson encoded there: runtime-locking APIs (`execute_pending_job`, `is_job_pending`) must
never be called inside an active `Context::with()` scope (rquickjs "RefCell already borrowed").

## `.deb` packaging for Cortex-A53 (stable Rust, AWS-LLRT Free)

```bash
bash scripts/build-cz-js-deb.sh 0.1.0       # builds dist/cardputer-zero-js_0.1.0_arm64.deb
CZ_JS_BIN=/path/to/cz-js bash scripts/build-cz-js-deb.sh   # package a prebuilt binary
```

The script cross-builds `cz-runner` with stable Rust using `RUSTFLAGS="-C target-cpu=cortex-a53"` for
`aarch64-unknown-linux-gnu`, runs a static ARMv8.0 instruction audit via `objdump -d` (**LSE atomics ≤
200, RCpc/dotprod instructions == 0** — safe on Cortex-A53), then packages:
- `/usr/local/bin/cz-js` (0755) with symlinks `/usr/local/bin/node`, `/usr/local/bin/zpm`,
  `/usr/local/bin/npm` → `cz-js`;
- `/usr/share/doc/cardputer-zero-js/types/cardputer.d.ts`.

CI: **`.github/workflows/cz-js-deb.yml`** (stable Rust only — zero legacy patching, zero yarn) runs the
full workspace test suite, the `alloc_audit` gate, the A53 cross-build + `.deb` packaging, and uploads the
artifact.

## Verification

```bash
cargo test --workspace          # full Rust unit/integration suite, all green under -D warnings (--test-threads=1 in gates)
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets   # zero-warning gate
RUSTFLAGS="-D warnings" cargo test -p cz-runner --test alloc_audit   # #[global_allocator] hot-path audit
bash /refs/verify/check_integrity.sh    # anti-cheat / Phase 1 file integrity
node /refs/phase14/verify_phase14.mjs   # Tokio-free multicall cz-js regression
node /refs/phase15/verify_phase15.mjs   # AWS-LLRT Free consolidation + load/RSS/FD-leak/hostile stress
npx --yes -p typescript tsc --noEmit --strict --target es2022 types/cardputer.d.ts  # real typecheck
```

Load/leak stress covered by Phase 15: 1,000 HTTPS keep-alive requests over 8 concurrent clients (FD delta
≤ 4, RSS growth ≤ 16 MiB), 16 concurrent in-runtime `fetch()` calls plus a 500-message `wss://` echo
stress, and hostile traffic resilience (20 abrupt TCP RSTs + 20 plaintext garbage connections, server
survives with FD count back to baseline).

`node` must be present on PATH for the E2E suites: the reference harnesses are plain Node scripts acting as
HTTP clients against the `cz-js` server processes.

## Docs

- `docs/SPEC.md` — Phase 1 specification; per-phase specs through Phase 15 live in `docs/`.
- **`docs/ZERO_ALLOC_GUIDELINES.md`** — zero-allocation engineering rules + mandatory audit.
- `docs/SPEC_PHASE15_AWS_LLRT_FREE_LOAD.md` — Phase 15 (AWS-LLRT Free consolidation, A53 `.deb` CI, load/leak stress).
- `types/cardputer.d.ts` — TypeScript declarations for all built-in modules plus the Web globals.
