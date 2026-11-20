# Cardputer Zero LLRT — Phase 2 Technical Specification (Node.js-Compatible HTTP Server)

Target use case: Run **plain, bundler-free Node.js-style HTTP web servers** (`import http from "node:http"`) directly on M5Stack CardputerZero (512MB RAM) and access them externally over **Tailscale** (`0.0.0.0:<port>`).

Phase 1 modules (`cardputer:display`, `cardputer:input`, `cardputer:app`, `cardputer:power`) and all Phase 1 tests MUST continue to pass without regression.

---

## 1. New Built-in Modules & Runtime Capabilities

### 1.1 Module `"node:http"` (also resolvable as `"http"`)
Must implement `rquickjs::module::ModuleDef` with both named exports and a `default` export object.

#### Exported Constants
- `STATUS_CODES`: plain object mapping numeric status codes (as string keys) to standard reason phrases (at least: 200, 201, 204, 301, 302, 304, 400, 401, 403, 404, 405, 413, 500, 502, 503).
- `METHODS`: array `["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]`.

#### `createServer(requestListener?) -> Server`
Creates an HTTP/1.1 server instance.
- If `requestListener` is a function, registers it as the `"request"` listener.

#### `Server` object methods & events
- `listen(port, hostOrCallback?, callback?) -> Server`
  - `port`: `number` (`0` = OS-assigned ephemeral port).
  - Default `host`: **`"0.0.0.0"`** (so external Tailscale clients can connect without extra config).
  - Supports both `listen(port, cb)` and `listen(port, host, cb)` overloads.
  - Binds a TCP listener immediately so `server.address()` is valid right after `listen()` returns (or inside `cb`).
  - Invokes `cb` and emits `"listening"` once bound.
- `address() -> { address: string, family: "IPv4" | "IPv6", port: number } | null`
  - Returns `null` before `listen()` or after `close()`.
- `close(callback?) -> Server`
  - Stops accepting new connections, closes the listener, emits `"close"`, and invokes `callback` if provided.
  - Once all active servers are closed (and no pending timers remain), the `cz-js` event loop exits cleanly with status `0`.
- `on(event, listener) -> Server`
  - Supports `"request"`, `"listening"`, `"close"`, `"error"`.

#### `IncomingMessage` (`req`) object passed to `"request"` listener
- `req.method: string` (uppercase, e.g. `"GET"`, `"POST"`)
- `req.url: string` (raw request target including query string, e.g. `"/api/status?v=1"`)
- `req.httpVersion: string` (e.g. `"1.1"`)
- `req.headers: Record<string, string>` (all header names lowercased!)
- `req.socket: { remoteAddress: string, remotePort: number, localAddress: string, localPort: number }`
- `req.setEncoding(encoding: string) -> req` (e.g. `"utf8"` / `"utf-8"`)
- `req.on(event: "data" | "end" | "error", listener: Function) -> req`
  - When `"data"` or `"end"` listeners are registered (standard Node.js pattern inside `createServer((req, res) => { let body = ""; req.on("data", c => body += c); req.on("end", () => { ... }); })`), emit body chunks (as `string` when UTF-8 text or `setEncoding("utf8")`, or string/Uint8Array) and then emit `"end"`.
  - **Crucial**: Even for empty-body requests (like `GET`), registering `req.on("end", fn)` MUST still fire `fn()`! And registering `req.on("data", ...)` followed by `req.on("end", ...)` synchronously inside the request listener MUST reliably deliver the request body and then `"end"` (either via microtask/next-tick drain or immediate dispatch after the request listener returns).

