//! Global WHATWG `URL` class + `"node:url"` / `"url"` builtin module.
//!
//! Parsing runs in Rust (`cz_net::url`, one owned href buffer, span-backed accessors, zero
//! intermediate `String`s per component); the JavaScript side only holds the component RECORD and
//! re-composes `href` when a property is assigned. `url.searchParams` is a LIVE view in both
//! directions exactly like Node: mutating it rewrites `.search`/`.href`, and assigning `.search`
//! refreshes the SAME params object in place.

use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::{Ctx, Function, Object, Result, Value};

use cz_net::parse_url;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install `globalThis.URL` (and make sure `URLSearchParams` exists) idempotently.
pub fn install_url<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ensure_url_search_params(ctx)?;
    let glue_done = ctx.globals().get::<_, bool>("__czUrlInstalled").unwrap_or(false);
    if INSTALLED.load(Ordering::SeqCst) && glue_done {
        return Ok(());
    }

    // Both arguments arrive as Values: `null`/`undefined` bases must not force a String conversion.
    let parse_native = Function::new(ctx.clone(), |ctx: Ctx<'js>, input: Value<'js>, base: Value<'js>| -> Result<Value<'js>> {
        let s = match input.as_string() { Some(v) => v.to_string()?, None => String::new() };
        let b = match base.as_string() {
            Some(v) => Some(v.to_string()?),
            None => None, // undefined / null / non-string: no base
        };
        url_parse_into(&ctx, &s, b.as_deref())
    })?;
    ctx.globals().set("__czUrlParse", parse_native)?;

    ctx.eval::<(), _>(URL_SHIM)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Build the component record object for a parsed URL, or `null` when invalid.
fn url_parse_into<'js>(ctx: &Ctx<'js>, input: &str, base: Option<&str>) -> Result<Value<'js>> {
    let owned = match parse_url(input, base) {
        Ok(u) => u,
        Err(_) => return Ok(Value::new_null(ctx.clone())),
    };
    let v = owned.view();
    let obj = Object::new(ctx.clone())?;
    obj.set("href", v.href())?;
    obj.set("protocol", v.protocol())?;
    obj.set("username", v.username())?;
    obj.set("password", v.password())?;
    // `hostname` keeps IPv6 brackets (`[::1]`), matching Node.
    obj.set("hostname", v.hostname())?;
    obj.set("port", v.port())?;
    obj.set("host", v.host().as_ref())?;
    obj.set("pathname", v.pathname())?;
    obj.set("search", v.search())?;
    obj.set("hash", v.hash())?;
    obj.set("origin", v.origin().as_ref())?;
    obj.set("special", v.is_special())?;
    obj.set("opaque", v.is_opaque())?;
    obj.set("hasAuthority", v.has_authority())?;
    Ok(obj.into_value())
}

/// `URLSearchParams` comes from the fetch glue; if fetch was never installed, evaluate just that
/// part of its shim so `node:url` still exports a working class.
fn ensure_url_search_params<'js>(ctx: &Ctx<'js>) -> Result<()> {
    if ctx.globals().get::<_, Value>("URLSearchParams").is_ok() {
        return Ok(());
    }
    ctx.eval::<(), _>(USP_FALLBACK)?;
    Ok(())
}

