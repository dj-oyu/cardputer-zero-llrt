//! `node:http` (also resolvable as `"http"`) — Node.js-compatible HTTP server module for
//! bundler-free ESM scripts on CardputerZero, plus the Rust-side registry that the `cz-js` event
//! loop drives.
//!
//! Design constraints (verified against rquickjs 0.14 before writing this):
//! - Native callbacks must take `ctx: Ctx<'js>` as their FIRST parameter instead of capturing a
//!   outer-context `Ctx` (lifetime rule), and must be `'static` (they only touch process-wide
//!   statics below).
//! - Every export name must be declared in `ModuleDef::declare` first; exporting an undeclared
//!   name fails with "Tried to export a value which was not previously declared".
//! - JavaScript objects are never stored across `Context::with()` scopes in Rust: each request is
//!   described by a plain descriptor object built fresh, and responses travel back to the engine
//!   as bytes through per-connection reply channels.
//!
//! Default bind host is `0.0.0.0` so external Tailscale peers can reach the server without extra
//! configuration, and `listen(0)` resolves a real ephemeral port readable via `server.address()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rquickjs::{
    convert::Coerced, Array, Ctx, Exception, FromJs, Function, Object, Result, String as JsString,
    TypedArray, Value,
};

use cz_hal::http::STATUS_CODES;
use cz_hal::http_server::{
    client_closed, take_disconnect_events, ConnectionEvent, Listener, ReplyFrame, ResponseAction,
};

/// Node.js-compatible HTTP methods exposed by the module.
pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

// ---------------------------------------------------------------------------
// Process-wide server registry
// ---------------------------------------------------------------------------

static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static TAKEN_EVENTS: AtomicUsize = AtomicUsize::new(0);

struct Registry {
    /// Bound listeners keyed by JS-visible server id.
    servers: Mutex<HashMap<u64, Listener>>,
    /// Queue side: the receiver is consumed lazily by the event loop / `__czNextRequest`.
    rx: Mutex<Option<Receiver<ConnectionEvent>>>,
    /// Sender clone shared by all listener threads.
    tx: Sender<ConnectionEvent>,
    /// Per-connection reply channels awaiting the JS response. Phase 2.5: the channel may carry
    /// a *sequence* (`StreamHead -> StreamChunk* -> StreamEnd`) instead of one buffered frame.
    replies: Mutex<HashMap<u64, Sender<ResponseAction>>>,
    /// Connections currently in streaming mode, keyed by conn id -> owning server id. Used so
    /// `server.close()` can terminate live SSE channels (spec §3) instead of leaving them open.
    streams: Mutex<HashMap<u64, u64>>,
}

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| {
        let (tx, rx) = channel::<ConnectionEvent>();
        Registry {
            servers: Mutex::new(HashMap::new()),
            rx: Mutex::new(Some(rx)),
            tx,
            replies: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
        }
    })
}

/// Number of currently listening servers (event-loop liveness condition).
pub fn listening_count() -> usize {
    registry().servers.lock().expect("servers lock").len()
}

/// True when engine-queued requests have not been pulled toward JavaScript yet. Counted before
/// `send()` so it can only over-report, never under-report (a stray iteration is cheap).
pub fn has_pending_requests() -> bool {
    cz_hal::http_server::events_queued() > TAKEN_EVENTS.load(Ordering::SeqCst)
}

/// Idle pause used by the event loop while servers are listening but nothing has arrived (~5ms
/// keeps CPU usage low on CardputerZero, per spec).
pub const IDLE_SLEEP: Duration = std::time::Duration::from_millis(5);

/// Pull one queued request (non-blocking). Requests belonging to closed servers are dropped
/// (their reply sender is discarded, which wakes that connection thread immediately).
pub fn pop_request() -> Option<ConnectionEvent> {
    let reg = registry();
    let rx_guard = reg.rx.lock().expect("rx lock");
    let rx = rx_guard.as_ref()?;
    match rx.try_recv() {
        Ok(ev) => {
            TAKEN_EVENTS.fetch_add(1, Ordering::SeqCst);
            Some(ev)
        }
        Err(_) => None,
    }
}

/// Pull the next request belonging to a still-listening server. Events whose server was already
/// closed are dropped here: their reply channel is discarded, which wakes that connection thread
/// immediately (`Disconnected` -> engine `500`) instead of letting a stale request leak into a
/// different context's dispatch table.
pub fn next_active_request() -> Option<ConnectionEvent> {
    loop {
        let ev = pop_request()?;
        if registry().servers.lock().expect("servers lock").contains_key(&ev.server_id) {
            return Some(ev);
        }
        // Closed server: drop the reply sender so that socket closes promptly.
    }
}

/// Clone the reply channel for `conn_id` WITHOUT removing it (streaming keeps it alive).
fn peek_sender(conn_id: u64) -> Option<Sender<ResponseAction>> {
    registry().replies.lock().expect("replies lock").get(&conn_id).cloned()
}

/// Hand the JS buffered response back to the owning connection thread (Phase 2 path).
pub fn deliver_response(conn_id: u64, frame: ReplyFrame) -> bool {
    let reg = registry();
    let tx = match reg.replies.lock().expect("replies lock").remove(&conn_id) {
        Some(t) => t,
        None => return false,
    };
    reg.streams.lock().expect("streams lock").remove(&conn_id);
    tx.send(ResponseAction::Buffered(frame)).is_ok()
}

/// Begin a chunked stream on this connection: flush headers NOW, keep the reply channel open.
fn begin_stream(conn_id: u64, server_id: u64, action_head: ResponseAction) -> bool {
    if client_closed(conn_id) {
        return false;
    }
    let reg = registry();
    let tx = match reg.replies.lock().expect("replies lock").get(&conn_id) {
        Some(t) => t.clone(),
        None => return false,
    };
    if tx.send(action_head).is_err() {
        return false;
    }
    reg.streams.lock().expect("streams lock").insert(conn_id, server_id);
    true
}

/// Push one chunk onto a live stream (immediate write + flush happens on the conn thread).
fn push_stream_chunk(conn_id: u64, data: Vec<u8>) -> bool {
    if client_closed(conn_id) || data.is_empty() {
        // Empty chunks are specified as no-ops that still report success while alive.
        return !client_closed(conn_id);
    }
    match peek_sender(conn_id) {
        Some(tx) => tx.send(ResponseAction::StreamChunk(data)).is_ok(),
        None => false,
    }
}

/// Finalize a stream: optional last chunk + terminating `0\r\n\r\n`, then release the channel.
fn end_stream(conn_id: u64, last: Option<Vec<u8>>, keep_alive: bool) -> bool {
    let reg = registry();
    let tx = match reg.replies.lock().expect("replies lock").remove(&conn_id) {
        Some(t) => t,
        None => return !client_closed(conn_id),
    };
    reg.streams.lock().expect("streams lock").remove(&conn_id);
    let mut ok = true;
    if let Some(bytes) = last {
        if !bytes.is_empty() && tx.send(ResponseAction::StreamChunk(bytes)).is_err() {
            ok = false;
        }
    }
    ok && tx.send(ResponseAction::StreamEnd { keep_alive }).is_ok()
}

/// Stop one server: stops accepting, releases the port, and cleanly terminates any live
/// streaming channels owned by it (spec \u00a73): send the terminator + report disconnect so JS
/// event-loop `"close"` listeners can clear intervals.
pub fn close_server(id: u64) -> bool {
    let reg = registry();
    let live: Vec<u64> = reg
        .streams
        .lock()
        .expect("streams lock")
        .iter()
        .filter(|(_, srv)| **srv == id)
        .map(|(conn, _)| *conn)
        .collect();
    for conn in live {
        if let Some(tx) = reg.replies.lock().expect("replies lock").remove(&conn) {
            // Force the terminating chunk; the reader thread then shuts that socket down.
            tx.send(ResponseAction::StreamEnd { keep_alive: false }).ok();
        }
        cz_hal::http_server::mark_client_closed_public(conn);
    }
    match reg.servers.lock().expect("servers lock").remove(&id) {
        Some(mut l) => cz_hal::http_server::stop_listener(&mut l),
        None => false,
    }
}

