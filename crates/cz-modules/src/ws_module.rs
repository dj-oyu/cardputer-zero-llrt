//! Global RFC 6455 `WebSocket` client (`ws://` + `wss://`) for `cz-runner`.
//!
//! The engine lives in `cz_net::ws` (std sockets + rustls/ring, 256 KiB worker stacks, stack frame
//! headers, in-place masking, zero-copy binary moves). This module only bridges native calls and a
//! small JavaScript class that carries browser-shaped state (`readyState`, `protocol`,
//! `binaryType`, `bufferedAmount`, `on*` handlers, `addEventListener`).

use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::function::Opt;
use rquickjs::{Array, Ctx, Exception, Function, Object, Result, Value};

use cz_net::{close_ws, next_ws_event, send_ws, terminate_ws, ws_buffered_amount, ws_connect, WsEngineEvent};

use crate::tls_module::tls_client_opts_from_js;
use crate::{bytes_from_value, vec_to_uint8array};

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install `globalThis.WebSocket` plus its native bridge idempotently.
pub fn install_ws<'js>(ctx: &Ctx<'js>) -> Result<()> {
    if INSTALLED.load(Ordering::SeqCst) && ctx.globals().get::<_, bool>("__czWsInstalled").unwrap_or(false) {
        return Ok(());
    }

    ctx.globals().set(
        "__czWsConnect",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, href: String, protocols: Opt<Value<'js>>, tls: Opt<Value<'js>>| -> Result<f64> {
                let mut list: Vec<String> = Vec::new();
                if let Some(pv) = protocols.as_ref() {
                    if let Some(s) = pv.as_string() {
                        list.push(s.to_string()?);
                    } else if let Ok(arr) = Array::from_value(pv.clone()) {
                        for i in 0..arr.len() {
                            if let Ok(v) = arr.get::<Value>(i) {
                                if let Some(s) = v.as_string() {
                                    list.push(s.to_string()?);
                                }
                            }
                        }
                    }
                }
                let opts = match tls.as_ref() {
                    Some(tv) => match Object::from_value(tv.clone()) {
                        Ok(o) => tls_client_opts_from_js(&ctx, &o)?,
                        Err(_) => Default::default(),
                    },
                    None => Default::default(),
                };
                match ws_connect(&href, list, opts) {
                    Ok(id) => Ok(id as f64),
                    Err(msg) => Err(Exception::throw_message(&ctx, &msg)),
                }
            },
        )?,
    )?;

    ctx.globals().set(
        "__czWsSend",
        Function::new(
            ctx.clone(),
            |id: f64, data: Value<'_>, opcode: f64| -> Result<bool> {
                // Strings arrive as UTF-8 bytes; binary arrives as Uint8Array / ArrayBuffer.
                let bytes = match data.as_string() {
                    Some(s) => s.to_string()?.into_bytes(),
                    None => bytes_from_value(&data)?,
                };
                Ok(send_ws(id as u64, bytes, opcode as u8))
            },
        )?,
    )?;

    ctx.globals().set(
        "__czWsClose",
        Function::new(
            ctx.clone(),
            |id: f64, code: f64, reason: String| -> bool {
                close_ws(id as u64, code as u16, reason)
            },
        )?,
    )?;

    ctx.globals().set("__czWsTerminate", Function::new(ctx.clone(), |id: f64| -> bool {
        terminate_ws(id as u64)
    })?)?;

    ctx.globals().set("__czWsBuffered", Function::new(ctx.clone(), |id: f64| -> f64 {
        ws_buffered_amount(id as u64) as f64
    })?)?;

    ctx.globals().set("__czWsActiveCount", Function::new(ctx.clone(), || -> usize { cz_net::ws_active_count() })?)?;
    ctx.globals().set("__czWsPending", Function::new(ctx.clone(), || -> bool { cz_net::ws_has_pending() })?)?;

    ctx.globals().set(
        "__czWsNext",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Value<'js>> {
            match next_ws_event() {
                None => Ok(Value::new_null(ctx)),
                Some(ev) => ws_event_descriptor(&ctx, ev),
            }
        })?,
    )?;

    ctx.eval::<(), _>(WS_SHIM)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Liveness for the runner's event loop (`WebSocket` objects still open).
