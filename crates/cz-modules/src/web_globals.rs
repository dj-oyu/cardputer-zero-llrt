//! Phase 13: WinterTC completion globals — `Event` / `CustomEvent` / `EventTarget`,
//! `structuredClone`, `performance.now()` and `queueMicrotask`.
//!
//! Zero-alloc design (`docs/ZERO_ALLOC_GUIDELINES.md`):
//! * `performance.now()` is ONE native `Instant::duration_since` call per tick (monotonic, NEVER
//!   wall clock); the process-start `Instant` and epoch origin live in `OnceLock`s, so the closure
//!   allocates nothing.
//! * The listener registry is one per-target null-prototype map of `(fn, once)` pair entries;
//!   dispatch snapshots with `slice()` only when listeners actually exist for the fired type.
//! * `structuredClone` memoizes seen objects in a `Map` so circular graphs terminate, and clones
//!   typed arrays through their OWN constructor (native element copy, no JS byte loops).
//! * Every shim below stays `'static` and QuickJS-safe (no optional chaining / nullish
//!   coalescing), and registers idempotently behind `globalThis.__czWebGlobalsInstalled`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, UNIX_EPOCH};

use rquickjs::{qjs, Ctx, Error, Function, Result, Value};

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Process-wide monotonic reference point (first `performance` touch) — `Instant`, NEVER `SystemTime`.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Wall-clock origin (`ms since Unix epoch`) captured once for `performance.timeOrigin`.
static TIME_ORIGIN_MS: OnceLock<f64> = OnceLock::new();

fn process_start() -> &'static Instant {
    PROCESS_START.get_or_init(Instant::now)
}

fn time_origin_ms() -> f64 {
    *TIME_ORIGIN_MS.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    })
}

/// Milliseconds since the monotonic process start (`Instant`, never wall clock).
fn now_ms() -> f64 {
    Instant::now().duration_since(*process_start()).as_secs_f64() * 1000.0
}

const MAX_CLONE_DEPTH: usize = 200;

struct PropEnumGuard {
    q: *mut qjs::JSContext,
    tab: *mut qjs::JSPropertyEnum,
    len: u32,
}

impl Drop for PropEnumGuard {
    fn drop(&mut self) {
        if !self.tab.is_null() {
            unsafe {
                qjs::JS_FreePropertyEnum(self.q, self.tab, self.len);
            }
        }
    }
}

enum FastClone<'js> {
    Done(Value<'js>),
    Fallback,
}