/// Close every listening server (used by shutdown paths and tests).
pub fn close_all_servers() {
    let reg = registry();
    let ids: Vec<u64> = reg.servers.lock().expect("servers lock").keys().copied().collect();
    for id in ids {
        close_server(id);
    }
}

/// Pull pending client-disconnect notifications (conn ids) for the JS event loop to fire
/// `"close"` listeners on `req`/`res`.
pub fn drain_disconnects() -> Vec<u64> {
    take_disconnect_events()
}

// ---------------------------------------------------------------------------
// JS glue
// ---------------------------------------------------------------------------

const HTTP_SHIM: &str = r#"
(function () {
    const servers = new Map();
    globalThis.__czHttpServers = servers;

    function czReportError(e) {
        const msg = (e && e.stack) ? e.stack : String((e && e.message) || e);
        if (globalThis.console && console.error) console.error(msg);
        else if (globalThis.__cz_print) __cz_print(2, [msg]);
    }

    function norm(name) { return String(name).toLowerCase(); }

    // ---- Live per-connection links (Phase 2.5 "close" events) ------------
    // connId -> { req, res, fired }. The event loop drains engine disconnect notifications
    // (__czDrainDisconnects) and fires "close" listeners at most once per request.
    const links = new Map();

    function czFireClose(link) {
        if (!link || link.fired) return false;
        link.fired = true;
        link.req._closed = true;
        link.res._closed = true;
        links.delete(link.connId);
        for (const fn of (link.req._lis.close || []).slice()) {
            try { fn(); } catch (e) { czReportError(e); }
        }
        for (const fn of (link.res._lis.close || []).slice()) {
            try { fn(); } catch (e) { czReportError(e); }
        }
        return true;
    }

    // Called by the Rust event loop every pass; returns how many close events fired.
    globalThis.__czHttpCheckCloses = function () {
        if (typeof __czDrainDisconnects !== "function") return 0;
        const ids = __czDrainDisconnects();
        let fired = 0;
        for (const id of ids) {
            const link = links.get(id);
            if (link) { czFireClose(link); fired++; }
        }
        return fired;
    };

    // ---- IncomingMessage --------------------------------------------------
    class CzIncomingMessage {
        constructor(desc) {
            this.method = desc.method;
            this.url = desc.url;
            this.httpVersion = desc.httpVersion;
            this.headers = desc.headers;
            this.socket = desc.socket;
            this.remoteAddress = desc.socket.remoteAddress;
            this.connection = desc.socket;
            this._enc = "utf8";
            this._bodyBytes = desc.bodyBytes;
            this._bodyText = desc.bodyText;
            this._connId = desc.connId;
            this._closed = false;
            this._lis = { data: [], end: [], error: [], close: [] };
            this._delivered = false;
        }
        setEncoding(enc) { this._enc = String(enc); return this; }
        on(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const key = String(ev);
            if (!this._lis[key]) this._lis[key] = [];
            this._lis[key].push(fn);
            // Late registration replays what already happened so async handlers still work.
            if (key === "data" && this._delivered) {
                const c = czChunk(this);
                if (c !== null) { try { fn(c); } catch (e) { czReportError(e); } }
            } else if (key === "end" && this._delivered) {
                try { fn(); } catch (e) { czReportError(e); }
            }
            return this;
        }
    }

    function czChunk(req) {
        if (!req._bodyBytes || req._bodyBytes.length === 0) return null;
        const enc = String(req._enc).toLowerCase();
        if (enc === "utf8" || enc === "utf-8") {
            return req._bodyText !== null ? req._bodyText : req._bodyBytes;
        }
        // Text bodies are delivered as strings, binary stays a Uint8Array.
        return req._bodyText !== null ? req._bodyText : req._bodyBytes;
    }

    function czDeliver(req) {
        if (req._delivered) return;
        req._delivered = true;
        const chunk = czChunk(req);
        if (chunk !== null) {
            for (const fn of req._lis.data.slice()) { try { fn(chunk); } catch (e) { czReportError(e); } }
        }
        for (const fn of req._lis.end.slice()) { try { fn(); } catch (e) { czReportError(e); } }
    }

    // ---- ServerResponse ---------------------------------------------------
    class CzServerResponse {
        constructor(desc) {
            this.statusCode = 200;
            this.statusMessage = undefined;
            this.headersSent = false;
            this.writableEnded = false;
            this.socket = desc.socket;
            this._connId = desc.connId;
            this._serverId = desc.serverId;
            this._keepAlive = desc.keepAlive === true;
            this._headers = new Map();
            this._parts = [];
            // Phase 2.5: set once flushHeaders()/SSE auto-detect enters chunked mode.
            this._streaming = false;
            this._closed = false;
            this._lis = { close: [] };
        }
        on(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const key = String(ev);
            if (!this._lis[key]) this._lis[key] = [];
            this._lis[key].push(fn);
            return this;
        }
        _emitPairs() {
            const pairs = [];
            for (const entry of this._headers.values()) pairs.push([entry.name, entry.value]);
            return pairs;
        }
        flushHeaders() {
            // Repeated flushHeaders() calls are a safe no-op (spec §1).
            if (this.headersSent) return;
            if (this.hasHeader("content-length")) {
                // Buffered (Content-Length) responses stay Phase 2: the engine serializes head +
                // body together at end(); nothing hits the wire until then.
                this.headersSent = true;
                return;
            }
            const ok = __czStreamStart(this._connId, this._serverId === undefined ? 0 : this._serverId,
                this.statusCode, this.statusMessage === undefined ? null : this.statusMessage,
                this._emitPairs(), this._keepAlive);
            this._streaming = true;
            this.headersSent = true;
            if (ok === false) this._closed = true;
        }
        setHeader(name, value) {
            if (value === undefined || value === null) this._headers.delete(norm(name));
            else this._headers.set(norm(name), { name: String(name), value: String(value) });
            return this;
        }
        getHeader(name) {
            const e = this._headers.get(norm(name));
            return e === undefined ? undefined : e.value;
        }
        hasHeader(name) { return this._headers.has(norm(name)); }
        removeHeader(name) { this._headers.delete(norm(name)); }
        _addPairs(obj) {
            for (const k of Object.keys(obj)) this.setHeader(k, obj[k]);
        }
        writeHead(code, a, b) {
            const c = Number(code);
            this.statusCode = Number.isFinite(c) ? c : 200;
            if (typeof a === "string") {
                this.statusMessage = a;
                if (b && typeof b === "object") this._addPairs(b);
            } else if (a && typeof a === "object") {
                this._addPairs(a);
            }
            return this;
        }
        write(chunk) {
            if (this.writableEnded) throw new Error("write after end");
            if (this._closed) return false;
            // SSE streams start immediately without an explicit flushHeaders(): any Content-Type
            // containing text/event-stream auto-enters streaming mode (spec §1).
            if (!this.headersSent) {
                const ct = String(this.getHeader("content-type") || "").toLowerCase();
                if (ct.indexOf("text/event-stream") !== -1) this.flushHeaders();
            }
            if (this._streaming) {
                if (chunk === undefined || chunk === null) return !this._closed;
                const ok = __czStreamChunk(this._connId, chunk);
                if (ok === false) this._closed = true;
                return ok === true && !this._closed;
            }
            if (chunk !== undefined && chunk !== null) this._parts.push(chunk);
            return true;
        }
        end(chunk, cb) {
            if (typeof chunk === "function") { cb = chunk; chunk = undefined; }
            if (this.writableEnded) return this; // repeated end() is a safe no-op
            if (this._streaming) {
                // Streaming finalize: last chunk (if non-empty) + terminating 0\r\n\r\n frame.
                const last = chunk === undefined || chunk === null ? null : chunk;
                const ok = __czStreamEnd(this._connId, last, this._keepAlive);
                this.headersSent = true;
                this.writableEnded = true;
                if (ok === false) this._closed = true;
                links.delete(this._connId); // normal finish: no disconnect event owed
                if (typeof cb === "function") { try { cb(); } catch (e) { czReportError(e); } }
                return this;
            }
            if (chunk !== undefined && chunk !== null) this._parts.push(chunk);
            this.headersSent = true;
            this.writableEnded = true;
            __czRespond(this._connId, this.statusCode,
                        this.statusMessage === undefined ? null : this.statusMessage,
                        this._emitPairs(), this._parts, this._keepAlive);
            links.delete(this._connId);
            if (typeof cb === "function") { try { cb(); } catch (e) { czReportError(e); } }
            return this;
        }
    }

    // ---- Server -----------------------------------------------------------
    class CzServer {
        constructor(listener) {
            this._ev = { request: [], listening: [], close: [], error: [] };
            this._id = null;
            this._addr = null;
            if (typeof listener === "function") this.on("request", listener);
        }
        on(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const key = String(ev);
            if (!this._ev[key]) this._ev[key] = [];
            this._ev[key].push(fn);
            return this;
        }
        emit(ev, a, b) {
            for (const fn of (this._ev[String(ev)] || []).slice()) {
                try { fn(a, b); } catch (e) { czReportError(e); }
            }
        }
        listen(port, a, b) {
            let host = "0.0.0.0";   // default: externally reachable over Tailscale
            let cb = null;
            if (typeof a === "function") cb = a;
            else if (typeof a === "string") { host = a; if (typeof b === "function") cb = b; }
            const p = Number(port) || 0;
            let info;
            try {
                info = __czListen(p, host);
            } catch (e) {
                this.emit("error", e);
                czReportError(e);
                return this;
            }
            this._id = info.id;
            servers.set(info.id, this);
            this._addr = { address: info.address, family: info.family, port: info.port };
            if (cb) { try { cb.call(this); } catch (e) { czReportError(e); } }
            this.emit("listening");
            return this;
        }
        address() { return this._addr; }
        close(cb) {
            if (cb !== undefined && typeof cb !== "function") throw new TypeError("callback must be a function");
            if (this._id !== null) {
                servers.delete(this._id);
                __czClose(this._id);
                this._id = null;
            }
            this._addr = null;
            this.emit("close");
            if (cb) { try { cb(); } catch (e) { czReportError(e); } }
            return this;
        }
    }

    function createServer(listener) { return new CzServer(listener); }

    // ---- Dispatch ---------------------------------------------------------
    function czDispatch(desc) {
        const server = servers.get(desc.serverId);
        const req = new CzIncomingMessage(desc);
        const res = new CzServerResponse(desc);
        // Track this connection so the event loop can fire "close" on disconnect (at most once).
        links.set(desc.connId, { connId: desc.connId, req: req, res: res, fired: false });
        const listeners = server ? (server._ev.request || []).slice() : [];
        if (listeners.length === 0) {
            // Nothing handles requests: answer instead of leaving the client hanging forever.
            res.statusCode = 404;
            res.setHeader("Content-Type", "application/json");
            res.end(JSON.stringify({ error: "no request listener" }));
            return true;
        }
        let threw = null;
        for (const fn of listeners) {
            try { fn(req, res); } catch (e) { threw = e; break; }
        }
        if (threw !== null) {
            czReportError(threw);
            if (!res.writableEnded) {
                try {
                    res.statusCode = 500;
                    res.setHeader("Content-Type", "text/plain");
                    res.end("Internal Server Error");
                } catch (_e2) { /* socket already gone */ }
            }
        }
        Promise.resolve().then(() => czDeliver(req));
        return true;
    }

    globalThis.__czHttpDispatch = czDispatch;
    globalThis.__czHttpDrainLoop = function () {
        let n = 0;
        for (;;) {
            const d = __czNextRequest();
            if (!d) break;
            n++;
            czDispatch(d);
        }
        return n;
    };

    globalThis.__czCreateServer = createServer;
    globalThis.__czHttpClassServer = CzServer;
    globalThis.__czHttpClassIncoming = CzIncomingMessage;
    globalThis.__czHttpClassResponse = CzServerResponse;
})();
"#;

