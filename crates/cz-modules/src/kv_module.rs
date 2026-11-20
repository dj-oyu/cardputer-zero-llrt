//! `cardputer:kv` — Rust-native key-value & structured-record store module.
//!
//! Every method on a returned `KvStore` handle is backed by a closure capturing the store's
//! shared [`std::sync::Arc`] into [`cz_hal::kv`]; payloads live entirely on the Rust heap and never
//! inside a JS `Map`/object. `row()` returns a **zero-copy** JS `Proxy` over an empty target whose
//! `get`/`set`/`deleteProperty`/`has` traps call native getter/setter/delete field functions, so
//! no stored field name is ever copied onto the JS object and multiple handles share one live
//! Rust record (plus native `incr()` stays immediately visible).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rquickjs::function::Opt;
use rquickjs::{Array, Ctx, Error, Exception, Function, Object, Result, TypedArray, Value};

use cz_hal::kv::{self, KvValue, KvStore, Stats};

use crate::bytes_from_value;

/// Convert a JS value into a native [`KvValue`] (null/undefined -> Null; bool/number/string;
/// Uint8Array/ArrayBuffer -> Bytes). Unknown objects fall back to Null to stay crash-safe.
fn value_to_kv(value: &Value<'_>) -> KvValue {
    if value.is_null() || value.is_undefined() {
        return KvValue::Null;
    }
    if value.is_bool() {
        return KvValue::Bool(value.as_bool().unwrap_or(false));
    }
    if value.is_string() {
        let s = value.get::<String>().unwrap_or_default();
        return KvValue::Str(s.into());
    }
    if value.is_number() {
        let n = value.get::<f64>().unwrap_or(0.0);
        return KvValue::from_f64(n);
    }
    // Byte buffers: reuse the shared typed-array/array-buffer extractor.
    match bytes_from_value(value) {
        Ok(b) => KvValue::Bytes(b.into()),
        Err(_) => KvValue::Null,
    }
}

/// Render a native [`KvValue`] back into a JS value (Bytes become a fresh copied Uint8Array).
fn kv_to_js<'js>(ctx: &Ctx<'js>, v: &KvValue) -> Result<Value<'js>> {
    match v {
        KvValue::Null => Ok(Value::new_null(ctx.clone())),
        KvValue::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        KvValue::Int(i) => Ok(Value::new_float(ctx.clone(), *i as f64)),
        KvValue::Float(f) => Ok(Value::new_float(ctx.clone(), *f)),
        KvValue::Str(s) => Ok(rquickjs::String::from_str(ctx.clone(), s.as_ref())?.into_value()),
        KvValue::Bytes(b) => {
            let ta = TypedArray::<u8>::new_copy(ctx.clone(), b)?;
            Ok(ta.into_value())
        }
    }
}

/// `Option<KvValue>` -> JS value; `None` yields `undefined` (missing keys/fields read as absent).
fn opt_kv_to_js<'js>(ctx: &Ctx<'js>, v: Option<KvValue>) -> Result<Value<'js>> {
    match v {
        Some(inner) => kv_to_js(ctx, &inner),
        None => Ok(Value::new_undefined(ctx.clone())),
    }
}

/// Normalize the always-supplied scalar option sentinels from the JS shim into store caps + an
/// optional persist path. Non-finite/negative numeric sentinels mean "use default"; an empty
/// persist string means "in-memory only". Keeping every native param a required primitive avoids
/// rquickjs's blanket-required `Option<T>` arity quirk entirely.
fn parse_opts(max_bytes: f64, max_entries: f64, persist: String) -> (u64, u64, Option<PathBuf>) {
    let mb = if max_bytes.is_finite() && max_bytes >= 0.0 { max_bytes as u64 } else { 0 };
    let me = if max_entries.is_finite() && max_entries >= 0.0 { max_entries as u64 } else { 0 };
    let pp = if persist.is_empty() { None } else { Some(PathBuf::from(persist)) };
    (mb, me, pp)
}

fn ttl_opt(n: Option<f64>) -> Option<f64> {
    n.filter(|x| x.is_finite() && *x > 0.0)
}