#### `ServerResponse` (`res`) object passed to `"request"` listener
- `res.statusCode: number` (default `200`)
- `res.statusMessage: string` (optional override; defaults to `STATUS_CODES[statusCode] || "OK"`)
- `res.headersSent: boolean` (initially `false`, `true` after response is finalized/sent)
- `res.writableEnded: boolean` (initially `false`, `true` after `end()`)
- `res.setHeader(name: string, value: string | number) -> res` (case-insensitive header key handling)
- `res.getHeader(name: string) -> string | undefined` (case-insensitive)
- `res.hasHeader(name: string) -> boolean`
- `res.removeHeader(name: string) -> void`
- `res.writeHead(statusCode: number, statusMessageOrHeaders?: string | Record<string, string | number>, headers?: Record<string, string | number>) -> res`
- `res.write(chunk: string | Uint8Array) -> boolean` (appends to response body buffer)
- `res.end(chunk?: string | Uint8Array, callback?: Function) -> res`
  - Finalizes and transmits the HTTP/1.1 response over the TCP connection.
  - Automatically sets `Content-Length` if not already set by the user.
  - Sets `Connection: keep-alive` or `Connection: close` appropriately.
  - Calling `end()` more than once is a safe no-op.

### 1.2 HTTP/1.1 Protocol & Robustness Requirements (in `crates/cz-hal` or `crates/cz-http`)
- **HTTP/1.1 Keep-Alive**: A single TCP connection MUST be able to handle multiple sequential HTTP/1.1 requests until the client closes or sends `Connection: close`.
- **Request Body Parsing**:
  - Support `Content-Length` bodies.
  - Support `Transfer-Encoding: chunked` request bodies.
  - Enforce a default max body limit of **1 MiB** (`1_048_576` bytes) to protect 512MB CardputerZero from OOM; return `413 Payload Too Large` and close the connection if exceeded.
- **Malformed Request Resilience**: Invalid HTTP request lines or headers MUST receive `400 Bad Request` and close that connection without crashing or stalling the server.
- **Concurrent Connections**: Accept connections concurrently (e.g. non-blocking accept / background reader threads forwarding parsed requests via channel to the main JS event loop) so 8+ simultaneous HTTP clients are all served cleanly without dropping connections.

### 1.3 Minimal `"node:fs"` (and `"fs"`) Built-in Module
For serving static assets (HTML/CSS/JS) in bundler-free on-device web apps:
- `readFileSync(path: string, encoding?: string | { encoding?: string }): string | Uint8Array`
  - If encoding is `"utf8"` or `"utf-8"`, returns `string`; otherwise returns `Uint8Array`.
- `existsSync(path: string): boolean`
- `statSync(path: string): { size: number, isFile: () => boolean, isDirectory: () => boolean }`
- Both named exports and `default` export object.

### 1.4 Timers & Event Loop (`crates/cz-runner`)
- Provide `globalThis.setTimeout(fn, delayMs?) -> number`, `clearTimeout(id)`, `setInterval(fn, delayMs?) -> number`, `clearInterval(id)`.
- Event loop in `cz-js`:
  - After evaluating the entry ES module and draining microtasks:
  - While any HTTP server is listening OR any timer is active (and `!app.shouldQuit()`):
    - Poll incoming HTTP requests (with short sleep / channel recv_timeout ~5ms to keep CPU idle low on CardputerZero!)
    - Dispatch each request to the JS `"request"` listener, deliver `"data"`/`"end"` events, drain QuickJS pending jobs, and flush completed `ServerResponse` payloads back to the TCP socket.
    - Fire due timers and drain QuickJS pending jobs.
  - If `SIGINT` or `SIGTERM` is received (when signal handlers are installed or by default when a server is running), exit the event loop cleanly with status `0`.

### 1.5 TypeScript Declarations (`types/cardputer.d.ts` + `types/node-http.d.ts` or combined in `types/cardputer.d.ts`)
- Keep all Phase 1 declarations in `types/cardputer.d.ts`.
- Add declarations for `"node:http"` (`createServer`, `STATUS_CODES`, `METHODS`, `Server`, `IncomingMessage`, `ServerResponse`) and `"node:fs"` (`readFileSync`, `existsSync`, `statSync`) in `types/cardputer.d.ts`.