/// Register the Rust primitives + JS shim on this context (idempotent). The named `'js` lifetime
/// is required so native closures can both capture and receive context values (rquickjs rule).
fn install_glue<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czListen",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, port: f64, host: String| -> Result<Object<'js>> {
            let id = NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst);
            let p = if port.is_finite() && port >= 0.0 && port <= 65535.0 { port as u16 } else { 0 };
            let listener = match cz_hal::http_server::spawn_listener(&host, p, id, registry().tx.clone()) {
                Ok(l) => l,
                Err(e) => {
                    return Err(Exception::throw_message(&ctx, &format!("listen {host}:{p} failed: {e}")));
                }
            };
            // Snapshot the bound address BEFORE moving the listener into the registry map.
            let address = listener.address.clone();
            let bound_port = listener.port;
            registry().servers.lock().expect("servers lock").insert(id, listener);
            let obj = Object::new(ctx.clone())?;
            obj.set("id", id as f64)?;
            obj.set("address", address.as_str())?;
            obj.set("family", "IPv4")?;
            obj.set("port", bound_port as f64)?;
            Ok(obj)
        })?,
    )?;

    ctx.globals().set(
        "__czClose",
        Function::new(ctx.clone(), |id: f64| -> bool { close_server(id as u64) })?,
    )?;

    ctx.globals().set(
        "__czListeningCount",
        Function::new(ctx.clone(), || -> usize { listening_count() })?,
    )?;

    ctx.globals().set(
        "__czRespond",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, conn_id: f64, status: f64, reason: Value<'js>, pairs: Array<'js>, parts: Array<'js>, keep_alive: bool| -> Result<bool> {
                let headers = collect_header_pairs(&pairs)?;
                let mut body: Vec<u8> = Vec::new();
                for v in parts.iter::<Value>() {
                    push_value_bytes(&ctx, v?, &mut body)?;
                }
                let reason_txt = value_to_opt_string(&ctx, reason)?;
                let code = if status.is_finite() { status as u16 } else { 200 };
                let frame = ReplyFrame {
                    status: code,
                    reason: reason_txt,
                    headers,
                    body,
                    keep_alive,
                };
                Ok(deliver_response(conn_id as u64, frame))
            },
        )?,
    )?;

    // ---- Phase 2.5 streaming natives -------------------------------------------------
    ctx.globals().set(
        "__czStreamStart",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, conn_id: f64, server_id: f64, status: f64, reason: Value<'js>, pairs: Array<'js>, keep_alive: bool| -> Result<bool> {
                let headers = collect_header_pairs(&pairs)?;
                let reason_txt = value_to_opt_string(&ctx, reason)?;
                let code = if status.is_finite() { status as u16 } else { 200 };
                Ok(begin_stream(
                    conn_id as u64,
                    server_id as u64,
                    ResponseAction::StreamHead {
                        status: code,
                        reason: reason_txt,
                        headers,
                        keep_alive,
                    },
                ))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czStreamChunk",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, conn_id: f64, chunk: Value<'js>| -> Result<bool> {
                let mut bytes: Vec<u8> = Vec::new();
                push_value_bytes(&ctx, chunk, &mut bytes)?;
                Ok(push_stream_chunk(conn_id as u64, bytes))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czStreamEnd",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, conn_id: f64, chunk: Value<'js>, keep_alive: bool| -> Result<bool> {
                let last = if chunk.is_null() || chunk.is_undefined() {
                    None
                } else {
                    let mut bytes: Vec<u8> = Vec::new();
                    push_value_bytes(&ctx, chunk, &mut bytes)?;
                    Some(bytes)
                };
                Ok(end_stream(conn_id as u64, last, keep_alive))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czConnAlive",
        Function::new(ctx.clone(), |conn_id: f64| -> bool { !client_closed(conn_id as u64) })?,
    )?;

    ctx.globals().set(
        "__czDrainDisconnects",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Array<'js>> {
            let arr = Array::new(ctx.clone())?;
            for (i, id) in drain_disconnects().iter().enumerate() {
                arr.set(i, *id as f64)?;
            }
            Ok(arr)
        })?,
    )?;

    ctx.globals().set(
        "__czNextRequest",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Value<'js>> {
            match next_active_request() {
                None => Ok(Value::new_null(ctx.clone())),
                Some(ev) => build_descriptor(&ctx, ev),
            }
        })?,
    )?;

    ctx.eval::<(), _>(HTTP_SHIM)?;
    Ok(())
}