// ---------------------------------------------------------------------------
// Store handle construction (every method captures the store Arc directly)
// ---------------------------------------------------------------------------

fn build_stats_object<'js>(ctx: &Ctx<'js>, st: &Stats) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("name", st.name.as_str())?;
    obj.set("entries", st.entries as f64)?;
    obj.set("bytes", st.bytes as f64)?;
    obj.set("maxBytes", st.max_bytes as f64)?;
    obj.set("maxEntries", st.max_entries as f64)?;
    obj.set("evictions", st.evictions as f64)?;
    obj.set("hits", st.hits as f64)?;
    obj.set("misses", st.misses as f64)?;
    Ok(obj)
}

/// Install global JS glue: the zero-copy row Proxy builder + an options-normalizing `open`
/// wrapper that extracts the option fields into primitives for the native open function.
const KV_SHIM: &str = r#"
(function () {
    if (globalThis.__czKvInstalled) return;

    // A row handle is a thin Proxy over an EMPTY target. Data field names are NEVER copied onto
    // the JS object; get/set/deleteProperty/has forward straight to native store functions.
    globalThis.__czKvMakeRow = function (getF, setF, delF, hasF) {
        return new Proxy({}, {
            get:    function (t, k) { if (typeof k === "string") return getF(k); return Reflect.get(t, k); },
            set:    function (t, k, v) { if (typeof k === "string") { setF(k, v); return true; } return Reflect.set(t, k, v); },
            deleteProperty: function (t, k) { if (typeof k === "string") return delF(k) === true; return Reflect.deleteProperty(t, k); },
            has:    function (t, k) { if (typeof k === "string") return hasF(k) === true; return Reflect.has(t, k); },
        });
    };

    // Normalizes open() options into primitive scalars for the native registry call.
    globalThis.__czKvOpen = function (name, opts) {
        const o = opts || {};
        // Always supply exactly four required primitives; unset options become sentinels:
        // numbers use -1 (-> store default), the path uses "" (-> in-memory only).
        let mb = (typeof o.maxBytes === "number" && isFinite(o.maxBytes)) ? o.maxBytes : -1;
        let me = (typeof o.maxEntries === "number" && isFinite(o.maxEntries)) ? o.maxEntries : -1;
        let pp = (typeof o.persistPath === "string") ? o.persistPath : "";
        return __czKvOpenNative(String(name), mb, me, pp);
    };

    globalThis.__czKvInstalled = true;
})();
"#;

