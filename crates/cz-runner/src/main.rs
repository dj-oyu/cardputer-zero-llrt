//! `cz-js` — Cardputer Zero LLRT CLI runner for plain ES modules (no bundler, no
//! `node_modules`). Supports every `cardputer:*` builtin (display, input, app, power, kv, json
//! plus the Phase 6A peripherals imu/audio/ir/io), Node-style `node:http`/`http` and
//! `node:fs`/`fs`, and relative file imports (`./hud.mjs`) resolved straight from disk.
//!
//! Builtin registration is delegated to `cz_modules::resolve_builtin` / `declare_builtin`: one
//! `ModuleKind` per specifier, so a new module only needs registering once inside `cz-modules`
//! (the runner resolves it automatically). The `peripheral_hw_modules_...` test below proves the
//! Phase 6A specifiers actually resolve and evaluate through this runner's loader.

use std::io::Write;
use std::path::PathBuf;

use rquickjs::loader::{FileResolver, Loader, Resolver, ScriptLoader};
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Error, Function, Module, Runtime};

const VERSION: &str = concat!("cardputer-zero-llrt ", env!("CARGO_PKG_VERSION"));

struct CzResolver {
    file: FileResolver,
}

impl CzResolver {
    fn new() -> Self {
        let mut file = FileResolver::default();
        file.add_path(".");
        // Accept both .js and .mjs script files on-device.
        for pattern in ["{}.js", "{}.mjs"] {
            file.add_pattern(pattern);
        }
        Self { file }
    }
}

impl Resolver for CzResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        // Builtins pass through unchanged: cardputer:* plus Node-style "node:http"/"http" and
        // "node:fs"/"fs" so plain bundler-free web-server scripts import like real Node.
        if cz_modules::resolve_builtin(name).is_some() {
            return Ok(name.to_string());
        }
        if name.starts_with('.') {
            // Relative ESM import resolved against the importing script's directory using
            // std paths (works with absolute module names on-device).
            let base_path = std::path::Path::new(base);
            let dir = match base_path.parent() {
                Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
                _ => return Err(Error::new_resolving(base, name)),
            };
            let joined = dir.join(name);
            let normalized = normalize_path(&joined);
            if normalized.is_file() {
                return Ok(normalized.display().to_string());
            }
            // Try implicit extensions (.mjs/.js) like Node ESM-lite on-device.
            for ext in ["mjs", "js"] {
                let with_ext = normalized.with_extension(ext);
                if with_ext.is_file() {
                    return Ok(with_ext.display().to_string());
                }
            }
            return Err(Error::new_resolving(base, name));
        }
        if name.starts_with('/') {
            // Absolute specifiers resolve straight from disk (Node ESM-style). The Phase 9A
            // suite imports `examples/aws-edge/*.mjs` from temp-dir scripts by absolute path,
            // mirroring how Node resolves file-URL-like module specifiers.
            let abs = std::path::Path::new(name);
            if abs.is_file() {
                return Ok(name.to_string());
            }
            for ext in ["mjs", "js"] {
                let with_ext = abs.with_extension(ext);
                if with_ext.is_file() {
                    return Ok(with_ext.display().to_string());
                }
            }
            return Err(Error::new_resolving(base, name));
        }
        // Bare specifiers: delegate to the file resolver search paths.
        self.file.resolve(_ctx, base, name, _attributes)
    }
}

/// Lexically normalize `..`/`.` components without touching the filesystem.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct CzLoader {
    script: ScriptLoader,
}

impl CzLoader {
    fn new() -> Self {
        let mut script = ScriptLoader::default();
        script.add_extension("mjs");
        script.add_extension("js");
        Self { script }
    }
}

impl Loader for CzLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        if let Some(kind) = cz_modules::resolve_builtin(name) {
            return cz_modules::declare_builtin(ctx, kind, name);
        }
        self.script.load(ctx, name, attributes)
    }
}

