//! Phase 8C: Node-compatible `node:tls` (alias `tls`) **raw TLS sockets** on the `cz-net`
//! rustls/ring engine (`cz-net::tls_socket`). Zero Tokio, zero C TLS libraries; every socket and
//! server runs on a detached worker thread whose events (`secureConnect`, `secureConnection`,
//! `data`, `end`, `close`, `error`) wake `cz-runner`'s Condvar idle wait instantly through
//! `cz_hal::http_server::notify_event_loop()` — there are NO polling loops.
//!
//! Semantics mirrored from Node.js:
//! - `connect(options, callback?)` / `connect(port, host?, options?, callback?)` returns a
//!   `TLSSocket` immediately; the callback fires once on `"secureConnect"`.
//!   Properties: `encrypted` (always `true`), `authorized`, `alpnProtocol` (`string | false`),
//!   `remoteAddress`, `remotePort`, `destroyed`.
//! - `createServer(options, secureConnectionListener?)` validates `key`/`cert` EAGERLY (like
//!   `https.createServer`) and returns a `Server` with `listen(port, host?, cb?)` (default host
//!   `"0.0.0.0"`, ephemeral port 0 supported), `address()`, `close(cb?)` and events
//!   `secureConnection | listening | close | error`.
//! - `TLSSocket.write(chunk, cb?)` accepts `string | Uint8Array`; `end(chunk?, cb?)` flushes,
//!   sends TLS `close_notify` and half-closes TCP; `destroy()` tears down abruptly (only
//!   `"close"` follows). `"data"` payloads are `Uint8Array` until `setEncoding("utf8")`.
//! - TLS 1.3 mTLS rejection arrives as a post-handshake `"error"` followed by `"close"`.

use rquickjs::{Array, Ctx, Exception, Function, Object, Result, Value};

use crate::vec_to_uint8array;
use cz_net::tls::{TlsClientOptions, TlsServerOptions};
use cz_net::tls_socket::{
    close_tls_server, connect_tls, create_tls_server, next_tls_event, tls_active_count,
    tls_destroy, tls_end, tls_has_pending, tls_write, TlsEngineEvent,
};

/// Live raw TLS sockets + listening servers (event-loop liveness, spec §1.4).
pub fn tls_active_count_js() -> usize {
    tls_active_count()
}

/// True while engine events await the JavaScript drain.
pub fn tls_has_pending_js() -> bool {
    tls_has_pending()
}

/// Append one JS value (string / Uint8Array / ArrayBuffer) as bytes — same rule as `node:http`.
fn push_bytes<'js>(ctx: &Ctx<'js>, v: Value<'js>, out: &mut Vec<u8>) -> Result<()> {
    crate::http_module::push_value_bytes(ctx, v, out)
}

/// Optional byte-valued TLS option (null/undefined -> None).
fn opt_tls_bytes<'js>(ctx: &Ctx<'js>, v: Value<'js>) -> Result<Option<Vec<u8>>> {
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let mut out = Vec::new();
    push_bytes(ctx, v, &mut out)?;
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// ALPN list: array of strings/byte values -> Vec<Vec<u8>>.
fn alpn_from_array<'js>(ctx: &Ctx<'js>, arr: &Array<'js>) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for v in arr.iter::<Value>() {
        let item = v?;
        if item.is_undefined() || item.is_null() {
            continue;
        }
        let mut bytes = Vec::new();
        push_bytes(ctx, item, &mut bytes)?;
        if !bytes.is_empty() {
            out.push(bytes);
        }
    }
    Ok(out)
}

