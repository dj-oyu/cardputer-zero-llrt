//! Global `fetch()` surface for `cz-js` (Phase 8A): HTTP/1.1 + HTTPS with the shared `cz-net`
//! TLS foundation, header-first resolution, streaming bodies (`ReadableStream`), immediate
//! cancellation (`AbortController` / `AbortSignal.timeout`), and the CardputerZero per-request
//! `tls` extension.
//!
//! Architecture mirrors the other `cz-modules` layers: process-wide Rust primitives registered as
//! `globalThis.__czFetch*` natives (the `'static` closure rule keeps them off any borrowed state)
//! plus a JavaScript shim installed with `ctx.eval` that wires them into Web-compatible classes
//! with a deferred-Promise registry. The engine in `cz-net` calls
//! `cz_hal::http_server::notify_event_loop()` on every header/chunk/completion/error/abort, so
//! `cz-runner`'s Condvar idle wait wakes immediately — there are NO polling loops.
//!
//! `json()` delegates to `globalThis.JSON.parse` (which the runner replaces with the cz-json NEON
//! engine) so large JSON responses transparently use the Lazy tape representation.

use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::{
    convert::Coerced, function::Opt, ArrayBuffer, Array, Ctx, Exception, FromJs, Function, Object,
    Result, String as JsString, Value,
};

use crate::web_globals::install_web_globals;
use crate::{json_module::json_parse_bytes_native, vec_to_uint8array};

use cz_net::fetch::{
    active_fetch_count, cancel_fetch, live_ids, next_event, parse_redirect_mode, release_slot,
    start_fetch, switch_buffered, switch_streaming, EngineEvent, FailKind, FetchSpec,
};
use cz_net::tls::TlsClientOptions;

/// True once `install_fetch` ran in this process (the shim itself is also idempotent).
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Live fetch slots (responses/streams not yet released). Used by `cz-runner` liveness.
pub fn fetch_active_count() -> usize {
    active_fetch_count()
}

/// True while any fetch/Response stream is outstanding. Used by `cz-runner` liveness.
pub fn fetch_has_pending() -> bool {
    fetch_has_pending_engine()
}

fn fetch_has_pending_engine() -> bool {
    cz_net::fetch::fetch_has_pending()
}

/// Interpret a `string | Uint8Array | ArrayBuffer` JS value as bytes (`None` for null/undefined).
fn opt_bytes<'js>(v: &Value<'js>) -> Result<Option<Vec<u8>>> {
    if v.is_null() || v.is_undefined() {
        return Ok(None);
    }
    if let Some(s) = v.as_string() {
        let text = s.to_string()?;
        let bytes = crate::bytes_from_value(v).ok();
        // A string is UTF-8 text; typed-array fallback only applies to non-string values.
        let _ = bytes;
        return Ok(Some(text.into_bytes()));
    }
    match crate::bytes_from_value(v) {
        Ok(b) => Ok(Some(b)),
        Err(_) => Ok(None),
    }
}

/// Decode an array of `[name, value]` pairs (the shim serializes `Headers` this way).
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

fn parse_tls_object<'js>(ctx: &Ctx<'js>, tls: &Object<'js>) -> Result<TlsClientOptions> {
    let null = Value::new_null(ctx.clone());
    let ca = tls.get::<_, Option<Value>>("ca")?.unwrap_or_else(|| Value::new_null(ctx.clone()));
    let cert = tls.get::<_, Option<Value>>("cert")?.unwrap_or_else(|| Value::new_null(ctx.clone()));
    let key = tls.get::<_, Option<Value>>("key")?.unwrap_or_else(|| Value::new_null(ctx.clone()));
    let reject = tls.get::<_, Option<bool>>("rejectUnauthorized")?;
    let _ = null;
    Ok(TlsClientOptions {
        ca_pem: opt_bytes(&ca)?,
        cert_pem: opt_bytes(&cert)?,
        key_pem: opt_bytes(&key)?,
        // Web-compatible default: verify server certificates unless explicitly disabled.
        reject_unauthorized: reject.unwrap_or(true),
        alpn_protocols: Vec::new(),
    })
}