pub fn ws_active_count() -> usize {
    cz_net::ws_active_count()
}

/// True while engine events wait for the JavaScript drain.
pub fn ws_has_pending() -> bool {
    cz_net::ws_has_pending()
}

/// Engine event -> plain descriptor object (binary payloads MOVE into a Uint8Array: zero copies).
fn ws_event_descriptor<'js>(ctx: &Ctx<'js>, ev: WsEngineEvent) -> Result<Value<'js>> {
    let obj = Object::new(ctx.clone())?;
    match ev {
        WsEngineEvent::Open { ws_id, protocol } => {
            obj.set("kind", "open")?;
            obj.set("wsId", ws_id as f64)?;
            obj.set("protocol", protocol.as_str())?;
        }
        WsEngineEvent::Text { ws_id, text } => {
            obj.set("kind", "text")?;
            obj.set("wsId", ws_id as f64)?;
            obj.set("text", text.as_str())?;
        }
        WsEngineEvent::Binary { ws_id, bytes } => {
            obj.set("kind", "binary")?;
            obj.set("wsId", ws_id as f64)?;
            obj.set("value", vec_to_uint8array(ctx, bytes)?)?;
        }
        WsEngineEvent::Close { ws_id, code, reason, was_clean } => {
            obj.set("kind", "close")?;
            obj.set("wsId", ws_id as f64)?;
            obj.set("code", code as f64)?;
            obj.set("reason", reason.as_str())?;
            obj.set("wasClean", was_clean)?;
        }
        WsEngineEvent::Error { ws_id, message } => {
            obj.set("kind", "error")?;
            obj.set("wsId", ws_id as f64)?;
            obj.set("message", message.as_str())?;
        }
    }
    Ok(obj.into_value())
}