/// Bound Rust printer used by the JS `console` shim. Level 0/1 write to stdout, level >= 2
/// writes to stderr. All values are pre-stringified by the JS shim.
fn bind_console(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let printer = Function::new(
        ctx.clone(),
        |level: i32, parts: Vec<String>| -> rquickjs::Result<()> {
            let line = parts.join(" ");
            if level >= 2 {
                std::io::stderr().write_all(line.as_bytes())?;
                std::io::stderr().write_all(b"\n")?;
            } else {
                std::io::stdout().write_all(line.as_bytes())?;
                std::io::stdout().write_all(b"\n")?;
            }
            Ok(())
        },
    )?;
    ctx.globals().set("__cz_print", printer)?;

    // console shim with LLRT-style stringification semantics.
    let shim = r#"
        (function () {
            function fmt(v) {
                if (typeof v === "string") return v;
                if (v === null) return "null";
                if (v === undefined) return "undefined";
                if (typeof v === "bigint") return String(v) + "n";
                if (typeof v === "object" || typeof v === "symbol") {
                    try { const j = JSON.stringify(v); return j === undefined ? String(v) : j; }
                    catch (_e) { return String(v); }
                }
                return String(v);
            }
            globalThis.console = {
                log:   (...a) => __cz_print(0, a.map(fmt)),
                warn:  (...a) => __cz_print(1, a.map(fmt)),
                error: (...a) => __cz_print(2, a.map(fmt)),
            };
        })();
    "#;
    ctx.eval::<(), _>(shim)?;
    Ok(())
}

/// Evaluate one ES module source under `name` (must be called inside a context scope).
#[cfg(test)]
fn eval_module(ctx: &Ctx<'_>, name: &str, source: &str) -> Result<(), String> {
    let promise = match Module::evaluate(ctx.clone(), name, source).catch(ctx) {
        Ok(p) => p,
        Err(caught) => return Err(render_caught(ctx, &caught)),
    };
    match promise.finish::<()>().catch(ctx) {
        Ok(()) => {}
        Err(caught) => return Err(render_caught(ctx, &caught)),
    }
    Ok(())
}

/// Drain pending QuickJS jobs. Call only when no context scope is active — pumping jobs while a
/// `Context::with` scope is open re-enters the runtime lock and panics inside rquickjs.
fn drain_jobs(rt: &Runtime) {
    while let Ok(true) = rt.execute_pending_job() {}
}

// ---------------------------------------------------------------------------
// Top-level-await module settlement (Phase 8A)
// ---------------------------------------------------------------------------
//
// `Promise::finish()` spins the QuickJS job queue and returns `WouldBlock` when the queue empties
// before a top-level awaited promise can settle (fetch/TLS waits on worker threads). Instead of
// blocking, we attach JavaScript settlement handlers through a tiny glue function (correct `this`
// binding) that record the outcome on `globalThis`, then pump the event loop OUTSIDE any
// `Context::with` scope until those flags flip.
const MODULE_GLUE: &str = r#"
    (function () {
        globalThis.__czModuleDone = false;
        globalThis.__czModuleErrText = "";
        globalThis.__czWireModule = function (promise) {
            promise.then(
                function (_v) { globalThis.__czModuleDone = true; },
                function (err) {
                    var text = "unknown script error";
                    if (err && typeof err === "object") {
                        var name = err.name ? String(err.name) : "Error";
                        var msg = err.message !== undefined && err.message !== null ? String(err.message) : String(err);
                        var head = msg.indexOf(name) === 0 ? msg : name + ": " + msg;
                        var stack = err.stack ? String(err.stack) : "";
                        text = stack && stack.indexOf(head) < 0 ? head + "\n" + stack : head;
                    } else if (err !== undefined && err !== null) {
                        text = String(err);
                    }
                    globalThis.__czModuleErrText = text;
                    globalThis.__czModuleDone = true;
                }
            );
        };
        globalThis.__czModuleSettled = function () { return globalThis.__czModuleDone ? 1 : 0; };
        globalThis.__czModuleError = function () { return globalThis.__czModuleErrText; };
    })();
"#;

/// Evaluate an ES module WITHOUT blocking on its promise: settlement lands on the JS flags above.
fn start_module<'js>(ctx: &Ctx<'js>, name: &str, source: &str) -> Result<(), String> {
    let promise = match Module::evaluate(ctx.clone(), name, source).catch(ctx) {
        Ok(p) => p,
        Err(caught) => return Err(render_caught(ctx, &caught)),
    };
    let wire: Function = ctx.globals().get("__czWireModule").map_err(|e| format!("glue missing: {e}"))?;
    wire.call::<_, ()>((promise.into_value(),)).map_err(|e| format!("wire call: {e}"))?;
    Ok(())
}

/// True once the module promise settled (JS flag). Only ever read between job drains.
fn module_settled(ctx: &Context) -> bool {
    ctx.with(|c| call_js_i64(&c, "__czModuleSettled", 0) != 0)
}