/// Minimal-but-complete `URLSearchParams` fallback (same shape as the fetch shim's class).
const USP_FALLBACK: &str = r#"
(function () {
    if (typeof globalThis.URLSearchParams !== "undefined") return;
    // WHATWG application/x-www-form-urlencoded decoding NEVER throws on a bare/invalid `%`
    // (Node parity: `new URLSearchParams("?q=100%").get("q") === "100%"`). Valid %XX runs are
    // percent-decoded, '+' becomes a space, anything else stays literal.
    function formDecode(s) {
        let out = "";
        const re = /%[0-9A-Fa-f]{2}(?:%[0-9A-Fa-f]{2})*|\+/g;
        let last = 0, m;
        while ((m = re.exec(s))) {
            out += s.slice(last, m.index);
            if (m[0] === "+") out += " ";
            else {
                try { out += decodeURIComponent(m[0]); } catch (_e) { out += m[0]; } // invalid UTF-8 stays raw
            }
            last = m.index + m[0].length;
        }
        return out + s.slice(last);
    }
    function parseInto(pairs, text) {
        pairs.length = 0;
        if (!text) return;
        const parts = String(text).replace(/^\?/, "").split("&");
        for (const p of parts) {
            if (!p) continue;
            const eq = p.indexOf("=");
            const k = eq < 0 ? p : p.slice(0, eq);
            const v = eq < 0 ? "" : p.slice(eq + 1);
            pairs.push([formDecode(k), formDecode(v)]);
        }
    }
    function URLSearchParams(init) {
        this._pairs = [];
        if (typeof init === "string" || init instanceof URLSearchParams) parseInto(this._pairs, String(init));
        else if (init && typeof init[Symbol.iterator] === "function") {
            for (const e of init) this._pairs.push([String(e[0]), String(e[1])]);
        }
    }
    URLSearchParams.prototype.append = function (k, v) { this._pairs.push([String(k), String(v)]); };
    URLSearchParams.prototype.set = function (k, v) {
        let placed = false; const key = String(k); const val = String(v);
        const out = [];
        for (const p of this._pairs) { if (p[0] !== key) out.push(p); else if (!placed) { out.push([key, val]); placed = true; } }
        if (!placed) out.push([key, val]);
        this._pairs = out;
    };
    URLSearchParams.prototype.get = function (k) { for (const p of this._pairs) if (p[0] === k) return p[1]; return null; };
    URLSearchParams.prototype.getAll = function (k) { const o = []; for (const p of this._pairs) if (p[0] === k) o.push(p[1]); return o; };
    URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
    URLSearchParams.prototype.delete = function (k) { this._pairs = this._pairs.filter((p) => p[0] !== k); };
    URLSearchParams.prototype.sort = function () { this._pairs.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); };
    URLSearchParams.prototype.toString = function () {
        const enc = (s) => encodeURIComponent(s).replace(/%20/g, "+");
        return this._pairs.map((p) => enc(p[0]) + "=" + enc(p[1])).join("&");
    };
    URLSearchParams.prototype.forEach = function (cb, thisArg) { for (const p of this._pairs) cb.call(thisArg, p[1], p[0], this); };
    URLSearchParams.prototype.keys = function () { return this._pairs.map((p) => p[0])[Symbol.iterator](); };
    URLSearchParams.prototype.values = function () { return this._pairs.map((p) => p[1])[Symbol.iterator](); };
    URLSearchParams.prototype.entries = function () { return this._pairs.map((p) => [p[0], p[1]])[Symbol.iterator](); };
    URLSearchParams.prototype[Symbol.iterator] = function () { return this.entries(); };
    globalThis.URLSearchParams = URLSearchParams;
})();
"#;