/// The browser-shaped `WebSocket` class. Engine pumps land events through `__czWsDrainLoop`.
const WS_SHIM: &str = r#"
(function () {
    if (globalThis.__czWsInstalled) return;

    const CONNECTING = 0, OPEN = 1, CLOSING = 2, CLOSED = 3;

    function hrefOf(v) {
        if (v && typeof v.href === "string") return v.href;
        return String(v);
    }

    function WebSocket(url, protocolsOrOptions) {
        if (!(this instanceof WebSocket)) throw new TypeError("Constructor WebSocket requires 'new'");
        let protocols = [];
        let tls = null;
        if (typeof protocolsOrOptions === "string") protocols = [protocolsOrOptions];
        else if (Array.isArray(protocolsOrOptions)) protocols = protocolsOrOptions.map(String);
        else if (protocolsOrOptions && typeof protocolsOrOptions === "object") {
            tls = protocolsOrOptions.tls || null;
            const p = protocolsOrOptions.protocols;
            if (typeof p === "string") protocols = [p];
            else if (Array.isArray(p)) protocols = p.map(String);
        }
        const href = hrefOf(url);
        if (!/^wss?:\/\//i.test(href)) throw new TypeError("WebSocket URL must be ws:// or wss://");
        this.url = href;
        this.readyState = CONNECTING;
        this.protocol = "";
        this.binaryType = "arraybuffer"; // Web default; "uint8array" also supported
        this.extensions = "";
        this.onopen = null;
        this.onmessage = null;
        this.onerror = null;
        this.onclose = null;
        this._listeners = {};
        this._id = __czWsConnect(href, protocols, tls ? tls : {});
        // Register BEFORE the event loop can pump: the Open event must find this JS object.
        globalThis.__czWsById.set(this._id, this);
    }

    Object.defineProperty(WebSocket.prototype, "bufferedAmount", {
        get() { return this._id === undefined ? 0 : __czWsBuffered(this._id); },
    });

    WebSocket.prototype.send = function (data) {
        if (this.readyState === CONNECTING) throw new InvalidStateError("WebSocket is not open: readyState 0 (CONNECTING)");
        if (this.readyState === CLOSED) return; // browsers silently drop sends after CLOSED
        let opcode;
        let payload;
        if (typeof data === "string") { opcode = 1; payload = data; }
        else if (data instanceof ArrayBuffer) { opcode = 2; payload = new Uint8Array(data); }
        else if (ArrayBuffer.isView(data)) { opcode = 2; payload = data; }
        else { opcode = 1; payload = String(data); }
        if (!__czWsSend(this._id, payload, opcode)) throw new Error("WebSocket send failed");
    };

    WebSocket.prototype.close = function (code, reason) {
        if (this.readyState === CLOSED) return;
        if (this.readyState === CONNECTING) {
            // Browsers abort a CONNECTING socket without a wire handshake.
            this.readyState = CLOSED;
            __czWsTerminate(this._id);
            this._dispatch("close", { code: 1006, reason: "", wasClean: false });
            return;
        }
        if (code === undefined && reason === undefined) { code = 1000; reason = ""; }
        if (code !== 1000 && (code < 3000 || code > 4999)) throw new InvalidStateError("close() code " + code + " is not overridable");
        const r = reason === undefined ? "" : String(reason);
        this.readyState = CLOSING;
        __czWsClose(this._id, code, r);
    };

    WebSocket.prototype.addEventListener = function (type, fn) {
        if (typeof fn !== "function") return;
        const k = String(type);
        (this._listeners[k] = this._listeners[k] || []).push(fn);
    };
    WebSocket.prototype.removeEventListener = function (type, fn) {
        const k = String(type);
        const list = this._listeners[k];
        if (!list) return;
        const i = list.indexOf(fn);
        if (i >= 0) list.splice(i, 1);
    };
    WebSocket.prototype._dispatch = function (type, event) {
        event.type = type;
        const handler = this["on" + type];
        if (typeof handler === "function") handler.call(this, event);
        const list = this._listeners[type];
        if (list) for (const fn of list.slice()) fn.call(this, event);
    };

    function InvalidStateError(msg) {
        const e = new Error(msg);
        e.name = "InvalidStateError";
        return e;
    }

    WebSocket.CONNECTING = CONNECTING;
    WebSocket.OPEN = OPEN;
    WebSocket.CLOSING = CLOSING;
    WebSocket.CLOSED = CLOSED;

    globalThis.__czWsDispatchOne = function (ev) {
        const ws = globalThis.__czWsById && globalThis.__czWsById.get(ev.wsId);
        if (!ws) return false; // GC'd socket: engine events for dead objects are dropped
        switch (ev.kind) {
            case "open":
                ws.readyState = OPEN;
                ws.protocol = ev.protocol || "";
                ws._dispatch("open", {});
                return true;
            case "text":
                if (ws.readyState !== OPEN) return true;
                ws._dispatch("message", { data: ev.text });
                return true;
            case "binary": {
                if (ws.readyState !== OPEN) return true;
                let data = ev.value; // native Uint8Array over the moved buffer (zero copy)
                if (ws.binaryType === "arraybuffer") data = data.buffer;
                else if (ws.binaryType === "blob" && typeof Blob !== "undefined") data = new Blob([data]);
                ws._dispatch("message", { data });
                return true;
            }
            case "close":
                ws.readyState = CLOSED;
                if (globalThis.__czWsById) globalThis.__czWsById.delete(ev.wsId);
                ws._dispatch("close", { code: ev.code, reason: ev.reason, wasClean: ev.wasClean });
                return true;
            case "error":
                ws._dispatch("error", { message: ev.message });
                return true;
            default:
                return false;
        }
    };

    globalThis.__czWsDrainLoop = function () {
        let n = 0;
        for (;;) {
            const ev = __czWsNext();
            if (ev === null || ev === undefined) break;
            n++;
            globalThis.__czWsDispatchOne(ev);
        }
        return n;
    };

    // Live-socket registry so engine events locate their JS objects; entries die with the socket.
    globalThis.__czWsById = new Map();

    globalThis.WebSocket = WebSocket;
    globalThis.__czWsClass = WebSocket;
    globalThis.__czWsInstalled = true;
})();
"#;

/// Builtin module shape for `"node:ws"` / `"ws"`: exports `{ WebSocket, default }`.
pub struct WsModule;

impl rquickjs::module::ModuleDef for WsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["WebSocket", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_ws(ctx)?;
        let cls = ctx.globals().get::<_, Value>("WebSocket")?;
        exports.export("WebSocket", cls.clone())?;
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("WebSocket", cls)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}