/// Register the Rust primitives + JS shim on this context (idempotent). The named `'js` lifetime
/// is required so native closures can receive context values (rquickjs lifetime rule).
pub fn install_fetch<'js>(ctx: &Ctx<'js>) -> Result<()> {
    // Phase 13: `AbortSignal` inherits `EventTarget`, so the WinterTC globals land FIRST
    // (idempotent: an already-populated context pays only a flag check).
    install_web_globals(ctx)?;
    // Natives first, then the shim that wraps them into Web-compatible classes.
    ctx.globals().set(
        "__czFetchStart",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, url: String, method: String, redirect: String, pairs: Array<'js>, body: Value<'js>, tls: Value<'js>| -> Result<f64> {
                let headers = collect_header_pairs(&pairs)?;
                let body_bytes = if body.is_undefined() || body.is_null() {
                    None
                } else if let Some(b) = crate::bytes_from_value(&body).ok() {
                    Some(b)
                } else if let Some(s) = body.as_string() {
                    Some(s.to_string()?.into_bytes())
                } else {
                    let coerced = Coerced::<String>::from_js(&ctx, body)?;
                    Some(coerced.0.into_bytes())
                };
                let tls_opts = if tls.is_undefined() || tls.is_null() {
                    TlsClientOptions::default()
                } else {
                    match tls.as_object() {
                        Some(o) => parse_tls_object(&ctx, &o)?,
                        None => TlsClientOptions::default(),
                    }
                };
                let spec = FetchSpec {
                    url,
                    method,
                    headers,
                    body: body_bytes,
                    redirect: parse_redirect_mode(redirect.as_str()),
                    tls: tls_opts,
                };
                match start_fetch(spec) {
                    Ok(id) => Ok(id as f64),
                    Err(msg) => Err(Exception::throw_type(&ctx, &msg)),
                }
            },
        )?,
    )?;

    ctx.globals().set(
        "__czFetchNext",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64| -> Result<Value<'js>> {
                match next_event(id as u64) {
                    None => Ok(Value::new_null(ctx.clone())),
                    Some(ev) => event_descriptor(&ctx, ev),
                }
            },
        )?,
    )?;

    ctx.globals().set("__czFetchCancel", Function::new(ctx.clone(), |id: f64| -> bool {
        cancel_fetch(id as u64)
    })?)?;
    ctx.globals().set("__czFetchStream", Function::new(ctx.clone(), |id: f64| -> bool {
        switch_streaming(id as u64)
    })?)?;
    ctx.globals().set("__czFetchBuffered", Function::new(ctx.clone(), |id: f64| -> bool {
        switch_buffered(id as u64)
    })?)?;
    ctx.globals().set("__czFetchRelease", Function::new(ctx.clone(), |id: f64| -> bool {
        release_slot(id as u64)
    })?)?;
    ctx.globals().set(
        "__czFetchLiveIds",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Array<'js>> {
            let arr = Array::new(ctx.clone())?;
            for (i, id) in live_ids().iter().enumerate() {
                arr.set(i, *id as f64)?;
            }
            Ok(arr)
        })?,
    )?;
    ctx.globals().set("__czFetchActiveCount", Function::new(ctx.clone(), || -> usize {
        active_fetch_count()
    })?)?;
    ctx.globals().set("__czFetchPending", Function::new(ctx.clone(), || -> bool {
        fetch_has_pending()
    })?)?;

    // ---- native UTF-8 helpers (Phase 10C) ------------------------------------
    // Replace the pure-JS char-by-char TextEncoder/TextDecoder loops with one native pass over a
    // borrowed slice of the JS buffer. `__czUtf8Decode` builds the QuickJS string directly from the
    // source bytes (valid UTF-8 needs no intermediate Rust `String` at all); the input argument stays
    // rooted on the JS stack while borrowed and NO JavaScript executes inside the borrow.
    ctx.globals().set(
        "__czUtf8Decode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: Value<'js>, fatal: Opt<Value<'js>>| -> Result<Value<'js>> {
                let strict = fatal.0.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                crate::with_borrowed_bytes(&input, |b| -> Result<Value<'js>> {
                    match std::str::from_utf8(b) {
                        Ok(text) => Ok(JsString::from_str(ctx.clone(), text)?.into_value()),
                        Err(_) if strict => {
                            Err(Exception::throw_type(&ctx, "The encoded data was not valid for encoding utf-8"))
                        }
                        // Lossy mode mirrors Node's default decoder (U+FFFD replacements).
                        Err(_) => Ok(JsString::from_str(ctx.clone(), String::from_utf8_lossy(b).as_ref())?.into_value()),
                    }
                })?
            },
        )?,
    )?;

    // UTF-8 payload of the JS string is taken once and moved into the ArrayBuffer-backed Uint8Array.
    ctx.globals().set(
        "__czUtf8Encode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, text: JsString| -> Result<Value<'js>> {
                let cs = text.to_cstring()?;
                vec_to_uint8array(&ctx, cs.as_str().as_bytes().to_vec())
            },
        )?,
    )?;

    // `Response.json()` byte entry point (same native as the one `install_json` registers, so the
    // binding works even when a context never imported `cardputer:json`).
    ctx.globals().set(
        "__czJsonParseBytes",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: Value<'js>| -> Result<Value<'js>> {
                json_parse_bytes_native(&ctx, &input)
            },
        )?,
    )?;

    ctx.eval::<(), _>(FETCH_SHIM)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Convert one engine event into the descriptor object consumed by the JS pump.
