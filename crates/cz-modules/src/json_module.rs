//! Phase 5 bridge: replaces `globalThis.JSON.parse` / `globalThis.JSON.stringify` with the
//! `cz-json` Cortex-A53 engine and exposes the `cardputer:json` module (`parseLazy`, `isLazy`,
//! `lazyStats` + a `default` export object).
//!
//! Routing happens entirely in Rust while JS Proxy traps stay thin:
//! * Documents `< 4096` bytes parse through the borrowed one-pass DOM (clean strings alias input).
//! * Documents `>= 4096` bytes build an immutable `Arc<[u8]>` + 16-byte `TapeNode` arena and hand
//!   back a transparent `Proxy` (over `{}` or `[]` so `Array.isArray` holds): reads never
//!   materialize siblings, writes store Copy-on-Write edits in Rust, and untouched subtrees keep
//!   their exact source bytes for zero-reparse `JSON.stringify`.
//! * `cardputer:kv` row handles (tagged `Symbol.for("cz.kvRow")`) serialize straight from the
//!   native `EntryPayload::Record` — no JS property walk at all.
//! * `replacer` / `space` / `reviver` calls delegate to the saved original QuickJS functions.

use std::borrow::Cow;
use std::ffi::c_void;

use rquickjs::function::Args;
use rquickjs::{Array, Ctx, CString, Exception, Function, Object, Result, Symbol, Value};

use cz_hal::kv::KvValue;
use cz_json::lazy::{Got, LazyDoc};
use cz_json::simd::{write_escaped_str, write_i64};
use cz_json::val::Val;
use cz_json::LAZY_THRESHOLD_BYTES;

/// Tag written onto Lazy proxy targets (`Symbol.for` = identical across realms).
const LAZY_TAG_KEY: &str = "cz.lazyJson";
/// Tag the KV shim writes onto `row()` proxy targets.
const KV_ROW_TAG_KEY: &str = "cz.kvRow";
/// Generic JS walk depth cap (protects against pathological graphs).
const MAX_WALK_DEPTH: usize = 200;

/// JS glue: saves the originals, replaces `JSON.parse`/`JSON.stringify`, and builds tagged
/// transparent Lazy proxies whose traps forward every string-key operation to Rust natives.
const JSON_SHIM: &str = r#"
(function () {
    if (globalThis.__czJsonInstalledV5) return;
    const LAZY = Symbol.for("cz.lazyJson");
    globalThis.__czMiss = {};                        // sentinel meaning "no such member"
    globalThis.__czOrigParse = JSON.parse;           // reviver fallback
    globalThis.__czOrigStringify = JSON.stringify;   // replacer / space fallback

    globalThis.__czLazyMake = function (id, node) {
        const isArray = __czLazyIsArray(id, node);
        const t = isArray ? [] : {};                 // Proxy over [] keeps Array.isArray true
        Reflect.set(t, LAZY, { id: id, node: node });
        const handler = {
            get: function (t, k) {
                if (typeof k !== "string") return Reflect.get(t, k);
                const v = __czLazyGet(id, node, k);
                if (v === globalThis.__czMiss) return Reflect.get(t, k); // prototype members
                return v;
            },
            set: function (t, k, v) {
                if (typeof k !== "string") return Reflect.set(t, k, v);
                __czLazySet(id, node, k, v);
                return true;
            },
            deleteProperty: function (t, k) {
                if (typeof k !== "string") return Reflect.deleteProperty(t, k);
                __czLazyDel(id, node, k);
                return true;
            },
            has: function (t, k) {
                if (typeof k !== "string") return Reflect.has(t, k);
                if (__czLazyHas(id, node, k)) return true;
                return Reflect.has(t, k);
            },
            ownKeys: function (t) {
                const keys = __czLazyOwnKeys(id, node);
                for (const tk of Reflect.ownKeys(t)) {
                    let seen = false;
                    for (const x of keys) { if (x === tk) { seen = true; break; } }
                    if (!seen) keys.push(tk);
                }
                return keys;
            },
            getOwnPropertyDescriptor: function (t, k) {
                if (typeof k !== "string") return Reflect.getOwnPropertyDescriptor(t, k);
                const d = __czLazyDesc(id, node, k);
                if (d !== undefined) return d;
                return Reflect.getOwnPropertyDescriptor(t, k);
            },
        };
        return new Proxy(t, handler);
    };

    function coerceText(v) {
        if (typeof v === "string") return v;
        if (typeof v === "symbol") throw new TypeError("Cannot convert a Symbol value to a string");
        return String(v);
    }

    JSON.parse = function (text) {
        const reviver = arguments.length > 1 ? arguments[1] : undefined;
        if (typeof reviver === "function") return globalThis.__czOrigParse(text, reviver);
        return __czJsonParseNative(coerceText(text));
    };

    JSON.stringify = function (value) {
        const replacer = arguments.length > 1 ? arguments[1] : undefined;
        const space = arguments.length > 2 ? arguments[2] : undefined;
        if (replacer !== undefined && replacer !== null) return globalThis.__czOrigStringify(value, replacer, space);
        if (space !== undefined && space !== null) return globalThis.__czOrigStringify(value, replacer, space);
        return __czJsonStringifyNative(value);
    };

    globalThis.__czJsonInstalledV5 = true;
})();
"#;

