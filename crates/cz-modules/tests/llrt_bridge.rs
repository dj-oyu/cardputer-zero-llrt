//! Black-box integration tests for the Phase 4A LLRT v0.10.0-beta integration surface exported
//! by `cz_modules`: root-level `ModuleDef` re-exports, `CARDPUTER_MODULE_NAMES`,
//! `pump_llrt_http`, and `has_active_http_servers`.

use rquickjs::{Context, Module, Runtime};

#[test]
fn llrt_public_defs_register_all_eight_modules_end_to_end() {
    // Every struct is importable at the crate ROOT (not just through submodules) and can be
    // declared + evaluated like any LLRT ModuleDef inside a fresh runtime (Phase 5 adds json).
    use cz_modules::{
        AppModule, CARDPUTER_MODULE_NAMES, DisplayModule, FsModule, HttpModule, InputModule,
        JsonModule, KvModule, ModuleKind, PowerModule, resolve_builtin,
    };

    assert_eq!(CARDPUTER_MODULE_NAMES.len(), 6);
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:display"));
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:kv"));
    assert!(CARDPUTER_MODULE_NAMES.contains(&"cardputer:json"));
    assert_eq!(resolve_builtin("node:http"), Some(ModuleKind::Http));
    assert_eq!(resolve_builtin("fs"), Some(ModuleKind::Fs));
    assert_eq!(resolve_builtin("cardputer:json"), Some(ModuleKind::Json));

    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    let out = ctx.with(|c| -> String {
        Module::declare_def::<DisplayModule, _>(c.clone(), "cardputer:display").expect("display");
        Module::declare_def::<InputModule, _>(c.clone(), "cardputer:input").expect("input");
        Module::declare_def::<AppModule, _>(c.clone(), "cardputer:app").expect("app");
        Module::declare_def::<PowerModule, _>(c.clone(), "cardputer:power").expect("power");
        Module::declare_def::<KvModule, _>(c.clone(), "cardputer:kv").expect("kv");
        Module::declare_def::<JsonModule, _>(c.clone(), "cardputer:json").expect("json");
        Module::declare_def::<HttpModule, _>(c.clone(), "node:http").expect("http");
        Module::declare_def::<FsModule, _>(c.clone(), "node:fs").expect("fs");
        let p = Module::evaluate(
            c.clone(),
            "llrt-bridge.mjs",
            r#"
            import * as display from "cardputer:display";
            import * as input from "cardputer:input";
            import * as app from "cardputer:app";
            import * as power from "cardputer:power";
            import * as kv from "cardputer:kv";
            import * as czJson from "cardputer:json";
            import * as http from "node:http";
            import * as fs from "node:fs";
            if (display.WIDTH !== 320 || display.HEIGHT !== 170) throw new Error("display dims");
            if (typeof input.poll !== "function") throw new Error("input.poll");
            if (typeof app.memoryUsage !== "function") throw new Error("app.memoryUsage");
            if (typeof power.status !== "function") throw new Error("power.status");
            const s = kv.open("llrt_bridge_store");
            s.set("k", 7);
            if (s.get("k") !== 7) throw new Error("kv roundtrip");
            if (typeof http.createServer !== "function" || !Array.isArray(http.METHODS)) throw new Error("http shape");
            if (typeof fs.readFileSync !== "function" || typeof fs.existsSync !== "function") throw new Error("fs shape");
            if (typeof czJson.parseLazy !== "function" || typeof czJson.isLazy !== "function" || typeof czJson.lazyStats !== "function") throw new Error("json shape");
            const lazy = czJson.parseLazy('{"a":1}');
            if (!czJson.isLazy(lazy) || lazy.a !== 1) throw new Error("json lazy handle");
            globalThis.__R = "LLRT_BRIDGE_ALL_OK";
            "#,
        )
        .expect("evaluate");
        p.finish::<()>().expect("finish");
        c.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert_eq!(out, "LLRT_BRIDGE_ALL_OK");
}

#[test]
fn pump_llrt_http_and_has_active_http_servers_track_listen_then_close() {
    use cz_modules::{has_active_http_servers, http_listening_count, pump_llrt_http};
    cz_modules::http_module::close_all_servers();

    assert!(!has_active_http_servers(), "no listeners before listen");
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");

    // Listen phase: pump inside the scope reports liveness (server active).
    let listen_pump = ctx.with(|c| -> bool {
        Module::declare_def::<cz_modules::HttpModule, _>(c.clone(), "node:http").expect("decl");
        let p = Module::evaluate(
            c.clone(),
            "pump-live.mjs",
            r#"
            import { createServer } from "node:http";
            const srv = createServer((req, res) => { res.writeHead(200); res.end("ok"); });
            srv.listen(0, "127.0.0.1");
            globalThis.__srv = srv;
            "#,
        )
        .expect("eval");
        p.finish::<()>().expect("finish");
        pump_llrt_http(&c).expect("live pump")
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(listen_pump, "pump must report true while a server listens");
    assert!(has_active_http_servers());
    assert_eq!(http_listening_count(), 1);

    // Close phase: registry empties synchronously, pump goes idle again. Call close() in JS so
    // the server method keeps its `this` binding (a bare Rust handle call would lose it).
    ctx.with(|c| {
        let _: i64 = c
            .eval("globalThis.__srv.close(); 1")
            .expect("js close");
    });
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(!has_active_http_servers(), "closed server must leave the registry");

    let idle_pump = ctx.with(|c| pump_llrt_http(&c)).expect("idle pump scope result");
    while let Ok(true) = rt.execute_pending_job() {}
    assert!(!idle_pump, "no listeners left -> pump reports false");
}