/// Convert a JS options object into `TlsServerOptions` (shared by validate + listen paths).
fn tls_server_opts_from_js<'js>(ctx: &Ctx<'js>, opts: &Object<'js>) -> Result<TlsServerOptions> {
    let key = opt_tls_bytes(ctx, opts.get::<_, Value>("key")?)?.ok_or_else(|| {
        Exception::throw_message(ctx, "tls.createServer requires a 'key' PEM private key")
    })?;
    let cert = opt_tls_bytes(ctx, opts.get::<_, Value>("cert")?)?.ok_or_else(|| {
        Exception::throw_message(ctx, "tls.createServer requires a 'cert' PEM certificate chain")
    })?;
    let ca = opt_tls_bytes(ctx, opts.get::<_, Value>("ca")?)?;
    let request_cert = opts.get::<_, Option<bool>>("requestCert")?.unwrap_or(false);
    let reject_unauthorized = opts.get::<_, Option<bool>>("rejectUnauthorized")?.unwrap_or(true);
    // rustls mandates-or-none (measured Phase 8B limitation): Node's verify-if-present combo
    // behaves as requestCert && rejectUnauthorized.
    let require_client_auth = request_cert && reject_unauthorized;
    let alpn_protocols = match opts.get::<_, Option<Array>>("ALPNProtocols")? {
        Some(arr) => alpn_from_array(ctx, &arr)?,
        None => Vec::new(),
    };
    Ok(TlsServerOptions {
        cert_pem: cert,
        key_pem: key,
        ca_pem: ca,
        require_client_auth,
        alpn_protocols,
    })
}

/// Convert the JS connect descriptor into `TlsClientOptions`.
fn tls_client_opts_from_js<'js>(ctx: &Ctx<'js>, desc: &Object<'js>) -> Result<TlsClientOptions> {
    let ca = opt_tls_bytes(ctx, desc.get::<_, Value>("ca")?)?;
    let cert = opt_tls_bytes(ctx, desc.get::<_, Value>("cert")?)?;
    let key = opt_tls_bytes(ctx, desc.get::<_, Value>("key")?)?;
    let reject = desc.get::<_, Option<bool>>("rejectUnauthorized")?.unwrap_or(true);
    let alpn = match desc.get::<_, Option<Array>>("ALPNProtocols")? {
        Some(arr) => alpn_from_array(ctx, &arr)?,
        None => Vec::new(),
    };
    Ok(TlsClientOptions {
        ca_pem: ca,
        cert_pem: cert,
        key_pem: key,
        reject_unauthorized: reject,
        alpn_protocols: alpn,
    })
}

/// One engine event -> the plain descriptor object consumed by the JS pump.
fn tls_event_descriptor<'js>(ctx: &Ctx<'js>, ev: TlsEngineEvent) -> Result<Value<'js>> {
    let obj = Object::new(ctx.clone())?;
    match ev {
        TlsEngineEvent::SecureConnect {
            socket_id,
            alpn_protocol,
            authorized,
            remote_address,
            remote_port,
        } => {
            obj.set("kind", "secure")?;
            obj.set("socketId", socket_id as f64)?;
            obj.set("alpn", alpn_protocol.as_deref().unwrap_or(""))?;
            obj.set("authorized", authorized)?;
            obj.set("remoteAddress", remote_address.as_str())?;
            obj.set("remotePort", remote_port as f64)?;
        }
        TlsEngineEvent::ServerConnection {
            server_id,
            socket_id,
            alpn_protocol,
            authorized,
            remote_address,
            remote_port,
        } => {
            obj.set("kind", "srvconn")?;
            obj.set("serverId", server_id as f64)?;
            obj.set("socketId", socket_id as f64)?;
            obj.set("alpn", alpn_protocol.as_deref().unwrap_or(""))?;
            obj.set("authorized", authorized)?;
            obj.set("remoteAddress", remote_address.as_str())?;
            obj.set("remotePort", remote_port as f64)?;
        }
        TlsEngineEvent::Data { socket_id, bytes } => {
            obj.set("kind", "data")?;
            obj.set("socketId", socket_id as f64)?;
            // Phase 10C: ownership move (no second memcpy of the decrypted bytes).
            obj.set("value", vec_to_uint8array(ctx, bytes)?)?;
        }
        TlsEngineEvent::End { socket_id } => {
            obj.set("kind", "end")?;
            obj.set("socketId", socket_id as f64)?;
        }
        TlsEngineEvent::Close { socket_id } => {
            obj.set("kind", "close")?;
            obj.set("socketId", socket_id as f64)?;
        }
        TlsEngineEvent::Error { socket_id, message } => {
            obj.set("kind", "error")?;
            obj.set("socketId", socket_id as f64)?;
            obj.set("message", message.as_str())?;
        }
        TlsEngineEvent::ServerError { server_id, message } => {
            obj.set("kind", "serverError")?;
            obj.set("serverId", server_id as f64)?;
            obj.set("message", message.as_str())?;
        }
    }
    Ok(obj.into_value())
}