/// Install the engine on `globalThis` (idempotent per context). Called by `cz-runner` at startup
/// and whenever `cardputer:json` is evaluated.
pub fn install_json<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.eval::<(), _>(JSON_SHIM)?;

    // ---- parse native -------------------------------------------------------
    let parse_native = Function::new(ctx.clone(), |ctx: Ctx<'js>, text: String| -> Result<Value<'js>> {
        do_parse(&ctx, text.as_str())
    })?;
    ctx.globals().set("__czJsonParseNative", parse_native)?;

    // ---- Lazy handle natives ------------------------------------------------
    let is_array = Function::new(ctx.clone(), |id: f64, node: f64| -> bool {
        match cz_json::lazy::doc(as_idx(id)) {
            Some(doc) => cz_json::tape::KIND_ARRAY == doc.tape.node(as_idx(node)).kind,
            None => false,
        }
    })?;
    ctx.globals().set("__czLazyIsArray", is_array)?;

    let get_fn = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, id: f64, node: f64, key: String| -> Result<Value<'js>> {
            let doc = need_doc(&ctx, id)?;
            let n = doc.tape.node(as_idx(node));
            if n.kind == cz_json::tape::KIND_ARRAY && key == "length" {
                return Ok(Value::new_float(ctx, n.child_count() as f64));
            }
            let got = doc.get(as_idx(node), key.as_str());
            got_to_js(&ctx, &doc, got, as_idx(node))
        },
    )?;
    ctx.globals().set("__czLazyGet", get_fn)?;

    let set_fn = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, id: f64, node: f64, key: String, val: Value<'js>| -> Result<()> {
            let doc = need_doc(&ctx, id)?;
            let owned = js_to_val(&ctx, &val, 0)?;
            doc.set(as_idx(node), key.as_str(), owned);
            Ok(())
        },
    )?;
    ctx.globals().set("__czLazySet", set_fn)?;

    let del_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, id: f64, node: f64, key: String| -> bool {
        match need_doc(&ctx, id) {
            Ok(doc) => doc.del(as_idx(node), key.as_str()),
            Err(_) => false,
        }
    })?;
    ctx.globals().set("__czLazyDel", del_fn)?;

    let has_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, id: f64, node: f64, key: String| -> bool {
        match need_doc(&ctx, id) {
            Ok(doc) => {
                let n = doc.tape.node(as_idx(node));
                (n.kind == cz_json::tape::KIND_ARRAY && key == "length") || doc.has(as_idx(node), key.as_str())
            }
            Err(_) => false,
        }
    })?;
    ctx.globals().set("__czLazyHas", has_fn)?;

    let keys_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, id: f64, node: f64| -> Result<Array<'js>> {
        let doc = need_doc(&ctx, id)?;
        let arr = Array::new(ctx)?;
        for (i, k) in doc.own_keys(as_idx(node)).iter().enumerate() {
            arr.set(i, k.as_str())?;
        }
        Ok(arr)
    })?;
    ctx.globals().set("__czLazyOwnKeys", keys_fn)?;

    let desc_fn = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, id: f64, node: f64, key: String| -> Result<Value<'js>> {
            let doc = need_doc(&ctx, id)?;
            let n = doc.tape.node(as_idx(node));
            if n.kind == cz_json::tape::KIND_ARRAY && key == "length" {
                // Mirror the target's non-configurable writable length property (invariant-safe).
                let d = Object::new(ctx.clone())?;
                d.set("value", n.child_count() as f64)?;
                d.set("writable", true)?;
                d.set("enumerable", false)?;
                d.set("configurable", false)?;
                return Ok(d.into_value());
            }
            if !doc.has(as_idx(node), key.as_str()) {
                return Ok(Value::new_undefined(ctx));
            }
            let got = doc.get(as_idx(node), key.as_str());
            let value = got_to_js(&ctx, &doc, got, as_idx(node))?;
            let d = Object::new(ctx.clone())?;
            d.set("value", value)?;
            d.set("writable", true)?;
            d.set("enumerable", true)?;
            d.set("configurable", true)?;
            Ok(d.into_value())
        },
    )?;
    ctx.globals().set("__czLazyDesc", desc_fn)?;

    // ---- stringify native ---------------------------------------------------
    let stringify_native = Function::new(ctx.clone(), |ctx: Ctx<'js>, v: Value<'js>| -> Result<Value<'js>> {
        do_stringify(&ctx, &v)
    })?;
    ctx.globals().set("__czJsonStringifyNative", stringify_native)?;

    // Diagnostics used by the module exports.
    let lazy_stats_glue = Function::new(ctx.clone(), |ctx: Ctx<'js>, v: Value<'js>| -> Result<Value<'js>> {
        stats_object(&ctx, &v)
    })?;
    ctx.globals().set("__czLazyStatsNative", lazy_stats_glue)?;

    Ok(())
}

fn as_idx(v: f64) -> u32 {
    if v.is_finite() && v >= 0.0 && v <= u32::MAX as f64 {
        v as u32
    } else {
        u32::MAX
    }
}