/// Convert `[name, value]` pairs from JS into header tuples (emission order preserved).
fn collect_header_pairs(pairs: &Array<'_>) -> Result<Vec<(String, String)>> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for v in pairs.iter::<Value>() {
        let pair = v?;
        if let Some(arr) = pair.as_array() {
            let name: String = arr.get::<JsString>(0)?.to_string()?;
            let value: String = arr.get::<JsString>(1)?.to_string()?;
            headers.push((name, value));
        }
    }
    Ok(headers)
}

/// Append one JS body part (string / Uint8Array / ArrayBuffer) as bytes. No JS execution runs
/// while the typed-array slice is borrowed.
fn push_value_bytes<'js>(ctx: &Ctx<'js>, v: Value<'js>, out: &mut Vec<u8>) -> Result<()> {
    if let Some(s) = v.as_string() {
        out.extend_from_slice(s.to_string()?.as_bytes());
    } else if let Ok(ta) = TypedArray::<u8>::from_value(v.clone()) {
        // SAFETY: no JS execution happens while this slice is borrowed.
        out.extend_from_slice(unsafe { ta.as_slice() });
    } else if let Some(ab) = rquickjs::ArrayBuffer::from_value(v.clone()) {
        if let Some(b) = unsafe { ab.as_bytes() } {
            out.extend_from_slice(b);
        }
    } else if !v.is_null() && !v.is_undefined() {
        // Coerce anything else exactly like Node's `res.write` stringification.
        let s = Coerced::<String>::from_js(ctx, v)?;
        out.extend_from_slice(s.0.as_bytes());
    }
    Ok(())
}

/// Optional status message: null/undefined -> None, otherwise string-coerced.
fn value_to_opt_string<'js>(ctx: &Ctx<'js>, reason: Value<'js>) -> Result<Option<String>> {
    if reason.is_null() || reason.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(Coerced::<String>::from_js(ctx, reason)?.0))
    }
}

/// Build a fresh plain-object request descriptor (never stored across contexts in Rust).
fn build_descriptor<'js>(ctx: &Ctx<'js>, ev: ConnectionEvent) -> Result<Value<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("serverId", ev.server_id as f64)?;
    obj.set("connId", ev.conn_id as f64)?;
    obj.set("method", ev.request.head.method.as_str())?;
    obj.set("url", ev.request.head.target.as_str())?;
    obj.set("httpVersion", ev.request.head.version.as_str())?;

    // Header names are lowercased; duplicates joined with ", " exactly like Node.js.
    let headers = Object::new(ctx.clone())?;
    let mut joined: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in &ev.request.head.headers {
        joined.entry(name.clone()).or_default().push(value.clone());
    }
    for (name, values) in joined {
        headers.set(name.as_str(), values.join(", ").as_str())?;
    }
    obj.set("headers", headers)?;

    let socket = Object::new(ctx.clone())?;
    socket.set("remoteAddress", ev.remote_address.as_str())?;
    socket.set("remotePort", ev.remote_port as f64)?;
    socket.set("localAddress", ev.local_address.as_str())?;
    socket.set("localPort", ev.local_port as f64)?;
    obj.set("socket", socket)?;

    obj.set("keepAlive", ev.keep_alive)?;

    // Body: always a Uint8Array copy, plus a string form when the payload is valid UTF-8.
    let bytes = TypedArray::<u8>::new_copy(ctx.clone(), &ev.request.body[..])?;
    obj.set("bodyBytes", bytes)?;
    match std::str::from_utf8(&ev.request.body) {
        Ok(text) => obj.set("bodyText", text)?,
        Err(_) => obj.set("bodyText", Value::new_null(ctx.clone()))?,
    }

    // Remember this connection's reply channel so res.end() can flush onto the socket.
    registry().replies.lock().expect("replies lock").insert(ev.conn_id, ev.reply);
    Ok(obj.into_value())
}

/// `node:http` module definition: named exports AND a `default` export object.
pub struct HttpModule;