fn event_descriptor<'js>(ctx: &Ctx<'js>, ev: EngineEvent) -> Result<Value<'js>> {
    let obj = Object::new(ctx.clone())?;
    match ev {
        EngineEvent::Headers { status, status_text, url, redirected, headers } => {
            obj.set("kind", "headers")?;
            obj.set("status", status as f64)?;
            obj.set("statusText", status_text.as_str())?;
            obj.set("url", url.as_str())?;
            obj.set("redirected", redirected)?;
            let pairs = Array::new(ctx.clone())?;
            for (i, (k, v)) in headers.iter().enumerate() {
                let pair = Array::new(ctx.clone())?;
                pair.set(0, k.as_str())?;
                pair.set(1, v.as_str())?;
                pairs.set(i, pair)?;
            }
            obj.set("headers", pairs)?;
        }
        EngineEvent::Chunk(bytes) => {
            obj.set("kind", "chunk")?;
            // Phase 10C: move the worker's chunk into the Uint8Array (ownership transfer, zero copy).
            obj.set("value", vec_to_uint8array(ctx, bytes)?)?;
        }
        EngineEvent::Body(bytes) => {
            obj.set("kind", "body")?;
            // Hand the whole buffered body to JavaScript as an ArrayBuffer (no extra copy).
            let ab = ArrayBuffer::new(ctx.clone(), bytes)?;
            obj.set("value", ab)?;
        }
        EngineEvent::Done => {
            obj.set("kind", "done")?;
        }
        EngineEvent::Failed { kind, message } => {
            obj.set("kind", "error")?;
            let tag = match kind {
                FailKind::Abort => "AbortError",
                FailKind::Timeout => "TimeoutError",
                FailKind::BodyTooLarge => "RangeError",
                FailKind::Redirect => "TypeError",
                FailKind::Network => "TypeError",
            };
            obj.set("errorName", tag)?;
            obj.set("message", message.as_str())?;
            obj.set("aborted", matches!(kind, FailKind::Abort))?;
        }
    }
    Ok(obj.into_value())
}