fn need_doc<'js>(ctx: &Ctx<'js>, id: f64) -> Result<std::sync::Arc<LazyDoc>> {
    cz_json::lazy::doc(as_idx(id)).ok_or_else(|| Exception::throw_type(ctx, "stale lazy JSON handle"))
}

// ---------------------------------------------------------------------------
// JSON.parse routing
// ---------------------------------------------------------------------------

fn do_parse<'js>(ctx: &Ctx<'js>, text: &str) -> Result<Value<'js>> {
    let bytes = text.as_bytes();
    if bytes.len() < LAZY_THRESHOLD_BYTES {
        // Fast path: one strict pass straight into JS values (clean strings alias the input).
        match cz_json::parse_dom(bytes) {
            Ok(dom) => dom_to_js(ctx, &dom),
            Err(e) => Err(Exception::throw_syntax(ctx, &e.to_string())),
        }
    } else {
        match LazyDoc::build(bytes) {
            Ok(doc) => {
                let id = cz_json::lazy::register(doc);
                make_lazy_handle(ctx, id, 0)
            }
            Err(e) => Err(Exception::throw_syntax(ctx, &e.to_string())),
        }
    }
}

/// `cardputer:json.parseLazy` — force a Lazy Tape handle regardless of document size.
fn parse_lazy<'js>(ctx: &Ctx<'js>, text: &str) -> Result<Value<'js>> {
    match LazyDoc::build(text.as_bytes()) {
        Ok(doc) => {
            let id = cz_json::lazy::register(doc);
            make_lazy_handle(ctx, id, 0)
        }
        Err(e) => Err(Exception::throw_syntax(ctx, &e.to_string())),
    }
}

fn is_lazy<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> Result<bool> {
    if let Some((id, node)) = lazy_tag(ctx, v)? {
        if let Some(doc) = cz_json::lazy::doc(id) {
            // A handle counts as "lazy" only while untouched (Copy-on-Write makes it real).
            return Ok(doc.is_clean() && doc.subtree_is_clean(node));
        }
    }
    Ok(false)
}

fn stats_object<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> Result<Value<'js>> {
    let (id, node) = match lazy_tag(ctx, v)? {
        Some(t) => t,
        None => return Ok(Value::new_null(ctx.clone())),
    };
    let doc = match cz_json::lazy::doc(id) {
        Some(d) => d,
        None => return Ok(Value::new_null(ctx.clone())),
    };
    let obj = Object::new(ctx.clone())?;
    obj.set("nodeCount", doc.node_count() as f64)?;
    obj.set("rawBytes", doc.raw_bytes() as f64)?;
    obj.set("materialized", !doc.subtree_is_clean(node))?;
    Ok(obj.into_value())
}

// ---------------------------------------------------------------------------
// Tag lookups through proxies (symbol keys pass straight to the target)
// ---------------------------------------------------------------------------

fn lazy_tag<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Result<Option<(u32, u32)>> {
    if !value.is_object() || value.is_function() {
        return Ok(None);
    }
    let obj = value.as_object().expect("object view");
    let sym = Symbol::new_global(ctx.clone(), LAZY_TAG_KEY)?;
    let tag: Option<Object<'js>> = obj.get(sym).unwrap_or(None);
    let tag = match tag {
        Some(t) => t,
        None => return Ok(None),
    };
    let id = tag.get::<_, Option<f64>>("id")?.unwrap_or(f64::NAN);
    let node = tag.get::<_, Option<f64>>("node")?.unwrap_or(f64::NAN);
    if id.is_finite() && node.is_finite() && id >= 0.0 && node >= 0.0 {
        Ok(Some((id as u32, node as u32)))
    } else {
        Ok(None)
    }
}

fn kv_row_tag<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Result<Option<(String, String)>> {
    if !value.is_object() || value.is_function() {
        return Ok(None);
    }
    let obj = value.as_object().expect("object view");
    let sym = Symbol::new_global(ctx.clone(), KV_ROW_TAG_KEY)?;
    let tag: Option<Object<'js>> = obj.get(sym).unwrap_or(None);
    let tag = match tag {
        Some(t) => t,
        None => return Ok(None),
    };
    let store = tag.get::<_, Option<String>>("store")?.unwrap_or_default();
    let key = tag.get::<_, Option<String>>("key")?.unwrap_or_default();
    if store.is_empty() || key.is_empty() {
        Ok(None)
    } else {
        Ok(Some((store, key)))
    }
}

fn make_lazy_handle<'js>(ctx: &Ctx<'js>, id: u32, node: u32) -> Result<Value<'js>> {
    let make: Function<'js> = ctx.globals().get("__czLazyMake")?;
    make.call((id as f64, node as f64))
}

// ---------------------------------------------------------------------------
// Value conversion
// ---------------------------------------------------------------------------