fn clone_fast_rec<'js>(
    ctx: &Ctx<'js>,
    val: &Value<'js>,
    seen: &mut HashMap<usize, Value<'js>>,
    obj_proto_ptr: usize,
    arr_proto_ptr: usize,
    depth: usize,
) -> Result<FastClone<'js>> {
    if depth > MAX_CLONE_DEPTH {
        return Ok(FastClone::Fallback);
    }
    if val.is_function() || val.is_symbol() {
        let mk: Function<'js> = ctx.globals().get("__czDataCloneErr")?;
        let err: Value<'js> = mk.call((if val.is_function() { "function" } else { "symbol" },))?;
        return Err(ctx.throw(err));
    }
    if !val.is_object() {
        // Primitives (null, undefined, bool, number, string, bigint) are immutable & shared.
        return Ok(FastClone::Done(val.clone()));
    }

    let q = ctx.as_raw().as_ptr();
    let raw = val.as_raw();
    let ptr_key = unsafe { raw.u.ptr as usize };
    if let Some(existing) = seen.get(&ptr_key) {
        return Ok(FastClone::Done(existing.clone()));
    }

    unsafe {
        // O(1) persistent-tree / rope-style CoW fork for LazyJSON Proxy handles.
        // Guarded by `JS_IsProxy` so plain non-Proxy objects pay ~0 overhead.
        if qjs::JS_IsProxy(raw) {
            if let Some(forked) = crate::json_module::try_clone_lazy(ctx, val)? {
                seen.insert(ptr_key, forked.clone());
                return Ok(FastClone::Done(forked));
            }
        }
        let is_arr = qjs::JS_IsArray(raw);
        let proto_raw = qjs::JS_GetPrototype(q, raw);
        if qjs::JS_IsException(proto_raw) {
            return Err(Error::Exception);
        }
        let proto_val = Value::from_raw(ctx.clone(), proto_raw);
        let proto_is_null = proto_val.is_null();
        let proto_ptr = if proto_val.is_object() {
            proto_val.as_raw().u.ptr as usize
        } else {
            0
        };

        if is_arr && proto_ptr == arr_proto_ptr {
            let mut len_i64: i64 = 0;
            if qjs::JS_GetLength(q, raw, &mut len_i64) < 0 {
                return Err(Error::Exception);
            }
            let len = if len_i64 < 0 { 0usize } else { len_i64 as usize };
            // Check if array has extra non-index enumerable properties (rare).
            let mut tab: *mut qjs::JSPropertyEnum = std::ptr::null_mut();
            let mut prop_cnt: u32 = 0;
            if qjs::JS_GetOwnPropertyNames(
                q,
                &mut tab,
                &mut prop_cnt,
                raw,
                (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_ENUM_ONLY) as i32,
            ) < 0
            {
                return Err(Error::Exception);
            }
            let _guard = PropEnumGuard { q, tab, len: prop_cnt };
            if (prop_cnt as usize) > len {
                // Extra named properties on array: delegate to JS fallback for full fidelity.
                return Ok(FastClone::Fallback);
            }
            drop(_guard);

            let out_raw = qjs::JS_NewArray(q);
            if qjs::JS_IsException(out_raw) {
                return Err(Error::Exception);
            }
            let out_val = Value::from_raw(ctx.clone(), out_raw);
            seen.insert(ptr_key, out_val.clone());

            for i in 0..len {
                let elem_raw = qjs::JS_GetPropertyUint32(q, raw, i as u32);
                if qjs::JS_IsException(elem_raw) {
                    return Err(Error::Exception);
                }
                let elem_val = Value::from_raw(ctx.clone(), elem_raw);
                let cloned = match clone_fast_rec(
                    ctx,
                    &elem_val,
                    seen,
                    obj_proto_ptr,
                    arr_proto_ptr,
                    depth + 1,
                )? {
                    FastClone::Done(c) => c,
                    FastClone::Fallback => return Ok(FastClone::Fallback),
                };
                let dup = qjs::JS_DupValue(q, cloned.as_raw());
                if qjs::JS_DefinePropertyValueUint32(
                    q,
                    out_val.as_raw(),
                    i as u32,
                    dup,
                    qjs::JS_PROP_C_W_E as i32,
                ) < 0
                {
                    return Err(Error::Exception);
                }
            }
            return Ok(FastClone::Done(out_val));
        }

        if !is_arr && (proto_ptr == obj_proto_ptr || proto_is_null) {
            let out_raw = if proto_is_null {
                qjs::JS_NewObjectProto(q, qjs::JS_NULL)
            } else {
                qjs::JS_NewObject(q)
            };
            if qjs::JS_IsException(out_raw) {
                return Err(Error::Exception);
            }
            let out_val = Value::from_raw(ctx.clone(), out_raw);
            seen.insert(ptr_key, out_val.clone());

            let mut tab: *mut qjs::JSPropertyEnum = std::ptr::null_mut();
            let mut len: u32 = 0;
            if qjs::JS_GetOwnPropertyNames(
                q,
                &mut tab,
                &mut len,
                raw,
                (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_ENUM_ONLY) as i32,
            ) < 0
            {
                return Err(Error::Exception);
            }
            let _guard = PropEnumGuard { q, tab, len };
            for i in 0..len {
                let atom = (*tab.add(i as usize)).atom;
                let prop_raw = qjs::JS_GetProperty(q, raw, atom);
                if qjs::JS_IsException(prop_raw) {
                    return Err(Error::Exception);
                }
                let prop_val = Value::from_raw(ctx.clone(), prop_raw);
                let cloned = match clone_fast_rec(
                    ctx,
                    &prop_val,
                    seen,
                    obj_proto_ptr,
                    arr_proto_ptr,
                    depth + 1,
                )? {
                    FastClone::Done(c) => c,
                    FastClone::Fallback => return Ok(FastClone::Fallback),
                };
                let dup = qjs::JS_DupValue(q, cloned.as_raw());
                if qjs::JS_DefinePropertyValue(
                    q,
                    out_val.as_raw(),
                    atom,
                    dup,
                    qjs::JS_PROP_C_W_E as i32,
                ) < 0
                {
                    return Err(Error::Exception);
                }
            }
            return Ok(FastClone::Done(out_val));
        }
    }

    Ok(FastClone::Fallback)
}

