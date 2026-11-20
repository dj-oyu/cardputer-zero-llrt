# Cardputer Zero LLRT — Phase 2.5 Technical Specification (HTTP/1.1 Streaming & SSE Push)

Target use case: Push real-time hardware telemetry (battery, key events, status updates) from CardputerZero to external browsers over Tailscale using standard **HTTP/1.1 Server-Sent Events (`EventSource`)** without blocking normal HTTP endpoints.

All Phase 1 (64+ tests, 9/9 E2E) and Phase 2 (111+ tests, 10/10 E2E) behaviors MUST continue to pass without regression.

---

## 1. Streaming Response API on `ServerResponse` (`node:http`)

### `res.flushHeaders(): void`
- Immediately serializes and transmits the HTTP/1.1 status line and response headers to the underlying TCP socket and flushes the socket.
- Sets `res.headersSent = true`.
- If `Content-Length` is NOT set, automatically adds `Transfer-Encoding: chunked` and switches the response into **streaming (chunked) mode**.
- Calling `flushHeaders()` more than once is a safe no-op.

### `res.write(chunk: string | Uint8Array): boolean`
- If `res.headersSent` is `false` AND the response `Content-Type` header contains `"text/event-stream"` (case-insensitive), automatically calls `flushHeaders()` so SSE streams begin immediately without requiring an explicit `flushHeaders()` call.
- When in **streaming mode**:
  - Immediately encodes non-empty `chunk` as an HTTP/1.1 chunk (`<hex-len>\r\n<bytes>\r\n`), writes it to the TCP socket, and flushes it so the client receives the data in real time BEFORE `res.end()` is called.
  - Returns `true` if queued/written, `false` if the underlying connection has already closed.
- When NOT in streaming mode (normal REST/HTML):
  - Keeps Phase 2 buffered behavior so `res.end()` computes `Content-Length` and supports HTTP/1.1 Keep-Alive seamlessly.

### `res.end(chunk?: string | Uint8Array, callback?: Function): ServerResponse`
- In streaming mode: writes `chunk` (if non-empty) as a chunked frame, then writes the terminating `0\r\n\r\n` chunk and flushes.
- Sets `res.writableEnded = true`. Calling `end()` more than once is a safe no-op.

---

## 2. Client Disconnect Detection (`"close"` Event)

- Both `req.on("close", listener)` and `res.on("close", listener)` MUST be supported.
- When a streaming connection's TCP socket fails to write (e.g. `BrokenPipe` / `ConnectionReset`) or the peer closes the socket:
  - The connection thread signals disconnect back to the JS event loop.
  - The JS event loop fires registered `"close"` listeners on `req` and `res` (at most once per request) so user code can `clearInterval(...)` and release resources cleanly.

---

## 3. Non-Blocking Concurrency Requirement

- While one or more SSE streaming responses are open and periodically pushing events via `setInterval`, the server MUST continue accepting and responding to normal HTTP requests (e.g. `GET /api/status`, `POST /api/...`) concurrently without blocking.
- When `server.close()` is called while SSE connections are open, the server must cleanly close active streaming channels and exit the event loop with code `0`.