/// Build JS values from a DOM tree. `undefined` never appears in JSON, so nothing maps to it.
fn dom_to_js<'js>(ctx: &Ctx<'js>, v: &Val<'_>) -> Result<Value<'js>> {
    match v {
        Val::Null => Ok(Value::new_null(ctx.clone())),
        Val::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        Val::Int(i) => Ok(Value::new_float(ctx.clone(), *i as f64)),
        Val::Float(f) => Ok(Value::new_float(ctx.clone(), *f)),
        Val::Str(s) => Ok(rquickjs::String::from_str(ctx.clone(), s.as_ref())?.into_value()),
        Val::Arr(items) => {
            let arr = Array::new(ctx.clone())?;
            for (i, it) in items.iter().enumerate() {
                arr.set(i, dom_to_js(ctx, it)?)?;
            }
            Ok(arr.into_value())
        }
        Val::Obj(fields) => {
            let obj = Object::new(ctx.clone())?;
            // Duplicate keys resolve to the LAST member because properties assign in order.
            for (k, val) in fields.iter() {
                obj.set(k.as_ref(), dom_to_js(ctx, val)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

/// Copy a JS value into an owned tree for Copy-on-Write storage (functions/undefined become null).
fn js_to_val<'js>(ctx: &Ctx<'js>, v: &Value<'js>, depth: usize) -> Result<Val<'static>> {
    if depth > MAX_WALK_DEPTH {
        return Err(Exception::throw_range(ctx, "Maximum nesting depth exceeded"));
    }
    if v.is_undefined() || v.is_null() || v.is_function() || v.is_symbol() {
        return Ok(Val::Null);
    }
    if v.is_bool() {
        return Ok(Val::Bool(v.as_bool().unwrap_or(false)));
    }
    if v.is_number() {
        return Ok(Val::Float(v.get::<f64>()?));
    }
    if v.is_string() {
        let s = CString::from_string(v.clone().into_string().expect("string view"))?;
        return Ok(Val::Str(Cow::Owned(s.as_str().to_string())));
    }
    let obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return Ok(Val::Null),
    };
    if obj.is_array() {
        let arr = obj.into_array().expect("array view");
        let len = arr.len();
        let mut items = Vec::with_capacity(len);
        for i in 0..len {
            let it = arr.get::<Value>(i)?;
            items.push(js_to_val(ctx, &it, depth + 1)?);
        }
        return Ok(Val::Arr(items));
    }
    let mut fields = Vec::new();
    for p in obj.props::<String, Value>() {
        let (k, val) = p?;
        if val.is_undefined() || val.is_function() || val.is_symbol() {
            continue;
        }
        fields.push((Cow::Owned(k), js_to_val(ctx, &val, depth + 1)?));
    }
    Ok(Val::Obj(fields))
}

fn got_to_js<'js>(ctx: &Ctx<'js>, doc: &LazyDoc, got: Got<'_>, parent: u32) -> Result<Value<'js>> {
    match got {
        Got::Missing => ctx.globals().get::<_, Value>("__czMiss"),
        Got::Scalar(s) => match s {
            cz_json::Scalar::Null => Ok(Value::new_null(ctx.clone())),
            cz_json::Scalar::Bool(b) => Ok(Value::new_bool(ctx.clone(), b)),
            cz_json::Scalar::Int(i) => Ok(Value::new_float(ctx.clone(), i as f64)),
            cz_json::Scalar::Float(f) => Ok(Value::new_float(ctx.clone(), f)),
            cz_json::Scalar::Clean(s) => Ok(rquickjs::String::from_str(ctx.clone(), s)?.into_value()),
            cz_json::Scalar::Escaped(s) => Ok(rquickjs::String::from_str(ctx.clone(), &s)?.into_value()),
        },
        // Direct members of the ROOT container keep transparent Proxy handles so CoW write
        // traps stay live. Grandchildren+ of untouched subtrees materialize ONCE into plain
        // native objects built straight from the Tape — no Proxy/handler churn per read.
        Got::Subtree(child) => {
            if parent == 0 || !doc.subtree_is_clean(child) {
                return make_lazy_handle(
                    ctx,
                    doc.id.load(std::sync::atomic::Ordering::SeqCst),
                    child,
                );
            }
            match materialize_native(ctx, doc, child) {
                Ok(v) => Ok(v),
                Err(_) => make_lazy_handle(ctx, doc.id.load(std::sync::atomic::Ordering::SeqCst), child),
            }
        }
        Got::Owned(v) => dom_to_js(ctx, &v),
    }
}

fn materialize_native<'js>(ctx: &Ctx<'js>, doc: &LazyDoc, node: u32) -> Result<Value<'js>> {
    let t = &doc.tape;
    let n = t.node(node);
    if n.kind == cz_json::tape::KIND_ARRAY {
        let arr = Array::new(ctx.clone())?;
        for i in 0..n.child_count() {
            if let Some(c) = t.child_at(node, i) {
                arr.set(i, materialize_native(ctx, doc, c)?)?;
            }
        }
        Ok(arr.into_value())
    } else if n.kind == cz_json::tape::KIND_OBJECT {
        let obj = Object::new(ctx.clone())?;
        for c in t.children(node) {
            if let Some(k) = t.key_bytes(c) {
                if let Ok(key) = k {
                    obj.set(key.as_ref(), materialize_native(ctx, doc, c)?)?;
                }
            }
        }
        Ok(obj.into_value())
    } else {
        match t.scalar(node) {
            Ok(s) => match s {
                cz_json::Scalar::Null => Ok(Value::new_null(ctx.clone())),
                cz_json::Scalar::Bool(b) => Ok(Value::new_bool(ctx.clone(), b)),
                cz_json::Scalar::Int(i) => Ok(Value::new_float(ctx.clone(), i as f64)),
                cz_json::Scalar::Float(f) => Ok(Value::new_float(ctx.clone(), f)),
                cz_json::Scalar::Clean(s) => Ok(rquickjs::String::from_str(ctx.clone(), s)?.into_value()),
                cz_json::Scalar::Escaped(s) => Ok(rquickjs::String::from_str(ctx.clone(), &s)?.into_value()),
            },
            Err(_) => Ok(Value::new_null(ctx.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON.stringify routing
// ---------------------------------------------------------------------------

// Thread-local stringify scratch buffer: capacity is retained across calls so hot stringify
// loops stop re-mallocating. Reentrant JSON.stringify (via `toJSON` callbacks) transparently
// falls back to a fresh Vec when the scratch is already borrowed.
thread_local! {
    static SCRATCH_OUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn do_stringify<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> Result<Value<'js>> {
    // 1. Transparent Lazy handles.
    if let Some((id, node)) = lazy_tag(ctx, v)? {
        let doc = need_doc(ctx, id as f64)?;
        let scratch_hit = SCRATCH_OUT.with(|c| {
            match c.try_borrow_mut() {
                Ok(mut out) => {
                    out.clear();
                    if doc.subtree_is_clean(node) {
                        out.extend_from_slice(doc.raw_slice(node));
                    } else {
                        doc.serialize(node, &mut *out);
                    }
                    // No JS runs inside `from_str` (pure allocation + copy): the borrow cannot
                    // re-enter while held. Zero extra Rust-side copies.
                    Some(unsafe { js_string_from_bytes(&ctx, &out) })
                }
                Err(_) => None, // reentrant stringify: fall back below
            }
        });
        if let Some(r) = scratch_hit {
            return Ok(r?);
        }
        let mut out = Vec::new();
        if doc.subtree_is_clean(node) {
            // Zero re-parse, zero re-serialize: the exact original bytes.
            out.extend_from_slice(doc.raw_slice(node));
        } else {
            doc.serialize(node, &mut out);
        }
        return Ok(unsafe { js_string_from_bytes(ctx, &out) }?);
    }
    // 2. cardputer:kv row handles — direct EntryPayload::Record serialization, no JS traps.
    if let Some((store, key)) = kv_row_tag(ctx, v)? {
        let scratch_hit = SCRATCH_OUT.with(|c| {
            match c.try_borrow_mut() {
                Ok(mut out) => {
                    out.clear();
                    write_kv_record(&mut *out, &store, &key);
                    // Record serialization touches Rust state only; `from_str` allocates a JS
                    // string without running JS — no reentrancy risk while borrowed.
                    Some(unsafe { js_string_from_bytes(&ctx, &out) })
                }
                Err(_) => None,
            }
        });
        if let Some(r) = scratch_hit {
            return Ok(r?);
        }
        let mut out = Vec::new();
        write_kv_record(&mut out, &store, &key);
        return Ok(unsafe { js_string_from_bytes(ctx, &out) }?);
    }
    // 3. Regular JS values.
    if v.is_undefined() || v.is_function() || v.is_symbol() {
        return Ok(Value::new_undefined(ctx.clone()));
    }
    // 3. Regular JS values.
    if v.is_undefined() || v.is_function() || v.is_symbol() {
        return Ok(Value::new_undefined(ctx.clone()));
    }
    // Scratch reuse for the generic walk too: the borrow is HELD across `walk_json`, so a
    // reentrant JSON.stringify (from a `toJSON` callback) fails `try_borrow_mut` and transparently
    // falls back to a fresh Vec below — no aliasing is ever possible.
    let scratch_hit = SCRATCH_OUT.with(|c| match c.try_borrow_mut() {
        Ok(mut out) => {
            out.clear();
            let mut stack: Vec<*mut c_void> = Vec::new();
            match walk_json(ctx, v, &mut *out, &mut stack, 0) {
                Ok(()) => Some(unsafe { js_string_from_bytes(&ctx, &out) }),
                Err(e) => Some(Err(e)),
            }
        }
        Err(_) => None,
    });
    if let Some(r) = scratch_hit {
        return r;
    }
    let mut out = Vec::new();
    let mut stack: Vec<*mut c_void> = Vec::new();
    walk_json(ctx, v, &mut out, &mut stack, 0)?;
    Ok(unsafe { js_string_from_bytes(ctx, &out) }?)
}

/// Build a JS string DIRECTLY from owned UTF-8 bytes via JS_NewStringLen - no intermediate
/// &str validation pass. Safety chain for the stringify outputs: (a) clean-subtree passthrough
/// bytes come from PARSE-VALIDATED input (H12/H13 invariant), (b) serialized/emitted bytes are
/// ASCII from writers or valid Rust Strings converted from JS. Ownership: the fresh JS value is
/// refcount-owned and immediately wrapped by Value::from_raw RAII (settled ownership table).
unsafe fn js_string_from_bytes<'js>(ctx: &Ctx<'js>, bytes: &[u8]) -> Result<Value<'js>> {
    let q = ctx.as_raw().as_ptr();
    let v = rquickjs::qjs::JS_NewStringLen(
        q,
        bytes.as_ptr() as *const ::core::ffi::c_char,
        bytes.len() as ::core::ffi::c_ulong /* size_t */,
    );
    if unsafe { rquickjs::qjs::JS_IsException(v) } {
        // Cold path (allocation failure): drop the exception marker and surface a Rust-side
        // error the runner converts to a JS throw. Matches prior from_str failure semantics.
        unsafe { rquickjs::qjs::JS_FreeValue(q, v) };
        return Err(Exception::throw_message(ctx, "string allocation failed"));
    }
    Ok(Value::from_raw(ctx.clone(), v))
}


/// Serialize a KV row straight from the Rust `EntryPayload::Record` (sorted field names).
fn write_kv_record(out: &mut Vec<u8>, store: &str, key: &str) {
    out.push(b'{');
    let handle = match cz_hal::kv::peek_store(store) {
        Some(h) => h,
        None => {
            out.push(b'}');
            return;
        }
    };
    let fields = match handle.lock().expect("kv lock").record_fields(key) {
        Some(f) => f,
        None => {
            // Rows backed by scalar/missing entries stringify as an empty object (like before).
            out.push(b'}');
            return;
        }
    };
    for (i, (name, val)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_escaped_str(out, name);
        out.push(b':');
        match val {
            KvValue::Null => out.extend_from_slice(b"null"),
            KvValue::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            KvValue::Int(v) => write_i64(out, *v),
            KvValue::Float(f) => cz_json::write_f64_json(out, *f),
            KvValue::Str(s) => write_escaped_str(out, s.as_ref()),
            // Byte payloads render as UTF-8 (lossy) strings — still valid JSON.
            KvValue::Bytes(b) => write_escaped_str(out, &String::from_utf8_lossy(b)),
        }
    }
    out.push(b'}');
}

/// ECMA-262 generic serializer over live JS values with circular-reference detection.
fn walk_json<'js>(
    ctx: &Ctx<'js>,
    v: &Value<'js>,
    out: &mut Vec<u8>,
    stack: &mut Vec<*mut c_void>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_WALK_DEPTH {
        return Err(Exception::throw_range(ctx, "Maximum nesting depth exceeded"));
    }
    if v.is_null() {
        out.extend_from_slice(b"null");
        return Ok(());
    }
    if v.is_bool() {
        out.extend_from_slice(if v.as_bool().unwrap_or(false) { b"true" } else { b"false" });
        return Ok(());
    }
    if v.is_string() {
        let s = CString::from_string(v.clone().into_string().expect("string view"))?;
        write_escaped_str(out, s.as_str());
        return Ok(());
    }
    if v.is_big_int() {
        return Err(Exception::throw_type(ctx, "Do not know how to serialize a BigInt"));
    }
    if v.is_number() {
        let n = v.get::<f64>()?;
        cz_json::write_f64_json(out, n); // NaN / infinity stringify as null per ECMA-262
        return Ok(());
    }
    if !v.is_object() {
        out.extend_from_slice(b"null");
        return Ok(());
    }

    let obj = v.as_object().expect("object view").clone();
    // Cycle detection uses the raw JSValue pointer of the object being emitted.
    let ptr = unsafe { rquickjs::qjs::JS_VALUE_GET_PTR(v.as_raw()) };
    if stack.contains(&ptr) {
        return Err(Exception::throw_type(ctx, "Circular reference detected"));
    }
    stack.push(ptr);

    // toJSON (Date and friends) takes precedence over structural walking.
    if obj.contains_key("toJSON")? {
        if let Ok(f) = obj.get::<_, Function>("toJSON") {
            // `toJSON` runs with the object as its receiver (Date and friends rely on `this`).
            let mut args = Args::new(ctx.clone(), 0);
            args.this(obj.clone())?;
            let produced: Value<'js> = args.apply(&f)?;
            walk_json(ctx, &produced, out, stack, depth + 1)?;
            stack.pop();
            return Ok(());
        }
    }

    if obj.is_array() {
        out.push(b'[');
        let arr = obj.into_array().expect("array view");
        let len = arr.len();
        for i in 0..len {
            if i > 0 {
                out.push(b',');
            }
            let it = arr.get::<Value>(i)?;
            // undefined / holes / functions / symbols inside arrays serialize as null.
            if it.is_undefined() || it.is_function() || it.is_symbol() {
                out.extend_from_slice(b"null");
            } else {
                walk_json(ctx, &it, out, stack, depth + 1)?;
            }
        }
        out.push(b']');
        stack.pop();
        return Ok(());
    }

    out.push(b'{');
    // Fast path: raw property enumeration straight through the QuickJS C API. This skips the
    // rquickjs per-member String/Value marshaling allocations for plain data objects.
    unsafe {
        let q = ctx.as_raw().as_ptr();
        let mut tab: *mut rquickjs::qjs::JSPropertyEnum = std::ptr::null_mut();
        let mut len: u32 = 0;
        let obj_raw = obj.as_value().as_raw();
        if rquickjs::qjs::JS_GetOwnPropertyNames(
            q,
            &mut tab,
            &mut len,
            obj_raw,
            (rquickjs::qjs::JS_GPN_STRING_MASK | rquickjs::qjs::JS_GPN_ENUM_ONLY) as i32,
        ) == 0
        {
            let mut first = true;
            for i in 0..len {
                let pe = &*tab.add(i as usize);
                let mut desc: rquickjs::qjs::JSPropertyDescriptor = std::mem::zeroed();
                if rquickjs::qjs::JS_GetOwnProperty(q, &mut desc, obj_raw, pe.atom) != 1 {
                    continue;
                }
                // NORMAL (non GETSET/ref) data properties only; getset needs JS-side invocation.
                if desc.flags & 0x30 != 0 {
                    rquickjs::qjs::JS_FreeValue(q, desc.value);
                    continue;
                }
                let val = Value::from_raw(ctx.clone(), desc.value);
                if val.is_undefined() || val.is_function() || val.is_symbol() {
                    continue; // ECMA: object members with these types vanish
                }
                if !first {
                    out.push(b',');
                }
                first = false;
                let mut klen = 0u64;
                let kptr = rquickjs::qjs::JS_AtomToCStringLen(q, &mut klen, pe.atom);
                if !kptr.is_null() {
                    let kb = std::slice::from_raw_parts(kptr as *const u8, klen as usize);
                    write_escaped_str(out, &String::from_utf8_lossy(kb));
                    rquickjs::qjs::JS_FreeCString(q, kptr);
                }
                out.push(b':');
                walk_json(ctx, &val, out, stack, depth + 1)?;
            }
            rquickjs::qjs::JS_FreePropertyEnum(q, tab, len);
            out.push(b'}');
            stack.pop();
            return Ok(());
        }
    }
    let mut first = true;
    for p in obj.props::<String, Value>() {
        let (k, val) = p?;
        if val.is_undefined() || val.is_function() || val.is_symbol() {
            continue; // ECMA: object members with these types vanish
        }
        if !first {
            out.push(b',');
        }
        first = false;
        write_escaped_str(out, &k);
        out.push(b':');
        walk_json(ctx, &val, out, stack, depth + 1)?;
    }
    out.push(b'}');
    stack.pop();
    Ok(())
}

// ---------------------------------------------------------------------------
// cardputer:json module definition
// ---------------------------------------------------------------------------

pub struct JsonModule;

impl rquickjs::module::ModuleDef for JsonModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["parseLazy", "isLazy", "lazyStats", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        // Make sure the engine (and its saved originals) exist even if the runner never installed
        // it, then export the Lazy diagnostics surface.
        install_json(ctx)?;

        let parse_lazy_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, text: String| -> Result<Value<'js>> {
            parse_lazy(&ctx, text.as_str())
        })?;
        let is_lazy_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, v: Value<'js>| -> Result<bool> {
            is_lazy(&ctx, &v)
        })?;
        let stats_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, v: Value<'js>| -> Result<Value<'js>> {
            stats_object(&ctx, &v)
        })?;

        exports.export("parseLazy", parse_lazy_fn.clone())?;
        exports.export("isLazy", is_lazy_fn.clone())?;
        exports.export("lazyStats", stats_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("parseLazy", parse_lazy_fn)?;
        default_obj.set("isLazy", is_lazy_fn)?;
        default_obj.set("lazyStats", stats_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod json_module_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    /// Evaluate a script with the JSON engine installed in this runtime.
    fn run_with_json(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            install_json(&ctx).unwrap();
            Module::declare_def::<JsonModule, _>(ctx.clone(), "cardputer:json").unwrap();
            let p = Module::evaluate(ctx.clone(), "json-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn small_json_parses_fast_and_stringifies_back_byte_exact() {
        assert_eq!(
            run_with_json(
                r#"
                const o = JSON.parse('{"a":1,"b":-42,"c":3.5,"s":"clean \u00e9","arr":[true,null]}');
                if (o.a !== 1 || o.b !== -42 || o.c !== 3.5 || o.arr[0] !== true) throw new Error("parse mismatch");
                const s = JSON.stringify({ x: 10, skip: undefined, fn: () => {}, arr: [1, undefined, NaN, Infinity] });
                if (s !== '{"x":10,"arr":[1,null,null,null]}') throw new Error("stringify shape: " + s);
                globalThis.__R = "SMALL_FAST_OK";
                "#
            ),
            "SMALL_FAST_OK"
        );
    }

    #[test]
    fn reviver_and_space_delegate_to_original_quickjs() {
        assert_eq!(
            run_with_json(
                r#"
                const rev = JSON.parse('{"n":5}', (k, v) => (k === "n" ? v * 2 : v));
                if (rev.n !== 10) throw new Error("reviver fallback");
                const pretty = JSON.stringify({ a: 1 }, null, 2);
                if (!pretty.includes('\n  "a": 1')) throw new Error("space fallback: " + pretty);
                const filtered = JSON.stringify({ a: 1, b: 2 }, ["a"]);
                if (filtered !== '{"a":1}') throw new Error("replacer array: " + filtered);
                globalThis.__R = "FALLBACKS_OK";
                "#
            ),
            "FALLBACKS_OK"
        );
    }

    #[test]
    fn invalid_json_throws_syntax_error_and_circular_throws_type_error() {
        assert_eq!(
            run_with_json(
                r#"
                for (const bad of ["", "{", '{"a":}', "[1,]", '"unterminated', "tru"]) {
                    let threw = false;
                    try { JSON.parse(bad); } catch (e) { threw = e instanceof SyntaxError; }
                    if (!threw) throw new Error("no SyntaxError for " + JSON.stringify(bad));
                }
                const cyc = {}; cyc.self = cyc;
                let t = false;
                try { JSON.stringify(cyc); } catch (e) { t = e instanceof TypeError; }
                if (!t) throw new Error("circular must throw TypeError");
                globalThis.__R = "ERRORS_OK";
                "#
            ),
            "ERRORS_OK"
        );
    }

    #[test]
    fn large_documents_are_transparent_lazy_with_cow_and_passthrough() {
        let big = format!("{{\"pad\":\"{}\",\"meta\":{{\"seq\":7}},\"items\":[1,2,3]}}", "x".repeat(5000));
        assert_eq!(
            run_with_json(&format!(
                r#"
                import * as czJson from "cardputer:json";
                const src = {};
                const doc = JSON.parse(src);
                if (!czJson.isLazy(doc)) throw new Error(">=4KB must be lazy");
                if (doc.pad.length !== 5000) throw new Error("lazy string read");
                if (doc.meta.seq !== 7 || doc.items[2] !== 3) throw new Error("nested lazy read");
                if (!Array.isArray(doc.items) || doc.items.length !== 3) throw new Error("array shape");
                if (JSON.stringify(doc) !== src) throw new Error("clean passthrough mismatch");
                const st = czJson.lazyStats(doc);
                if (st.rawBytes !== src.length || st.materialized !== false) throw new Error("stats " + JSON.stringify(st));
                doc.meta.seq = 99;
                if (doc.meta.seq !== 99) throw new Error("CoW readback");
                if (czJson.isLazy(doc)) throw new Error("mutated handle must not report lazy");
                if (!JSON.stringify(doc).includes('"seq":99')) throw new Error("CoW not reflected");
                globalThis.__R = "LAZY_OK";
                "#,
                format!("{:?}", big)
            )),
            "LAZY_OK"
        );
    }

    #[test]
    fn wide_lazy_objects_use_the_adaptive_hash_index_path() {
        let mut wide = String::from("{");
        for i in 0..64 {
            if i > 0 {
                wide.push(',');
            }
            wide.push_str(&format!("\"field_{i}\":{}", i * 10));
        }
        wide.push('}');
        assert_eq!(
            run_with_json(&format!(
                r#"
                import * as czJson from "cardputer:json";
                const src = {};
                const doc = czJson.parseLazy(src);
                if (doc.field_0 !== 0 || doc.field_33 !== 330 || doc.field_63 !== 630) throw new Error("hash lookup");
                if (doc.missing === undefined && Object.keys(doc).length !== 64) throw new Error("keys");
                globalThis.__R = "WIDE_OK";
                "#,
                format!("{:?}", wide)
            )),
            "WIDE_OK"
        );
    }

    #[test]
    fn lazy_object_keys_for_in_and_delete_work_transparently() {
        let big = format!("{{\"a\":1,\"b\":2,\"pad\":\"{}\"}}", "y".repeat(5000));
        assert_eq!(
            run_with_json(&format!(
                r#"
                const doc = JSON.parse({:?});
                if (Object.keys(doc).join(",") !== "a,b,pad") throw new Error("ownKeys: " + Object.keys(doc).join(","));
                if (!("b" in doc)) throw new Error("in trap");
                delete doc.b;
                if ("b" in doc) throw new Error("delete trap not applied");
                const seen = []; for (const k in doc) seen.push(k);
                if (seen.join(",") !== "a,pad") throw new Error("for-in: " + seen.join(","));
                doc.added = true;
                if (doc.added !== true) throw new Error("CoW add readback");
                globalThis.__R = "TRAPS_OK";
                "#,
                big
            )),
            "TRAPS_OK"
        );
    }

    #[test]
    fn module_default_export_shape_exposes_all_three_helpers() {
        assert_eq!(
            run_with_json(
                r#"
                import def from "cardputer:json";
                import * as ns from "cardputer:json";
                for (const n of ["parseLazy","isLazy","lazyStats"]) {
                    if (typeof ns[n] !== "function") throw new Error("named missing " + n);
                    if (typeof def[n] !== "function") throw new Error("default missing " + n);
                }
                if (def.lazyStats({}) !== null) throw new Error("lazyStats on plain object must be null");
                if (def.isLazy({}) !== false) throw new Error("isLazy plain");
                const h = def.parseLazy('{"tiny":1}');   // forcing lazy even below the threshold
                if (!def.isLazy(h) || h.tiny !== 1) throw new Error("parseLazy forced handle");
                globalThis.__R = "MODULE_SHAPE_OK";
                "#
            ),
            "MODULE_SHAPE_OK"
        );
    }
}