impl rquickjs::module::ModuleDef for HttpModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in [
            "createServer",
            "STATUS_CODES",
            "METHODS",
            "Server",
            "IncomingMessage",
            "ServerResponse",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_glue(ctx)?;

        let create_fn = ctx.globals().get::<_, Function>("__czCreateServer")?;
        exports.export("createServer", create_fn.clone())?;

        let codes = Object::new(ctx.clone())?;
        for (code, phrase) in STATUS_CODES {
            codes.set(code.to_string(), *phrase)?;
        }
        exports.export("STATUS_CODES", codes.clone())?;

        let methods = Array::new(ctx.clone())?;
        for (i, m) in METHODS.iter().enumerate() {
            methods.set(i, *m)?;
        }
        exports.export("METHODS", methods.clone())?;

        let server_cls = ctx.globals().get::<_, Function>("__czHttpClassServer")?;
        exports.export("Server", server_cls.clone())?;
        let incoming_cls = ctx.globals().get::<_, Function>("__czHttpClassIncoming")?;
        exports.export("IncomingMessage", incoming_cls.clone())?;
        let response_cls = ctx.globals().get::<_, Function>("__czHttpClassResponse")?;
        exports.export("ServerResponse", response_cls.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("createServer", create_fn)?;
        default_obj.set("STATUS_CODES", codes)?;
        default_obj.set("METHODS", methods)?;
        default_obj.set("Server", ctx.globals().get::<_, Function>("__czHttpClassServer")?)?;
        default_obj.set("IncomingMessage", ctx.globals().get::<_, Function>("__czHttpClassIncoming")?)?;
        default_obj.set("ServerResponse", ctx.globals().get::<_, Function>("__czHttpClassResponse")?)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

/// Install only the JS primitives/shim without declaring a module (used by `cz-runner` tests that
/// drive the event loop directly).
pub fn ensure_glue<'js>(ctx: &Ctx<'js>) -> Result<()> {
    install_glue(ctx)
}

#[cfg(test)]
mod http_module_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    /// The HTTP registry is process-wide, so socket-level tests must run one at a time (Phase 1
    /// module tests do the same for env-var/global state): plain `cargo test` runs tests in
    /// parallel, which would otherwise interleave servers and queued requests.
    pub(crate) fn http_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop leftover servers/events from previous tests before starting a fresh socket test.
    fn reset_engine_state() {
        close_all_servers();
        for _ in 0..256 {
            if pop_request().is_none() {
                break;
            }
        }
        assert_eq!(listening_count(), 0, "engine should be idle before a socket test");
    }

    fn run_script(source: &str) -> Result<String> {
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        let out = ctx.with(|ctx| -> Result<String> {
            // Minimal console shim so error paths print somewhere.
            let printer = Function::new(ctx.clone(), |level: i32, parts: Vec<String>| -> Result<()> {
                let line = parts.join(" ");
                if level >= 2 { eprintln!("stderr> {line}"); } else { println!("stdout> {line}"); }
                Ok(())
            })?;
            ctx.globals().set("__cz_print", printer)?;
            ctx.eval::<(), _>(r#"(function(){ globalThis.console = { log:(...a)=>__cz_print(0,a.map(String)), warn:(...a)=>__cz_print(1,a.map(String)), error:(...a)=>__cz_print(2,a.map(String)) }; })();"#)?;
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let promise = Module::evaluate(ctx.clone(), "http-test.mjs", source)?;
            // NOTE: never pump jobs inside an active `ctx.with()` scope (rquickjs re-enters the
            // runtime lock there and panics with "RefCell already borrowed").
            promise.finish::<()>()?;
            ctx.globals().get::<_, String>("__R").or(Ok("NO_RESULT".into()))
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    /// Extract the numeric `port` value out of an `address()` JSON string.
    fn port_of(json: &str) -> u16 {
        let key = "\"port\":";
        let idx = json.find(key).expect("port field");
        let rest = &json[idx + key.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().expect("numeric port")
    }

    #[test]
    fn exports_shape_named_and_default() {
        let _lock = http_test_lock();
        reset_engine_state();
        let src = r#"
            import http from "node:http";
            import * as ns from "node:http";
            const needNames = ["createServer","STATUS_CODES","METHODS","Server","IncomingMessage","ServerResponse"];
            for (const n of needNames) if (!(n in ns)) throw new Error("missing named export " + n);
            for (const n of needNames) if (!(n in http)) throw new Error("missing default member " + n);
            if (http.STATUS_CODES[200] !== "OK") throw new Error("200 phrase wrong");
            if (http.STATUS_CODES[413] !== "Payload Too Large") throw new Error("413 phrase wrong");
            if (JSON.stringify(http.METHODS) !== JSON.stringify(["GET","HEAD","POST","PUT","PATCH","DELETE","OPTIONS"])) {
                throw new Error("METHODS wrong " + JSON.stringify(http.METHODS));
            }
            if (typeof http.createServer !== "function") throw new Error("createServer not function");
            const s = http.createServer(() => {});
            if (!(s instanceof http.Server)) throw new Error("createServer must build a Server");
            if (typeof s.listen !== "function" || typeof s.address !== "function" || typeof s.close !== "function" || typeof s.on !== "function") {
                throw new Error("Server methods missing");
            }
            globalThis.__R = "HTTP_EXPORTS_OK";
        "#;
        assert_eq!(run_script(src).unwrap(), "HTTP_EXPORTS_OK");
    }

    #[test]
    fn listen_zero_on_default_host_reports_tailscale_ready_address_then_null_after_close() {
        let _lock = http_test_lock();
        reset_engine_state();
        let src = r#"
            import http from "node:http";
            const s = http.createServer((req, res) => { res.end("x"); });
            const events = [];
            s.on("listening", () => events.push("listening"));
            let cbSeen = null;
            s.listen(0, "0.0.0.0", () => { cbSeen = s.address(); });
            if (!cbSeen) throw new Error("callback must see an address synchronously");
            if (cbSeen.address !== "0.0.0.0") throw new Error("default bind host must be 0.0.0.0, got " + cbSeen.address);
            if (!(cbSeen.port > 0)) throw new Error("ephemeral port must resolve, got " + cbSeen.port);
            if (cbSeen.family !== "IPv4") throw new Error("family must be IPv4");
            if (events.join(",") !== "listening") throw new Error("listening event missing: " + events.join(","));
            if (s.address().port !== cbSeen.port) throw new Error("address() must stay valid");
            let closed = false;
            s.close(() => { closed = true; });
            if (!closed) throw new Error("close callback must fire");
            if (s.address() !== null) throw new Error("address() must be null after close");
            s.on("request", () => {}); // server objects remain usable after close
            globalThis.__R = "HTTP_LISTEN_CLOSE_OK";
        "#;
        assert_eq!(run_script(src).unwrap(), "HTTP_LISTEN_CLOSE_OK");
    }

    #[test]
    fn listen_port_callback_overload_and_host_override_work() {
        let _lock = http_test_lock();
        reset_engine_state();
        let src = r#"
            import http from "node:http";
            const s = http.createServer();
            s.listen(0, () => { /* overload without host */ });
            if (!s.address()) throw new Error("listen(port, cb) must bind synchronously");
            const s2 = http.createServer();
            let order = [];
            s2.on("listening", () => order.push("L"));
            s2.listen(0, "127.0.0.1", () => order.push("CB"));
            if (order.join(",") !== "CB,L") throw new Error("callback then listening event expected, got " + order.join(","));
            if (s2.address().address !== "127.0.0.1") throw new Error("explicit host override lost: " + s2.address().address);
            s.close(); s2.close();
            globalThis.__R = "HTTP_LISTEN_OVERLOADS_OK";
        "#;
        assert_eq!(run_script(src).unwrap(), "HTTP_LISTEN_OVERLOADS_OK");
    }

    #[test]
    fn response_header_api_case_insensitive_and_double_end_is_noop() {
        let _lock = http_test_lock();
        reset_engine_state();
        let src = r#"
            import http from "node:http";
            const res = new http.ServerResponse({ connId: 999, keepAlive: false, socket: { remoteAddress: "1.2.3.4", remotePort: 5 } });
            if (res.headersSent !== false || res.writableEnded !== false) throw new Error("initial flags wrong");
            res.setHeader("X-Powered-By", "Cz0");
            if (!res.hasHeader("x-powered-by")) throw new Error("hasHeader must be case-insensitive");
            if (res.getHeader("X-POWERED-BY") !== "Cz0") throw new Error("getHeader case-insensitive failed");
            res.removeHeader("X-PoWeReD-bY");
            if (res.hasHeader("x-powered-by")) throw new Error("removeHeader failed");
            res.writeHead(404, { "Content-Type": "application/json" });
            if (res.statusCode !== 404) throw new Error("writeHead status lost");
            if (res.getHeader("content-type") !== "application/json") throw new Error("writeHead headers lost");
            res.writeHead(200, "Custom Reason", { "X-A": 7 });
            if (res.statusMessage !== "Custom Reason") throw new Error("statusMessage override lost");
            if (res.getHeader("x-a") !== "7") throw new Error("numeric header stringified");
            res.write("part1");
            res.end("part2");
            if (res.writableEnded !== true || res.headersSent !== true) throw new Error("end flags not set");
            const before = res.statusCode;
            res.end("again"); // safe no-op
            if (res.statusCode !== before) throw new Error("double end mutated state");
            globalThis.__R = "HTTP_RES_API_OK";
        "#;
        // connId 999 does not exist in the registry: __czRespond returns false but must not throw.
        assert_eq!(run_script(src).unwrap(), "HTTP_RES_API_OK");
    }

    #[test]
    fn request_listener_end_to_end_delivers_data_and_end_over_real_socket() -> Result<()> {
        let _lock = http_test_lock();
        reset_engine_state();
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let src = r#"
            import http from "node:http";
            const s = http.createServer((req, res) => {
                let body = "";
                req.setEncoding("utf8");
                req.on("data", (c) => { body += c; });
                req.on("end", () => {
                    res.writeHead(200, { "Content-Type": "application/json", "X-Echo": String(body.length) });
                    res.end(JSON.stringify({ method: req.method, url: req.url, v: req.httpVersion, hdr: req.headers["x-test"], body }));
                });
            });
            s.listen(0, "127.0.0.1", () => { globalThis.__ADDR = JSON.stringify(s.address()); });
            globalThis.__SERVER = s;
        "#;
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            let printer = Function::new(ctx.clone(), |level: i32, parts: Vec<String>| -> Result<()> {
                if level >= 2 { eprintln!("stderr> {}", parts.join(" ")); } else { println!("stdout> {}", parts.join(" ")); }
                Ok(())
            })?;
            ctx.globals().set("__cz_print", printer)?;
            ctx.eval::<(), _>(r#"(function(){ globalThis.console={log:(...a)=>__cz_print(0,a.map(String)),error:(...a)=>__cz_print(2,a.map(String))}; })();"#)?;
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let p = Module::evaluate(ctx.clone(), "e2e-http.mjs", src)?;
            p.finish::<()>()?;
            Ok(())
        })?;
        while let Ok(true) = rt.execute_pending_job() {}

        // listen() binds synchronously, so the port is already known right after evaluation.
        let addr = take_addr(&ctx).expect("listen callback recorded an address");
        let mut sock = TcpStream::connect(("127.0.0.1", addr)).expect("connect to module server");
        sock.set_nodelay(true).ok();
        sock.write_all(
            b"POST /api/echo?x=1 HTTP/1.1\r\nHost: z\r\nX-Test: tailscale\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .unwrap();

        // Poll like the real cz-js event loop: the reader thread queues the request asynchronously,
        // so keep draining (with a short idle sleep) until it is dispatched, then pump JS jobs.
        drive_until_dispatched(&rt, &ctx, 1);

        let mut text = String::new();
        sock.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "response was {text:?}");
        assert!(text.contains("X-Echo: 5\r\n"), "data listener length header missing in {text:?}");
        let body_start = text.find("\r\n\r\n").unwrap() + 4;
        let json: String = text[body_start..].chars().collect();
        assert!(json.contains("\"method\":\"POST\""), "method missing {json}");
        assert!(json.contains("\"url\":\"/api/echo?x=1\""), "url+query missing {json}");
        assert!(json.contains("\"v\":\"1.1\""), "httpVersion missing {json}");
        assert!(json.contains("\"hdr\":\"tailscale\""), "lowercased header lookup failed {json}");
        assert!(json.contains("\"body\":\"hello\""), "body delivered {json}");

        // Close through JavaScript so `this` is bound correctly inside Server.prototype.close.
        ctx.with(|ctx| ctx.eval::<(), _>("globalThis.__SERVER.close();"))?;
        while let Ok(true) = rt.execute_pending_job() {}
        Ok(())
    }

    #[test]
    fn empty_get_requests_still_fire_end_and_keepalive_dispatches_both() -> Result<()> {
        let _lock = http_test_lock();
        reset_engine_state();
        use std::io::Write;
        use std::net::TcpStream;

        let src = r#"
            import http from "node:http";
            const log = [];
            globalThis.__LOG = log;
            const s = http.createServer((req, res) => {
                req.on("end", () => {
                    log.push(req.url);
                    res.writeHead(200, { "Content-Type": "text/plain", "X-N": String(log.length) });
                    res.end("seen:" + log.join(","));
                });
            });
            s.listen(0, "127.0.0.1", () => { globalThis.__ADDR = JSON.stringify(s.address()); });
            globalThis.__SERVER = s;
        "#;
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let p = Module::evaluate(ctx.clone(), "ka.mjs", src)?;
            p.finish::<()>()?;
            Ok(())
        })?;
        while let Ok(true) = rt.execute_pending_job() {}
        let addr = take_addr(&ctx).expect("addr available");
        let mut sock = TcpStream::connect(("127.0.0.1", addr)).expect("connect keep-alive");
        sock.set_nodelay(true).ok();
        // Two sequential requests over one socket (HTTP/1.1 keep-alive).
        sock.write_all(b"GET /first HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();
        drive_until_dispatched(&rt, &ctx, 1);
        let first = read_response(&mut sock);
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "first={first:?}");
        assert!(first.ends_with("seen:/first"));
        assert!(first.contains("Connection: keep-alive\r\n"), "keep-alive header missing {first:?}");

        sock.write_all(b"GET /second HTTP/1.1\r\nHost: z\r\nConnection: close\r\n\r\n").unwrap();
        drive_until_dispatched(&rt, &ctx, 1);
        let second = read_response(&mut sock);
        assert!(second.contains("X-N: 2\r\n"), "second response headers {second:?}");
        assert!(second.ends_with("seen:/first,/second"), "empty-body end fired for both {second:?}");

        ctx.with(|ctx| ctx.eval::<(), _>("globalThis.__SERVER.close();"))?;
        while let Ok(true) = rt.execute_pending_job() {}
        Ok(())
    }

    #[test]
    fn requests_arriving_after_server_close_are_dropped_not_dispatched() -> Result<()> {
        let _lock = http_test_lock();
        close_all_servers();
        use std::io::Write;
        use std::net::TcpStream;

        let src = r#"
            import http from "node:http";
            globalThis.__dispatches = 0;
            const s = http.createServer((req, res) => { globalThis.__dispatches++; res.end("should-not-run"); });
            let sock = null;
            s.listen(0, "127.0.0.1", () => { globalThis.__ADDR = JSON.stringify(s.address()); });
            globalThis.__SERVER = s;
        "#;
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let p = Module::evaluate(ctx.clone(), "closed.mjs", src)?;
            p.finish::<()>()?;
            Ok(())
        })?;
        while let Ok(true) = rt.execute_pending_job() {}

        let addr = take_addr(&ctx).expect("addr");
        // Establish the TCP connection while the server is listening, then close it BEFORE the
        // request bytes are written: the queued event must be dropped (never dispatched).
        let mut sock = TcpStream::connect(("127.0.0.1", addr)).expect("connect before close");
        sock.set_nodelay(true).ok();

        // Deterministic accept proof (replaces racing-with-accept immediate close): dispatch one
        // normal request on a second connection first. accept_loop services the backlog FIFO, so
        // once /early lands the earlier `sock` already owns its reader thread — closing after
        // that can never miss the pending connection in the backlog.
        {
            let mut early = TcpStream::connect(("127.0.0.1", addr)).expect("early connect");
            early.set_nodelay(true).ok();
            early
                .write_all(b"GET /early HTTP/1.1\r\nHost: z\r\nConnection: close\r\n\r\n")
                .unwrap();
            drive_until_dispatched(&rt, &ctx, 1);
            let early_text = read_response(&mut early);
            assert!(early_text.starts_with("HTTP/1.1 200"), "pre-close evidence frame {early_text:?}");
        }

        ctx.with(|ctx| ctx.eval::<(), _>("globalThis.__SERVER.close();"))?;
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(listening_count(), 0, "closed server must leave the registry");

        // Objective queueing proof instead of a fixed 80 ms guess: after writing the late bytes
        // poll until the engine-queued event count grows past the post-close baseline.
        let queued_before = cz_hal::http_server::events_queued();
        sock.write_all(b"GET /late HTTP/1.1\r\nHost: z\r\nConnection: close\r\n\r\n").unwrap();
        let mut waited = 0u32;
        while cz_hal::http_server::events_queued() == queued_before && waited < 400 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            waited += 1;
        }
        assert!(waited < 400, "late request never reached the engine queue");
        let dispatched = ctx.with(|ctx| -> i64 {
            let drain: Function = ctx.globals().get("__czHttpDrainLoop").unwrap();
            drain.call(()).unwrap_or(0)
        });
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(dispatched, 0, "stale request for a closed server must not be dispatched");

        // The client is answered rather than left hanging (reply channel dropped -> engine 500).
        let text = read_response(&mut sock);
        assert!(text.starts_with("HTTP/1.1 500 Internal Server Error"), "post-close frame {text:?}");
        // /early dispatched exactly once BEFORE close as accept-liveness evidence; the late
        // request must never have run the listener afterwards, so the count stays at one.
        let seen = ctx.with(|ctx| ctx.eval::<String, _>("String(globalThis.__dispatches)")).unwrap();
        assert_eq!(seen, "1", "listener ran after close or never ran pre-close (count={seen})");
        Ok(())
    }

    // helpers shared by the socket-level module tests
    fn take_addr(ctx: &Context) -> Option<u16> {
        let raw = ctx.with(|ctx| -> String {
            ctx.globals().get::<_, String>("__ADDR").unwrap_or_default()
        });
        if raw.is_empty() {
            return None;
        }
        Some(port_of(&raw))
    }

    /// Drive the event loop until at least `want` requests were dispatched (~5ms idle pauses,
    /// mirroring how cz-js polls so async reader-thread queueing cannot race a single pass).
    fn drive_until_dispatched(rt: &Runtime, ctx: &Context, want: i64) {
        let mut seen = 0i64;
        for _ in 0..400 {
            seen += ctx.with(|ctx| -> i64 {
                let drain: Function = ctx.globals().get("__czHttpDrainLoop").unwrap();
                drain.call(()).unwrap_or(0)
            });
            // Jobs must only be pumped outside an active with() scope.
            while let Ok(true) = rt.execute_pending_job() {}
            if seen >= want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("expected {want} dispatched request(s), saw {seen}");
    }

    fn read_response(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut acc = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&tmp[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        // include the Content-Length body when present
                        let head_end = find_head_end(&acc);
                        if let Some(cl) = header_val(&acc[..head_end], "content-length") {
                            let want: usize = cl.trim().parse().unwrap_or(0);
                            while acc.len() < head_end + 4 + want {
                                match sock.read(&mut tmp) {
                                    Ok(0) => break,
                                    Ok(m) => acc.extend_from_slice(&tmp[..m]),
                                    Err(_) => break,
                                }
                            }
                        }
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&acc).to_string()
    }

    fn find_head_end(b: &[u8]) -> usize {
        for i in 0..b.len().saturating_sub(3) {
            if &b[i..i + 4] == b"\r\n\r\n" {
                return i;
            }
        }
        b.len()
    }

    fn header_val(head: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(head);
        for line in text.split("\r\n").skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case(name) {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }

    /// Install a minimal Date.now()-based timer registry on a bare test context (mirrors the
    /// `cz-js` event-loop contract: `__czTimersTick` fires due callbacks and returns remaining).
    fn test_timers<'js>(ctx: &Ctx<'js>) -> Result<()> {
        ctx.eval::<(), _>(
            r#"
            (function () {
                const timers = new Map();
                let nextId = 1;
                globalThis.setInterval = function (fn, delay) {
                    if (typeof fn !== "function") throw new TypeError("interval callback must be a function");
                    const step = Number(delay) > 0 ? Number(delay) : 1;
                    const id = nextId++;
                    timers.set(id, { fn: fn, at: Date.now() + step, every: step });
                    return id;
                };
                globalThis.clearInterval = function (id) { timers.delete(id); };
                globalThis.setTimeout = function (fn, delay) {
                    if (typeof fn !== "function") throw new TypeError("timeout callback must be a function");
                    const id = nextId++;
                    timers.set(id, { fn: fn, at: Date.now() + Math.max(0, Number(delay) || 0), every: null });
                    return id;
                };
                globalThis.clearTimeout = function (id) { timers.delete(id); };
                globalThis.__czTimerCount = function () { return timers.size; };
                globalThis.__czTimersTick = function () {
                    const now = Date.now();
                    const due = [];
                    for (const pair of timers.entries()) if (pair[1].at <= now) due.push(pair);
                    for (const pair of due) {
                        const id = pair[0], t = pair[1];
                        if (!timers.has(id)) continue;
                        if (t.every !== null && t.every > 0) timers.set(id, { fn: t.fn, at: now + t.every, every: t.every });
                        else timers.delete(id);
                        try { t.fn(); } catch (e) { if (globalThis.console && console.error) console.error(String(e && e.message || e)); }
                    }
                    return timers.size;
                };
            })();
            "#,
        )?;
        Ok(())
    }

    /// Poll the JS side (drain HTTP + close notifications + pump jobs OUTSIDE `with()` scopes)
    /// until `probe` sees a non-empty result or budget runs out.
    fn eval_poll(rt: &Runtime, ctx: &Context, expr: &str, budget_ms: u128) -> String {
        let t0 = std::time::Instant::now();
        loop {
            let got = ctx.with(|c| -> String {
                let _ = c.eval::<i64, _>("__czHttpDrainLoop()").unwrap_or(0);
                let _ = c.eval::<i64, _>("__czTimersTick()").unwrap_or(0);
                let _ = c.eval::<i64, _>("__czHttpCheckCloses()").unwrap_or(0);
                c.eval::<String, _>(format!("String({expr})")).unwrap_or_default()
            });
            while let Ok(true) = rt.execute_pending_job() {}
            if !got.is_empty() && got != "undefined" && got != "null" {
                return got;
            }
            if std::time::Instant::now() - t0 > std::time::Duration::from_millis(budget_ms as u64) {
                return got;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn sse_flush_headers_pushes_live_chunks_and_close_fires_on_disconnect() -> Result<()> {
        let _lock = http_test_lock();
        reset_engine_state();
        use std::io::{Read, Write};
        use std::net::TcpStream;

        // Handler uses an explicit flushHeaders(), pushes via setInterval (like the E2E), and
        // cleans up through BOTH req.on("close") and res.on("close") exactly once.
        let src = r#"
            import http from "node:http";
            let active = 0, closed = 0, seq = 0, cleaned = false, writeResults = [];
            globalThis.__STATS = () => JSON.stringify({ active, closed, seq });
            let timer = null;
            const cleanup = () => { if (cleaned) return; cleaned = true; try { clearInterval(timer); } catch (e) { errs.push("ci:" + e); } active--; closed++; };
            const s = http.createServer((req, res) => {
                res.writeHead(200, { "Content-Type": "text/event-stream; charset=utf-8", "Cache-Control": "no-cache" });
                res.flushHeaders();
                active++;
                req.on("close", cleanup);
                res.on("close", cleanup);
                const push = () => {
                    if (cleaned) return;
                    seq++;
                    writeResults.push(res.write("event: telemetry\ndata: {\"seq\":" + seq + "}\n\n"));
                };
                push();
                timer = setInterval(push, 15);
            });
            s.listen(0, "127.0.0.1", () => { globalThis.__ADDR = JSON.stringify(s.address()); });
            globalThis.__SERVER = s;
            globalThis.__WR = () => JSON.stringify(writeResults);
        "#;
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            test_timers(&ctx)?;
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let p = Module::evaluate(ctx.clone(), "sse.mjs", src)?;
            p.finish::<()>()?;
            Ok(())
        })?;
        while let Ok(true) = rt.execute_pending_job() {}
        let addr = take_addr(&ctx).expect("addr");

        let mut sock = TcpStream::connect(("127.0.0.1", addr)).expect("connect sse");
        sock.set_nodelay(true).ok();
        sock.set_read_timeout(Some(std::time::Duration::from_millis(100))).ok();
        sock.write_all(b"GET /events HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();

        // Pump the JS event loop (dispatch + timers + close checks) exactly like cz-js does,
        // collecting wire bytes as they arrive in real time.
        let mut acc: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut pump = |rt: &Runtime, ctx: &Context, acc: &mut Vec<u8>, sock: &mut TcpStream| {
            ctx.with(|c| {
                let _ = c.eval::<i64, _>("__czHttpDrainLoop()");
                let _ = c.eval::<i64, _>("__czTimersTick()");
                let _ = c.eval::<i64, _>("__czHttpCheckCloses()");
            });
            while let Ok(true) = rt.execute_pending_job() {}
            match sock.read(&mut tmp) {
                Ok(n) if n > 0 => acc.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {}
                _ => {}
            }
        };
        // Headers must complete on their own (flushHeaders fired before any end()).
        let head_end = loop {
            if let Some(idx) = (0..acc.len().saturating_sub(3)).find(|&i| &acc[i..i + 4] == b"\r\n\r\n") {
                break idx;
            }
            pump(&rt, &ctx, &mut acc, &mut sock);
        };
        let head = String::from_utf8_lossy(&acc[..head_end]).to_string();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "head {head:?}");
        assert_eq!(header_val(head.as_bytes(), "content-type").as_deref(), Some("text/event-stream; charset=utf-8"));
        assert_eq!(header_val(head.as_bytes(), "transfer-encoding").as_deref(), Some("chunked"), "chunked mode {head:?}");
        assert!(header_val(head.as_bytes(), "content-length").is_none(), "no CL while streaming {head:?}");

        // Keep pumping: live chunk frames must appear BEFORE any terminator (no res.end()).
        let mut body: Vec<u8> = Vec::new();
        for _ in 0..600 {
            pump(&rt, &ctx, &mut body, &mut sock);
            let text = String::from_utf8_lossy(&body).to_string();
            if text.contains("\"seq\":2") {
                assert!(!text.contains("0\r\n\r\n"), "stream terminated before disconnect");
                break;
            }
        }
        // push #1 fired synchronously during dispatch and may share the first read with the
        // head bytes, so inspect the FULL wire accumulated so far (head tail + later frames).
        let mut wire: Vec<u8> = Vec::new();
        wire.extend_from_slice(&acc[head_end + 4..]);
        wire.extend_from_slice(&body);
        let dbg = ctx.with(|c| c.eval::<String, _>(
            "JSON.stringify({ hasInterval: typeof setInterval, err: globalThis.__HANDLER_ERR || null })",
        )).unwrap_or_default();
        assert!(
            String::from_utf8_lossy(&wire).contains("event: telemetry\ndata: {\"seq\":1}\n\n"),
            "live push 1 (wire {wire:?}, js {dbg})"
        );
        // (Terminator absence for the pre-disconnect window was asserted in the pump loop above.)
        // res.write returned booleans during streaming (never undefined).
        let wr = ctx.with(|c| c.eval::<String, _>("globalThis.__WR()")).unwrap();
        assert!(wr.starts_with("[true") && !wr.contains("undefined"), "write booleans {wr}");

        // Client hangs up: engine detects EOF, event loop fires BOTH close listeners ONCE and the
        // interval is cleared.
        // Client hangs up: the engine detects EOF/RST, and the event loop fires BOTH close
        // listeners exactly once so user cleanup can clearInterval(...).
        drop(sock);
        // Wait for the close event with an explicit condition (not mere value presence),
        // pumping timers + close checks each iteration like the cz-js event loop does.
        let mut stats = String::new();
        let t_close = std::time::Instant::now();
        while t_close.elapsed() < std::time::Duration::from_millis(4000) {
            ctx.with(|c| {
                let _ = c.eval::<i64, _>("__czTimersTick()");
                let _ = c.eval::<i64, _>("__czHttpCheckCloses()");
            });
            while let Ok(true) = rt.execute_pending_job() {}
            stats = ctx.with(|c| c.eval::<String, _>("String(globalThis.__STATS())")).unwrap_or_default();
            if stats.contains("\"closed\":1") && stats.contains("\"active\":0") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert!(stats.contains("\"closed\":1"), "close fired exactly once: {stats}");
        assert!(stats.contains("\"active\":0"), "cleanup decremented active streams: {stats}");
        // Later writes on a dead stream report false (spec \u00a71 write() semantics).
        let wr_late = ctx.with(|c| c.eval::<String, _>("globalThis.__WR()")).unwrap_or_default();
        assert!(wr_late.contains("false"), "write reported false after disconnect: {wr_late}");
        let timers = ctx.with(|c| c.eval::<i64, _>("__czTimerCount()")).unwrap_or(9);
        assert_eq!(timers, 0, "interval cleared by close cleanup");

        ctx.with(|ctx| ctx.eval::<(), _>("globalThis.__SERVER.close();"))?;
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(listening_count(), 0);
        Ok(())
    }

    #[test]
    fn stream_end_writes_terminator_and_double_flush_headers_is_noop() -> Result<()> {
        let _lock = http_test_lock();
        reset_engine_state();
        use std::io::{Read, Write};
        use std::net::TcpStream;

        // Auto-streaming via Content-Type (NO explicit flushHeaders call), double flush is a
        // safe no-op, and end(chunk) frames the last chunk + terminating 0\r\n\r\n.
        let src = r#"
            import http from "node:http";
            globalThis.__RESULT = null;
            const s = http.createServer((req, res) => {
                res.writeHead(200, { "Content-Type": "text/event-stream" });
                const firstWrite = res.write("auto-head\n");            // triggers auto flushHeaders
                res.flushHeaders(); res.flushHeaders();                  // safe no-ops
                const ended = res.end("tail");
                __RESULT = JSON.stringify({ firstWrite, headersSent: res.headersSent,
                                           ended: ended === res, writableEnded: res.writableEnded });
            });
            s.listen(0, "127.0.0.1", () => { globalThis.__ADDR = JSON.stringify(s.address()); });
            globalThis.__SERVER = s;
        "#;
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            Module::declare_def::<HttpModule, _>(ctx.clone(), "node:http")?;
            let p = Module::evaluate(ctx.clone(), "sse-end.mjs", src)?;
            p.finish::<()>()?;
            Ok(())
        })?;
        while let Ok(true) = rt.execute_pending_job() {}
        let addr = take_addr(&ctx).expect("addr");

        let mut sock = TcpStream::connect(("127.0.0.1", addr)).expect("connect");
        sock.set_nodelay(true).ok();
        // Connection: close -> engine shuts the socket after StreamEnd, so read_to_string ends.
        sock.write_all(b"GET /stream HTTP/1.1\r\nHost: z\r\nConnection: close\r\n\r\n").unwrap();
        drive_until_dispatched(&rt, &ctx, 1);
        let mut text = String::new();
        let mut tmp = [0u8; 4096];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => text.push_str(&String::from_utf8_lossy(&tmp[..n])),
                Err(_) => break,
            }
            if text.ends_with("0\r\n\r\n") || text.contains("0\r\n\r\n") && text.ends_with("\r\n\r\n") {
                break;
            }
        }
        assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 1, "single head, double flush no-op {text:?}");
        assert!(text.contains("Transfer-Encoding: chunked\r\n"), "chunked mode {text:?}");
        assert!(!text.contains("Content-Length"), "no CL in streaming wire {text:?}");
        // "auto-head\n" is 10 bytes -> hex a; "tail" is 4 -> hex 4.
        assert!(text.contains("a\r\nauto-head\n\r\n"), "hex-10 frame {text:?}");
        assert!(text.contains("4\r\ntail\r\n"), "final chunk {text:?}");
        assert!(text.ends_with("0\r\n\r\n"), "terminator last {text:?}");

        let res_json = eval_poll(&rt, &ctx, "globalThis.__RESULT", 1500);
        assert!(res_json.contains("\"firstWrite\":true"), "auto-stream write true: {res_json}");
        assert!(res_json.contains("\"headersSent\":true"), "headers sent: {res_json}");
        assert!(res_json.contains("\"writableEnded\":true"), "writableEnded set: {res_json}");
        assert!(res_json.contains("\"ended\":true"), "end() returns this: {res_json}");

        ctx.with(|ctx| ctx.eval::<(), _>("globalThis.__SERVER.close();"))?;
        while let Ok(true) = rt.execute_pending_job() {}
        Ok(())
    }
}

// Test-only re-export so other #[cfg(test)] modules can serialize against the process-wide
// HTTP registry (the same mutex the Phase 2 socket tests use).
#[cfg(test)]
pub(crate) use http_module_tests::http_test_lock;