/// Register the store object methods on an `Object`, each capturing its own `Arc<KvStore>`.
fn store_methods<'js>(ctx: &Ctx<'js>, store: Arc<Mutex<KvStore>>) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;

    let s = store.clone();
    let set_fn = Function::new(ctx.clone(), move |key: String, value: Value<'js>, ttl: Opt<f64>| {
        let mut st = s.lock().expect("kv store lock");
        st.set(&key, value_to_kv(&value), ttl_opt(ttl.0));
    })?;

    let s = store.clone();
    let get_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>, key: String| -> Result<Value<'js>> {
        let mut st = s.lock().expect("kv store lock");
        opt_kv_to_js(&ctx, st.get(&key))
    })?;

    let s = store.clone();
    let has_fn = Function::new(ctx.clone(), move |key: String| -> bool {
        let mut st = s.lock().expect("kv store lock");
        st.has(&key)
    })?;

    let s = store.clone();
    let del_fn = Function::new(ctx.clone(), move |key: String| -> bool {
        let mut st = s.lock().expect("kv store lock");
        st.del(&key)
    })?;

    let s = store.clone();
    let keys_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>, prefix: Opt<String>| -> Result<Array<'js>> {
        let mut st = s.lock().expect("kv store lock");
        let list = st.keys(prefix.0.as_deref());
        let arr = Array::new(ctx.clone())?;
        for (i, k) in list.iter().enumerate() {
            arr.set(i, k.as_str())?;
        }
        Ok(arr)
    })?;

    let s = store.clone();
    let set_field_fn = Function::new(
        ctx.clone(),
        move |key: String, field: String, value: Value<'js>, ttl: Opt<f64>| {
            let mut st = s.lock().expect("kv store lock");
            st.set_field(&key, &field, value_to_kv(&value), ttl_opt(ttl.0));
        },
    )?;

    let s = store.clone();
    let get_field_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>, key: String, field: String| -> Result<Value<'js>> {
        let mut st = s.lock().expect("kv store lock");
        opt_kv_to_js(&ctx, st.get_field(&key, &field))
    })?;

    let s = store.clone();
    let incr_fn = Function::new(
        ctx.clone(),
        move |key: String, field: String, delta: Opt<f64>, ttl: Opt<f64>| -> f64 {
            let d = delta.0.filter(|n| n.is_finite()).unwrap_or(1.0);
            let mut st = s.lock().expect("kv store lock");
            st.incr(&key, &field, d, ttl_opt(ttl.0))
        },
    )?;

    let s = store.clone();
    let stats_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> Result<Object<'js>> {
        let mut st = s.lock().expect("kv store lock");
        build_stats_object(&ctx, &st.stats())
    })?;

    let s = store.clone();
    let flush_fn = Function::new(ctx.clone(), move || -> Result<bool> {
        let mut st = s.lock().expect("kv store lock");
        st.flush().map_err(Error::from)
    })?;

    let s = store.clone();
    let clear_fn = Function::new(ctx.clone(), move || {
        let mut st = s.lock().expect("kv store lock");
        st.clear();
    })?;

    // row(key, ttlSec?) builds a zero-copy Proxy backed by per-handle native traps.
    obj.set("set", set_fn)?;
    obj.set("get", get_fn)?;
    obj.set("has", has_fn)?;
    obj.set("del", del_fn)?;
    obj.set("keys", keys_fn)?;
    obj.set("setField", set_field_fn)?;
    obj.set("getField", get_field_fn)?;
    obj.set("incr", incr_fn)?;
    obj.set("stats", stats_fn)?;
    obj.set("flush", flush_fn)?;
    obj.set("clear", clear_fn)?;

    let s_row = store.clone();
    let row_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, ttl: Opt<f64>| -> Result<Value<'js>> {
            let builder: Function<'js> = ctx.globals().get("__czKvMakeRow")?;
            let use_ttl = ttl_opt(ttl.0);

            let g = s_row.clone();
            let get_key = key.clone();
            let get_f = Function::new(ctx.clone(), move |c: Ctx<'js>, field: String| -> Result<Value<'js>> {
                let mut st = g.lock().expect("kv store lock");
                opt_kv_to_js(&c, st.get_field(&get_key, &field))
            })?;

            let k2 = key.clone();
            let s2 = s_row.clone();
            let set_f = Function::new(
                ctx.clone(),
                move |_c: Ctx<'js>, field: String, value: Value<'js>| -> bool {
                    let mut st = s2.lock().expect("kv store lock");
                    st.set_field(&k2, &field, value_to_kv(&value), ttl_opt(use_ttl));
                    true
                },
            )?;

            let k3 = key.clone();
            let s3 = s_row.clone();
            let del_f = Function::new(ctx.clone(), move |field: String| -> bool {
                let mut st = s3.lock().expect("kv store lock");
                st.del_field(&k3, &field)
            })?;

            let k4 = key.clone();
            let s4 = s_row.clone();
            let has_f = Function::new(ctx.clone(), move |field: String| -> bool {
                let mut st = s4.lock().expect("kv store lock");
                st.get_field(&k4, &field).is_some()
            })?;

            // The builder returns a Proxy object value directly; return it unchanged (no data
            // properties live on the JS object — only native traps back the reads/writes).
            Ok(builder.call((get_f, set_f, del_f, has_f))?)
        },
    )?;
    obj.set("row", row_fn)?;

    Ok(obj)
}

/// `cardputer:kv` module definition (named exports AND a `default` export object).
pub struct KvModule;