/// Register the Rust primitives + JS shim on this context (idempotent via `__czTlsInstalled`).
fn install_tls_glue<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czTlsConnect",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, desc: Object<'js>| -> Result<f64> {
                let host: String = desc.get::<_, Option<String>>("host")?.unwrap_or_else(|| "localhost".to_string());
                let port: f64 = desc.get::<_, f64>("port")?;
                if !port.is_finite() || port < 0.0 || port > 65535.0 {
                    return Err(Exception::throw_message(&ctx, "tls.connect requires a numeric 'port' between 0 and 65535"));
                }
                let servername = match desc.get::<_, Value>("servername")? {
                    v if v.is_undefined() || v.is_null() => None,
                    v => {
                        if let Some(s) = v.as_string() {
                            Some(s.to_string()?)
                        } else {
                            None
                        }
                    }
                };
                let opts = tls_client_opts_from_js(&ctx, &desc)?;
                match connect_tls(opts, host, port as u16, servername) {
                    Ok(id) => Ok(id as f64),
                    Err(msg) => Err(Exception::throw_message(&ctx, &msg)),
                }
            },
        )?,
    )?;

    ctx.globals().set(
        "__czTlsValidateServer",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, opts: Object<'js>| -> Result<bool> {
            let tls_opts = tls_server_opts_from_js(&ctx, &opts)?;
            // Build the rustls ServerConfig eagerly (PEM parse + key/cert match + mTLS verifier)
            // and discard it: invalid configurations throw here, like `https.createServer`.
            cz_net::tls::build_server_config(&tls_opts)
                .map(|_| true)
                .map_err(|msg| Exception::throw_message(&ctx, &format!("tls.createServer configuration error: {msg}")))
        })?,
    )?;

    ctx.globals().set(
        "__czTlsListen",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, port: f64, host: String, opts: Object<'js>| -> Result<Object<'js>> {
                let tls_opts = tls_server_opts_from_js(&ctx, &opts)?;
                let p = if port.is_finite() && port >= 0.0 && port <= 65535.0 { port as u16 } else { 0 };
                let (id, address, bound_port) = create_tls_server(&tls_opts, &host, p)
                    .map_err(|msg| Exception::throw_message(&ctx, &msg))?;
                let info = Object::new(ctx.clone())?;
                info.set("id", id as f64)?;
                info.set("address", address.as_str())?;
                info.set("family", "IPv4")?;
                info.set("port", bound_port as f64)?;
                Ok(info)
            },
        )?,
    )?;

    ctx.globals().set(
        "__czTlsWrite",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, chunk: Value<'js>| -> Result<bool> {
                let mut bytes = Vec::new();
                push_bytes(&ctx, chunk, &mut bytes)?;
                Ok(tls_write(id as u64, bytes))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czTlsEnd",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, chunk: Value<'js>| -> Result<bool> {
                let mut bytes = Vec::new();
                if !chunk.is_undefined() && !chunk.is_null() {
                    push_bytes(&ctx, chunk, &mut bytes)?;
                }
                Ok(tls_end(id as u64, bytes))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czTlsDestroy",
        Function::new(ctx.clone(), |id: f64| -> bool { tls_destroy(id as u64) })?,
    )?;

    ctx.globals().set(
        "__czTlsCloseServer",
        Function::new(ctx.clone(), |id: f64| -> bool { close_tls_server(id as u64) })?,
    )?;

    ctx.globals().set(
        "__czTlsNext",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Value<'js>> {
            match next_tls_event() {
                None => Ok(Value::new_null(ctx.clone())),
                Some(ev) => tls_event_descriptor(&ctx, ev),
            }
        })?,
    )?;

    ctx.globals().set(
        "__czTlsActiveCount",
        Function::new(ctx.clone(), || -> f64 { tls_active_count_js() as f64 })?,
    )?;
    ctx.globals().set(
        "__czTlsPending",
        Function::new(ctx.clone(), || -> bool { tls_has_pending_js() })?,
    )?;

    ctx.eval::<(), _>(TLS_SHIM)?;
    Ok(())
}