/// The `URL` class itself: getters read the Rust-parsed RECORD; setters re-compose `href`.
const URL_SHIM: &str = r##"
(function () {
    if (globalThis.__czUrlInstalled) return;
    const DEFAULT_PORTS = { "http:": "80", "ws:": "80", "https:": "443", "wss:": "443", "ftp:": "21" };

    function URL(input, base) {
        if (!(this instanceof URL)) throw new TypeError("Constructor URL requires 'new'");
        const s = String(input && typeof input.href === "string" ? input.href : input);
        const b = (base === undefined || base === null) ? null : String(base && typeof base.href === "string" ? base.href : base);
        const p = __czUrlParse(s, b);
        if (p === null) throw new TypeError("Invalid URL");
        this._f = p;
        this._params = null;
    }

    // Re-compose the href from the stored record (hierarchical or opaque form).
    function compose(f) {
        let out = f.protocol;
        if (f.opaque) return out + f.pathname + f.search + f.hash;
        out += "//";
        if (f.username) {
            out += f.username;
            if (f.password) out += ":" + f.password;
            out += "@";
        }
        out += f.hostname;
        if (f.port) out += ":" + f.port;
        out += f.pathname + f.search + f.hash;
        return out;
    }

    function refreshHost(f) {
        // `.host` is host[:port]; a default port stays elided like Node does.
        let host = f.hostname;
        if (f.port && f.port !== (DEFAULT_PORTS[f.protocol] || "")) host += ":" + f.port;
        f.host = host;
    }

    function queryOf(search) { return search ? search.replace(/^\?/, "") : ""; }

    function buildParams(url) {
        const usp = new URLSearchParams(queryOf(url._f.search));
        // Live view: any mutation rewrites the parent URL's .search (and therefore .href).
        const flush = () => { url._f.search = usp.toString() ? "?" + usp.toString() : ""; refreshHost(url._f); url._href = compose(url._f); };
        const orig = { append: usp.append, set: usp.set, delete: usp.delete, sort: usp.sort };
        usp.append = function (k, v) { orig.append.call(this, k, v); flush(); };
        usp.set = function (k, v) { orig.set.call(this, k, v); flush(); };
        usp.delete = function (k) { orig.delete.call(this, k); flush(); };
        usp.sort = function () { orig.sort.call(this); flush(); };
        url._flushParams = flush;
        return usp;
    }

    function reparseInto(f, href) {
        const p = __czUrlParse(href, null);
        if (p === null) throw new TypeError("Invalid URL");
        f.protocol = p.protocol; f.username = p.username; f.password = p.password;
        f.hostname = p.hostname; f.port = p.port; f.host = p.host;
        f.pathname = p.pathname; f.search = p.search; f.hash = p.hash;
        f.origin = p.origin; f.opaque = p.opaque; f.hasAuthority = p.hasAuthority; f.special = p.special;
    }

    Object.defineProperty(URL.prototype, "href", {
        get() { return this._href === undefined ? (this._href = compose(this._f)) : this._href; },
        set(v) { reparseInto(this._f, String(v)); this._href = this._f.href; if (this._params) refillParams(this._params, queryOf(this._f.search)); },
    });
    Object.defineProperty(URL.prototype, "protocol", {
        get() { return this._f.protocol; },
        set(v) { const s = String(v).replace(/:$/, "") + ":"; reparseInto(this._f, s + compose(this._f).slice(this._f.protocol.length)); this._href = this._f.href; },
    });
    Object.defineProperty(URL.prototype, "username", { get() { return this._f.username; }, set(v) { this._f.username = String(v); refreshHost(this._f); this._href = compose(this._f); } });
    Object.defineProperty(URL.prototype, "password", { get() { return this._f.password; }, set(v) { this._f.password = String(v); this._href = compose(this._f); } });
    Object.defineProperty(URL.prototype, "host", {
        get() { return this._f.host; },
        set(v) {
            const s = String(v);
            const m = s.match(/^(\[[^\]]*\]|[^:]*)(?::(\d*))?$/);
            if (!m) throw new TypeError("Invalid host");
            this._f.hostname = m[1]; this._f.port = m[2] === undefined || m[2] === "" ? "" : (m[2] === (DEFAULT_PORTS[this._f.protocol] || "") ? "" : m[2]);
            refreshHost(this._f); this._href = compose(this._f);
        },
    });
    Object.defineProperty(URL.prototype, "hostname", {
        get() { return this._f.hostname; },
        set(v) { this._f.hostname = String(v); refreshHost(this._f); this._href = compose(this._f); },
    });
    Object.defineProperty(URL.prototype, "port", {
        get() { return this._f.port; },
        set(v) { const s = String(v); this._f.port = s === "" || s === (DEFAULT_PORTS[this._f.protocol] || "") ? "" : s; refreshHost(this._f); this._href = compose(this._f); },
    });
    Object.defineProperty(URL.prototype, "pathname", { get() { return this._f.pathname; }, set(v) { this._f.pathname = String(v); this._href = compose(this._f); } });
    Object.defineProperty(URL.prototype, "search", {
        get() { return this._f.search; },
        // Assigning .search must REFRESH THE SAME live params object in place (Node identity).
        set(v) {
            let s = String(v);
            if (s && s[0] !== "?") s = "?" + s;
            this._f.search = s;
            this._href = compose(this._f);
            if (this._params) refillParams(this._params, queryOf(s));
        },
    });
    Object.defineProperty(URL.prototype, "hash", {
        get() { return this._f.hash; },
        set(v) { let s = String(v); if (s && s[0] !== "#") s = "#" + s; this._f.hash = s === "#" ? "" : s; this._href = compose(this._f); },
    });
    Object.defineProperty(URL.prototype, "origin", { get() { return this._f.origin; } });
    Object.defineProperty(URL.prototype, "searchParams", {
        get() { if (!this._params) this._params = buildParams(this); return this._params; },
    });

    function refillParams(usp, text) {
        const fresh = new URLSearchParams(text);
        usp._pairs = fresh._pairs;
    }

    URL.prototype.toString = function () { return this.href; };
    URL.prototype.toJSON = function () { return this.href; };
    URL.prototype[Symbol.for("nodejs.util.inspect.custom")] = function () { return this.href; };

    URL.canParse = function (input, base) {
        try {
            const s = String(input && typeof input.href === "string" ? input.href : input);
            const b = (base === undefined || base === null) ? null : String(base && typeof base.href === "string" ? base.href : base);
            return __czUrlParse(s, b) !== null;
        } catch (_) { return false; }
    };
    URL.parse = function (input, base) {
        try { return new URL(input, base); } catch (_) { return null; }
    };

    globalThis.URL = URL;
    globalThis.__czUrlInstalled = true;
})();
"##;

/// Builtin module shape for `"node:url"` / `"url"`: exports `{ URL, URLSearchParams, default }`.
pub struct UrlModule;

impl rquickjs::module::ModuleDef for UrlModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["URL", "URLSearchParams", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_url(ctx)?;
        let url_cls = ctx.globals().get::<_, Value>("URL")?;
        let usp_cls = ctx.globals().get::<_, Value>("URLSearchParams")?;
        exports.export("URL", url_cls.clone())?;
        exports.export("URLSearchParams", usp_cls.clone())?;
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("URL", url_cls)?;
        default_obj.set("URLSearchParams", usp_cls)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}