/// Public shim installing `fetch`, `Headers`, `Request`, `Response`, `AbortController`,
/// `AbortSignal` (+ `.abort()` / `.timeout()` statics), a minimal `ReadableStream` and
/// `URLSearchParams`. QuickJS-safe syntax only (no optional chaining / nullish coalescing).
pub(crate) const FETCH_SHIM: &str = r#"
(function () {
    if (globalThis.__czFetchInstalled) return;

    // ------------------------------------------------------------------ Headers
    function normName(n) { return String(n).toLowerCase(); }
    function Headers(init) {
        this._map = new Map();
        if (init == null) return;
        if (init instanceof Headers) {
            init.forEach(function (value, name) { this._map.set(name, value); }, this);
            return;
        }
        if (typeof init === "object" && typeof init[Symbol.iterator] === "function") {
            const it = init[Symbol.iterator]();
            for (;;) {
                const step = it.next();
                if (step.done) break;
                const pair = step.value;
                if (pair && pair.length >= 2) this.append(pair[0], pair[1]);
            }
            return;
        }
        if (typeof init === "object") {
            const keys = Object.keys(init);
            for (const k of keys) this.set(k, init[k]);
        }
    }
    Headers.prototype.get = function (name) {
        const v = this._map.get(normName(name));
        return v === undefined ? null : v;
    };
    Headers.prototype.set = function (name, value) {
        this._map.set(normName(name), String(value));
    };
    Headers.prototype.append = function (name, value) {
        const n = normName(name), s = String(value);
        const prev = this._map.get(n);
        this._map.set(n, prev === undefined ? s : prev + ", " + s);
    };
    Headers.prototype.has = function (name) { return this._map.has(normName(name)); };
    Headers.prototype.delete = function (name) { this._map.delete(normName(name)); };
    Headers.prototype.forEach = function (cb, thisArg) {
        for (const entry of this._map.entries()) cb.call(thisArg || globalThis, entry[1], entry[0], this);
    };
    Headers.prototype.keys = function () {
        const names = Array.from(this._map.keys());
        let i = 0;
        return { next: function () { return i < names.length ? { value: names[i++], done: false } : { value: undefined, done: true }; },
                 [Symbol.iterator]: function () { return this; } };
    };
    Headers.prototype.values = function () {
        const vals = Array.from(this._map.values());
        let i = 0;
        return { next: function () { return i < vals.length ? { value: vals[i++], done: false } : { value: undefined, done: true }; },
                 [Symbol.iterator]: function () { return this; } };
    };
    Headers.prototype.entries = function () {
        const pairs = Array.from(this._map.entries());
        let i = 0;
        return { next: function () { return i < pairs.length ? { value: pairs[i++], done: false } : { value: undefined, done: true }; },
                 [Symbol.iterator]: function () { return this; } };
    };
    Headers.prototype[Symbol.iterator] = function () { return this.entries(); };

    // ------------------------------------------------------------------ AbortSignal / AbortController
    function makeAbortError(reason) {
        if (reason !== undefined && reason !== null) return reason;
        const e = new Error("This operation was aborted");
        e.name = "AbortError";
        return e;
    }
    function AbortSignal() {
        EventTarget.call(this); // Phase 13: share the generic listener registry (`_et`)
        this.aborted = false;
        this.reason = undefined;
        this.onabort = null;
    }
    // WinterTC: `AbortSignal` IS an `EventTarget` (`signal instanceof EventTarget === true`);
    // addEventListener / removeEventListener / dispatchEvent come from the shared prototype.
    AbortSignal.prototype = Object.create(EventTarget.prototype);
    AbortSignal.prototype.constructor = AbortSignal;
    AbortSignal.prototype.throwIfAborted = function () {
        if (this.aborted) throw this.reason === undefined ? makeAbortError(undefined) : this.reason;
    };
    AbortSignal.prototype._fire = function () {
        if (this.onabort) { try { this.onabort({ type: "abort", target: this }); } catch (_e) {} }
        // One Web-shaped abort Event through dispatchEvent (listener errors never leak, §2.6).
        try { this.dispatchEvent(new Event("abort")); } catch (_e) {}
    };
    AbortSignal.abort = function (reason) {
        const s = new AbortSignal();
        s.aborted = true;
        s.reason = reason === undefined ? makeAbortError(undefined) : reason;
        return s;
    };
    AbortSignal.timeout = function (ms) {
        const s = new AbortSignal();
        const err = new Error("This operation was aborted");
        err.name = "TimeoutError";
        setTimeout(function () {
            if (!s.aborted) { s.aborted = true; s.reason = err; s._fire(); }
        }, ms);
        return s;
    };
    function AbortController() { this.signal = new AbortSignal(); }
    AbortController.prototype.abort = function (reason) {
        if (this.signal.aborted) return;
        this.signal.aborted = true;
        this.signal.reason = reason === undefined ? makeAbortError(undefined) : reason;
        this.signal._fire();
    };

    // ------------------------------------------------------------------ URLSearchParams
    function URLSearchParams(init) {
        this._pairs = [];
        if (typeof init === "string") {
            const text = init.startsWith("?") ? init.slice(1) : init;
            if (text.length > 0) {
                const parts = text.split("&");
                for (const p of parts) {
                    const eq = p.indexOf("=");
                    if (eq < 0) this.append(decodeURIComponent(p.replace(/\+/g, " ")), "");
                    else this.append(decodeURIComponent(p.slice(0, eq).replace(/\+/g, " ")),
                                       decodeURIComponent(p.slice(eq + 1).replace(/\+/g, " ")));
                }
            }
        } else if (init && typeof init === "object") {
            if (typeof init[Symbol.iterator] === "function") {
                for (const pair of init) this.append(pair[0], pair[1]);
            } else {
                const keys = Object.keys(init);
                for (const k of keys) this.append(k, init[k]);
            }
        }
    }
    URLSearchParams.prototype.append = function (k, v) { this._pairs.push([String(k), String(v)]); };
    URLSearchParams.prototype.set = function (k, v) {
        const key = String(k); let done = false;
        const out = [];
        for (const p of this._pairs) { if (p[0] === key && !done) { out.push([key, String(v)]); done = true; } else if (p[0] !== key) out.push(p); }
        if (!done) out.push([key, String(v)]);
        this._pairs = out;
    };
    URLSearchParams.prototype.get = function (k) {
        const key = String(k);
        for (const p of this._pairs) if (p[0] === key) return p[1];
        return null;
    };
    URLSearchParams.prototype.getAll = function (k) {
        const key = String(k); const out = [];
        for (const p of this._pairs) if (p[0] === key) out.push(p[1]);
        return out;
    };
    URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
    URLSearchParams.prototype.delete = function (k) {
        const key = String(k);
        this._pairs = this._pairs.filter(function (p) { return p[0] !== key; });
    };
    URLSearchParams.prototype.sort = function () {
        this._pairs.sort(function (a, b) { return a[0] < b[0] ? -1 : (a[0] > b[0] ? 1 : 0); });
    };
    URLSearchParams.prototype.toString = function () {
        const enc = function (s) {
            return encodeURIComponent(s).replace(/%20/g, "+").replace(/!'()*/g, function (c) { return c === "!" ? "%21" : (c === "'" ? "%27" : (c === "(" ? "%28" : "%29")); });
        };
        const parts = [];
        for (const p of this._pairs) parts.push(enc(p[0]) + "=" + enc(p[1]));
        return parts.join("&");
    };
    URLSearchParams.prototype.forEach = function (cb, thisArg) {
        for (const p of this._pairs) cb.call(thisArg || globalThis, p[1], p[0], this);
    };
    URLSearchParams.prototype.keys = function () {
        const ks = this._pairs.map(function (p) { return p[0]; });
        let i = 0;
        return { next: function () { return i < ks.length ? { value: ks[i++], done: false } : { value: undefined, done: true }; }, [Symbol.iterator]: function () { return this; } };
    };
    URLSearchParams.prototype.values = function () {
        const vs = this._pairs.map(function (p) { return p[1]; });
        let i = 0;
        return { next: function () { return i < vs.length ? { value: vs[i++], done: false } : { value: undefined, done: true }; }, [Symbol.iterator]: function () { return this; } };
    };
    URLSearchParams.prototype.entries = function () {
        const ps = this._pairs.slice();
        let i = 0;
        return { next: function () { return i < ps.length ? { value: ps[i++], done: false } : { value: undefined, done: true }; }, [Symbol.iterator]: function () { return this; } };
    };
    URLSearchParams.prototype[Symbol.iterator] = function () { return this.entries(); };

    // ------------------------------------------------------------------ fetch state + engine pump
    const states = new Map();          // id -> per-request JavaScript state
    globalThis.__czFetchStates = states;

    // -----------------------------------------------------------------------
    // Text encoding (installed only when absent; runner keeps any existing globals)
    // -----------------------------------------------------------------------

    function byteSource(input) {
        if (input == null) return null;
        if (input instanceof Uint8Array || input instanceof Uint16Array || input instanceof Int32Array || input instanceof Float32Array) return input;
        if (typeof ArrayBuffer !== "undefined") {
            if (input instanceof ArrayBuffer) return new Uint8Array(input);
            if (typeof SharedArrayBuffer !== "undefined" && input instanceof SharedArrayBuffer) return new Uint8Array(input);
        }
        // Views (typed arrays AND DataView) expose their backing `buffer`, so a `DataView` slice
        // decodes as a zero-copy view over the same memory instead of an array-like character copy.
        var o = input.buffer || null;
        if (!o) return null;
        var off = Number(input.byteOffset) || 0;
        var len = Number(input.byteLength);
        if (Number.isFinite(len)) return new Uint8Array(o, off, Math.min(len, (o.byteLength || 0) - off));
        return new Uint8Array(o, off);
    }

    if (typeof globalThis.TextEncoder !== "function") {
        globalThis.TextEncoder = class TextEncoder {
            constructor() { this.encoding = "utf-8"; }
            encode(text) {
                // Phase 10C: single native UTF-8 pass — `__czUtf8Encode` transfers the encoded bytes
                // into the ArrayBuffer backing the view instead of pushing one JS number per code
                // point (the old char-by-char loop is gone).
                return __czUtf8Encode(text === undefined ? "" : String(text));
            }
            encodeInto(text, dest) {
                var bytes = this.encode(text);
                var view = byteSource(dest);
                var max = view ? Math.min(bytes.length, view.length) : 0;
                // Bulk copy through a view over the destination (no byte-by-byte JavaScript writes).
                if (view && max > 0) new Uint8Array(view.buffer, view.byteOffset, max).set(bytes.subarray(0, max));
                return { read: max, written: max };
            }
        };
    }

    if (typeof globalThis.TextDecoder !== "function") {
        globalThis.TextDecoder = class TextDecoder {
            constructor(label, options) {
                var l = String(label || "utf-8").toLowerCase();
                if (l !== "utf-8" && l !== "utf8") throw new TypeError("TextDecoder supports only utf-8 in cardputer-zero-llrt");
                this.encoding = "utf-8";
                var opts = options || {};
                this.fatal = !!opts.fatal;
                this.ignoreBOM = !!opts.ignoreBOM;
            }
            decode(input) {
                if (input == null) return "";
                var u8 = byteSource(input);
                if (!u8) return String(input);
                var start = 0;
                if (!this.ignoreBOM && u8.length >= 3 && u8[0] === 0xef && u8[1] === 0xbb && u8[2] === 0xbf) start = 3;
                // Phase 10C: ONE native pass over a borrowed slice of the buffer. `subarray` skips the
                // BOM as a view (zero copy) and `fatal` surfaces invalid UTF-8 as a real TypeError;
                // lossy mode mirrors Node's default decoder with U+FFFD replacements.
                return __czUtf8Decode(start === 0 ? u8 : u8.subarray(3), this.fatal);
            }
        };
    }

    function decodeText(bytes) {
        var dec = (typeof TextDecoder === "function") ? new TextDecoder("utf-8") : null;
        if (!dec) throw new TypeError("TextDecoder unavailable");
        return dec.decode(bytes);
    }

    function newState(id, signal) {
        const s = {
            id: id,
            headersResolved: false,
            headerResolve: null, headerReject: null,
            bodyPromise: null, bodyResolve: null, bodyReject: null,
            chunks: [],                  // streaming chunks awaiting a reader
            waitingReads: [],            // pending reader.read() resolvers
            closed: false,               // no more chunks will arrive (done/error)
            failure: null,               // terminal error object
            mode: "buffered",
            consumedChunks: 0,
            bufferedBytes: null,
            signal: signal || null,
            response: null,
            released: false,
        };
        s.bodyPromise = new Promise(function (res, rej) { s.bodyResolve = res; s.bodyReject = rej; });
        return s;
    }

    function releaseIfNeeded(s) {
        // Release once a terminal event arrived AND nothing is waiting on this slot anymore. Both
        // consumption styles finish here (buffered read + stream done/cancel), so the mode is not
        // part of the guard; leaking slots would keep the runner alive forever after SSE streams.
        if (!s.released && s.closed && s.waitingReads.length === 0) {
            s.released = true;
            __czFetchRelease(s.id);
        }
    }

    function rejectWaitingReads(s, err) {
        const pend = s.waitingReads.slice();
        s.waitingReads.length = 0;
        for (const w of pend) w.reject(err);
    }

    function deliver(s, ev) {
        if (ev.kind === "headers") {
            s.headersResolved = true;
            s.status = ev.status;
            s.statusText = ev.statusText;
            s.url = ev.url;
            s.redirected = !!ev.redirected;
            s.headerPairs = ev.headers || [];
            const hdr = new Headers();
            for (const pair of s.headerPairs) hdr.append(pair[0], pair[1]);
            s.headersObj = hdr;
            if (s.response) s.response._finalizeHeaders(ev.status, ev.statusText, ev.url, ev.redirected, hdr);
            if (s.headerResolve) s.headerResolve(s.response);
            return;
        }
        if (ev.kind === "chunk") {
            s.chunks.push(ev.value);
            drainIntoReaders(s);
            return;
        }
        if (ev.kind === "body") {
            s.bufferedBytes = new Uint8Array(ev.value);
            s.closed = true;
            s.bodyResolve(s.bufferedBytes);
            drainIntoReaders(s);
            releaseIfNeeded(s);
            return;
        }
        if (ev.kind === "done") {
            s.closed = true;
            drainIntoReaders(s);
            // Buffered consumers waiting on the folded chunks resolve from the collected prefix.
            if (s.mode === "buffered" && !s.bufferedBytes) finalizeBufferedFromChunks(s);
            releaseIfNeeded(s);
            return;
        }
        // error
        s.closed = true;
        let err;
        if (ev.aborted && s.signal && s.signal.reason !== undefined) err = s.signal.reason;
        else {
            err = new Error(ev.message || "fetch failed");
            err.name = ev.errorName || "TypeError";
        }
        if (ev.errorName === "AbortError" && !(s.signal && s.signal.aborted)) {
            const e = new Error("This operation was aborted");
            e.name = "AbortError";
            err = e;
        }
        s.failure = err;
        if (!s.headersResolved && s.headerReject) s.headerReject(err);
        else s.bodyReject(err);
        rejectWaitingReads(s, err);
        releaseIfNeeded(s);
    }

    function finalizeBufferedFromChunks(s) {
        let total = 0;
        for (const c of s.chunks) total += c.length;
        if (total > 16 * 1024 * 1024) {
            const e = new RangeError("Response body exceeded the buffered limit");
            s.bodyReject(e);
            return;
        }
        const out = new Uint8Array(total);
        let off = 0;
        for (const c of s.chunks) { out.set(c, off); off += c.length; }
        s.bufferedBytes = out;
        s.bodyResolve(out);
    }

    function drainIntoReaders(s) {
        while (s.waitingReads.length > 0 && s.chunks.length > 0) {
            const w = s.waitingReads.shift();
            w.resolve({ value: s.chunks.shift(), done: false });
        }
        if (s.closed && (s.failure || s.chunks.length === 0)) {
            const pend = s.waitingReads.slice();
            s.waitingReads.length = 0;
            if (s.failure) { for (const w of pend) w.reject(s.failure); }
            else { for (const w of pend) w.resolve({ value: undefined, done: true }); }
            releaseIfNeeded(s);
        }
    }

    globalThis.__czFetchDispatchId = function (id) {
        const s = states.get(id);
        if (!s) return 0;
        let n = 0;
        for (;;) {
            const ev = __czFetchNext(id);
            if (!ev) break;
            n++;
            deliver(s, ev);
        }
        return n;
    };

    globalThis.__czFetchDrainLoop = function () {
        const ids = __czFetchLiveIds();
        let n = 0;
        for (const id of ids) n += globalThis.__czFetchDispatchId(id);
        return n;
    };

    // ------------------------------------------------------------------ Request / Response
    function Request(input, init) {
        init = init || {};
        this.url = String(input);
        this.method = (init.method || "GET").toUpperCase();
        this.headers = new Headers(init.headers);
        this.redirect = init.redirect || "follow";
        this.signal = init.signal || null;
        this.tls = init.tls || null;
        this.body = init.body === undefined ? null : init.body;
    }

    function ReadableStream(state) {
        this._state = state;
        this.locked = false;
    }
    ReadableStream.prototype.getReader = function () {
        const s = this._state;
        if (this.locked) throw new TypeError("ReadableStream is already locked");
        this.locked = true;
        const self = this;
        // Streaming mode consumes the bounded engine queue (backpressure applies to the worker).
        if (!s.closed) __czFetchStream(s.id);
        s.mode = "stream";
        const reader = {
            read: function () {
                if (s.chunks.length > 0) return Promise.resolve({ value: s.chunks.shift(), done: false });
                if (s.failure) return Promise.reject(s.failure);
                if (s.closed) { releaseIfNeeded(s); return Promise.resolve({ value: undefined, done: true }); }
                return new Promise(function (resolve, reject) {
                    s.waitingReads.push({ resolve: resolve, reject: reject });
                });
            },
            cancel: function (reason) {
                s.chunks.length = 0;
                __czFetchCancel(s.id);
                const err = reason === undefined ? makeAbortError(undefined) : reason;
                rejectWaitingReads(s, err);
                s.failure = null;
                s.closed = true;
                releaseIfNeeded(s);
                return Promise.resolve();
            },
            releaseLock: function () { self.locked = false; },
        };
        return reader;
    };
    ReadableStream.prototype.cancel = function (reason) {
        const reader = this.getReader();
        return reader.cancel(reason).then(function () { reader.releaseLock(); });
    };
    ReadableStream.prototype[Symbol.asyncIterator] = function () {
        const reader = this.getReader();
        const it = {
            next: function () { return reader.read(); },
            return: function () {
                // `break` in for-await loops cancels cleanly.
                return reader.cancel().then(function () { reader.releaseLock(); return { value: undefined, done: true }; });
            },
            [Symbol.asyncIterator]: function () { return this; },
        };
        return it;
    };

    function Response(state) {
        this._state = state;
        this._stream = null;
        this.bodyUsed = false;
        this.status = 0;
        this.ok = false;
        this.headers = new Headers();
        this.url = "";
        this.redirected = false;
        this.statusText = "";
        // Phase 10C: cloned Responses share one buffered body, so each consumer needs its own
        // defensive copy. An uncloned single-use Response can hand out the received view directly.
        this._shared = false;
    }
    Response.prototype._finalizeHeaders = function (status, statusText, url, redirected, headers) {
        this.status = status;
        this.ok = status >= 200 && status < 300;
        this.statusText = statusText;
        this.url = url;
        this.redirected = !!redirected;
        this.headers = headers;
    };
    Object.defineProperty(Response.prototype, "body", {
        get: function () {
            if (!this._stream) {
                // First stream access switches the engine to streaming mode (prefix is replayed).
                const s = this._state;
                if (s.consumedChunks === 0 && !s.closed) __czFetchStream(s.id);
                s.mode = "stream";
                this._stream = new ReadableStream(s);
            }
            return this._stream;
        },
    });
    async function consumeBody(self) {
        const s = self._state;
        if (self.bodyUsed) throw new TypeError("Response body is already used");
        if (s.mode === "stream") throw new TypeError("Response body is already being streamed");
        self.bodyUsed = true;
        if (!s.bufferedBytes) __czFetchBuffered(s.id);
        await s.bodyPromise;
        releaseIfNeeded(s);
        return s.bufferedBytes;
    }
    Response.prototype.text = async function () {
        const bytes = await consumeBody(this);
        return decodeText(bytes);
    };
    Response.prototype.json = async function () {
        const bytes = await consumeBody(this);
        // Direct byte entry into cz-json: `>= 4 KiB` documents stay Lazy with ZERO intermediate JS
        // string, and small documents take the one-pass DOM. `JSON.parse` remains the fallback.
        if (typeof globalThis.__czJsonParseBytes === "function") return globalThis.__czJsonParseBytes(bytes);
        return globalThis.JSON.parse(decodeText(bytes));
    };
    Response.prototype.bytes = async function () {
        const bytes = await consumeBody(this);
        // Uncloned + single use: return the received Uint8Array itself (no second allocation).
        if (!this._shared) return bytes;
        const copy = new Uint8Array(bytes.length);
        copy.set(bytes, 0);
        return copy;
    };
    Response.prototype.arrayBuffer = async function () {
        const bytes = await consumeBody(this);
        // The engine's ArrayBuffer is already exact-sized, so an uncloned Response can move it out.
        if (!this._shared && bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength) {
            return bytes.buffer;
        }
        const buffer = new ArrayBuffer(bytes.length);
        const view = new Uint8Array(buffer);
        view.set(bytes, 0);
        return buffer;
    };
    Response.prototype.clone = function () {
        const s = this._state;
        if (this.bodyUsed || s.consumedChunks > 0) throw new TypeError("Response body already consumed; cannot clone");
        const c = new Response(s);
        c._finalizeHeaders(this.status, this.statusText, this.url, this.redirected, new Headers(this.headers));
        // Both sides now share one buffered body: force defensive copies on consumption.
        this._shared = true;
        c._shared = true;
        return c;
    };

    function normalizeBody(body, headers) {
        if (body === undefined || body === null) return null;
        if (typeof body === "string") {
            if (!headers.has("content-type")) headers.set("Content-Type", "text/plain;charset=UTF-8");
            return new TextEncoder().encode(body);
        }
        if (body instanceof URLSearchParams) {
            if (!headers.has("content-type")) headers.set("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8");
            return new TextEncoder().encode(body.toString());
        }
        if (typeof Uint8Array !== "undefined" && body instanceof Uint8Array) return body;
        if (body instanceof ArrayBuffer) return new Uint8Array(body);
        if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
        // Phase 12: Blob bodies hand out their owned bytes; FormData serializes multipart/form-data.
        if (typeof globalThis.FormData === "function" && body instanceof globalThis.FormData) {
            const boundary = globalThis.__czMultipartBoundary();
            if (!headers.has("content-type")) headers.set("Content-Type", "multipart/form-data; boundary=" + boundary);
            return body._serialize(boundary);
        }
        if (typeof globalThis.Blob === "function" && body instanceof globalThis.Blob) {
            if (!headers.has("content-type") && body.type) headers.set("Content-Type", body.type);
            return body._bytes;
        }
        if (typeof body === "object" && typeof body.toString === "function" && !headers.has("content-type")) {
            headers.set("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8");
            return new TextEncoder().encode(body.toString());
        }
        throw new TypeError("Unsupported fetch body type");
    }

    async function fetch(input, init) {
        let req;
        if (input instanceof Request) {
            req = input;
            if (init) {
                if (init.method) req.method = String(init.method).toUpperCase();
                if (init.headers) req.headers = new Headers(init.headers);
                if (init.body !== undefined) req.body = init.body;
                if (init.redirect) req.redirect = init.redirect;
                if (init.signal) req.signal = init.signal;
                if (init.tls) req.tls = init.tls;
            }
        } else {
            req = new Request(String(input), init);
        }
        const signal = req.signal;
        if (signal && signal.aborted) throw signal.reason === undefined ? makeAbortError(undefined) : signal.reason;

        // Phase 12: normalizeBody FIRST so Content-Type derived from Blob/FormData/urlencoded
        // bodies is present when the header pairs are serialized onto the wire.
        const bodyBytes = normalizeBody(req.body, req.headers);
        const pairs = [];
        req.headers.forEach(function (value, name) { pairs.push([name, value]); });
        const id = __czFetchStart(req.url, req.method, String(req.redirect || "follow"), pairs,
                                  bodyBytes === null ? null : bodyBytes,
                                  req.tls ? req.tls : {});
        const s = newState(id, signal);
        states.set(id, s);
        const response = new Response(s);
        s.response = response;

        // Engine events wake the event loop; this promise settles when headers arrive.
        const headerPromise = new Promise(function (resolve, reject) {
            s.headerResolve = resolve; s.headerReject = reject;
        });
        if (signal) {
            signal.addEventListener("abort", function () {
                __czFetchCancel(id);
                globalThis.__czFetchDispatchId(id);
            });
        }
        // Pump immediately so already-arrived events do not wait for the next loop pass.
        globalThis.__czFetchDispatchId(id);
        return await headerPromise;
    }

    globalThis.fetch = fetch;
    globalThis.Headers = Headers;
    globalThis.Request = Request;
    globalThis.Response = Response;
    globalThis.AbortController = AbortController;
    globalThis.AbortSignal = AbortSignal;
    globalThis.ReadableStream = ReadableStream;
    if (typeof globalThis.URLSearchParams === "undefined") globalThis.URLSearchParams = URLSearchParams;
    globalThis.__czFetchInstalled = true;
})();
"#;