/// QuickJS-safe shim (no optional chaining / nullish coalescing): TLSSocket + Server classes, a
/// deferred listener registry keyed by engine ids, and `__czTlsDrainLoop` for the runner pump.
const TLS_SHIM: &str = r#"
(function () {
    if (globalThis.__czTlsInstalled) { return; }

    const tlsSockets = new Map();   // engine socket id -> TLSSocket
    const tlsServers = new Map();   // engine server id -> Server
    globalThis.__czTlsSockets = tlsSockets;
    globalThis.__czTlsServers = tlsServers;

    function czReportError(e) {
        const msg = (e && e.stack) ? e.stack : String((e && e.message) || e);
        if (globalThis.console && console.error) console.error(msg);
        else if (globalThis.__cz_print) __cz_print(2, [msg]);
    }

    function bytesOf(v) {
        if (v == null) return null;
        if (typeof v === "string") {
            if (typeof TextEncoder === "function") return new TextEncoder().encode(v);
            const out = [];
            for (let i = 0; i < v.length; i++) {
                let cp = v.codePointAt(i);
                if (cp > 0xffff) i++;
                if (cp < 0x80) out.push(cp);
                else if (cp < 0x800) out.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
                else if (cp < 0x10000) out.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
                else out.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
            }
            return new Uint8Array(out);
        }
        if (typeof Uint8Array !== "undefined" && v instanceof Uint8Array) return v;
        if (typeof ArrayBuffer !== "undefined") {
            if (v instanceof ArrayBuffer) return new Uint8Array(v);
            if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
        }
        // Arrays of PEM strings/bytes concatenate, exactly like Node's `ca: [...]`.
        if (Array.isArray(v)) {
            const parts = [];
            for (const part of v) {
                const b = bytesOf(part);
                if (b) parts.push(b);
            }
            let total = 0;
            for (const p of parts) total += p.length;
            const merged = new Uint8Array(total);
            let off = 0;
            for (const p of parts) { merged.set(p, off); off += p.length; }
            return merged;
        }
        return null;
    }

    function decodeUtf8(bytes) {
        if (typeof TextDecoder === "function") return new TextDecoder("utf-8").decode(bytes);
        let s = "";
        for (let i = 0; i < bytes.length; i++) {
            const b = bytes[i];
            if (b < 0x80) s += String.fromCharCode(b);
            else if ((b & 0xe0) === 0xc0) { const c2 = bytes[++i]; s += String.fromCharCode(((b & 0x1f) << 6) | (c2 & 0x3f)); }
            else if ((b & 0xf0) === 0xe0) { const c2 = bytes[++i], c3 = bytes[++i]; s += String.fromCharCode(((b & 0x0f) << 12) | ((c2 & 0x3f) << 6) | (c3 & 0x3f)); }
            else { const c2 = bytes[++i], c3 = bytes[++i], c4 = bytes[++i];
                   const cp = ((b & 0x07) << 18) | ((c2 & 0x3f) << 12) | ((c3 & 0x3f) << 6) | (c4 & 0x3f);
                   const hi = 0xd800 + (((cp - 0x10000) >> 10) & 0x3ff), lo = 0xdc00 + ((cp - 0x10000) & 0x3ff);
                   s += String.fromCharCode(hi, lo); }
        }
        return s;
    }

    // ------------------------------------------------------------------ TLSSocket
    class CzTlsSocket {
        constructor(info) {
            this._id = info.id;
            this.encrypted = true;
            this.authorized = !!info.authorized;
            this.alpnProtocol = info.alpn ? String(info.alpn) : false;
            this.remoteAddress = info.remoteAddress || "";
            this.remotePort = info.remotePort || 0;
            this.destroyed = false;
            this.readable = true;
            this.writable = true;
            this._enc = undefined;
            this._lis = { secureConnect: [], data: [], end: [], close: [], error: [] };
            this._endFired = false;
            this._closeFired = false;
        }
        setEncoding(enc) { this._enc = String(enc); return this; }
        on(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const key = String(ev);
            if (!this._lis[key]) this._lis[key] = [];
            this._lis[key].push(fn);
            return this;
        }
        once(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const self = this;
            const key = String(ev);
            const wrap = function (arg) { self.off(key, wrap); fn.call(self, arg); };
            return this.on(key, wrap);
        }
        off(ev, fn) {
            const key = String(ev);
            const list = this._lis[key];
            if (!list) return this;
            const pos = list.indexOf(fn);
            if (pos >= 0) list.splice(pos, 1);
            return this;
        }
        removeListener(ev, fn) { return this.off(ev, fn); }
        _fire(ev, arg) {
            for (const fn of (this._lis[String(ev)] || []).slice()) {
                try { fn.call(this, arg); } catch (e) { czReportError(e); }
            }
        }
        _emitEngine(ev, arg) {
            if (ev === "end") {
                if (this._endFired) return;
                this._endFired = true;
                this.readable = false;
            }
            if (ev === "close") {
                if (this._closeFired) return;
                this._closeFired = true;
                this.destroyed = true;
                this.readable = false;
                this.writable = false;
                tlsSockets.delete(this._id);
            }
            this._fire(ev, arg);
        }
        write(chunk, cb) {
            if (typeof chunk === "function") { cb = chunk; chunk = undefined; }
            if (this.destroyed || !this.writable) {
                if (typeof cb === "function") { cb(new Error("write after end")); }
                return false;
            }
            const ok = __czTlsWrite(this._id, chunk === undefined ? null : chunk);
            if (typeof cb === "function") { try { cb(); } catch (e) { czReportError(e); } }
            return ok === true;
        }
        end(chunk, cb) {
            if (typeof chunk === "function") { cb = chunk; chunk = undefined; }
            if (!this.destroyed && this.writable) {
                this.writable = false;
                __czTlsEnd(this._id, chunk === undefined ? null : chunk);
            }
            if (typeof cb === "function") { try { cb(); } catch (e) { czReportError(e); } }
            return this;
        }
        destroy() {
            if (!this.destroyed) {
                this.destroyed = true;
                this.writable = false;
                __czTlsDestroy(this._id);
            }
            return this;
        }
    }

    // ------------------------------------------------------------------ Server
    class CzTlsServer {
        constructor(options) {
            this._opts = options === undefined || options === null ? {} : options;
            this._ev = { secureConnection: [], listening: [], close: [], error: [] };
            this._id = null;
            this._addr = null;
        }
        on(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const key = String(ev);
            if (!this._ev[key]) this._ev[key] = [];
            this._ev[key].push(fn);
            return this;
        }
        once(ev, fn) {
            if (typeof fn !== "function") throw new TypeError("listener must be a function");
            const self = this;
            const key = String(ev);
            const wrap = function (arg) { self.off(key, wrap); fn.call(self, arg); };
            return this.on(key, wrap);
        }
        off(ev, fn) {
            const key = String(ev);
            const list = this._ev[key];
            if (!list) return this;
            const pos = list.indexOf(fn);
            if (pos >= 0) list.splice(pos, 1);
            return this;
        }
        removeListener(ev, fn) { return this.off(ev, fn); }
        emit(ev, arg) {
            for (const fn of (this._ev[String(ev)] || []).slice()) {
                try { fn.call(this, arg); } catch (e) { czReportError(e); }
            }
            return true;
        }
        listen(port, a, b) {
            let host = "0.0.0.0";   // Node-style default: reachable on every interface
            let cb = null;
            if (typeof a === "function") cb = a;
            else if (typeof a === "string") { host = a; if (typeof b === "function") cb = b; }
            const p = Number(port) || 0;
            let info;
            try {
                info = __czTlsListen(p, host, this._opts);
            } catch (e) {
                this.emit("error", e);
                czReportError(e);
                return this;
            }
            this._id = info.id;
            tlsServers.set(info.id, this);
            this._addr = { address: info.address, family: info.family, port: info.port };
            if (cb) { try { cb.call(this); } catch (e) { czReportError(e); } }
            this.emit("listening");
            return this;
        }
        address() { return this._addr; }
        close(cb) {
            if (cb !== undefined && typeof cb !== "function") throw new TypeError("callback must be a function");
            if (this._id !== null) {
                tlsServers.delete(this._id);
                __czTlsCloseServer(this._id);   // stops accepting + closes open connections
                this._id = null;
            }
            this._addr = null;
            this.emit("close");
            if (cb) { try { cb(); } catch (e) { czReportError(e); } }
            return this;
        }
    }

    // ------------------------------------------------------------------ connect / createServer
    function connect() {
        const args = Array.prototype.slice.call(arguments);
        let port = null, host = null, opts = null, cb = null;
        for (const a of args) {
            if (typeof a === "number") { if (port === null) port = a; }
            else if (typeof a === "string") { if (host === null) host = a; }
            else if (typeof a === "function") cb = a;
            else if (a !== undefined && a !== null && typeof a === "object") opts = a;
        }
        if (opts) {
            if (opts.port !== undefined) port = Number(opts.port);
            if (opts.host !== undefined) host = String(opts.host);
        }
        if (host === null) host = "localhost";
        if (port === null || !Number.isFinite(port)) throw new TypeError("tls.connect requires a numeric 'port'");
        let servername = host;
        if (opts && opts.servername !== undefined) servername = opts.servername === null ? null : String(opts.servername);
        // IP literals must not send SNI (Node behavior); the Rust engine maps them anyway.
        if (/^\d+\.\d+\.\d+\.\d+$/.test(host) && !(opts && opts.servername !== undefined)) servername = null;

        const desc = {
            host: host,
            port: Number(port),
            servername: servername,
            ca: opts && opts.ca !== undefined ? bytesOf(opts.ca) : null,
            cert: opts && opts.cert !== undefined ? bytesOf(opts.cert) : null,
            key: opts && opts.key !== undefined ? bytesOf(opts.key) : null,
            rejectUnauthorized: !(opts && opts.rejectUnauthorized === false),
            ALPNProtocols: opts && Array.isArray(opts.ALPNProtocols) ? opts.ALPNProtocols : [],
        };
        const id = __czTlsConnect(desc);
        const sock = new CzTlsSocket({ id: id, remoteAddress: host, remotePort: Number(port) });
        tlsSockets.set(id, sock);
        if (typeof cb === "function") sock.once("secureConnect", function () { cb.call(sock); });
        return sock;
    }

    function createServer(options, listener) {
        // Eager option validation (spec §1.2/§1.3): invalid PEM/key throws immediately.
        globalThis.__czTlsValidateServer(options === undefined || options === null ? {} : options);
        const srv = new CzTlsServer(options);
        if (typeof listener === "function") srv.on("secureConnection", listener);
        return srv;
    }

    // ------------------------------------------------------------------ engine dispatch pump
    function czDispatchTls(ev) {
        const kind = ev.kind;
        if (kind === "secure") {
            const s = tlsSockets.get(ev.socketId);
            if (!s) return false;
            s.authorized = !!ev.authorized;
            s.alpnProtocol = ev.alpn ? String(ev.alpn) : false;
            if (ev.remoteAddress) s.remoteAddress = String(ev.remoteAddress);
            if (ev.remotePort) s.remotePort = Number(ev.remotePort);
            s._emitEngine("secureConnect");
            return true;
        }
        if (kind === "srvconn") {
            const server = tlsServers.get(ev.serverId);
            const sock = new CzTlsSocket({
                id: ev.socketId,
                authorized: !!ev.authorized,
                alpn: ev.alpn,
                remoteAddress: ev.remoteAddress,
                remotePort: ev.remotePort,
            });
            tlsSockets.set(ev.socketId, sock);
            if (server) server.emit("secureConnection", sock);
            return true;
        }
        if (kind === "data") {
            const s = tlsSockets.get(ev.socketId);
            if (!s) return false;
            let chunk = ev.value;
            const enc = String(s._enc || "").toLowerCase();
            if (enc === "utf8" || enc === "utf-8") chunk = decodeUtf8(chunk);
            s._emitEngine("data", chunk);
            return true;
        }
        if (kind === "end") {
            const s = tlsSockets.get(ev.socketId);
            if (s) s._emitEngine("end");
            return true;
        }
        if (kind === "close") {
            const s = tlsSockets.get(ev.socketId);
            if (s) s._emitEngine("close");
            return true;
        }
        if (kind === "error") {
            const err = new Error(String(ev.message || "TLS error"));
            err.name = "Error";
            const s = tlsSockets.get(ev.socketId);
            if (s) s._emitEngine("error", err);
            return true;
        }
        if (kind === "serverError") {
            const server = tlsServers.get(ev.serverId);
            if (server) {
                const err = new Error(String(ev.message || "TLS error"));
                err.name = "Error";
                server.emit("error", err);
            }
            return true;
        }
        return false;
    }

    globalThis.__czTlsDispatchOne = czDispatchTls;
    globalThis.__czTlsDrainLoop = function () {
        let n = 0;
        for (;;) {
            const ev = __czTlsNext();
            if (!ev) break;
            n++;
            czDispatchTls(ev);
        }
        return n;
    };

    globalThis.__czTlsConnectFn = connect;
    globalThis.__czTlsCreateServerFn = createServer;
    globalThis.__czTlsClassSocket = CzTlsSocket;
    globalThis.__czTlsClassServer = CzTlsServer;
    globalThis.__czTlsInstalled = true;
})();
"#;

/// `node:tls` / `tls` module definition: named exports AND a `default` export object with the
/// Node symbol shape (`connect`, `createServer`, `TLSSocket`, `Server`).
pub struct TlsModule;

impl rquickjs::module::ModuleDef for TlsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["connect", "createServer", "TLSSocket", "Server", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_tls_glue(ctx)?;

        let connect_fn = ctx.globals().get::<_, Function>("__czTlsConnectFn")?;
        let create_fn = ctx.globals().get::<_, Function>("__czTlsCreateServerFn")?;
        let socket_cls = ctx.globals().get::<_, Function>("__czTlsClassSocket")?;
        let server_cls = ctx.globals().get::<_, Function>("__czTlsClassServer")?;

        exports.export("connect", connect_fn.clone())?;
        exports.export("createServer", create_fn.clone())?;
        exports.export("TLSSocket", socket_cls.clone())?;
        exports.export("Server", server_cls.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("connect", connect_fn)?;
        default_obj.set("createServer", create_fn)?;
        default_obj.set("TLSSocket", socket_cls)?;
        default_obj.set("Server", server_cls)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}