fn structured_clone_fast<'js>(ctx: Ctx<'js>, val: Value<'js>) -> Result<Value<'js>> {
    // Cache Object.prototype and Array.prototype pointers once per call (2 cheap lookups).
    let obj_ctor: rquickjs::Object<'js> = ctx.globals().get("Object")?;
    let obj_proto: Value<'js> = obj_ctor.get("prototype")?;
    let arr_ctor: rquickjs::Object<'js> = ctx.globals().get("Array")?;
    let arr_proto: Value<'js> = arr_ctor.get("prototype")?;
    let obj_proto_ptr = unsafe { obj_proto.as_raw().u.ptr as usize };
    let arr_proto_ptr = unsafe { arr_proto.as_raw().u.ptr as usize };

    let mut seen: HashMap<usize, Value<'js>> = HashMap::new();
    match clone_fast_rec(&ctx, &val, &mut seen, obj_proto_ptr, arr_proto_ptr, 0)? {
        FastClone::Done(v) => Ok(v),
        FastClone::Fallback => {
            let fb: Function<'js> = ctx.globals().get("__czCloneFallback")?;
            fb.call((val,))
        }
    }
}

/// Install `Event`, `CustomEvent`, `EventTarget`, `structuredClone`, `performance` and
/// `queueMicrotask` on this context (idempotent). `install_fetch` / `install_ws` call this first
/// so `AbortSignal` and `WebSocket` can inherit from `EventTarget` in any context shape.
pub fn install_web_globals<'js>(ctx: &Ctx<'js>) -> Result<()> {
    if INSTALLED.load(Ordering::SeqCst) && ctx.globals().get::<_, bool>("__czWebGlobalsInstalled").unwrap_or(false) {
        return Ok(());
    }

    // Monotonic clock natives: closures capture nothing (OnceLock lookups only).
    ctx.globals().set("__czTimeOrigin", time_origin_ms())?;
    ctx.globals().set("__czPerformanceNow", Function::new(ctx.clone(), || -> f64 { now_ms() }))?;
    ctx.globals().set("__czStructuredCloneFast", Function::new(ctx.clone(), structured_clone_fast)?)?;

    ctx.eval::<(), _>(WEB_GLOBALS_SHIM)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

const WEB_GLOBALS_SHIM: &str = r#"
(function () {
    if (globalThis.__czWebGlobalsInstalled) return;

    // ---- performance (monotonic std::time::Instant clock, milliseconds since process start) ----
    globalThis.performance = {
        timeOrigin: __czTimeOrigin,
        now: function () { return __czPerformanceNow(); },
    };

    // ---- Event / CustomEvent ---------------------------------------------------------------
    function Event(type, init) {
        this.type = String(type);
        this.bubbles = !!(init && init.bubbles);
        this.cancelable = !!(init && init.cancelable);
        this.composed = !!(init && init.composed);
        this.defaultPrevented = false;
        this.target = null;
        this.currentTarget = null;
        this.eventPhase = 0;
        this.timeStamp = performance.now();
        this._stopProp = false;
        this._stopImmediate = false;
    }
    Event.prototype.preventDefault = function () { if (this.cancelable) this.defaultPrevented = true; };
    Event.prototype.stopPropagation = function () { this._stopProp = true; };
    Event.prototype.stopImmediatePropagation = function () { this._stopProp = true; this._stopImmediate = true; };
    Event.prototype.composedPath = function () { return [this.target || this]; };

    function CustomEvent(type, init) {
        Event.call(this, type, init);
        this.detail = init !== undefined && init !== null && "detail" in init ? init.detail : null;
    }
    CustomEvent.prototype = Object.create(Event.prototype);
    CustomEvent.prototype.constructor = CustomEvent;

    // ---- EventTarget ----------------------------------------------------------------------
    // ONE null-prototype map per target: type -> live array of [listener, once] pairs.
    function EventTarget() { this._et = Object.create(null); }

    function onceFlag(options) { return !!(options && options.once); }

    EventTarget.prototype.addEventListener = function (type, listener, options) {
        if (listener === undefined || listener === null) return;
        if (typeof listener !== "function" && typeof listener.handleEvent !== "function") return;
        const k = String(type);
        const map = this._et || (this._et = Object.create(null));
        const list = map[k];
        if (!list) { map[k] = [[listener, onceFlag(options)]]; return; }
        for (let i = 0; i < list.length; i++) {
            // Identical (type, listener) registrations are ignored (§2.5); `once` upgrades in place.
            if (list[i][0] === listener) {
                if (onceFlag(options)) list[i][1] = true;
                return;
            }
        }
        list.push([listener, onceFlag(options)]);
    };

    EventTarget.prototype.removeEventListener = function (type, listener) {
        const map = this._et;
        if (!map || listener === undefined || listener === null) return;
        const list = map[String(type)];
        if (!list) return;
        for (let i = 0; i < list.length; i++) {
            if (list[i][0] === listener) { list.splice(i, 1); break; }
        }
    };

    // Protected fire core shared by dispatchEvent AND the WebSocket engine pump: snapshot the
    // live list, honour `once` (removed BEFORE the callback, §2.6 step 12), stop on
    // stopImmediatePropagation(). `swallowErrors` mirrors the Web rule that listener exceptions
    // never reach the dispatcher (the WS pump keeps its historical propagating shape).
    EventTarget.prototype.__czFire = function (type, event, swallowErrors) {
        const map = this._et || (this._et = Object.create(null));
        const list = map[String(type)];
        if (!list || list.length === 0) return;
        const snapshot = list.slice();
        for (let i = 0; i < snapshot.length; i++) {
            if (event._stopImmediate) break;
            const entry = snapshot[i];
            if (entry[1]) {
                const live = map[String(type)];
                const idx = live.indexOf(entry);
                if (idx >= 0) live.splice(idx, 1);
            }
            const fn = entry[0];
            const call = typeof fn === "function" ? fn : fn.handleEvent;
            // Phase 15B (DOM spec): object listeners get `this === listener`; plain function
            // listeners keep the traditional event-target receiver.
            const receiver = typeof fn === "function" ? this : fn;
            if (swallowErrors) { try { call.call(receiver, event); } catch (_e) {} }
            else { call.call(receiver, event); }
        }
    };

    EventTarget.prototype.dispatchEvent = function (event) {
        if (!event || typeof event.type !== "string") throw new TypeError("dispatchEvent requires an event with a string type");
        event.target = this;
        event.currentTarget = this;
        if (!(event instanceof Event)) {
            event._stopImmediate = false;
            if (event.defaultPrevented === undefined) event.defaultPrevented = false;
            if (typeof event.preventDefault !== "function") {
                event.preventDefault = function () { if (this.cancelable !== false) this.defaultPrevented = true; };
            }
        }
        this.__czFire(event.type, event, true);
        return !event.defaultPrevented;
    };

    // ---- structuredClone -------------------------------------------------------------------
    function dataCloneError(msg) {
        const e = new Error(msg);
        e.name = "DataCloneError";
        return e;
    }

    function cloneValue(v, seen) {
        if (typeof v === "function" || typeof v === "symbol") {
            throw dataCloneError("could not clone " + typeof v + " values");
        }
        if (v === null || typeof v !== "object") return v; // primitives (incl. bigint) are shared
        if (seen.has(v)) return seen.get(v);               // circular reference -> existing copy

        if (Array.isArray(v)) {
            const out = [];
            seen.set(v, out);
            for (let i = 0; i < v.length; i++) out[i] = cloneValue(v[i], seen);
            const extra = Object.keys(v);
            for (let i = 0; i < extra.length; i++) {
                const k = extra[i];
                const n = Number(k);
                if (Number.isInteger(n) && String(n) === k && n >= 0 && n < v.length) continue; // index slots done
                out[k] = cloneValue(v[k], seen);
            }
            return out;
        }
        if (v instanceof Date) { const d = new Date(v.getTime()); seen.set(v, d); return d; }
        if (v instanceof RegExp) {
            const r = new RegExp(v.source, v.flags);
            r.lastIndex = v.lastIndex;
            seen.set(v, r);
            return r;
        }
        if (v instanceof Map) {
            const m = new Map();
            seen.set(v, m);
            const entries = [];
            v.forEach(function (val, key) { entries.push([key, val]); }); // snapshot: iteration while cloning must not alias
            for (let i = 0; i < entries.length; i++) m.set(cloneValue(entries[i][0], seen), cloneValue(entries[i][1], seen));
            return m;
        }
        if (v instanceof Set) {
            const s = new Set();
            seen.set(v, s);
            const vals = [];
            v.forEach(function (val) { vals.push(val); });
            for (let i = 0; i < vals.length; i++) s.add(cloneValue(vals[i], seen));
            return s;
        }
        if (v instanceof ArrayBuffer) { const ab = v.slice(0); seen.set(v, ab); return ab; } // backing BYTES copied
        if (ArrayBuffer.isView(v)) {
            let copy;
            if (typeof DataView !== "undefined" && v instanceof DataView) {
                copy = new DataView(v.buffer.slice(v.byteOffset, v.byteOffset + v.byteLength));
            } else {
                copy = new v.constructor(v); // typed-array ctor over a view: native element copy
            }
            seen.set(v, copy);
            return copy;
        }
        // Plain objects keep their prototype (null-prototype records included).
        const out = Object.create(Object.getPrototypeOf(v));
        seen.set(v, out);
        const keys = Object.keys(v);
        for (let i = 0; i < keys.length; i++) out[keys[i]] = cloneValue(v[keys[i]], seen);
        return out;
    }

    globalThis.__czDataCloneErr = function (kind) {
        return dataCloneError("could not clone " + kind + " values");
    };
    globalThis.__czCloneFallback = function (value) { return cloneValue(value, new Map()); };
    globalThis.structuredClone = function (value) { return __czStructuredCloneFast(value); };

    // ---- queueMicrotask --------------------------------------------------------------------
    // Schedules on the QuickJS promise-job queue (`Promise.resolve().then`); the runner drains
    // jobs OUTSIDE context scopes, so microtasks land before any setTimeout macrotask.
    globalThis.queueMicrotask = function (fn) {
        if (typeof fn !== "function") throw new TypeError("queueMicrotask requires a callback function");
        Promise.resolve().then(function () { fn(); }); // callbacks receive NO arguments (§queueMicrotask)
    };

    globalThis.Event = Event;
    globalThis.CustomEvent = CustomEvent;
    globalThis.EventTarget = EventTarget;
    globalThis.__czWebGlobalsInstalled = true;
})();
"#;