impl rquickjs::module::ModuleDef for KvModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["open", "drop", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        ctx.eval::<(), _>(KV_SHIM)?;

        // Native open: registry lookup/creation + eager persist load.
        let open_native = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, name: String, max_bytes: f64, max_entries: f64, persist: String| -> Result<Object<'js>> {
                let (mb, me, pp) = parse_opts(max_bytes, max_entries, persist);
                let store = kv::open_store(&name, mb, me, pp.clone())
                    .map_err(|e| Exception::throw_internal(&ctx, &format!("kv open failed: {e}")))?;
                store_methods(&ctx, store)
            },
        )?;
        ctx.globals().set("__czKvOpenNative", open_native.clone())?;

        let drop_fn = Function::new(ctx.clone(), |name: String| -> bool { kv::drop_store(&name) })?;
        let js_open: Function<'js> = ctx.globals().get("__czKvOpen")?;

        exports.export("open", js_open.clone())?;
        exports.export("drop", drop_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("open", js_open)?;
        default_obj.set("drop", drop_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}


#[cfg(test)]
mod kv_module_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    fn run_script(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<KvModule, _>(ctx.clone(), "cardputer:kv").unwrap();
            let p = Module::evaluate(ctx.clone(), "kv-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn kv_set_get_has_del_and_keys_via_module() {
        assert_eq!(
            run_script(
                r#"
                import * as kv from "cardputer:kv";
                const s = kv.open("m_basic");
                s.set("a", 1); s.set("b", "txt"); s.set("c", new Uint8Array([1,2,3]));
                if (s.get("a") !== 1) throw new Error("int mismatch");
                if (s.get("b") !== "txt") throw new Error("str mismatch");
                const by = s.get("c");
                if (!(by instanceof Uint8Array) || by[2] !== 3) throw new Error("bytes mismatch");
                if (!s.has("a")) throw new Error("has a");
                if (s.del("a") !== true) throw new Error("del true expected");
                if (s.has("a")) throw new Error("a should be gone");
                if (JSON.stringify(s.keys()) !== JSON.stringify(["b","c"])) throw new Error("keys " + JSON.stringify(s.keys()));
                globalThis.__R = "KV_BASIC_OK";
                "#
            ),
            "KV_BASIC_OK"
        );
    }

    #[test]
    fn kv_row_handle_zero_copy_and_shared_live_record() {
        assert_eq!(
            run_script(
                r#"
                import * as kv from "cardputer:kv";
                const s = kv.open("m_row");
                const r1 = s.row("alice");
                const r2 = s.row("alice");
                r1.views = 10; r1.views++; r1.views += 4;
                r1.score = 1.5; r1.score *= 2;
                r1.name = "Alice"; r1.admin = true;
                if (r2.views !== 15 || r2.score !== 3 || r2.name !== "Alice" || r2.admin !== true) {
                    throw new Error("not shared: " + JSON.stringify([r2.views, r2.score, r2.name, r2.admin]));
                }
                const own = Object.getOwnPropertyNames(r1);
                if (own.length !== 0) throw new Error("data copied onto handle: " + JSON.stringify(own));
                const next = s.incr("alice", "views", 5);
                if (next !== 20 || r1.views !== 20) throw new Error("incr not visible: " + next + "," + r1.views);
                delete r1.admin;
                if (r2.admin !== undefined || s.getField("alice","admin") !== undefined) throw new Error("delete failed");
                globalThis.__R = "KV_ROW_OK";
                "#
            ),
            "KV_ROW_OK"
        );
    }

    #[test]
    fn kv_stats_incr_and_default_export_shape() {
        assert_eq!(
            run_script(
                r#"
                import kv from "cardputer:kv";
                import * as ns from "cardputer:kv";
                for (const n of ["open","drop"]) { if (!(n in ns)) throw new Error("missing named " + n); if (!(n in kv)) throw new Error("missing default " + n); }
                const s = kv.open("m_stats", { maxEntries: 10, maxBytes: 4096 });
                const st = s.stats();
                if (st.name !== "m_stats") throw new Error("name");
                if (st.maxEntries !== 10 || st.maxBytes !== 4096) throw new Error("caps " + JSON.stringify(st));
                const n = s.incr("cnt", "views");
                if (n !== 1) throw new Error("incr default delta " + n);
                if (s.incr("cnt", "views", 2) !== 3) throw new Error("incr delta 2");
                globalThis.__R = "KV_STATS_OK";
                "#
            ),
            "KV_STATS_OK"
        );
    }

    #[test]
    fn kv_lru_eviction_and_flush_false_in_memory() {
        assert_eq!(
            run_script(
                r#"
                import * as kv from "cardputer:kv";
                const l = kv.open("m_lru", { maxEntries: 3, maxBytes: 1048576 });
                l.set("k1","v1"); l.set("k2","v2"); l.set("k3","v3");
                l.get("k1");
                l.set("k4","v4");
                if (l.has("k2")) throw new Error("k2 should be evicted");
                if (!(l.has("k1") && l.has("k3") && l.has("k4"))) throw new Error("wrong survivors");
                if (l.stats().evictions < 1) throw new Error("no evictions counted");
                const s2 = kv.open("m_nomem_persist");
                if (s2.flush() !== false) throw new Error("flush must be false without persistPath");
                globalThis.__R = "KV_LRU_OK";
                "#
            ),
            "KV_LRU_OK"
        );
    }

    #[test]
    fn kv_drop_frees_named_store() {
        assert_eq!(
            run_script(
                r#"
                import * as kv from "cardputer:kv";
                kv.open("m_drop");
                if (kv.drop("m_drop") !== true) throw new Error("drop should return true");
                if (kv.drop("m_drop") !== false) throw new Error("second drop should be false");
                globalThis.__R = "KV_DROP_OK";
                "#
            ),
            "KV_DROP_OK"
        );
    }

    #[test]
    fn kv_flush_and_reload_roundtrip_across_runtime_in_same_process() {
        let dir = std::env::temp_dir().join(format!("cz-kv-mod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.czkv");
        std::fs::remove_file(&path).ok();
        // Write side.
        run_script(&format!(
            r#"
            import * as kv from "cardputer:kv";
            const s = kv.open("persist_a", {{ persistPath: {:?} }});
            s.set("g","hello_cz"); s.set("raw", new Uint8Array([1,2,3,255]));
            const r = s.row("u_9"); r.views = 77; r.theme = "amber";
            if (s.flush() !== true) throw new Error("flush true expected");
            globalThis.__R = "PERSIST_WRITE_OK";
            "#,
            path.display().to_string()
        ));
        assert!(path.exists(), "persisted file missing");
        assert!(!std::path::Path::new(&format!("{}.tmp", path.display())).exists(), ".tmp lingering");
        // Drop registry then reload in a fresh store name to force disk load.
        let got = run_script(&format!(
            r#"
            import * as kv from "cardputer:kv";
            const s = kv.open("persist_b", {{ persistPath: {:?} }});
            if (s.get("g") !== "hello_cz") throw new Error("g mismatch");
            const raw = s.get("raw");
            if (!(raw instanceof Uint8Array) || raw[3] !== 255) throw new Error("raw mismatch");
            const r = s.row("u_9");
            if (r.views !== 77 || r.theme !== "amber") throw new Error("row restore " + JSON.stringify([r.views,r.theme]));
            globalThis.__R = "PERSIST_READ_OK";
            "#,
            path.display().to_string()
        ));
        assert_eq!(got, "PERSIST_READ_OK");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opt_num_helper_still_reachable_and_kv_to_js_handles_bytes() {
        // Guard that shared helpers compile & bytes render into a Uint8Array via the module.
        let out = run_script(
            r#"
            import * as kv from "cardputer:kv";
            const s = kv.open("m_bytes_js");
            s.set("x", new Uint8Array([9,9,9]));
            const v = s.get("x");
            if (!(v instanceof Uint8Array)) throw new Error("not u8");
            globalThis.__R = "BYTES_JS_OK";
            "#,
        );
        assert_eq!(out, "BYTES_JS_OK");
    }
}