/// The rejected error text when the module threw (empty when it resolved cleanly).
fn module_error_text(ctx: &Context) -> Option<String> {
    match ctx.with(|c| c.globals().get::<_, String>("__czModuleErrText")) {
        Ok(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// Pump the engine (HTTP + fetch + timers + QuickJS jobs) until the module settles. Returns `Ok(true)`
/// when settled, `Ok(false)` when nothing can make progress anymore (deadlock). Never pumps jobs
/// while a context scope is open (rquickjs RefCell rule).
fn settle_module(rt: &Runtime, ctx: &Context) -> Result<bool, String> {
    let started = std::time::Instant::now();
    let mut idle_passes = 0usize;
    while !module_settled(ctx) {
        if cz_modules::should_quit() {
            return Ok(false);
        }
        loop_pass(rt, ctx);
        if module_settled(ctx) {
            return Ok(true);
        }
        let (_, timers, extra_work, fetch_active, tls_active, ws_active) = loop_liveness(rt, ctx);
        if !extra_work && timers <= 0 && fetch_active == 0 && tls_active == 0 && ws_active == 0
            && cz_modules::http_listening_count() == 0
        {
            idle_passes += 1;
            // Brief grace window: a worker thread may be between notify and dispatch.
            if idle_passes > 60 {
                return Ok(false);
            }
        } else {
            idle_passes = 0;
        }
        if started.elapsed().as_secs() > 3600 {
            return Err("module evaluation exceeded the 1h safety valve".to_string());
        }
        cz_modules::http_idle_pause();
    }
    Ok(true)
}


/// Install Node-compatible timers on `globalThis`. Backed by a JS registry that the Rust event
/// loop advances with `__czTimersTick()` (Date.now() based; returns the remaining count).
fn bind_timers(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let shim = r#"
        (function () {
            if (globalThis.__czTimersInstalled) return;
            const timers = new Map();
            let nextId = 1;
            // Phase 10C: a shared frozen empty list stands in for "no extra arguments", so the
            // common setTimeout(fn, delay) call never allocates through Array.prototype.slice.
            const EMPTY_ARGS = Object.freeze([]);
            function delayOf(d) {
                const n = Number(d);
                return Number.isFinite(n) && n > 0 ? n : 0;
            }
            globalThis.setTimeout = function (fn, delay) {
                if (typeof fn !== "function") throw new TypeError("setTimeout callback must be a function");
                // Only slice when the caller really passed extra arguments (arguments.length > 2).
                const extra = arguments.length > 2 ? Array.prototype.slice.call(arguments, 2) : EMPTY_ARGS;
                const id = nextId++;
                timers.set(id, { fn: fn, at: Date.now() + delayOf(delay), every: null, extra: extra });
                return id;
            };
            globalThis.setInterval = function (fn, delay) {
                if (typeof fn !== "function") throw new TypeError("setInterval callback must be a function");
                const extra = arguments.length > 2 ? Array.prototype.slice.call(arguments, 2) : EMPTY_ARGS;
                const id = nextId++;
                const step = delayOf(delay);
                timers.set(id, { fn: fn, at: Date.now() + (step > 0 ? step : 1), every: step > 0 ? step : 1, extra: extra });
                return id;
            };
            globalThis.clearTimeout = function (id) { timers.delete(id); };
            globalThis.clearInterval = function (id) { timers.delete(id); };
            globalThis.__czTimerCount = function () { return timers.size; };
            globalThis.__czTimersTick = function () {
                const now = Date.now();
                const due = [];
                for (const entry of timers.entries()) {
                    if (entry[1].at <= now) due.push(entry);
                }
                due.sort(function (a, b) { return a[1].at - b[1].at; });
                for (const pair of due) {
                    const id = pair[0], t = pair[1];
                    // A callback may clear/replace timers, so re-check membership before firing.
                    if (!timers.has(id)) continue;
                    // Phase 10C: mutate `t.at` IN PLACE for intervals instead of allocating a fresh
                    // timer object on every tick (the interval registry entry is reused).
                    if (t.every !== null) t.at = Date.now() + t.every;
                    else timers.delete(id);
                    try { t.fn.apply(undefined, t.extra); }
                    catch (e) {
                        const msg = (e && e.stack) ? e.stack : String((e && e.message) || e);
                        if (globalThis.console && console.error) console.error(msg);
                    }
                }
                return timers.size;
            };
            globalThis.__czTimersInstalled = true;
        })();
    "#;
    ctx.eval::<(), _>(shim)?;
    Ok(())
}

/// One JS scope per event-loop iteration drives every engine drain in the exact order the
/// standalone passes used to run them (fetch -> TLS -> HTTP close notifications -> HTTP requests ->
/// timers). Without it `loop_pass` would open five separate `ctx.with()` scopes and perform five
/// `globalThis` lookups on every idle pass.
const LOOP_DRAIN_GLUE: &str = r#"
    (function () {
        if (globalThis.__czLoopDrainInstalled) return;
        // Full pass: engine events first, then HTTP closes/requests, then due timers.
        globalThis.__czLoopDrain = function () {
            var n = 0;
            if (typeof __czFetchDrainLoop === "function") n += __czFetchDrainLoop();
            if (typeof __czWsDrainLoop === "function") n += __czWsDrainLoop();
            if (typeof __czTlsDrainLoop === "function") n += __czTlsDrainLoop();
            if (typeof __czHttpCheckCloses === "function") n += __czHttpCheckCloses();
            if (typeof __czHttpDrainLoop === "function") n += __czHttpDrainLoop();
            if (typeof __czTimersTick === "function") n += __czTimersTick();
            return n;
        };
        // Follow-up rescan for work created BY the pass above; deliberately skips timers so an
        // interval cannot fire twice inside one event-loop iteration.
        globalThis.__czRescanDrain = function () {
            var n = 0;
            if (typeof __czHttpCheckCloses === "function") n += __czHttpCheckCloses();
            if (typeof __czHttpDrainLoop === "function") n += __czHttpDrainLoop();
            if (typeof __czFetchDrainLoop === "function") n += __czFetchDrainLoop();
            if (typeof __czWsDrainLoop === "function") n += __czWsDrainLoop();
            if (typeof __czTlsDrainLoop === "function") n += __czTlsDrainLoop();
            return n;
        };
        globalThis.__czLoopDrainInstalled = true;
    })();
"#;

/// Install the combined drain glue (idempotent; safe when no engine module was ever imported).
fn bind_loop_drain_glue(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(LOOP_DRAIN_GLUE)?;
    Ok(())
}

/// Invoke `fnName()` returning an i64 count; missing/non-function yields `fallback`.
fn call_js_i64(ctx: &Ctx<'_>, fn_name: &str, fallback: i64) -> i64 {
    match ctx.globals().get::<_, Function>(fn_name) {
        Ok(f) => {
            // Annotated binding (not a partial turbofish) so both generic parameters infer.
            let n: i64 = f.call(()).unwrap_or(fallback);
            n
        }
        Err(_) => fallback,
    }
}

/// True when the JS HTTP dispatch glue exists in this context (node:http was imported).
fn has_http_glue(ctx: &Ctx<'_>) -> bool {
    ctx.globals().get::<_, Function>("__czHttpDrainLoop").is_ok()
}

/// One event-loop iteration: dispatch queued HTTP requests (which delivers `data`/`end` through
/// JS jobs), flush their responses, then fire due timers.
///
/// CRITICAL rquickjs rule learned from a panic backtrace: every `Runtime` API that locks the
/// runtime (`execute_pending_job`, `is_job_pending`) must be called OUTSIDE an active
/// `Context::with()` scope, otherwise the runtime `RefCell` is re-entered and panics with
/// "RefCell already borrowed". So scopes are opened, closed, and only then jobs are pumped.
fn loop_pass(rt: &Runtime, ctx: &Context) {
    // Phase 10C: ONE scope per iteration drives arrived fetch events, raw TLS socket events, HTTP
    // "close" notifications, queued HTTP requests and due timers — the exact order the five separate
    // `ctx.with()` scopes used to use (engine threads still `notify_event_loop()` on every arrival).
    // QuickJS promise jobs created along the way are pumped strictly OUTSIDE the scope (RefCell rule).
    ctx.with(|c| call_js_i64(&c, "__czLoopDrain", 0));
    drain_jobs(rt);

    // 3. Give work CREATED by the pass above one more dispatch round (still a single scope, and it
    //    skips timers so an interval can never fire twice inside one iteration).
    if rt.is_job_pending() || cz_modules::http_has_pending() || cz_modules::fetch_has_pending()
        || cz_modules::tls_has_pending() || cz_modules::ws_has_pending()
    {
        ctx.with(|c| call_js_i64(&c, "__czRescanDrain", 0));
        drain_jobs(rt);
    }
}

/// Liveness snapshot: listening servers, pending JS timers, plus queued engine requests, pending
/// QuickJS jobs, queued TLS events and live TLS sockets/servers. Runtime queries run outside any
/// context scope (rquickjs RefCell rule).
fn loop_liveness(rt: &Runtime, ctx: &Context) -> (usize, i64, bool, usize, usize, usize) {
    let listening = cz_modules::http_listening_count();
    let queued = cz_modules::http_has_pending()
        || cz_modules::fetch_has_pending()
        || cz_modules::tls_has_pending()
        || cz_modules::ws_has_pending();
    let job_pending = rt.is_job_pending();
    let timers = ctx.with(|c| call_js_i64(&c, "__czTimerCount", 0));
    // Live fetch slots keep the process alive even when no node:http server exists.
    let fetch_active = cz_modules::fetch_active_count();
    // Phase 8C: open raw TLS sockets + listening TLS servers keep the loop alive too.
    let tls_active = cz_modules::tls_active_count();
    // Phase 11: open WebSocket connections must hold the loop until their close handshake lands.
    let ws_active = cz_modules::ws_active_count();
    (listening, timers, queued || job_pending, fetch_active, tls_active, ws_active)
}

/// Keep the process alive while any HTTP server listens or any timer is pending. The idle pause
/// (~5ms) keeps CardputerZero CPU usage low; SIGINT/SIGTERM set the shared quit flag.
fn run_event_loop(rt: &Runtime, ctx: &Context) -> Result<(), String> {
    if cz_modules::should_quit() {
        return Ok(());
    }
    let has_http = ctx.with(|c| has_http_glue(&c));
    if cz_modules::http_listening_count() > 0 && !cz_modules::install_signal_handlers() {
        eprintln!("cz-js: warning: could not install SIGINT/SIGTERM handlers");
    }
    let started = std::time::Instant::now();
    // Safety valve: a wedged handler must not spin the process forever.
    loop {
        if cz_modules::should_quit() {
            break;
        }
        loop_pass(rt, ctx);
        let (listening, timers, extra_work, fetch_active, tls_active, ws_active) = loop_liveness(rt, ctx);
        if listening == 0 && timers <= 0 && !extra_work && fetch_active == 0 && tls_active == 0 && ws_active == 0 {
            break;
        }
        if !has_http && listening == 0 && timers <= 0 && fetch_active == 0 && tls_active == 0 && ws_active == 0 {
            break;
        }
        if started.elapsed().as_secs() > 3600 {
            eprintln!("cz-js: event loop exceeded the 1h safety valve; exiting");
            break;
        }
        cz_modules::http_idle_pause();
    }
    Ok(())
}

/// Render a caught JS/Rust error with message and stack where available.
fn render_caught(ctx: &Ctx<'_>, caught: &CaughtError<'_>) -> String {
    let _ = ctx;
    let text = format!("{caught}");
    if let CaughtError::Exception(ex) = caught {
        let msg = ex.message().unwrap_or_else(|| "unknown exception".into());
        let stack = ex.stack().unwrap_or_default();
        if stack.is_empty() || stack.contains(&msg) {
            return msg;
        }
        return format!("{msg}\n{stack}");
    }
    if text.trim().is_empty() {
        return "unknown script error".into();
    }
    text
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--version" {
        println!("{VERSION}");
        return;
    }

    let (name, source) = if args.len() >= 3 && args[1] == "-e" {
        ("-e".to_string(), args[2].clone())
    } else if args.len() >= 2 && !args[1].starts_with('-') {
        let path = PathBuf::from(&args[1]);
        let abs = path.canonicalize().unwrap_or(path.clone());
        let text = match std::fs::read_to_string(&abs) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cz-js: cannot read {}: {e}", abs.display());
                std::process::exit(1);
            }
        };
        (abs.display().to_string(), text)
    } else {
        eprintln!("usage: cz-js [--version] [-e <code> | <script.mjs>]");
        std::process::exit(1);
    };

    let rt = Runtime::new().expect("runtime");
    // Final Boss R2 (w7_js_core): QuickJS defaults `malloc_gc_threshold` to 256 KiB
    // (`rt->malloc_gc_threshold = 256 * 1024` in quickjs.c JS_NewRuntime), so the 6,000-object
    // allocation burst in w7_js_core crosses it mid-loop and pays FULL cycle-GC sweeps for
    // garbage reference counting already collected. Host A/B (retained-live-heap probe,
    // /tmp/w7_keep.mjs x3): 118/124/135 ms at default -> **94/94/96 ms** with a raised threshold
    // (-20% median; sweeps are pure overhead while everything live stays reachable). 4 MiB
    // matches LLRT practice, bounds uncollected cyclic garbage well inside CardputerZero's
    // 512 MB RAM, and emits zero new atomics (plain field store in JS_SetGCThreshold).
    rt.set_gc_threshold(4 * 1024 * 1024);
    rt.set_loader(CzResolver::new(), CzLoader::new());
    let ctx = Context::full(&rt).expect("context");

    let run_result = ctx.with(|ctx| -> Result<(), String> {
        bind_console(&ctx).map_err(|e| format!("{e}"))?;
        bind_timers(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 10C: one JS scope per event-loop pass drains fetch + TLS + HTTP closes/requests +
        // timers in the original order.
        bind_loop_drain_glue(&ctx).map_err(|e| format!("loop drain glue: {e}"))?;
        ctx.globals().set("__czRunnerManagedPump", true).map_err(|e| format!("{e}"))?;
        // Phase 5: swap globalThis.JSON.parse / JSON.stringify for the cz-json NEON engine
        // (fast < 4 KiB path, transparent Lazy handles for larger documents, kv-row shortcut).
        cz_modules::install_json(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 8A: global fetch()/Headers/Request/Response/AbortController/AbortSignal/
        // ReadableStream (+ URLSearchParams when absent) built on cz-net's sync worker engine.
        cz_modules::install_fetch(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 12: globalThis.crypto + crypto.subtle (ring-only SHA/HMAC/AES-GCM, in-place
        // getRandomValues fill, RFC 4122 randomUUID).
        cz_modules::install_web_crypto(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 12: native atob/btoa + Blob/File/FormData (+ Response.blob()/formData()).
        // Runs after install_fetch so the Response prototype patch finds the class.
        cz_modules::install_web(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 11: global WHATWG URL (Rust-parsed record + live searchParams) and the RFC 6455
        // WebSocket client (`ws://` + `wss://`) driven by cz-net's worker engine.
        cz_modules::install_url(&ctx).map_err(|e| format!("{e}"))?;
        cz_modules::install_ws(&ctx).map_err(|e| format!("{e}"))?;
        // Phase 9A: globalThis.process (env/argv/cwd/exit/platform/arch) so cloud REST clients
        // can read AWS_ENDPOINT_URL & friends; full std::env::args() keeps script CLI args live.
        cz_modules::install_process(&ctx, &args).map_err(|e| format!("{e}"))?;
        // Phase 8A: top-level `await fetch(...)` must not deadlock — evaluate the module without
        // blocking on its promise (JS glue records settlement), then pump the loop outside scopes.
        ctx.eval::<(), _>(MODULE_GLUE).map_err(|e| format!("module glue: {e}"))?;
        start_module(&ctx, &name, &source)
    });
    // Drain any remaining QuickJS jobs outside the context scope.
    drain_jobs(&rt);

    match run_result {
        Ok(()) => {
            // Settle the module's top-level await by pumping HTTP/fetch/timers + jobs (never while
            // a context scope is open), then exit non-zero when the script threw.
            match settle_module(&rt, &ctx) {
                Ok(true) => {}
                Ok(false) => {
                    let msg = module_error_text(&ctx)
                        .unwrap_or_else(|| "script's top-level await never resolved (deadlock)".to_string());
                    eprintln!("cz-js: {msg}");
                    std::process::exit(1);
                }
                Err(msg) => {
                    eprintln!("cz-js: {msg}");
                    std::process::exit(1);
                }
            }
            if let Some(text) = module_error_text(&ctx) {
                eprintln!("cz-js: {text}");
                std::io::stdout().flush().ok();
                std::io::stderr().flush().ok();
                std::process::exit(1);
            }
            // Stay alive while an HTTP server is listening or timers are pending, then exit 0.
            if let Err(msg) = run_event_loop(&rt, &ctx) {
                eprintln!("cz-js: {msg}");
                std::process::exit(1);
            }
            drain_jobs(&rt);
            std::io::stdout().flush().ok();
            std::io::stderr().flush().ok();
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("cz-js: {msg}");
            std::process::exit(1);
        }
    }
}


#[cfg(test)]
mod runner_tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn version_string_shape() {
        assert!(VERSION.starts_with("cardputer-zero-llrt "));
        assert!(VERSION.contains("0.1.0"));
    }

    #[test]
    fn console_and_module_eval_end_to_end() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            let src = r#"
                import * as app from "cardputer:app";
                const info = app.info();
                console.log("RUNNER_APP_OK", info.device === "M5Stack CardputerZero", 42, null);
                globalThis.__ok = info.runtime === "cardputer-zero-llrt";
            "#;
            eval_module(&ctx, "inline.mjs", src).expect("eval should pass");
            let ok: bool = ctx.globals().get("__ok").unwrap();
            assert!(ok);
        });
    }

    #[test]
    fn peripheral_hw_modules_resolve_and_evaluate_through_the_runner_loader() {
        // Registration proof for cardputer:imu / audio / ir / io inside the real cz-js loader:
        // named + default exports must resolve without any env-var overrides present.
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            let src = r#"
                import * as imu from "cardputer:imu";
                import imuDef from "cardputer:imu";
                import * as audio from "cardputer:audio";
                import * as ir from "cardputer:ir";
                import * as io from "cardputer:io";
                if (typeof imu.devices !== "function" || typeof imuDef.read !== "function") {
                    throw new Error("imu surface");
                }
                const r = imu.read();
                if (typeof r.available !== "boolean") throw new Error("imu read type");
                const wav = audio.generateWav(440, 20, 0.5);
                if (!(wav instanceof Uint8Array) || wav.length !== 44 + 882 * 2) {
                    throw new Error("wav shape " + wav.length);
                }
                if (typeof audio.beep !== "function" || typeof audio.playWav !== "function") {
                    throw new Error("audio surface");
                }
                const pulses = ir.encodeNec(0x01, 0x02);
                if (!(pulses instanceof Uint32Array) || pulses.length !== 67) throw new Error("nec len");
                const dec = ir.decodeNec(pulses);
                if (!dec || !dec.valid) throw new Error("nec decode");
                if (typeof ir.sendRaw !== "function" || typeof ir.sendNec !== "function") {
                    throw new Error("ir tx surface");
                }
                const ports = io.listPorts();
                for (const k of ["i2c", "spi", "gpio", "uart"]) {
                    if (!Array.isArray(ports[k])) throw new Error("ports." + k + " must be an array");
                }
                if (io.isReservedI2cAddr(0x55) !== true || io.isReservedI2cAddr(0x10) !== false) {
                    throw new Error("reserved guard");
                }
                if (typeof io.uartWrite !== "function" || typeof io.uartRead !== "function") {
                    throw new Error("uart surface");
                }
                console.log("P6_HW_OK");
                globalThis.__p6 = true;
            "#;
            eval_module(&ctx, "inline_p6.mjs", src).expect("peripheral module eval");
            let ok: bool = ctx.globals().get("__p6").unwrap();
            assert!(ok, "Phase 6A peripherals must evaluate");
        });
    }

    #[test]
    fn thrown_error_propagates_as_string() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let res = ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            eval_module(&ctx, "boom.mjs", r#"throw new Error("runner_boom_test");"#)
        });
        drain_jobs(&rt);
        let err = res.expect_err("throwing module must produce Err");
        assert!(err.contains("runner_boom_test"), "error text was: {err}");
    }

    #[test]
    fn relative_mjs_imports_resolve_from_script_dir() {
        let dir = std::env::temp_dir().join(format!("cz-runner-rel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let lib = dir.join("lib.mjs");
        std::fs::write(&lib, "export const twice = (x) => x * 2;\n").expect("write lib");
        let main = dir.join("main.mjs");
        std::fs::write(
            &main,
            "import { twice } from \"./lib.mjs\";\nglobalThis.__ok = twice(21) === 42;\n",
        )
        .expect("write main");

        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let abs = main.canonicalize().unwrap();
        ctx.with(|ctx| {
            let src = std::fs::read_to_string(&abs).unwrap();
            eval_module(&ctx, &abs.display().to_string(), &src).expect("relative import eval");
            let ok: bool = ctx.globals().get("__ok").unwrap();
            assert!(ok, "relative ./lib.mjs import must work");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unresolved_import_produces_error_text() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let res = ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            eval_module(
                &ctx,
                "miss.mjs",
                "import { nope } from \"./definitely-missing-module.mjs\";\nglobalThis.x = nope;\n",
            )
        });
        drain_jobs(&rt);
        let err = res.expect_err("missing module import must error");
        assert!(err.contains("definitely-missing-module"), "err was: {err}");
    }

    fn loop_setup() -> (Runtime, Context) {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|c| {
            bind_console(&c).unwrap();
            bind_timers(&c).unwrap();
            bind_loop_drain_glue(&c).unwrap();
        });
        (rt, ctx)
    }

    /// The quit flag and event-loop liveness are *process-wide* statics (real Ctrl-C semantics),
    /// so tests that drive `run_event_loop` must not run concurrently with each other.
    fn event_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn event_loop_fires_timeout_then_exits_promptly() {
        let _lock = event_test_lock();
        cz_modules::reset_quit_flag();
        let (rt, ctx) = loop_setup();
        ctx.with(|c| {
            eval_module(
                &c,
                "inline_timeout.mjs",
                r#"
                    globalThis.globalA = null;
                    setTimeout((tag) => { globalThis.globalA = tag; }, 10, "fired");
                    globalThis.__touched = true;
                "#,
            )
            .unwrap();
        });
        drain_jobs(&rt);
        let started = std::time::Instant::now();
        run_event_loop(&rt, &ctx).unwrap();
        assert!(started.elapsed().as_millis() < 2000, "loop hung: {:?}", started.elapsed());
        let got = ctx.with(|c| c.eval::<Option<String>, _>("globalThis.globalA").unwrap());
        assert_eq!(got.as_deref(), Some("fired"), "timeout callback must run via the loop");
    }

    #[test]
    fn event_loop_fires_interval_ticks_and_exits_after_self_clear() {
        let _lock = event_test_lock();
        cz_modules::reset_quit_flag();
        let (rt, ctx) = loop_setup();
        ctx.with(|c| {
            eval_module(
                &c,
                "inline_interval.mjs",
                r#"
                    globalThis.globalB = 0;
                    const id = setInterval(() => {
                        globalThis.globalB += 1;
                        if (globalThis.globalB >= 3) clearInterval(id);
                    }, 4);
                "#,
            )
            .unwrap();
        });
        drain_jobs(&rt);
        let started = std::time::Instant::now();
        run_event_loop(&rt, &ctx).unwrap();
        let ticks = ctx.with(|c| c.eval::<i64, _>("globalThis.globalB").unwrap());
        assert!(ticks >= 3, "interval must tick at least 3 times, got {ticks}");
        assert!(started.elapsed().as_millis() < 4000, "interval loop too slow: {:?}", started.elapsed());
    }

    #[test]
    fn event_loop_serves_one_request_then_exits_when_server_closes() {
        let _lock = event_test_lock();
        cz_modules::reset_quit_flag();
        let (rt, ctx) = loop_setup();
        ctx.with(|c| {
            eval_module(
                &c,
                "inline_http.mjs",
                r#"
                    import { createServer } from "node:http";
                    globalThis.globalC = null;
                    const seen = [];
                    const server = createServer((req, res) => {
                        seen.push(req.url);
                        globalThis.globalC = seen.slice();
                        res.writeHead(201, { "Content-Type": "text/plain", "X-CZ": "yes" });
                        res.end("PONG_BODY");
                        setTimeout(() => server.close(), 20);
                    });
                    server.listen(0, "127.0.0.1");
                    globalThis.__port = server.address().port;
                "#,
            )
            .unwrap();
        });
        drain_jobs(&rt);
        let port = ctx.with(|c| c.eval::<u16, _>("globalThis.__port").unwrap());
        assert!(port > 0, "ephemeral port should be reported");
        let client = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            use std::io::{Read, Write};
            stream
                .write_all(b"GET /api/ping HTTP/1.1
Host: z
Connection: close

")
                .unwrap();
            let mut buf = String::new();
            loop {
                let mut chunk = [0u8; 512];
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
            buf
        });
        let started = std::time::Instant::now();
        run_event_loop(&rt, &ctx).unwrap();
        let raw = client.join().unwrap();
        assert!(raw.starts_with("HTTP/1.1 201 Created"), "status line was: {raw:?}");
        assert!(raw.contains("PONG_BODY") && raw.contains("X-CZ: yes"), "response was: {raw:?}");
        let urls = ctx.with(|c| c.eval::<Option<String>, _>("JSON.stringify(globalThis.globalC)").unwrap());
        assert_eq!(urls.as_deref(), Some("[\"/api/ping\"]"));
        assert_eq!(cz_modules::http_listening_count(), 0, "closed server must leave the registry");
        assert!(started.elapsed().as_millis() < 5000, "http loop did not exit: {:?}", started.elapsed());
    }

    #[test]
    fn quit_flag_stops_a_listening_loop_immediately() {
        let _lock = event_test_lock();
        cz_modules::reset_quit_flag();
        let (rt, ctx) = loop_setup();
        ctx.with(|c| {
            eval_module(
                &c,
                "inline_quit.mjs",
                r#"
                    import { createServer } from "node:http";
                    const server = createServer((req, res) => { res.end("x"); });
                    server.listen(0, "127.0.0.1");
                    globalThis.__srv = server;
                "#,
            )
            .unwrap();
        });
        drain_jobs(&rt);
        assert!(cz_modules::http_listening_count() > 0, "server should be listening");
        // Ctrl-C / SIGTERM semantics: setting the quit flag makes run_event_loop return at once
        // even though a server is still bound.
        cz_modules::request_quit();
        let started = std::time::Instant::now();
        run_event_loop(&rt, &ctx).unwrap();
        assert!(started.elapsed().as_millis() < 500, "quit flag must short-circuit the loop");
        // Close through JS with proper `this` binding (the shim's close() reads this._id).
        ctx.with(|c| -> rquickjs::Result<()> { c.eval::<(), _>("globalThis.__srv.close();") })
            .expect("close should succeed");
        let until_idle = std::time::Instant::now();
        while cz_modules::http_listening_count() > 0 && until_idle.elapsed().as_millis() < 1000 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(cz_modules::http_listening_count(), 0);
        cz_modules::reset_quit_flag();
    }

    // Compile-time usage guard for the console value plumbing types.
    const _CONSOLE_TYPES_OK: usize = std::mem::size_of::<rquickjs::Array<'static>>();
}
