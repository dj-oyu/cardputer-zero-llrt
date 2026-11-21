//! `cardputer:kv` — Rust-native key-value & structured-record store module.
//!
//! Every method on a returned `KvStore` handle is backed by a closure capturing the store's
//! shared [`std::sync::Arc`] into [`cz_hal::kv`]; payloads live entirely on the Rust heap and never
//! inside a JS `Map`/object. The four row-trap natives — `(key, field, ...)` shaped
//! `getField`/`setField`/`delField`/`hasField` — are created ONCE per `kv.open()` store inside
//! [`store_methods`]. The shim defines `row(key, ttl?)` **once per store** as a thin JS wrapper
//! calling the global `__czKvMakeRow(key, ttl, getF, setF, delF, hasF)`, which wraps those shared
//! natives in a slim JS `Proxy` over an empty target — **zero new Rust closures per `row()`**.
//! No stored
//! field name is ever copied onto the JS object and multiple handles share one live Rust record
//! (plus native `incr()` stays immediately visible).

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

/// Render a borrowed native [`KvValue`] back into a JS value (Bytes become a fresh copied
/// Uint8Array). Takes `&KvValue` so Visitor-API callers hand payloads over without cloning.
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
    if (globalThis.__czKvInstalledV2) return;

    // A row handle is a thin Proxy over an EMPTY target. Data field names are NEVER copied onto
    // the JS object; the traps forward to STORE-LEVEL native functions that take (key, field,...).
    // Per row() we allocate ONLY a plain target + its tag object: ONE shared ROW_HANDLER (H18)
    // serves every handle of every store, and zero Rust closures are created per call.
    // The hidden `Symbol.for("cz.kvRow")` tag on the target lets JSON.stringify serialize the
    // record straight from Rust EntryPayload::Record without walking any JS property (symbols are
    // invisible to Object.getOwnPropertyNames, so the zero-copy own-prop proof stays intact).
    globalThis.__czKvMakeRow = function (storeName, key, ttl, getF, setF, delF, hasF) {
        // NaN sentinel when unset: rquickjs rejects an EXPLICIT undefined for Opt<f64>, but a
        // plain number always converts; ttl_opt() filters non-finite to None on the native side.
        const useTtl = (typeof ttl === "number" && isFinite(ttl) && ttl > 0) ? ttl : NaN;
        const t = {};
        Reflect.set(t, ROW_SYM, {
            store: String(storeName),
            key: String(key),
            ttl: useTtl,
            get: getF,
            set: setF,
            del: delF,
            has: hasF,
        });
        // ONE shared handler for every row in the process (Epic insight applied to KV):
        // traps read the handle's key/ttl/natives from its own hidden tag — zero per-row
        // handler closure allocations. The empty extensible target keeps all Proxy invariants.
        return new Proxy(t, ROW_HANDLER);
    };

    const ROW_SYM = Symbol.for("cz.kvRow");
    const ROW_HANDLER = {
        get: function (t, k) {
            const tg = Reflect.get(t, ROW_SYM);
            if (tg !== undefined && typeof k === "string") return tg.get(tg.key, k);
            return Reflect.get(t, k);
        },
        set: function (t, k, v) {
            const tg = Reflect.get(t, ROW_SYM);
            if (tg !== undefined && typeof k === "string") { tg.set(tg.key, k, v, tg.ttl); return true; }
            return Reflect.set(t, k, v);
        },
        deleteProperty: function (t, k) {
            const tg = Reflect.get(t, ROW_SYM);
            if (tg !== undefined && typeof k === "string") return tg.del(tg.key, k) === true;
            return Reflect.deleteProperty(t, k);
        },
        has: function (t, k) {
            const tg = Reflect.get(t, ROW_SYM);
            if (tg !== undefined && typeof k === "string") return tg.has(tg.key, k) === true;
            return Reflect.has(t, k);
        },
    };

    // Normalizes open() options into primitive scalars for the native registry call.
    globalThis.__czKvOpen = function (name, opts) {
        const o = opts || {};
        // Always supply exactly four required primitives; unset options become sentinels:
        // numbers use -1 (-> store default), the path uses "" (-> in-memory only).
        let mb = (typeof o.maxBytes === "number" && isFinite(o.maxBytes)) ? o.maxBytes : -1;
        let me = (typeof o.maxEntries === "number" && isFinite(o.maxEntries)) ? o.maxEntries : -1;
        let pp = (typeof o.persistPath === "string") ? o.persistPath : "";
        const s = __czKvOpenNative(String(name), mb, me, pp);
        // Attach `row` ONCE per store: it reuses the store-level natives (getField/setField/
        // delField/hasField, all `(key, field, ...)` shaped) so row() allocates zero Rust
        // closures — only a thin Proxy over an empty target per call.
        s.row = function (key, ttl) {
            return __czKvMakeRow(String(name), String(key), ttl, this.getField, this.setField, this.delField, this.hasField);
        };
        return s;
    };

    globalThis.__czKvInstalledV2 = true;
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
    // Visitor API: render the JS value straight from the borrowed native payload — zero `KvValue`
    // clone on every read (`st.get` clones for backward compatibility only).
    let get_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>, key: String| -> Result<Value<'js>> {
        let mut st = s.lock().expect("kv store lock");
        st.with_value(&key, |v| match v {
            Some(inner) => kv_to_js(&ctx, inner),
            None => Ok(Value::new_undefined(ctx.clone())),
        })
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
    let get_field_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, field: String| -> Result<Value<'js>> {
            let mut st = s.lock().expect("kv store lock");
            st.with_field(&key, &field, |v| match v {
                Some(inner) => kv_to_js(&ctx, inner),
                None => Ok(Value::new_undefined(ctx.clone())),
            })
        },
    )?;

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

    // Row-trap natives live at STORE level: they take (key, field, ...) and are created ONCE
    // per kv.open() store, then reused by every row() call through the shim (zero Rust
    // closures per row). `hasField` mirrors the row `in` trap semantics (live field present).
    let s = store.clone();
    let del_field_fn = Function::new(ctx.clone(), move |key: String, field: String| -> bool {
        let mut st = s.lock().expect("kv store lock");
        st.del_field(&key, &field)
    })?;

    let s = store.clone();
    let has_field_fn = Function::new(ctx.clone(), move |key: String, field: String| -> bool {
        let mut st = s.lock().expect("kv store lock");
        // Presence probe over the Visitor API never clones the payload.
        st.with_field(&key, &field, |v| v.is_some())
    })?;

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
    obj.set("delField", del_field_fn)?;
    obj.set("hasField", has_field_fn)?;
    // NOTE: `row` is attached once per store by the shim's __czKvOpen wrapper (see KV_SHIM),
    // reusing these store-level natives — no Rust closure is created per row() call.

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
    fn row_2000_calls_reuse_store_level_natives_without_per_call_state() {
        // 2,000 row() calls on one key must share the store-level natives: every handle sees the
        // same live record, own props stay empty (zero-copy), and views lands exactly at 2000.
        let out = run_script(
            r#"
            import * as kv from "cardputer:kv";
            const s = kv.open("m_row_stress", { maxEntries: 8 });
            const warm = s.row("u0");
            warm.views = 0;
            for (let i = 0; i < 2000; i++) {
                const r = s.row("u0");
                if (r.missing !== undefined) throw new Error("stale value at " + i);
                r.views++;
            }
            const fin = s.getField("u0", "views");
            if (fin !== 2000) throw new Error("views=" + fin);
            const lastRow = s.row("u0");
            if (lastRow.views !== 2000) throw new Error("fresh row mismatch " + lastRow.views);
            if (Object.getOwnPropertyNames(lastRow).length !== 0) throw new Error("own props leaked");
            lastRow.tag = "live";
            if (s.row("u0").tag !== "live") throw new Error("shared record broken");
            if (!("tag" in s.row("u0"))) throw new Error("has trap broken");
            delete s.row("u0").tag;
            if ("tag" in s.row("u0")) throw new Error("delete trap broken");
            globalThis.__R = "ROW_REUSE_OK";
            "#,
        );
        assert_eq!(out, "ROW_REUSE_OK");
    }

    #[test]
    fn row_ttl_applies_per_row_via_shared_setter() {
        // Rows created with a TTL expire through the shared store-level setter; rows created
        // without one keep fields indefinitely.
        let out = run_script(
            r#"
            import * as kv from "cardputer:kv";
            const s = kv.open("m_row_ttl");
            const shortRow = s.row("eph", 0.12);
            shortRow.a = 1;
            const plainRow = s.row("keep");
            plainRow.b = 2;
            if (shortRow.a !== 1 || plainRow.b !== 2) throw new Error("initial reads");
            const until = Date.now() + 300;
            while (Date.now() < until) { }
            if (s.getField("eph", "a") !== undefined) throw new Error("row ttl not applied");
            if (plainRow.b !== 2) throw new Error("plain row ttl leaked");
            globalThis.__R = "ROW_TTL_OK";
            "#,
        );
        assert_eq!(out, "ROW_TTL_OK");
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
